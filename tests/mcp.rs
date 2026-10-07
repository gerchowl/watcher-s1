//! `watcher-s1 mcp` through the real binary, speaking JSON-RPC over its stdio.
//!
//! Every wait is bounded by a deadline and polls an observable fact (a
//! response, a notification, a file); nothing sleeps for a fixed time hoping
//! something happened. Every line the server writes to stdout must be a
//! JSON-RPC message: the harness panics on anything else.
#![cfg(feature = "mcp")]

mod common;
use common::BIN;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

/// Upper bound for any single wait; a healthy run needs a fraction of it.
const BOUND: Duration = Duration::from_secs(30);

struct Server {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<Value>,
    /// Notifications that arrived while waiting for something else.
    notes: Vec<Value>,
    next_id: u64,
}

impl Server {
    fn spawn(state: &Path, extra: &[&str]) -> Server {
        let mut child = Command::new(BIN)
            .arg("mcp")
            .arg("--state-dir")
            .arg(state)
            .args(extra)
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

    /// One request; the `result` of its response. Notifications seen on the
    /// way are kept in `notes`.
    fn rpc(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let deadline = Instant::now() + BOUND;
        loop {
            let m = self
                .recv(deadline)
                .unwrap_or_else(|| panic!("no response to {method} in {BOUND:?}"));
            if m["id"] == json!(id) {
                assert!(m.get("error").is_none(), "{method} failed: {m}");
                return m["result"].clone();
            }
            if m.get("method").is_some() && m.get("id").is_none() {
                self.notes.push(m);
            }
        }
    }

    /// A tool call: (isError, the JSON the tool put in its text content).
    fn call(&mut self, tool: &str, args: Value) -> (bool, Value) {
        let r = self.rpc("tools/call", json!({"name": tool, "arguments": args}));
        let text = r["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("no text in {r}"));
        let is_err = r["isError"].as_bool().unwrap_or(false);
        let v = serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_owned()));
        (is_err, v)
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
        std::thread::sleep(Duration::from_millis(50));
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

    // Status and list agree, and a second wait repeats the verdict.
    let st = s.ok("watch_status", json!({"id": id}));
    assert_eq!(
        (st["state"].as_str(), st["alive"].as_bool()),
        (Some("finished"), Some(false)),
        "{st}"
    );
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
    let release = dir.path().join("release");
    let script = format!(
        "while [ ! -e '{}' ]; do sleep 0.1; done; echo released; exit 5",
        release.display()
    );

    let mut a = Server::spawn(dir.path(), &[]);
    let id = a.start(&["sh", "-c", &script], json!({"heartbeat": "1s"}));
    let st = a.ok("watch_status", json!({"id": id}));
    assert_eq!(st["state"], "running", "{st}");
    let pid = st["watcher_pid"].as_u64().unwrap() as u32;
    // Hand out a heartbeat so the cursor is not at zero when the server dies.
    let next = a.ok("watch_wait", json!({"id": id, "until": "next", "timeout_s": 25}));
    assert_eq!(next["events"][0]["reason"], "heartbeat", "{next}");
    assert_eq!(next["events"][0]["seq"], 1);
    a.kill();
    assert!(
        watcher_s1::mcp::runs::pid_alive(pid),
        "the watcher died with the server"
    );

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
    until("the watcher to be reaped", || !watcher_s1::mcp::runs::pid_alive(pid));
}

#[test]
fn next_hands_out_each_event_once() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Server::spawn(dir.path(), &[]);
    // Silent for longer than --silence: stalled, then the final event.
    let id = s.start(&["sleep", "3"], json!({"silence": "1s"}));
    let first = s.ok("watch_wait", json!({"id": id, "until": "next", "timeout_s": 25}));
    assert_eq!(first["events"][0]["state"], "stalled", "{first}");
    assert_eq!(first["events"][0]["seq"], 1);
    assert_eq!(first["timed_out"], false);
    // A short wait on a quiet run times out, and says so.
    let quiet = s.ok("watch_wait", json!({"id": id, "until": "final", "timeout_s": 0.2}));
    assert_eq!(quiet["timed_out"], true, "{quiet}");
    assert!(quiet["events"].as_array().unwrap().is_empty());
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
}

