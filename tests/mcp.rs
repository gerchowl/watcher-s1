//! `watcher-s1 mcp` through the real binary, speaking JSON-RPC over its stdio.
//!
//! Every wait is bounded by a deadline and polls an observable fact (a
//! response, a notification, a file); nothing sleeps for a fixed time hoping
//! something happened. Jobs start and finish on gate files (a job announces
//! readiness by writing a file; it ends when the test creates another), never
//! on a timer the test races. Every line the server writes to stdout must be
//! a JSON-RPC message: the harness panics on anything else.
#![cfg(feature = "mcp")]

mod common;
use common::{BIN, alive};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};
use watcher_s1::control::{ControlServer, StatusReply};

/// Upper bound for any single wait; a healthy run needs a fraction of it.
const BOUND: Duration = Duration::from_secs(30);

struct Server {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<Value>,
    /// Notifications that arrived while waiting for something else.
    notes: Vec<Value>,
    /// Responses to requests nobody has asked for yet.
    responses: HashMap<u64, Value>,
    next_id: u64,
}

impl Server {
    fn spawn(state: &Path, extra: &[&str]) -> Server {
        Server::spawn_env(state, extra, &[])
    }

    fn spawn_env(state: &Path, extra: &[&str], env: &[(&str, &str)]) -> Server {
        let mut child = Command::new(BIN)
            .arg("mcp")
            .arg("--state-dir")
            .arg(state)
            .args(extra)
            .envs(env.iter().copied())
            .env_remove("SYSTEMONE_URL")
            .env_remove("WATCHER_S1_PARENT")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let out = child.stdout.take().unwrap();
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(out).lines() {
                let Ok(line) = line else { break };
                let v: Value = serde_json::from_str(&line)
                    .unwrap_or_else(|e| panic!("stdout carried a non-JSON line ({e}): {line}"));
                assert_eq!(v["jsonrpc"], "2.0", "not a JSON-RPC message: {line}");
                if tx.send(v).is_err() {
                    break;
                }
            }
        });
        let mut s = Server {
            child,
            stdin,
            rx,
            notes: Vec::new(),
            responses: HashMap::new(),
            next_id: 0,
        };
        s.rpc(
            "initialize",
            json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}}),
        );
        s.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        s
    }

    fn send(&mut self, v: &Value) {
        writeln!(self.stdin, "{v}").unwrap();
        self.stdin.flush().unwrap();
    }

    /// Next message, whichever it is, within `deadline`.
    fn recv(&mut self, deadline: Instant) -> Option<Value> {
        self.rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .ok()
    }

    /// Send a request without waiting; pair with [`Server::finish`].
    fn begin(&mut self, method: &str, params: Value) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        id
    }

    /// The `result` of request `id`'s response, within `deadline`.
    fn finish_by(&mut self, id: u64, what: &str, deadline: Instant) -> Option<Value> {
        loop {
            if let Some(m) = self.responses.remove(&id) {
                assert!(m.get("error").is_none(), "{what} failed: {m}");
                return Some(m["result"].clone());
            }
            let m = self.recv(deadline)?;
            match m["id"].as_u64() {
                Some(got) if m.get("method").is_none() => {
                    self.responses.insert(got, m);
                }
                _ if m.get("method").is_some() && m.get("id").is_none() => self.notes.push(m),
                _ => {}
            }
        }
    }

    fn finish(&mut self, id: u64, what: &str) -> Value {
        self.finish_by(id, what, Instant::now() + BOUND)
            .unwrap_or_else(|| panic!("no response to {what} in {BOUND:?}"))
    }

    /// One request; the `result` of its response. Notifications seen on the
    /// way are kept in `notes`.
    fn rpc(&mut self, method: &str, params: Value) -> Value {
        let id = self.begin(method, params);
        self.finish(id, method)
    }

    /// A tool call's `(isError, json)` out of an rpc result.
    fn unpack(r: &Value) -> (bool, Value) {
        let text = r["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("no text in {r}"));
        let is_err = r["isError"].as_bool().unwrap_or(false);
        let v = serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_owned()));
        (is_err, v)
    }

    /// Start a tool call without waiting.
    fn begin_call(&mut self, tool: &str, args: Value) -> u64 {
        self.begin("tools/call", json!({"name": tool, "arguments": args}))
    }

    /// `ping` must be answered within `within`, whatever else is going on.
    fn ping_within(&mut self, within: Duration) {
        let id = self.begin("ping", json!({}));
        self.finish_by(id, "ping", Instant::now() + within)
            .unwrap_or_else(|| panic!("ping was not answered within {within:?}: the server is blocked"));
    }

    /// A tool call: (isError, the JSON the tool put in its text content).
    fn call(&mut self, tool: &str, args: Value) -> (bool, Value) {
        let r = self.rpc("tools/call", json!({"name": tool, "arguments": args}));
        Server::unpack(&r)
    }

    /// A tool call that must succeed.
    fn ok(&mut self, tool: &str, args: Value) -> Value {
        let (err, v) = self.call(tool, args);
        assert!(!err, "{tool} reported an error: {v}");
        v
    }

    /// Start `argv` (no System One), returning the run id.
    fn start(&mut self, argv: &[&str], extra: Value) -> String {
        let mut args = json!({"cmd": argv, "s1": false});
        for (k, v) in extra.as_object().into_iter().flatten() {
            args[k] = v.clone();
        }
        self.ok("watch_start", args)["id"].as_str().unwrap().to_owned()
    }

    /// Wait for a notification matching `pred`, from `notes` or the wire.
    fn note_where(&mut self, pred: impl Fn(&Value) -> bool) -> Option<Value> {
        let deadline = Instant::now() + BOUND;
        loop {
            if let Some(i) = self.notes.iter().position(&pred) {
                return Some(self.notes.remove(i));
            }
            let m = self.recv(deadline)?;
            if m.get("method").is_some() && m.get("id").is_none() {
                self.notes.push(m);
            }
        }
    }

    /// Kill the server outright, as a crashed session would.
    fn kill(mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn wait_final(s: &mut Server, id: &str) -> Value {
    s.ok("watch_wait", json!({"id": id, "until": "final", "timeout_s": 25}))
}

fn final_event(v: &Value) -> &Value {
    assert_eq!(v["timed_out"], false, "{v}");
    let last = v["events"]
        .as_array()
        .unwrap()
        .last()
        .unwrap_or_else(|| panic!("no events: {v}"));
    assert!(!last["exit"].is_null(), "last event is not final: {last}");
    last
}

/// Poll until `f` holds, within `BOUND`.
fn until(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + BOUND;
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn control_sock(state: &Path, id: &str) -> PathBuf {
    state.join("runs").join(id).join("control.sock")
}

/// The run is over for real: `watch_status` says not alive, the final event
/// is in, and the control socket is gone (the watcher has exited). The final
/// event is written before the watcher exits, so asserting `alive: false`
/// any earlier is a race.
fn settled(s: &mut Server, state: &Path, id: &str) {
    until("the watcher to exit", || {
        let st = s.ok("watch_status", json!({"id": id}));
        st["alive"] == false && st["state"] == "finished" && !control_sock(state, id).exists()
    });
}

/// A shell job that announces readiness by creating `ready`, then waits for
/// `gate` and exits with `code`.
fn gated(dir: &Path, name: &str, code: i32) -> (Vec<String>, PathBuf, PathBuf) {
    let (ready, gate) = (dir.join(format!("{name}.ready")), dir.join(format!("{name}.gate")));
    let script = format!(
        "echo up > '{}'; while [ ! -e '{}' ]; do sleep 0.05; done; exit {code}",
        ready.display(),
        gate.display()
    );
    (vec!["sh".into(), "-c".into(), script], ready, gate)
}

fn wait_file(p: &Path) {
    until(&format!("{} to exist", p.display()), || p.exists());
}

fn argv(v: &[String]) -> Vec<&str> {
    v.iter().map(String::as_str).collect()
}

fn ev(run: &str, state: &str, reason: &str, exit: Option<i32>) -> String {
    json!({
        "run_id": run, "state": state, "reason": reason, "severity": "info",
        "exit": exit.map(|c| json!({"code": c, "signal": null})), "evidence_tail": "t\n",
    })
    .to_string()
        + "\n"
}

/// A run directory made by hand (what an earlier server, or an attacker,
/// could leave): `meta.json` and nothing else.
fn fixture(state: &Path, id: &str, meta_extra: Value) -> PathBuf {
    let dir = state.join("runs").join(id);
    std::fs::create_dir_all(&dir).unwrap();
    let mut meta = json!({
        "id": id, "cmd": ["fixture"], "cwd": "/", "started": "2026-01-01T00:00:00.000Z",
        "started_ms": 1_700_000_000_000u64, "watcher_pid": 0, "options": {}, "status": "running",
    });
    for (k, v) in meta_extra.as_object().into_iter().flatten() {
        meta[k] = v.clone();
    }
    std::fs::write(dir.join("meta.json"), meta.to_string()).unwrap();
    dir
}

fn append(path: &Path, text: &str) {
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    f.write_all(text.as_bytes()).unwrap();
}

/// A stand-in watcher: a control socket that answers `status` (job pid 4242)
/// until dropped, so a fixture run reads as alive. Answers `stop` by
/// recording it.
struct FakeWatcher {
    stop: Arc<AtomicBool>,
    stops: Arc<std::sync::Mutex<Vec<Duration>>>,
    t: Option<std::thread::JoinHandle<()>>,
}

impl FakeWatcher {
    fn new(sock: &Path) -> FakeWatcher {
        let mut server = ControlServer::bind(sock).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stops = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (flag, log) = (stop.clone(), stops.clone());
        let t = std::thread::spawn(move || {
            while !flag.load(Ordering::SeqCst) {
                let status = || StatusReply {
                    pid: Some(4242),
                    pgid: Some(4242),
                    state: "progressing",
                    elapsed_ms: 1,
                };
                if let Some(g) = server.service(&status) {
                    log.lock().unwrap().push(g);
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        FakeWatcher {
            stop,
            stops,
            t: Some(t),
        }
    }
}

impl Drop for FakeWatcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.t.take() {
            let _ = t.join();
        }
    }
}

#[test]
fn handshake_tools_guide_and_exit_code() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Server::spawn(dir.path(), &[]);

    // Capabilities: tools and resources, and no channel unless asked.
    let init = s.rpc(
        "initialize",
        json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}),
    );
    assert_eq!(init["serverInfo"]["name"], "watcher-s1");
    assert!(init["capabilities"]["tools"].is_object() && init["capabilities"]["resources"].is_object());
    assert!(init["capabilities"].get("experimental").is_none(), "{init}");

    let tools = s.rpc("tools/list", json!({}));
    let mut names: Vec<&str> = tools["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    names.sort();
    assert_eq!(
        names,
        ["watch_list", "watch_start", "watch_status", "watch_stop", "watch_wait"]
    );
    for t in tools["tools"].as_array().unwrap() {
        assert!(t["description"].as_str().is_some_and(|d| d.len() > 20), "{t}");
        assert_eq!(t["inputSchema"]["type"], "object");
    }

    let listed = s.rpc("resources/list", json!({}));
    assert_eq!(listed["resources"][0]["uri"], "watcher-s1://guide");
    let guide = s.rpc("resources/read", json!({"uri": "watcher-s1://guide"}));
    assert_eq!(guide["contents"][0]["mimeType"], "text/markdown");
    assert_eq!(guide["contents"][0]["text"], watcher_s1::GUIDE);

    let id = s.start(&["sh", "-c", "echo hi; exit 3"], json!({}));
    let waited = wait_final(&mut s, &id);
    let fin = final_event(&waited);
    assert_eq!(fin["exit"]["code"], 3, "{waited}");
    assert_eq!(fin["state"], "failing");
    assert!(waited["lines"][0].as_str().unwrap().contains("exit=3"), "{waited}");
    assert_eq!(waited["state"], "finished");

    // Run directory layout.
    let run = dir.path().join("runs").join(&id);
    for f in ["events.jsonl", "output.log", "meta.json"] {
        assert!(run.join(f).is_file(), "{f}");
    }
    let log = std::fs::read_to_string(run.join("output.log")).unwrap();
    assert_eq!(log.trim(), "hi", "the log holds the job's output only");
    let meta: Value = serde_json::from_str(&std::fs::read_to_string(run.join("meta.json")).unwrap()).unwrap();
    assert_eq!(meta["cmd"], json!(["sh", "-c", "echo hi; exit 3"]));
    assert!(meta["cwd"].is_string() && meta["started"].is_string() && meta["watcher_pid"].is_u64());
    assert_eq!(meta["status"], "running");

    // Status and list agree, and a second wait repeats the verdict.
    settled(&mut s, dir.path(), &id);
    let st = s.ok("watch_status", json!({"id": id}));
    assert_eq!(st["exit"]["code"], 3);
    let list = s.ok("watch_list", json!({}));
    assert_eq!(list["runs"][0]["id"], id.as_str());
    let again = wait_final(&mut s, &id);
    assert_eq!(again["already_seen"], true, "{again}");
    assert_eq!(final_event(&again)["exit"]["code"], 3);
}

#[test]
fn tool_errors_are_reported_to_the_model() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Server::spawn(dir.path(), &[]);
    for (tool, args) in [
        ("watch_wait", json!({"id": "nope", "until": "final"})),
        ("watch_wait", json!({"id": "../etc", "until": "next"})),
        ("watch_status", json!({"id": "nope"})),
        ("watch_stop", json!({"id": "nope"})),
        ("watch_start", json!({"cmd": []})),
        ("watch_start", json!({"cmd": ["true"], "cwd": "/no/such/dir"})),
        ("watch_start", json!({"cmd": ["true"], "heartbeat": "0.2s"})),
        ("watch_start", json!({"cmd": ["true"], "timeout": "soon"})),
    ] {
        let (err, v) = s.call(tool, args.clone());
        assert!(err, "{tool} {args} should fail, got {v}");
    }
    // Nothing was left behind by the refused starts.
    assert!(s.ok("watch_list", json!({}))["runs"].as_array().unwrap().is_empty());

    let id = s.start(&["true"], json!({}));
    wait_final(&mut s, &id);
    let (err, v) = s.call("watch_stop", json!({"id": id}));
    assert!(err && v.as_str().unwrap().contains("already finished"), "{v}");
    let id = s.start(&["sleep", "30"], json!({}));
    let (err, v) = s.call("watch_stop", json!({"id": id, "signal": "KILL"}));
    assert!(err && v.as_str().unwrap().contains("never SIGKILL"), "{v}");
    s.ok("watch_stop", json!({"id": id, "grace_s": 20}));
}

#[test]
fn runs_outlive_the_server_and_a_new_one_can_watch_them() {
    let dir = tempfile::tempdir().unwrap();
    let (job, _ready, release) = gated(dir.path(), "outlive", 5);

    let mut a = Server::spawn(dir.path(), &[]);
    let id = a.start(&argv(&job), json!({"heartbeat": "1s"}));
    let st = a.ok("watch_status", json!({"id": id}));
    assert_eq!(st["state"], "running", "{st}");
    let pid = st["watcher_pid"].as_u64().unwrap() as i32;
    // Hand out a heartbeat so the cursor is not at zero when the server dies.
    let next = a.ok("watch_wait", json!({"id": id, "until": "next", "timeout_s": 25}));
    assert_eq!(next["events"][0]["reason"], "heartbeat", "{next}");
    assert_eq!(next["events"][0]["seq"], 1);
    a.kill();
    assert!(alive(pid), "the watcher died with the server");

    let mut b = Server::spawn(dir.path(), &[]);
    let st = b.ok("watch_status", json!({"id": id}));
    assert_eq!(
        (st["state"].as_str(), st["alive"].as_bool()),
        (Some("running"), Some(true)),
        "{st}"
    );
    assert!(b.ok("watch_list", json!({}))["runs"][0]["id"] == id.as_str());
    // The cursor survived: the first heartbeat is not handed out twice.
    let next = b.ok("watch_wait", json!({"id": id, "until": "next", "timeout_s": 25}));
    assert!(next["events"][0]["seq"].as_u64().unwrap() >= 2, "{next}");

    std::fs::write(&release, "").unwrap();
    let waited = wait_final(&mut b, &id);
    let fin = final_event(&waited);
    assert_eq!(fin["exit"]["code"], 5, "{waited}");
    assert_eq!(waited["state"], "finished");
    // Only unseen events plus the verdict come back, heartbeats counted.
    assert!(
        waited["events"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["reason"] != "heartbeat"),
        "{waited}"
    );
    settled(&mut b, dir.path(), &id);
    until("the watcher to be reaped", || !alive(pid));
}

#[test]
fn next_hands_out_each_event_once() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Server::spawn(dir.path(), &[]);
    // Silent for longer than --silence: stalled, then (once released) the final event.
    let (job, _ready, gate) = gated(dir.path(), "next", 0);
    let id = s.start(&argv(&job), json!({"silence": "1s"}));
    let first = s.ok("watch_wait", json!({"id": id, "until": "next", "timeout_s": 25}));
    assert_eq!(first["events"][0]["state"], "stalled", "{first}");
    assert_eq!(first["events"][0]["seq"], 1);
    assert_eq!(first["timed_out"], false);
    // The job is held at its gate, so a short wait on it must time out, and say so.
    let quiet = s.ok("watch_wait", json!({"id": id, "until": "final", "timeout_s": 0.2}));
    assert_eq!(quiet["timed_out"], true, "{quiet}");
    assert!(quiet["events"].as_array().unwrap().is_empty());
    std::fs::write(&gate, "").unwrap();
    let second = s.ok("watch_wait", json!({"id": id, "until": "next", "timeout_s": 25}));
    let fin = final_event(&second);
    assert_eq!(
        (fin["seq"].as_u64(), fin["state"].as_str()),
        (Some(2), Some("done")),
        "{second}"
    );
    // Past the end, `next` answers with the verdict again instead of hanging.
    let after = s.ok("watch_wait", json!({"id": id, "until": "next", "timeout_s": 5}));
    assert_eq!(after["already_seen"], true, "{after}");
    settled(&mut s, dir.path(), &id);
}

