//! `--heartbeat DUR`: periodic status events on the monotonic clock, in the
//! same sink as every other event, never counted as child output.
//!
//! Timing discipline: tests synchronise on observed events and release the
//! child through a gate file instead of sleeping and hoping; where a time
//! matters it is a floor (a tick can never come early), never a ceiling a
//! descheduled watcher could break (grid precision lives in unit tests); and verdict assertions tolerate the
//! correct fail-open `null`.

mod common;
use common::*;
use serde_json::Value;
use std::io::{Read as _, Write as _};
use std::os::fd::FromRawFd;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const LIMIT: Duration = Duration::from_secs(60);

fn heartbeats(ev: &[Value]) -> Vec<&Value> {
    ev.iter().filter(|e| e["reason"] == "heartbeat").collect()
}

fn cfg(e: &Env, url: &str) -> PathBuf {
    // A generous deadline: the fake answers at once, so a null verdict means
    // the in-flight guard, not a slow runner.
    e.config(&format!("[systemone]\nurls = [{url:?}]\ntimeout_s = 10\n"))
}

/// A file the child polls for; the test opens it once it has seen what it
/// was waiting for.
struct Gate(PathBuf);

impl Gate {
    fn new(e: &Env) -> Self {
        Gate(e.path("gate"))
    }
    fn open(&self) {
        std::fs::write(&self.0, "").unwrap();
    }
    /// A second gate beside the first, for a child with two phases.
    fn open_second(&self) {
        std::fs::write(self.0.with_extension("2"), "").unwrap();
    }
}

/// `body` runs under `sh -c`; `$1` is the gate path and `$WAIT` blocks until
/// the gate opens.
const WAIT: &str = "while [ ! -e \"$1\" ]; do sleep 0.05; done";
/// The second gate: the first one's path with a `.2` extension.
const WAIT2: &str = "while [ ! -e \"$1.2\" ]; do sleep 0.05; done";

fn spawn_gated(e: &Env, args: &[&str], gate: &Gate, body: &str) -> Child {
    let mut c = e.cmd();
    c.args(args)
        .args(["--", "sh", "-c"])
        .arg(body.replace("$WAIT2", WAIT2).replace("$WAIT", WAIT))
        .arg("sh")
        .arg(&gate.0)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    c.spawn().unwrap()
}

