//! Shared helpers for the integration tests: run the real binary with an
//! isolated environment, collect its events, and fake a System One server.
#![allow(dead_code)]

use serde_json::Value;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const BIN: &str = env!("CARGO_BIN_EXE_watcher-s1");

pub struct Env {
    pub dir: tempfile::TempDir,
}

impl Env {
    pub fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    pub fn path(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    /// A command for the binary with no ambient System One config, a
    /// private breaker state dir, and events going to `events.jsonl`.
    pub fn cmd(&self) -> Command {
        let mut c = Command::new(BIN);
        c.env_remove("SYSTEMONE_URL")
            .env_remove("WATCHER_S1_PARENT")
            .env("XDG_CONFIG_HOME", self.path("xdg"))
            .env("WATCHER_S1_STATE_DIR", self.path("state"))
            .arg("--events")
            .arg(self.path("events.jsonl"))
            .stdin(Stdio::null());
        c
    }

    /// Write a config file and return its path.
    pub fn config(&self, toml: &str) -> PathBuf {
        let p = self.path("config.toml");
        std::fs::write(&p, toml).unwrap();
        p
    }

    pub fn events(&self) -> Vec<Value> {
        read_events(&self.path("events.jsonl"))
    }
}

pub fn read_events(p: &Path) -> Vec<Value> {
    let schema: Value = serde_json::from_str(watcher_s1::event::SCHEMA_JSON).unwrap();
    let v = jsonschema::validator_for(&schema).unwrap();
    std::fs::read_to_string(p)
        .unwrap_or_default()
        .lines()
        .map(|l| {
            let e: Value = serde_json::from_str(l).unwrap_or_else(|err| panic!("bad event line {l:?}: {err}"));
            let errs: Vec<String> = v.iter_errors(&e).map(|x| x.to_string()).collect();
            assert!(errs.is_empty(), "event violates schema: {errs:?}\n{e}");
            e
        })
        .collect()
}

pub struct Run {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: String,
    pub elapsed: Duration,
}

impl Run {
    pub fn out(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }
}

pub fn run(mut c: Command) -> Run {
    let t0 = Instant::now();
    let o = c.stdout(Stdio::piped()).stderr(Stdio::piped()).output().unwrap();
    Run {
        status: o.status,
        stdout: o.stdout,
        stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
        elapsed: t0.elapsed(),
    }
}

pub fn last(events: &[Value]) -> &Value {
    events.last().expect("at least one event")
}

pub fn states(events: &[Value]) -> Vec<String> {
    events
        .iter()
        .map(|e| format!("{}/{}", e["state"].as_str().unwrap(), e["reason"].as_str().unwrap()))
        .collect()
}

pub fn alive(pid: i32) -> bool {
    // A zombie still answers kill(0); treat it as gone.
    if unsafe { libc::kill(pid, 0) } != 0 {
        return false;
    }
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(s) => !s.rsplit(')').next().unwrap_or("").trim_start().starts_with('Z'),
        Err(_) => cfg!(not(target_os = "linux")),
    }
}

/// What the fake server does with each request.
#[derive(Clone)]
pub enum Reply {
    /// 200 with these `noul` answers.
    Answers {
        failing: f64,
        clean_done: f64,
    },
    Status(u16),
    /// Accept, then never answer.
    Hang,
}

/// A one-thread fake System One endpoint.
pub struct FakeS1 {
    pub url: String,
    pub hits: Arc<AtomicUsize>,
    pub bodies: Arc<Mutex<Vec<Value>>>,
}

impl FakeS1 {
    pub fn start(reply: Reply) -> Self {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}/v1/systemone", l.local_addr().unwrap().port());
        let hits = Arc::new(AtomicUsize::new(0));
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let (h, b) = (hits.clone(), bodies.clone());
        std::thread::spawn(move || {
            for s in l.incoming() {
                let Ok(mut s) = s else { continue };
                h.fetch_add(1, Ordering::SeqCst);
                let reply = reply.clone();
                let b = b.clone();
                std::thread::spawn(move || {
                    let body = read_request(&mut s);
                    if let Ok(v) = serde_json::from_slice::<Value>(&body) {
                        b.lock().unwrap().push(v);
                    }
                    let resp = match reply {
                        Reply::Hang => {
                            std::thread::sleep(Duration::from_secs(30));
                            return;
                        }
                        Reply::Status(code) => format!("HTTP/1.1 {code} Nope\r\nContent-Length: 0\r\n\r\n"),
                        Reply::Answers { failing, clean_done } => {
                            let j = serde_json::json!({
                                "model": "fake",
                                "answers": {
                                    "failing": {"type": "noul", "noul": failing},
                                    "clean_done": {"type": "noul", "noul": clean_done}
                                },
                                "usage": {"input_tokens": 1, "output_tokens": 1},
                                "latency_ms": 1.0
                            })
                            .to_string();
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{j}",
                                j.len()
                            )
                        }
                    };
                    let _ = s.write_all(resp.as_bytes());
                });
            }
        });
        Self { url, hits, bodies }
    }

    pub fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
}

fn read_request(s: &mut std::net::TcpStream) -> Vec<u8> {
    let mut raw = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = s.read(&mut buf).unwrap_or(0);
        if n == 0 {
            return Vec::new();
        }
        raw.extend_from_slice(&buf[..n]);
        if let Some(h) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&raw[..h]).to_ascii_lowercase();
            let len: usize = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:").map(|v| v.trim().parse().unwrap_or(0)))
                .unwrap_or(0);
            if raw.len() >= h + 4 + len {
                return raw[h + 4..h + 4 + len].to_vec();
            }
        }
    }
}

/// A URL nothing listens on (bound then dropped).
pub fn dead_url() -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    format!("http://127.0.0.1:{port}/v1/systemone")
}