#[test]
fn stop_escalates_to_sigkill_on_the_job_group() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Server::spawn(dir.path(), &[]);
    // The shell ignores TERM and so do its children: only SIGKILL ends it.
    // `ready` is written after the trap is in place, so the stop cannot race it.
    let ready = dir.path().join("ready");
    let script = format!(
        "trap '' TERM; echo up > '{}'; while :; do sleep 0.2; done",
        ready.display()
    );
    let id = s.start(&["sh", "-c", &script], json!({"silence": 0}));
    wait_file(&ready);
    let t0 = Instant::now();
    let stopped = s.ok("watch_stop", json!({"id": id, "grace_s": 1}));
    assert_eq!(stopped["escalated_to_sigkill"], true, "{stopped}");
    assert_eq!(stopped["ended"], true, "{stopped}");
    assert_eq!(stopped["final"]["exit"]["signal"], 9, "{stopped}");
    assert_eq!(stopped["final"]["state"], "failing");
    assert_eq!(stopped["final"]["reason"], "stopped");
    assert!(stopped["line"].as_str().unwrap().contains("signal=9"));
    assert!(t0.elapsed() >= Duration::from_secs(1), "TERM got no grace");
    settled(&mut s, dir.path(), &id);
}

#[test]
fn stop_with_term_needs_no_escalation_for_a_polite_job() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Server::spawn(dir.path(), &[]);
    let id = s.start(&["sleep", "60"], json!({}));
    let stopped = s.ok("watch_stop", json!({"id": id}));
    assert_eq!(stopped["escalated_to_sigkill"], false, "{stopped}");
    assert_eq!(stopped["final"]["exit"]["signal"], 15, "{stopped}");
    assert_eq!(stopped["final"]["reason"], "stopped", "{stopped}");
    assert!(stopped["pgid"].as_i64().is_some_and(|p| p > 1), "{stopped}");
    settled(&mut s, dir.path(), &id);
}