/// Poll `f` every 50 ms until it holds or `limit` passes.
fn wait_until(limit: Duration, mut f: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + limit;
    while Instant::now() < end {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    f()
}

fn finish(child: Child) -> std::process::Output {
    child.wait_with_output().unwrap()
}

#[test]
fn heartbeats_then_the_final_event() {
    let e = Env::new();
    let r = run({
        let mut c = e.cmd();
        c.args(["--no-s1", "-q", "--heartbeat", "2s", "--", "sleep", "7"]);
        c
    });
    assert!(r.status.success(), "the exit code is the child's");
    let ev = e.events();
    let st = states(&ev);
    // Ticks at 2, 4 and 6 s; a watcher descheduled for a second may lose the
    // last one to coalescing, never gain one.
    let hb = heartbeats(&ev);
    assert!((2..=3).contains(&hb.len()), "{st:?}");
    assert_eq!(st.last().map(String::as_str), Some("done/exit"), "{st:?}");
    assert_eq!(st.len(), hb.len() + 1, "{st:?}");
    let mut prev = 0;
    for h in &hb {
        assert_eq!(
            (h["state"].as_str(), h["severity"].as_str()),
            (Some("progressing"), Some("info"))
        );
        assert_eq!(h["exit"], Value::Null);
        assert_eq!(h["s1"], Value::Null);
        assert_eq!(h["last_line"], Value::Null, "no output yet");
        assert!(h.get("heartbeats_dropped").is_none());
        assert_eq!(
            (h["bytes_since_last"].as_u64(), h["lines_since_last"].as_u64()),
            (Some(0), Some(0))
        );
        // Never before its tick, one per grid point. How late a tick may
        // be is the scheduler's business (a descheduled watcher is still a
        // correct one): the exact grid arithmetic and coalescing are
        // pinned deterministically by the `supervise` unit tests.
        let at = h["elapsed_ms"].as_u64().unwrap();
        let k = (at + 1000) / 2000;
        assert!(k > prev, "ticks go forward, one per grid point: {st:?}");
        prev = k;
        assert!(at >= 2000 * k, "tick {k} at {at} ms came early");
        // One key for all of a job's heartbeats, distinct from the final's.
        assert!(h["dedup_key"].as_str().unwrap().contains(":heartbeat:"));
        assert_eq!(h["dedup_key"], hb[0]["dedup_key"]);
    }
    assert_ne!(last(&ev)["dedup_key"], hb[0]["dedup_key"]);
}

#[test]
fn heartbeats_carry_progress_and_a_clean_last_line() {
    let e = Env::new();
    let gate = Gate::new(&e);
    let child = spawn_gated(
        &e,
        &["--no-s1", "-q", "--heartbeat", "1s"],
        &gate,
        // 300 x's in red, then blank lines: the line is cleaned and cut. The
        // second burst is released only after the test saw the first.
        "printf 'one\\n'; printf '\\033[31m'; head -c 300 /dev/zero | tr '\\0' x; \
         printf '\\033[0m\\n\\n  \\n'; $WAIT; echo more; $WAIT2; exit 3",
    );
    // Phase 1: a heartbeat shows the cut line, then the child prints `more`;
    // phase 2: a heartbeat shows that, and only then does the child exit.
    // Both by observation, not by timing.
    let seen = |want: &str| wait_until(LIMIT, || heartbeats(&e.events()).iter().any(|h| h["last_line"] == want));
    let ok1 = seen(&"x".repeat(200));
    gate.open();
    let ok2 = seen("more");
    gate.open_second();
    let o = finish(child);
    assert!(ok1 && ok2, "{:?}", states(&e.events()));
    assert_eq!(o.status.code(), Some(3), "the exit code is the child's");
    let ev = e.events();
    let hb = heartbeats(&ev);
    // Counters are per interval, so they add up to what the child wrote,
    // wherever the ticks fell: 4 + 1 lines, 318 + 5 raw bytes (escapes count).
    let sum = |k: &str| hb.iter().map(|h| h[k].as_u64().unwrap()).sum::<u64>();
    assert_eq!(sum("lines_since_last"), 5);
    assert_eq!(sum("bytes_since_last"), 323);
    let more = hb.iter().find(|h| h["last_line"] == "more").unwrap();
    assert_eq!(more["lines_since_last"], 1, "per interval, not cumulative");
    assert_eq!(last(&ev)["reason"], "exit");
}

#[test]
fn a_heartbeat_during_silence_carries_stalled_and_resets_nothing() {
    let e = Env::new();
    let gate = Gate::new(&e);
    let child = spawn_gated(
        &e,
        &["--no-s1", "-q", "--silence", "600ms", "--heartbeat", "1s"],
        &gate,
        "echo start; $WAIT; echo end",
    );
    let stalled_hb = || heartbeats(&e.events()).iter().any(|h| h["state"] == "stalled");
    let ok = wait_until(LIMIT, stalled_hb);
    gate.open();
    finish(child);
    assert!(ok, "{:?}", states(&e.events()));
    let ev = e.events();
    let st = states(&ev);
    // The heartbeats (our output) neither reset the silence timer nor
    // trigger `resumed`: exactly one of each, and `resumed` only at `end`.
    assert_eq!(st.iter().filter(|s| *s == "stalled/silence").count(), 1, "{st:?}");
    assert_eq!(st.iter().filter(|s| *s == "progressing/resumed").count(), 1, "{st:?}");
    let resumed = st.iter().position(|s| s == "progressing/resumed").unwrap();
    let at = ev
        .iter()
        .position(|x| x["reason"] == "heartbeat" && x["state"] == "stalled")
        .unwrap();
    let h = &ev[at];
    assert_eq!(h["severity"], "info");
    assert_eq!(h["last_line"], "start");
    assert!(at < resumed, "{st:?}");
    assert_eq!(last(&ev)["state"], "done");
}

#[test]
fn a_heartbeat_during_a_prompt_carries_waiting_on_input() {
    let e = Env::new();
    let gate = Gate::new(&e);
    let child = spawn_gated(
        &e,
        &["--no-s1", "-q", "--prompt-after", "300ms", "--heartbeat", "1s"],
        &gate,
        "printf 'Continue? [y/N] '; $WAIT",
    );
    let ok = wait_until(LIMIT, || {
        heartbeats(&e.events()).iter().any(|h| h["state"] == "waiting_on_input")
    });
    gate.open();
    finish(child);
    assert!(ok, "{:?}", states(&e.events()));
    let ev = e.events();
    let h = heartbeats(&ev)
        .into_iter()
        .find(|h| h["state"] == "waiting_on_input")
        .unwrap();
    assert_eq!(h["severity"], "info");
}

#[test]
fn no_system_one_request_unless_heartbeat_s1() {
    let s1 = FakeS1::start(Reply::Answers {
        failing: 0.1,
        clean_done: 0.9,
    });
    let args = ["-q", "--silence", "0", "--heartbeat", "1s"];
    let script = "echo working; $WAIT";

    // Without --heartbeat-s1 only the final event consults System One.
    let e = Env::new();
    let c = cfg(&e, &s1.url);
    let gate = Gate::new(&e);
    let mut a = vec!["--config", c.to_str().unwrap()];
    a.extend(args);
    let child = spawn_gated(&e, &a, &gate, script);
    let ok = wait_until(LIMIT, || heartbeats(&e.events()).len() >= 2);
    gate.open();
    finish(child);
    assert!(ok);
    assert!(heartbeats(&e.events()).iter().all(|h| h["s1"].is_null()));
    assert_eq!(s1.hits(), 1, "only the final event");

    // With it: the verdict is attached and the state untouched. A request
    // may be skipped (one already in flight) or fail open, so the contract
    // is "never more requests than ticks", not "one per tick".
    let before = s1.hits();
    let e = Env::new();
    let c = cfg(&e, &s1.url);
    let gate = Gate::new(&e);
    let mut a = vec!["--config", c.to_str().unwrap()];
    a.extend(args);
    a.push("--heartbeat-s1");
    let child = spawn_gated(&e, &a, &gate, script);
    let ok = wait_until(LIMIT, || {
        let ev = e.events();
        heartbeats(&ev).iter().filter(|h| !h["s1"].is_null()).count() >= 2
    });
    gate.open();
    finish(child);
    assert!(ok, "{:?}", states(&e.events()));
    let ev = e.events();
    let hb = heartbeats(&ev);
    let with: Vec<_> = hb.iter().filter(|h| !h["s1"].is_null()).collect();
    assert!(s1.hits() - before >= with.len() && s1.hits() - before <= hb.len() + 1);
    for h in &hb {
        if !h["s1"].is_null() {
            assert_eq!(h["s1"]["endpoint"], s1.url);
        }
        assert_eq!(
            (h["state"].as_str(), h["severity"].as_str()),
            (Some("progressing"), Some("info"))
        );
    }
    assert_eq!(
        ev.iter().position(|x| x["reason"] == "exit"),
        Some(ev.len() - 1),
        "heartbeats precede the final"
    );
}

#[test]
fn a_flagging_verdict_never_changes_a_heartbeat_state() {
    let s1 = FakeS1::start(Reply::Answers {
        failing: 0.99,
        clean_done: 0.01,
    });
    let e = Env::new();
    let c = cfg(&e, &s1.url);
    let gate = Gate::new(&e);
    let child = spawn_gated(
        &e,
        &[
            "--config",
            c.to_str().unwrap(),
            "-q",
            "--silence",
            "0",
            "--heartbeat",
            "1s",
            "--heartbeat-s1",
        ],
        &gate,
        "echo 'FAILED step'; $WAIT; exit 0",
    );
    let ok = wait_until(LIMIT, || heartbeats(&e.events()).iter().any(|h| !h["s1"].is_null()));
    gate.open();
    finish(child);
    assert!(ok, "{:?}", states(&e.events()));
    let ev = e.events();
    let h = heartbeats(&ev).into_iter().find(|h| !h["s1"].is_null()).unwrap();
    assert!(h["s1"]["fused"].as_f64().unwrap() > 0.5);
    for h in heartbeats(&ev) {
        assert_eq!(
            (h["state"].as_str(), h["severity"].as_str()),
            (Some("progressing"), Some("info"))
        );
    }
}

#[test]
fn heartbeat_s1_fails_open() {
    let e = Env::new();
    let c = cfg(&e, &dead_url());
    let gate = Gate::new(&e);
    let child = spawn_gated(
        &e,
        &[
            "--config",
            c.to_str().unwrap(),
            "-q",
            "--silence",
            "0",
            "--heartbeat",
            "1s",
            "--heartbeat-s1",
        ],
        &gate,
        "echo x; $WAIT",
    );
    let ok = wait_until(LIMIT, || !heartbeats(&e.events()).is_empty());
    gate.open();
    let o = finish(child);
    assert!(ok && o.status.success());
    let ev = e.events();
    assert!(heartbeats(&ev).iter().all(|h| h["s1"].is_null()));
}

#[test]
fn a_slow_endpoint_is_never_asked_twice_at_once() {
    // The endpoint accepts and never answers; the request outlives many
    // ticks. Heartbeats keep coming with `s1: null`, and exactly one request
    // is ever made for them (not one per two ticks).
    let s1 = FakeS1::start(Reply::Hang);
    let e = Env::new();
    let c = e.config(&format!("[systemone]\nurls = [{:?}]\ntimeout_s = 30\n", s1.url));
    let gate = Gate::new(&e);
    let child = spawn_gated(
        &e,
        &[
            "--config",
            c.to_str().unwrap(),
            "-q",
            "--silence",
            "0",
            "--heartbeat",
            "1s",
            "--heartbeat-s1",
        ],
        &gate,
        "echo x; $WAIT",
    );
    let ok = wait_until(LIMIT, || heartbeats(&e.events()).len() >= 5);
    assert_eq!(s1.hits(), 1, "one request in flight across five ticks");
    gate.open();
    finish(child);
    assert!(ok);
    assert!(heartbeats(&e.events()).iter().all(|h| h["s1"].is_null()));
}

#[test]
fn bad_heartbeat_flags_are_usage_errors() {
    for args in [
        &["--heartbeat", "0.5s"][..],
        &["--heartbeat", "999ms"],
        &["--heartbeat", "0"],
        &["--heartbeat-s1"],
    ] {
        let e = Env::new();
        let r = run({
            let mut c = e.cmd();
            c.args(args).args(["--", "true"]);
            c
        });
        assert_eq!(r.status.code(), Some(2), "{args:?}: {}", r.stderr);
        assert!(e.events().is_empty());
    }
    let e = Env::new();
    let r = run({
        let mut c = e.cmd();
        c.args(["--heartbeat", "0.5s", "--", "true"]);
        c
    });
    assert!(r.stderr.contains("at least 1s"), "{}", r.stderr);
}

#[test]
fn a_blocked_loop_coalesces_missed_ticks() {
    let e = Env::new();
    let mut c = e.cmd();
    // The child outlives every stall: an overshooting sleep on a slow runner
    // can never make it exit while the watcher is stopped. We end the run.
    c.args(["--no-s1", "-q", "--heartbeat", "1s", "--", "sleep", "30"]);
    let mut child = c.spawn().unwrap();
    let pid = child.id() as i32;
    // Readiness first: a heartbeat proves the watcher is up and ticking.
    assert!(wait_until(LIMIT, || !heartbeats(&e.events()).is_empty()));
    unsafe { libc::kill(pid, libc::SIGSTOP) };
    std::thread::sleep(Duration::from_millis(3700)); // at least 3 grid points pass unseen
    unsafe { libc::kill(pid, libc::SIGCONT) };
    // ... and at least two more ticks after resuming, by observation.
    let ok = wait_until(LIMIT, || heartbeats(&e.events()).len() >= 4);
    unsafe { libc::kill(pid, libc::SIGTERM) };
    let st = child.wait().unwrap();
    assert!(ok);
    assert_eq!(st.signal(), Some(libc::SIGTERM), "{st:?}");
    let ev = e.events();
    let at: Vec<u64> = heartbeats(&ev)
        .iter()
        .map(|h| h["elapsed_ms"].as_u64().unwrap())
        .collect();
    // Coalescing, in terms of the grid: at most one emission per 1 s grid
    // interval (a late catch-up may sit right before the next grid point,
    // that is allowed), ...
    let cells: Vec<u64> = at.iter().map(|a| a / 1000).collect();
    assert!(
        cells.windows(2).all(|w| w[0] < w[1]),
        "two heartbeats in one interval: {at:?}"
    );
    // ... and the grid points the stop swallowed were skipped, not replayed:
    // at least three passed unseen, one catch-up covers them all.
    let last_cell = *cells.last().unwrap();
    assert!(
        cells.len() as u64 + 2 <= last_cell,
        "missed ticks were replayed instead of coalesced: {at:?}"
    );
}

#[test]
fn log_mode_emits_heartbeats_since_attach() {
    let e = Env::new();
    let log = e.path("job.log");
    std::fs::write(&log, "old content\n").unwrap();
    let mut c = e.cmd();
    c.args(["--no-s1", "-q", "--silence", "0", "--heartbeat", "1s", "--log"])
        .arg(&log);
    let child = c.spawn().unwrap();
    // Readiness, not a guess: the first heartbeat proves the watcher is
    // attached and has primed its ring from the file (and starts a fresh
    // interval), however slow the runner is to start it.
    let first = wait_until(LIMIT, || !heartbeats(&e.events()).is_empty());
    assert!(first, "no heartbeat after attach");
    let mut f = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
    f.write_all(b"step 1 done\n").unwrap();
    // ... and a couple of intervals more, polled instead of slept.
    let seen = wait_until(LIMIT, || {
        let ev = e.events();
        heartbeats(&ev).iter().any(|h| h["last_line"] == "step 1 done") && heartbeats(&ev).len() >= 3
    });
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    let o = child.wait_with_output().unwrap();
    assert_eq!(o.status.signal(), Some(libc::SIGTERM));
    let ev = e.events();
    let hb = heartbeats(&ev);
    assert!(seen, "{:?}", states(&ev));
    assert_eq!(states(&ev).iter().filter(|s| *s != "progressing/heartbeat").count(), 0);
    assert!(hb[0]["cmd"].as_str().unwrap().starts_with("--log "));
    // What was already in the file at attach is not "since last": the first
    // heartbeat saw nothing new, the one after the append saw exactly it.
    assert_eq!(
        (hb[0]["bytes_since_last"].as_u64(), hb[0]["lines_since_last"].as_u64()),
        (Some(0), Some(0))
    );
    assert_eq!(hb[0]["last_line"], "old content", "primed on attach");
    let with = hb.iter().find(|h| h["last_line"] == "step 1 done").unwrap();
    assert_eq!(
        (with["bytes_since_last"].as_u64(), with["lines_since_last"].as_u64()),
        (Some(12), Some(1))
    );
    assert!(hb[0]["elapsed_ms"].as_u64().unwrap() >= 1000, "never before its tick");
}

#[test]
fn last_line_survives_the_ring_and_chunk_boundaries() {
    // Output far beyond the 16 KiB evidence ring: the last non-empty line is
    // long gone from it, and a line longer than the ring keeps its start.
    for (script, want) in [
        (
            "echo important; head -c 20000 /dev/zero | tr '\\0' '\\n'; $WAIT",
            "important".to_string(),
        ),
        (
            "printf BEGIN; head -c 20000 /dev/zero | tr '\\0' x; echo; $WAIT",
            format!("BEGIN{}", "x".repeat(195)),
        ),
        (
            // A CSI and a multi-byte char split by flushes between printfs.
            "printf 'a\\033['; sleep 0.1; printf '31mb\\303'; sleep 0.1; printf '\\251c\\n'; $WAIT",
            "ab\u{e9}c".to_string(),
        ),
    ] {
        let e = Env::new();
        let gate = Gate::new(&e);
        let child = spawn_gated(&e, &["--no-s1", "-q", "--pipe", "--heartbeat", "1s"], &gate, script);
        let ok = wait_until(LIMIT, || {
            heartbeats(&e.events()).iter().any(|h| h["last_line"] == want.as_str())
        });
        gate.open();
        finish(child);
        let ev = e.events();
        assert!(
            ok,
            "want {want:?}, got {:?}",
            heartbeats(&ev)
                .iter()
                .map(|h| h["last_line"].clone())
                .collect::<Vec<_>>()
        );
    }
}

/// A pipe whose buffer is full of valid JSON lines and that nobody reads:
/// returns (read end, blocking write end, bytes of filler).
fn stalled_pipe() -> (std::fs::File, std::fs::File, usize) {
    let mut fds = [0i32; 2];
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    let (r, w) = unsafe { (std::fs::File::from_raw_fd(fds[0]), std::fs::File::from_raw_fd(fds[1])) };
    let fl = unsafe { libc::fcntl(fds[1], libc::F_GETFL) };
    unsafe { libc::fcntl(fds[1], libc::F_SETFL, fl | libc::O_NONBLOCK) };
    let line = FILLER.as_bytes();
    let mut n = 0;
    while (&w).write(line).is_ok() {
        n += line.len();
    }
    // Back to blocking: the watcher's writes must wait like a real stuck reader.
    unsafe { libc::fcntl(fds[1], libc::F_SETFL, fl) };
    (r, w, n)
}

const FILLER: &str = "{\"filler\":true}\n";

#[test]
fn a_stalled_event_reader_never_freezes_the_timeout() {
    let e = Env::new();
    let (mut r, w, filler) = stalled_pipe();
    assert!(filler > 4096);
    let pidfile = e.path("sleep.pid");
    let mut c: Command = e.cmd_bare();
    c.args([
        "--no-s1",
        "-q",
        "--pipe",
        "--silence",
        "0",
        "--heartbeat",
        "1s",
        "--timeout",
        "2s",
        "--kill-grace",
        "2s",
        "--events-fd",
    ])
    .arg(std::os::fd::AsRawFd::as_raw_fd(&w).to_string())
    .args(["--", "sh", "-c", "echo $$ > \"$1\"; exec sleep 60", "sh"])
    .arg(&pidfile)
    .stdout(Stdio::null())
    .stderr(Stdio::null());
    let mut child = c.spawn().unwrap();
    drop(w); // the watcher's dup is now the only writer
    assert!(wait_until(LIMIT, || pidfile.exists()
        && std::fs::read_to_string(&pidfile)
            .is_ok_and(|s| s.trim().parse::<i32>().is_ok())));
    let sleeper: i32 = std::fs::read_to_string(&pidfile).unwrap().trim().parse().unwrap();
    let t0 = Instant::now();
    // The timeout (2 s) plus grace (2 s) plus slack: with the old inline
    // writes the child was still alive long after, until the pipe drained.
    let killed = wait_until(Duration::from_secs(20), || !alive(sleeper));
    let took = t0.elapsed();
    assert!(
        killed,
        "the child outlived --timeout behind a stalled event reader ({took:?})"
    );
    // The final event waits for the reader, like any write to a full pipe.
    assert!(
        child.try_wait().unwrap().is_none(),
        "exited without delivering the final event"
    );
    // Resume reading: everything arrives, whole lines, the final event last.
    let mut got = String::new();
    r.read_to_string(&mut got).unwrap();
    let st = child.wait().unwrap();
    assert!(st.signal().is_some() || st.code().is_some());
    let own: Vec<&str> = got.lines().filter(|l| *l != FILLER.trim_end()).collect();
    assert!(got.starts_with(FILLER), "filler first, in order");
    std::fs::write(e.path("events.jsonl"), own.join("\n") + "\n").unwrap();
    let ev = e.events(); // parses and schema-validates every line
    assert!(!ev.is_empty());
    let l = last(&ev);
    assert_eq!(
        (l["state"].as_str(), l["reason"].as_str()),
        (Some("failing"), Some("timeout"))
    );
    assert!(
        ev[..ev.len() - 1].iter().all(|x| x["reason"] == "heartbeat"),
        "{:?}",
        states(&ev)
    );
}

/// The reviewer's repro (#26, B3): the default, non-quiet stderr sink on a
/// pipe nobody reads. Events and the supervisor's own diagnostics share that
/// stream; neither may hold the loop up, so the timeout and the kill happen
/// on schedule while stderr is blocked.
#[test]
fn a_stalled_stderr_never_freezes_the_timeout() {
    let e = Env::new();
    let (mut r, w, filler) = stalled_pipe();
    assert!(filler > 4096);
    let pidfile = e.path("sleep.pid");
    let mut c: Command = e.cmd_bare(); // no --events, no -q: stderr is the sink
    c.args([
        "--no-s1",
        "--pipe",
        "--heartbeat",
        "1s",
        "--silence",
        "0",
        "--timeout",
        "3s",
        "--kill-grace",
        "0.1s",
        "--",
        "sh",
        "-c",
        "echo $$ > \"$1\"; exec sleep 60",
        "sh",
    ])
    .arg(&pidfile)
    .stdout(Stdio::null())
    .stderr(Stdio::from(w)); // the watcher holds the only write end
    let mut child = c.spawn().unwrap();
    drop(c);
    // The child announces itself through a file, not through stderr.
    assert!(wait_until(LIMIT, || std::fs::read_to_string(&pidfile)
        .is_ok_and(|s| s.trim().parse::<i32>().is_ok())));
    let sleeper: i32 = std::fs::read_to_string(&pidfile).unwrap().trim().parse().unwrap();
    let t0 = Instant::now();
    // timeout 3 s + grace 0.1 s + generous slack; a frozen loop never gets here.
    let killed = wait_until(Duration::from_secs(20), || !alive(sleeper));
    let took = t0.elapsed();
    assert!(
        killed,
        "the child outlived --timeout behind a stalled stderr ({took:?})"
    );
    // The final flush waits for the reader, like any write to a full pipe.
    assert!(
        child.try_wait().unwrap().is_none(),
        "exited without delivering the final event"
    );
    // Drain: whole lines, the final event last, diagnostics between them.
    let mut got = String::new();
    r.read_to_string(&mut got).unwrap();
    let st = child.wait().unwrap();
    assert!(st.signal().is_some() || st.code().is_some());
    let prefix = watcher_s1::event::STDERR_PREFIX;
    let own: Vec<&str> = got.lines().filter_map(|l| l.strip_prefix(prefix)).collect();
    std::fs::write(e.path("events.jsonl"), own.join("\n") + "\n").unwrap();
    let ev = e.events(); // parses and schema-validates every line
    let l = last(&ev);
    assert_eq!(
        (l["state"].as_str(), l["reason"].as_str()),
        (Some("failing"), Some("timeout"))
    );
    assert!(got.contains("SIGTERM to process group"), "the diagnostic arrived too");
}