#[test]
fn stop_escalates_to_sigkill_on_the_job_group() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Server::spawn(dir.path(), &[]);
    // The shell ignores TERM and so do its children: only SIGKILL ends it.
    let id = s.start(
        &["sh", "-c", "trap '' TERM; while :; do sleep 0.2; done"],
        json!({"silence": 0}),
    );
    let t0 = Instant::now();
    let stopped = s.ok("watch_stop", json!({"id": id, "grace_s": 1}));
    assert_eq!(stopped["escalated_to_sigkill"], true, "{stopped}");
    assert_eq!(stopped["ended"], true, "{stopped}");
    assert_eq!(stopped["final"]["exit"]["signal"], 9, "{stopped}");
    assert_eq!(stopped["final"]["state"], "failing");
    assert!(stopped["line"].as_str().unwrap().contains("signal=9"));
    assert!(t0.elapsed() >= Duration::from_secs(1), "TERM got no grace");
    assert_eq!(s.ok("watch_status", json!({"id": id}))["state"], "finished");
}

#[test]
fn stop_with_term_needs_no_escalation_for_a_polite_job() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Server::spawn(dir.path(), &[]);
    let id = s.start(&["sleep", "60"], json!({}));
    let stopped = s.ok("watch_stop", json!({"id": id}));
    assert_eq!(stopped["escalated_to_sigkill"], false, "{stopped}");
    assert_eq!(stopped["final"]["exit"]["signal"], 15, "{stopped}");
}

#[test]
fn a_killed_watcher_is_reported_lost_not_waited_on() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Server::spawn(dir.path(), &[]);
    let id = s.start(&["sleep", "60"], json!({}));
    let st = s.ok("watch_status", json!({"id": id}));
    let pid = st["watcher_pid"].as_i64().unwrap() as i32;
    // The job's group, from the process table, so it does not outlive the test.
    let pgid = Command::new("ps")
        .args(["-A", "-o", "pid=,ppid=,pgid="])
        .output()
        .unwrap()
        .stdout;
    let pgid: i32 = String::from_utf8_lossy(&pgid)
        .lines()
        .filter_map(|l| {
            let f: Vec<i32> = l.split_whitespace().filter_map(|x| x.parse().ok()).collect();
            (f.len() == 3 && f[1] == pid).then_some(f[2])
        })
        .next()
        .expect("the job is a child of the watcher");
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
}

#[test]
fn old_runs_are_pruned_at_start_and_live_ones_kept() {
    let dir = tempfile::tempdir().unwrap();
    let old = dir.path().join("runs/1-0");
    std::fs::create_dir_all(&old).unwrap();
    let meta = json!({
        "id": "1-0", "cmd": ["x"], "cwd": "/", "started": "1970-01-01T00:00:00.001Z", "started_ms": 1,
        "watcher_pid": i32::MAX, "options": {}, "events": old.join("events.jsonl"), "log": old.join("output.log"),
    });
    std::fs::write(old.join("meta.json"), meta.to_string()).unwrap();
    let mut s = Server::spawn(dir.path(), &[]);
    assert!(!old.exists(), "a run from 1970 survived the prune");
    assert!(s.ok("watch_list", json!({}))["runs"].as_array().unwrap().is_empty());
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

    // Heartbeats every second over a 3s silent job: stalled at 1s, final at 3s.
    let id = s.start(&["sleep", "3"], json!({"silence": "1s", "heartbeat": "1s"}));
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

    let fin = s
        .note_where(is_ours)
        .expect("no channel notification for the final event");
    assert_eq!(fin["params"]["meta"]["state"], "done", "{fin}");
    assert!(fin["params"]["content"].as_str().unwrap().contains("exit=0"), "{fin}");

    // Heartbeats were produced but never pushed.
    let waited = wait_final(&mut s, &id);
    assert!(waited["heartbeats_skipped"].as_u64().unwrap_or(0) >= 1, "{waited}");
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