#[test]
fn stop_kills_a_descendant_that_ignores_term_even_when_the_leader_exits_early() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Server::spawn(dir.path(), &[]);
    // The leader dies on TERM; its same-group child ignores TERM and records
    // its pid only after the trap is installed.
    let pidfile = dir.path().join("grandchild.pid");
    let script = format!(
        "sh -c \"trap '' TERM; echo \\$\\$ > '{}'; while :; do sleep 0.2; done\" & while :; do sleep 0.2; done",
        pidfile.display()
    );
    let id = s.start(&["sh", "-c", &script], json!({"silence": 0}));
    wait_file(&pidfile);
    until("the pid to be written", || {
        std::fs::read_to_string(&pidfile).is_ok_and(|t| t.trim().parse::<i32>().is_ok())
    });
    let grandchild: i32 = std::fs::read_to_string(&pidfile).unwrap().trim().parse().unwrap();
    assert!(alive(grandchild));
    let stopped = s.ok("watch_stop", json!({"id": id, "grace_s": 30}));
    assert_eq!(stopped["ended"], true, "{stopped}");
    assert_eq!(stopped["final"]["reason"], "stopped", "{stopped}");
    assert_eq!(
        stopped["final"]["exit"]["signal"], 15,
        "the leader obeyed TERM: {stopped}"
    );
    assert!(
        stopped["waited_ms"].as_u64().unwrap() < 20_000,
        "the supervisor must not wait out the grace once the leader is gone: {stopped}"
    );
    until("the TERM-ignoring descendant to die", || !alive(grandchild));
    settled(&mut s, dir.path(), &id);
}

#[test]
fn a_killed_watcher_is_reported_lost_not_waited_on() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Server::spawn(dir.path(), &[]);
    let id = s.start(&["sleep", "60"], json!({}));
    // The job's group, from the supervisor itself (it answers once the child runs).
    let mut st = Value::Null;
    until("the job to have a process group", || {
        st = s.ok("watch_status", json!({"id": id}));
        st["job_pgid"].as_i64().is_some_and(|p| p > 1)
    });
    let pid = st["watcher_pid"].as_i64().unwrap() as i32;
    let pgid = st["job_pgid"].as_i64().unwrap() as i32;
    // The watcher goes without a trace (what `watch_stop` must never do).
    unsafe {
        libc::kill(pid, libc::SIGKILL);
        libc::kill(-pgid, libc::SIGKILL);
    }
    let waited = s.ok("watch_wait", json!({"id": id, "until": "final", "timeout_s": 25}));
    assert_eq!(
        (waited["state"].as_str(), waited["timed_out"].as_bool()),
        (Some("lost"), Some(false)),
        "{waited}"
    );
    assert!(waited["note"].is_string());
    assert_eq!(s.ok("watch_status", json!({"id": id}))["state"], "lost");
    // The socket file is stale and nothing listens; stop refuses cleanly.
    let (err, v) = s.call("watch_stop", json!({"id": id}));
    assert!(err && v.as_str().unwrap().contains("gone"), "{v}");
}

#[test]
fn old_runs_are_pruned_at_start_and_live_ones_kept() {
    let dir = tempfile::tempdir().unwrap();
    let old = fixture(dir.path(), "1-0", json!({"started_ms": 1}));
    let mut s = Server::spawn(dir.path(), &[]);
    assert!(!old.exists(), "a run from 1970 survived the prune");
    assert!(s.ok("watch_list", json!({}))["runs"].as_array().unwrap().is_empty());
}

#[test]
fn the_server_keeps_pruning_while_it_runs_but_never_an_active_run() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Server::spawn_env(dir.path(), &[], &[("WATCHER_S1_MCP_PRUNE_SECS", "1")]);
    let dead = fixture(dir.path(), "1-0", json!({"started_ms": 1}));
    let active = fixture(dir.path(), "2-0", json!({"started_ms": 2}));
    let _watcher = FakeWatcher::new(&control_sock(dir.path(), "2-0"));
    until("the periodic prune to remove the dead old run", || !dead.exists());
    assert!(active.exists(), "an active run must never be pruned, however old");
    let runs = s.ok("watch_list", json!({}));
    assert_eq!(runs["runs"][0]["id"], "2-0", "{runs}");
}

#[test]
fn a_stale_pid_is_never_signalled() {
    // After a crash and PID reuse, an old watcher_pid names an unrelated
    // process. Nothing may treat it as the watcher: not status, not stop.
    let dir = tempfile::tempdir().unwrap();
    let mut bystander = Command::new("sleep").arg("60").spawn().unwrap();
    let pid = bystander.id();
    fixture(
        dir.path(),
        "1-0",
        json!({"watcher_pid": pid, "started_ms": common_now_ms()}),
    );
    let mut s = Server::spawn(dir.path(), &[]);
    let st = s.ok("watch_status", json!({"id": "1-0"}));
    assert_eq!(
        (st["state"].as_str(), st["alive"].as_bool()),
        (Some("lost"), Some(false)),
        "{st}"
    );
    let (err, v) = s.call("watch_stop", json!({"id": "1-0"}));
    assert!(err, "{v}");
    let waited = s.ok("watch_wait", json!({"id": "1-0", "until": "final", "timeout_s": 5}));
    assert_eq!(waited["state"], "lost", "{waited}");
    assert!(alive(pid as i32), "the bystander was signalled");
    // Forged group fields in the events change nothing either.
    let forged =
        json!({"run_id": "r", "state": "progressing", "reason": "heartbeat", "pgid": 1, "pid": 1, "exit": null});
    append(&dir.path().join("runs/1-0/events.jsonl"), &format!("{forged}\n"));
    let (err, _) = s.call("watch_stop", json!({"id": "1-0"}));
    assert!(err);
    assert!(alive(pid as i32));
    bystander.kill().unwrap();
    bystander.wait().unwrap();
}

fn common_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

#[test]
fn a_forged_meta_cannot_move_state_outside_the_run_directory() {
    let dir = tempfile::tempdir().unwrap();
    let outside = dir.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("cursor"), "DO NOT OVERWRITE").unwrap();
    let run = fixture(dir.path(), "stale", json!({"id": "../outside"}));
    append(&run.join("events.jsonl"), &ev("r", "done", "exit", Some(0)));
    let mut s = Server::spawn(dir.path(), &[]);
    for args in [
        ("watch_wait", json!({"id": "stale", "until": "final", "timeout_s": 0})),
        ("watch_status", json!({"id": "stale"})),
        ("watch_stop", json!({"id": "stale"})),
    ] {
        let (err, v) = s.call(args.0, args.1.clone());
        assert!(err && v.as_str().unwrap().contains("refusing"), "{}: {v}", args.0);
    }
    // An absolute id, and `..` ids, are not run ids at all.
    for id in ["/etc", "../outside", "..", "a/../b"] {
        let (err, _) = s.call("watch_wait", json!({"id": id, "until": "final", "timeout_s": 0}));
        assert!(err, "{id}");
    }
    assert_eq!(
        std::fs::read_to_string(outside.join("cursor")).unwrap(),
        "DO NOT OVERWRITE"
    );
    assert!(!outside.join("cursor.lock").exists());
}

#[test]
fn symlinked_run_directories_and_cursors_are_not_followed() {
    let dir = tempfile::tempdir().unwrap();
    // A run directory that is a symlink to a directory holding a valid run.
    let target = fixture(dir.path(), "target", json!({}));
    append(&target.join("events.jsonl"), &ev("r", "done", "exit", Some(0)));
    std::os::unix::fs::symlink(&target, dir.path().join("runs/linked")).unwrap();
    // A real run whose cursor is a symlink to an outside file.
    let real = fixture(dir.path(), "real", json!({}));
    append(&real.join("events.jsonl"), &ev("r", "done", "exit", Some(0)));
    let victim = dir.path().join("victim");
    std::fs::write(&victim, "DO NOT OVERWRITE").unwrap();
    std::os::unix::fs::symlink(&victim, real.join("cursor")).unwrap();
    let mut s = Server::spawn(dir.path(), &[]);
    let (err, v) = s.call("watch_wait", json!({"id": "linked", "until": "final", "timeout_s": 0}));
    assert!(err, "{v}");
    let (_err, _v) = s.call("watch_wait", json!({"id": "real", "until": "final", "timeout_s": 0}));
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "DO NOT OVERWRITE");
}

#[test]
fn a_fifo_meta_json_cannot_hang_the_server() {
    let dir = tempfile::tempdir().unwrap();
    let run = dir.path().join("runs/fifo");
    std::fs::create_dir_all(&run).unwrap();
    nix::unistd::mkfifo(&run.join("meta.json"), nix::sys::stat::Mode::from_bits_truncate(0o600)).unwrap();
    // Even startup (which prunes) must get past it.
    let mut s = Server::spawn(dir.path(), &[]);
    let t0 = Instant::now();
    let (err, _) = s.call("watch_status", json!({"id": "fifo"}));
    assert!(err);
    assert!(s.ok("watch_list", json!({}))["runs"].as_array().unwrap().is_empty());
    s.ping_within(Duration::from_secs(5));
    assert!(t0.elapsed() < Duration::from_secs(10));
}

#[test]
fn the_server_answers_a_ping_during_a_long_wait_and_a_long_stop() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Server::spawn(dir.path(), &[]);
    let (job, ready, gate) = gated(dir.path(), "ping", 0);
    let waiting = s.start(&argv(&job), json!({"silence": 0}));
    wait_file(&ready);
    let w = s.begin_call("watch_wait", json!({"id": waiting, "until": "final", "timeout_s": 25}));
    for _ in 0..5 {
        s.ping_within(Duration::from_secs(2));
        std::thread::sleep(Duration::from_millis(100));
    }
    std::fs::write(&gate, "").unwrap();
    let (err, v) = Server::unpack(&s.finish(w, "watch_wait"));
    assert!(!err && v["timed_out"] == false, "{v}");

    // A stop that takes its whole grace: the job ignores TERM.
    let trap_ready = dir.path().join("trap.ready");
    let script = format!(
        "trap '' TERM; echo up > '{}'; while :; do sleep 0.2; done",
        trap_ready.display()
    );
    let id = s.start(&["sh", "-c", &script], json!({"silence": 0}));
    wait_file(&trap_ready);
    let stop = s.begin_call("watch_stop", json!({"id": id, "grace_s": 3}));
    for _ in 0..5 {
        s.ping_within(Duration::from_secs(2));
        std::thread::sleep(Duration::from_millis(100));
    }
    let (err, v) = Server::unpack(&s.finish(stop, "watch_stop"));
    assert!(!err && v["escalated_to_sigkill"] == true, "{v}");
}

#[test]
fn huge_and_unterminated_lines_are_bounded_and_do_not_stall_the_server() {
    let dir = tempfile::tempdir().unwrap();
    let run = fixture(dir.path(), "1-0", json!({"started_ms": common_now_ms()}));
    let _watcher = FakeWatcher::new(&control_sock(dir.path(), "1-0"));
    // 24 MiB with no newline at all, then (later) a good event.
    let events = run.join("events.jsonl");
    {
        let mut f = std::fs::File::create(&events).unwrap();
        let chunk = vec![b'x'; 1 << 20];
        for _ in 0..24 {
            f.write_all(&chunk).unwrap();
        }
    }
    let mut s = Server::spawn(dir.path(), &[]);
    let t0 = Instant::now();
    let w = s.ok("watch_wait", json!({"id": "1-0", "until": "next", "timeout_s": 0}));
    assert_eq!(w["timed_out"], true, "{w}");
    assert!(t0.elapsed() < Duration::from_secs(10), "{:?}", t0.elapsed());
    s.ping_within(Duration::from_secs(2));
    // Finish the junk line, then a real event: it is found, the junk skipped.
    append(&events, &format!("\n{}", ev("r", "stalled", "silence", None)));
    let w = s.ok("watch_wait", json!({"id": "1-0", "until": "next", "timeout_s": 10}));
    assert_eq!(w["events"][0]["reason"], "silence", "{w}");
    let st = s.ok("watch_status", json!({"id": "1-0"}));
    assert_eq!(st["events"], 1, "{st}");
}

#[test]
fn responses_are_capped_and_a_sustained_producer_cannot_hold_a_wait() {
    let dir = tempfile::tempdir().unwrap();
    let run = fixture(dir.path(), "1-0", json!({"started_ms": common_now_ms()}));
    let _watcher = FakeWatcher::new(&control_sock(dir.path(), "1-0"));
    let events = run.join("events.jsonl");
    // A history much bigger than one read pass: 150 000 heartbeats (~25 MiB).
    {
        let mut f = std::fs::File::create(&events).unwrap();
        let line = ev("r", "progressing", "heartbeat", None);
        let block = line.repeat(1000);
        for _ in 0..150 {
            f.write_all(block.as_bytes()).unwrap();
        }
    }
    // A producer that keeps appending for the whole test.
    let stop = Arc::new(AtomicBool::new(false));
    let producer = {
        let (stop, events) = (stop.clone(), events.clone());
        std::thread::spawn(move || {
            let block = ev("r", "progressing", "heartbeat", None).repeat(2000);
            // Bounded volume (about 65 MiB): how much history the summary
            // has to digest afterwards must not depend on how fast the
            // machine runs the rest of the test.
            for _ in 0..200 {
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                append(&events, &block);
                std::thread::sleep(Duration::from_millis(2));
            }
        })
    };
    let mut s = Server::spawn(dir.path(), &[]);
    for mode in ["next", "final"] {
        let t0 = Instant::now();
        let w = s.ok("watch_wait", json!({"id": "1-0", "until": mode, "timeout_s": 0}));
        assert!(t0.elapsed() < Duration::from_secs(10), "{mode}: {:?}", t0.elapsed());
        assert!(w["events"].as_array().unwrap().len() <= 100, "{mode}");
        assert!(w["lines"].to_string().len() < 256 * 1024);
        if mode == "next" {
            assert_eq!(w["events"].as_array().unwrap().len(), 100, "{w}");
            assert_eq!(w["more"], true, "{w}");
        } else {
            assert_eq!(w["timed_out"], true, "{w}");
        }
        s.ping_within(Duration::from_secs(2));
    }
    stop.store(true, Ordering::SeqCst);
    producer.join().unwrap();
    // Each status call scans a bounded amount and remembers where it got to:
    // repeated calls converge on the whole history.
    // Wait on progress, not on a wall-clock guess: each call must advance the
    // saved position (a stall fails after `BOUND` without progress); how long
    // the whole history takes depends on the machine, and is not what is tested.
    let (mut last, mut seen_at) = (0, Instant::now());
    loop {
        let st = s.ok("watch_status", json!({"id": "1-0"}));
        let n = st["events"].as_u64().unwrap();
        if n >= 150_000 && st.get("partial").is_none() {
            break;
        }
        if n > last {
            (last, seen_at) = (n, Instant::now());
        }
        assert!(seen_at.elapsed() < BOUND, "the summary stalled at {n} events: {st}");
    }
    let size = std::fs::metadata(dir.path().join("runs/1-0/summary")).unwrap().len();
    assert!(size < 64 * 1024, "the summary stays small: {size}");
}

#[test]
fn many_edge_events_arrive_in_capped_batches_without_loss() {
    let dir = tempfile::tempdir().unwrap();
    let run = fixture(dir.path(), "1-0", json!({"started_ms": common_now_ms()}));
    let _watcher = FakeWatcher::new(&control_sock(dir.path(), "1-0"));
    let n = 250;
    append(
        &run.join("events.jsonl"),
        &ev("r", "stalled", "silence", None).repeat(n),
    );
    let mut s = Server::spawn(dir.path(), &[]);
    let mut seqs = Vec::new();
    while seqs.len() < n {
        let w = s.ok("watch_wait", json!({"id": "1-0", "until": "next", "timeout_s": 5}));
        let got: Vec<u64> = w["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["seq"].as_u64().unwrap())
            .collect();
        assert!(!got.is_empty() && got.len() <= 100, "{w}");
        seqs.extend(got);
    }
    assert_eq!(seqs, (1..=n as u64).collect::<Vec<_>>());
}

#[test]
fn overlapping_waits_get_distinct_events_even_across_two_servers() {
    let dir = tempfile::tempdir().unwrap();
    let run = fixture(dir.path(), "1-0", json!({"started_ms": common_now_ms()}));
    let _watcher = FakeWatcher::new(&control_sock(dir.path(), "1-0"));
    let events = run.join("events.jsonl");
    append(&events, "");
    let mut a = Server::spawn(dir.path(), &[]);
    let mut b = Server::spawn(dir.path(), &[]);
    let a1 = a.begin_call("watch_wait", json!({"id": "1-0", "until": "next", "timeout_s": 25}));
    let a2 = a.begin_call("watch_wait", json!({"id": "1-0", "until": "next", "timeout_s": 25}));
    let b1 = b.begin_call("watch_wait", json!({"id": "1-0", "until": "next", "timeout_s": 25}));
    // Let all three be waiting (a ping to each proves the request before it was read).
    a.ping_within(Duration::from_secs(5));
    b.ping_within(Duration::from_secs(5));
    for i in 0..3 {
        append(&events, &ev("r", "stalled", &format!("silence{i}"), None));
        std::thread::sleep(Duration::from_millis(150));
    }
    let mut got: Vec<u64> = Vec::new();
    let mut results = vec![a.finish(a1, "watch_wait"), a.finish(a2, "watch_wait")];
    results.push(b.finish(b1, "watch_wait"));
    for r in &results {
        let (err, v) = Server::unpack(r);
        assert!(!err, "{v}");
        for e in v["events"].as_array().unwrap() {
            got.push(e["seq"].as_u64().unwrap());
        }
    }
    got.sort();
    got.dedup();
    let total: usize = got.len();
    assert_eq!(total, 3, "three waiters, three events, no duplicates: {got:?}");
}

#[test]
fn a_run_that_cannot_be_launched_is_recorded_as_failed_and_nothing_runs() {
    use std::os::unix::fs::PermissionsExt;
    if unsafe { libc::geteuid() } == 0 {
        return; // root ignores directory permissions
    }
    let dir = tempfile::tempdir().unwrap();
    let locked = dir.path().join("locked");
    std::fs::create_dir(&locked).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let marker = dir.path().join("marker");
    let mut s = Server::spawn(&dir.path().join("state"), &[]);
    let script = format!("touch '{}'", marker.display());
    let (err, v) = s.call(
        "watch_start",
        json!({"cmd": ["sh", "-c", script], "cwd": locked, "s1": false}),
    );
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(err && v.as_str().unwrap().contains("marked failed"), "{v}");
    let runs = s.ok("watch_list", json!({}));
    assert_eq!(runs["runs"][0]["state"], "failed", "{runs}");
    assert!(runs["runs"][0]["error"].is_string(), "{runs}");
    let waited = s.ok(
        "watch_wait",
        json!({"id": runs["runs"][0]["id"], "until": "final", "timeout_s": 5}),
    );
    assert_eq!(waited["state"], "failed", "{waited}");
    assert!(!marker.exists());
}

#[test]
fn long_state_directories_still_get_a_control_socket() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("s".repeat(100));
    let sockets = tempfile::tempdir().unwrap();
    let mut s = Server::spawn_env(&state, &[], &[("XDG_RUNTIME_DIR", sockets.path().to_str().unwrap())]);
    let (job, ready, gate) = gated(dir.path(), "long", 0);
    let id = s.start(&argv(&job), json!({"silence": 0}));
    wait_file(&ready);
    assert!(sockets.path().join("watcher-s1").is_dir(), "the short socket directory");
    let st = s.ok("watch_status", json!({"id": id}));
    assert_eq!(
        (st["state"].as_str(), st["alive"].as_bool()),
        (Some("running"), Some(true)),
        "{st}"
    );
    let stopped = s.ok("watch_stop", json!({"id": id}));
    assert_eq!(stopped["ended"], true, "{stopped}");
    let _ = gate;
}

#[test]
fn channel_pushes_edge_events_but_not_heartbeats() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Server::spawn(dir.path(), &["--channel"]);
    let init = s.rpc(
        "initialize",
        json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}),
    );
    assert_eq!(
        init["capabilities"]["experimental"]["claude/channel"],
        json!({}),
        "{init}"
    );
    assert!(
        init["instructions"]
            .as_str()
            .unwrap()
            .contains("<channel source=\"watcher-s1\"")
    );

    // Heartbeats every second over a silent job held at its gate: stalled at
    // 1s, the final event when the gate opens.
    let (job, _ready, gate) = gated(dir.path(), "chan", 0);
    let id = s.start(&argv(&job), json!({"silence": "1s", "heartbeat": "1s"}));
    let is_ours =
        |m: &Value| m["method"] == "notifications/claude/channel" && m["params"]["meta"]["run_id"] == id.as_str();
    let stalled = s.note_where(is_ours).expect("no channel notification for the stall");
    assert_eq!(stalled["params"]["meta"]["state"], "stalled", "{stalled}");
    assert_eq!(stalled["params"]["meta"]["reason"], "silence");
    assert!(
        stalled["params"]["content"]
            .as_str()
            .unwrap()
            .starts_with("stalled silence"),
        "{stalled}"
    );

    // At least one heartbeat has been produced (and, below, never pushed).
    until("a heartbeat after the stall", || {
        s.ok("watch_status", json!({"id": id}))["events"].as_u64().unwrap() >= 2
    });
    std::fs::write(&gate, "").unwrap();
    let fin = s
        .note_where(is_ours)
        .expect("no channel notification for the final event");
    assert_eq!(fin["params"]["meta"]["state"], "done", "{fin}");
    assert!(fin["params"]["content"].as_str().unwrap().contains("exit=0"), "{fin}");

    // Heartbeats were produced but never pushed.
    let waited = wait_final(&mut s, &id);
    assert!(waited["heartbeats_skipped"].as_u64().unwrap_or(0) >= 1, "{waited}");
    settled(&mut s, dir.path(), &id);
    let all: Vec<&Value> = s.notes.iter().filter(|m| is_ours(m)).collect();
    assert!(
        all.iter().all(|m| m["params"]["meta"]["reason"] != "heartbeat"),
        "{all:?}"
    );
}

#[test]
fn a_failing_final_event_is_pushed_with_its_exit() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Server::spawn(dir.path(), &["--channel"]);
    let id = s.start(&["sh", "-c", "echo boom; exit 7"], json!({}));
    let note = s
        .note_where(|m| m["method"] == "notifications/claude/channel" && m["params"]["meta"]["run_id"] == id.as_str())
        .expect("no channel notification");
    assert_eq!(note["params"]["meta"]["state"], "failing", "{note}");
    assert_eq!(note["params"]["meta"]["reason"], "exit");
    assert!(note["params"]["content"].as_str().unwrap().contains("exit=7"), "{note}");
}

#[test]
fn no_channel_flag_means_no_pushes() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Server::spawn(dir.path(), &[]);
    let id = s.start(&["sh", "-c", "exit 1"], json!({}));
    wait_final(&mut s, &id);
    assert!(
        s.notes.iter().all(|m| m["method"] != "notifications/claude/channel"),
        "{:?}",
        s.notes
    );
}

#[test]
fn watch_stop_sends_exactly_one_stop_with_the_grace_and_no_signal() {
    let dir = tempfile::tempdir().unwrap();
    let run = fixture(dir.path(), "1-0", json!({"started_ms": common_now_ms()}));
    let watcher = FakeWatcher::new(&control_sock(dir.path(), "1-0"));
    let events = run.join("events.jsonl");
    append(&events, &ev("r", "progressing", "heartbeat", None));
    // The stand-in supervisor "ends the job" once it has been asked to stop.
    let stops = watcher.stops.clone();
    let ender = std::thread::spawn(move || {
        until("the stop request", || !stops.lock().unwrap().is_empty());
        let fin = json!({"run_id": "r", "state": "failing", "reason": "stopped", "severity": "error",
            "exit": {"code": null, "signal": 15}, "evidence_tail": ""});
        append(&events, &format!("{fin}\n"));
    });
    let mut s = Server::spawn(dir.path(), &[]);
    let stopped = s.ok("watch_stop", json!({"id": "1-0", "grace_s": 3}));
    ender.join().unwrap();
    assert_eq!(stopped["ended"], true, "{stopped}");
    assert_eq!(stopped["final"]["reason"], "stopped", "{stopped}");
    assert_eq!(stopped["escalated_to_sigkill"], false);
    assert_eq!(
        stopped["pgid"], 4242,
        "the group comes from the supervisor, not the events: {stopped}"
    );
    assert_eq!(*watcher.stops.lock().unwrap(), [Duration::from_secs(3)]);
    // Only TERM is a thing.
    let (err, v) = s.call("watch_stop", json!({"id": "1-0", "signal": "INT"}));
    assert!(err && v.as_str().unwrap().contains("TERM"), "{v}");
}
