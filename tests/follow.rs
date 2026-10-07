//! `watcher-s1 follow` and `watcher-s1 guide` through the real binary.
//!
//! Tests synchronise on observable facts, never on sleeps: `follow` prints
//! `watcher-s1 follow: watching FILE (offset N)` on stderr once it has opened
//! and positioned, and each event it consumes shows up as a stdout line. Every
//! wait is bounded and kills the child on failure.

mod common;
use common::*;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{JoinHandle, sleep, spawn};
use std::time::{Duration, Instant};
use watcher_s1::event::{Event, Exit, RunInfo, Severity, Sink, State, rfc3339};

/// Upper bound for any single wait; a healthy run needs a fraction of it.
const BOUND: Duration = Duration::from_secs(20);

static CLOCK: AtomicU64 = AtomicU64::new(0);

/// An event as the real producer builds it. Each gets a distinct timestamp,
/// so two files differ within the first bytes, as real ones do.
fn event(run: &str, parent: Option<&str>, state: State, exit: Option<Exit>) -> Event {
    let info = RunInfo {
        host: "h".into(),
        run_id: run.into(),
        cmd: "job".into(),
        pid: 1,
        pgid: 1,
        caused_by: parent.map(Into::into),
    };
    let mut e = info.event(state, Severity::Info, "exit", format!("{run} says hi\n"));
    e.ts = rfc3339(1_700_000_000_000 + u128::from(CLOCK.fetch_add(1000, Ordering::SeqCst)));
    e.exit = exit;
    e
}

fn live(run: &str) -> Event {
    event(run, None, State::Progressing, None)
}

fn nested(run: &str, parent: &str, exit: Option<Exit>) -> Event {
    let state = if exit.is_some() {
        State::Done
    } else {
        State::Progressing
    };
    event(run, Some(parent), state, exit)
}

fn done(run: &str) -> Event {
    event(
        run,
        None,
        State::Done,
        Some(Exit {
            code: Some(0),
            signal: None,
        }),
    )
}

fn line(e: &Event) -> String {
    serde_json::to_string(e).unwrap() + "\n"
}

fn append(p: &Path, s: &str) {
    let mut f = OpenOptions::new().create(true).append(true).open(p).unwrap();
    f.write_all(s.as_bytes()).unwrap();
}

fn collect(mut r: impl Read + Send + 'static, into: Arc<Mutex<Vec<u8>>>) -> JoinHandle<()> {
    spawn(move || {
        let mut b = [0u8; 8192];
        while let Ok(n) = r.read(&mut b) {
            if n == 0 {
                break;
            }
            into.lock().unwrap().extend_from_slice(&b[..n]);
        }
    })
}

/// A running `follow` whose output is drained continuously.
struct Follow {
    child: Child,
    out: Arc<Mutex<Vec<u8>>>,
    err: Arc<Mutex<Vec<u8>>>,
    readers: Vec<JoinHandle<()>>,
}

fn text(b: &Arc<Mutex<Vec<u8>>>) -> String {
    String::from_utf8_lossy(&b.lock().unwrap()).into_owned()
}

impl Follow {
    fn start(args: &[&str]) -> Self {
        let mut child = Command::new(BIN)
            .arg("follow")
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let (out, err) = (Arc::new(Mutex::default()), Arc::new(Mutex::default()));
        let readers = vec![
            collect(child.stdout.take().unwrap(), out.clone()),
            collect(child.stderr.take().unwrap(), err.clone()),
        ];
        Follow {
            child,
            out,
            err,
            readers,
        }
    }

    fn out(&self) -> String {
        text(&self.out)
    }

    fn err(&self) -> String {
        text(&self.err)
    }

    /// Wait until `ok` holds; the child dying first, or `BOUND`, fails the test.
    fn wait_until(&mut self, what: &str, ok: impl Fn(&Self) -> bool) {
        let t0 = Instant::now();
        while !ok(self) {
            let gone = self.child.try_wait().unwrap();
            assert!(
                gone.is_none() && t0.elapsed() < BOUND,
                "gave up waiting for {what} (exit {gone:?})\nstdout: {}\nstderr: {}",
                self.out(),
                self.err()
            );
            sleep(Duration::from_millis(10));
        }
    }

    /// Opened and positioned; returns the offset it reported.
    fn ready(&mut self) -> u64 {
        self.wait_until("the readiness line", |f| {
            f.err().contains("watcher-s1 follow: watching ")
        });
        let err = self.err();
        let l = err.lines().find(|l| l.contains("watching ")).unwrap();
        let off = l.rsplit_once("(offset ").and_then(|(_, r)| r.strip_suffix(')'));
        off.unwrap_or_else(|| panic!("bad readiness line {l:?}"))
            .parse()
            .unwrap()
    }

    fn wait_lines(&mut self, n: usize) {
        self.wait_until(&format!("{n} output lines"), |f| f.out().lines().count() >= n);
    }

    fn running(&mut self) -> bool {
        self.child.try_wait().unwrap().is_none()
    }

    /// Wait for exit (bounded; killed and reaped on failure) and return
    /// (exit code, stdout, stderr).
    fn finish(mut self) -> (Option<i32>, String, String) {
        let t0 = Instant::now();
        let status = loop {
            if let Some(s) = self.child.try_wait().unwrap() {
                break s;
            }
            assert!(
                t0.elapsed() < BOUND,
                "follow did not exit\nstdout: {}\nstderr: {}",
                self.out(),
                self.err()
            );
            sleep(Duration::from_millis(10));
        };
        for r in std::mem::take(&mut self.readers) {
            r.join().unwrap();
        }
        (status.code(), self.out(), self.err())
    }
}

impl Drop for Follow {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn states(out: &str) -> Vec<&str> {
    out.lines().map(|l| l.trim_start().split(' ').next().unwrap()).collect()
}

#[test]
fn waits_for_a_file_that_appears_late() {
    let e = Env::new();
    let p = e.path("late.jsonl");
    let mut c = Follow::start(&[p.to_str().unwrap()]);
    sleep(Duration::from_millis(300));
    assert!(c.running(), "must wait, not fail");
    assert!(!c.err().contains("watching"), "nothing to watch yet");
    append(&p, &line(&done("r1")));
    c.wait_until("readiness", |f| f.err().contains("watching"));
    let (code, out, err) = c.finish();
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(out, "done exit info exit=0 - r1 says hi\n");
    assert!(err.contains(&format!("watching {} (offset 0)", p.display())), "{err}");
}

#[test]
fn buffers_a_partial_last_line() {
    let e = Env::new();
    let p = e.path("e.jsonl");
    let last = line(&done("r1"));
    let (a, b) = last.split_at(30);
    append(&p, &line(&live("r1")));
    append(&p, a);
    let mut c = Follow::start(&[p.to_str().unwrap()]);
    c.ready();
    c.wait_lines(1);
    assert!(c.running(), "a half line is not an event");
    assert_eq!(c.out().lines().count(), 1);
    append(&p, b);
    let (code, out, err) = c.finish();
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(states(&out), ["progressing", "done"], "{out}");
}

#[test]
fn a_multibyte_character_split_across_appends_survives() {
    let e = Env::new();
    let p = e.path("e.jsonl");
    let mut ev = done("r1");
    ev.evidence_tail = "r\u{e9}sum\u{e9} \u{1f600}\n".into();
    let l = line(&ev);
    let at = l.find('\u{e9}').unwrap() + 1;
    let (a, b) = l.as_bytes().split_at(at);
    let mut f = OpenOptions::new().create(true).append(true).open(&p).unwrap();
    f.write_all(a).unwrap();
    let mut c = Follow::start(&[p.to_str().unwrap()]);
    c.ready();
    f.write_all(b).unwrap();
    let (code, out, err) = c.finish();
    assert_eq!(code, Some(0), "{err}");
    assert!(out.contains("r\u{e9}sum\u{e9} \u{1f600}"), "{out}");
    assert!(!err.contains("malformed"), "{err}");
}

#[test]
fn signal_exits_are_shown() {
    let e = Env::new();
    let p = e.path("e.jsonl");
    let mut ev = event(
        "r1",
        None,
        State::Failing,
        Some(Exit {
            code: None,
            signal: Some(9),
        }),
    );
    ev.reason = "signal".into();
    ev.severity = Severity::Error;
    append(&p, &line(&ev));
    let (code, out, err) = Follow::start(&[p.to_str().unwrap()]).finish();
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(out, "failing signal error signal=9 - r1 says hi\n");
}

#[test]
fn nested_final_event_does_not_stop_it() {
    let e = Env::new();
    let p = e.path("e.jsonl");
    append(&p, &line(&live("outer")));
    append(&p, "this is not json\n");
    append(
        &p,
        &line(&nested(
            "inner",
            "outer",
            Some(Exit {
                code: Some(0),
                signal: None,
            }),
        )),
    );
    let mut c = Follow::start(&[p.to_str().unwrap()]);
    c.ready();
    c.wait_lines(2);
    assert!(c.running(), "inner's final event must not end it");
    append(&p, &line(&done("outer")));
    let (code, out, err) = c.finish();
    assert_eq!(code, Some(0), "{err}");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 3, "{out}");
    assert!(lines[1].starts_with("  done"), "nested run is indented: {out}");
    assert!(lines[2].starts_with("done exit info exit=0"), "{out}");
    assert_eq!(err.matches("malformed").count(), 1, "{err}");
}

#[test]
fn a_leading_empty_object_does_not_poison_the_lock() {
    let e = Env::new();
    let p = e.path("e.jsonl");
    append(&p, "{}\n");
    let mut c = Follow::start(&[p.to_str().unwrap()]);
    c.ready();
    c.wait_until("the warning", |f| f.err().contains("malformed"));
    assert_eq!(c.out(), "", "a malformed object prints nothing");
    append(&p, &line(&done("real")));
    let (code, out, err) = c.finish();
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(states(&out), ["done"]);
    assert_eq!(err.matches("malformed").count(), 1, "{err}");
}

#[test]
fn a_bogus_exit_value_is_not_a_final_event() {
    let e = Env::new();
    let p = e.path("e.jsonl");
    append(&p, "{\"run_id\":\"r\",\"exit\":false}\n");
    let mut c = Follow::start(&[p.to_str().unwrap()]);
    c.ready();
    c.wait_until("the warning", |f| f.err().contains("malformed"));
    assert!(c.running(), "exit:false must not end the follow");
    append(&p, &line(&done("r")));
    let (code, out, _) = c.finish();
    assert_eq!(code, Some(0));
    assert_eq!(states(&out), ["done"]);
}

#[test]
fn new_ignores_a_prior_completed_run() {
    let e = Env::new();
    let p = e.path("e.jsonl");
    append(&p, &line(&done("old")));
    let len = std::fs::metadata(&p).unwrap().len();
    let mut c = Follow::start(&["--new", p.to_str().unwrap()]);
    assert_eq!(c.ready(), len, "positioned at the end");
    assert!(c.running(), "the old run must be ignored");
    append(&p, &line(&live("new")));
    append(&p, &line(&done("new")));
    let (code, out, err) = c.finish();
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(out.lines().count(), 2, "{out}");
    assert!(!out.contains("old says"), "{out}");
}

#[test]
fn new_on_a_tiny_existing_file_neither_panics_nor_loses_the_run() {
    // Existing content of 3 bytes (a complete line) and of 30 bytes (the
    // start of an event): both shorter than the cached head.
    for existing in ["{}\n".to_string(), line(&done("old"))[..30].to_string()] {
        let e = Env::new();
        let p = e.path("e.jsonl");
        let rest_of_first = if existing.ends_with('\n') {
            String::new()
        } else {
            let l = line(&done("old"));
            l[existing.len()..].to_string()
        };
        append(&p, &existing);
        let mut c = Follow::start(&["--new", p.to_str().unwrap()]);
        assert_eq!(c.ready(), existing.len() as u64);
        append(&p, &(rest_of_first + &line(&done("new"))));
        let (code, out, err) = c.finish();
        assert_eq!(code, Some(0), "existing {existing:?}: {err}");
        assert_eq!(out, "done exit info exit=0 - new says hi\n", "{err}");
        assert!(!err.contains("panicked"), "{err}");
    }
}

#[test]
fn new_notices_a_truncate_and_regrow() {
    let e = Env::new();
    let p = e.path("e.jsonl");
    // Longer than the cached head, so the head is what tells files apart.
    append(&p, &line(&live("old")));
    let mut c = Follow::start(&["--new", p.to_str().unwrap()]);
    c.ready();
    // Same inode, regrown past the old offset before any poll can see it
    // short, and the new content ends the run it locks onto.
    let mut fresh = done("new");
    fresh.evidence_tail = format!("{}\nnew says hi\n", "x".repeat(400));
    std::fs::write(&p, line(&fresh)).unwrap();
    let (code, out, err) = c.finish();
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(out, "done exit info exit=0 - new says hi\n");
    assert!(!err.contains("malformed"), "read a suffix of the new event: {err}");
}

#[test]
fn truncation_resets_the_partial_buffer() {
    let e = Env::new();
    let p = e.path("e.jsonl");
    // A long first run, cut off mid-line, then the file is reused.
    let old = line(&live("old"));
    append(&p, &line(&live("old")));
    append(&p, &old[..old.len() - 10]);
    let mut c = Follow::start(&[p.to_str().unwrap()]);
    c.ready();
    c.wait_lines(1);
    // Truncate in place and wait until that is observable before regrowing.
    std::fs::write(&p, "").unwrap();
    sleep(Duration::from_millis(500));
    assert!(c.running());
    append(&p, &line(&done("old")));
    let (code, out, err) = c.finish();
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(states(&out), ["progressing", "done"], "{out}");
    assert!(!err.contains("malformed"), "stale partial line leaked: {err}");
}

#[test]
fn truncated_and_regrown_between_polls_is_noticed() {
    let e = Env::new();
    let p = e.path("e.jsonl");
    append(&p, &line(&live("old")));
    let mut c = Follow::start(&[p.to_str().unwrap()]);
    c.ready();
    c.wait_lines(1);
    // Same inode, longer than before, different head; same run (it is locked on).
    let mut long = done("old");
    long.evidence_tail = format!("{}\n", "x".repeat(400));
    std::fs::write(&p, line(&long)).unwrap();
    let (code, out, err) = c.finish();
    assert_eq!(code, Some(0), "{err}");
    assert!(out.lines().nth(1).unwrap().starts_with("done exit"), "{out}");
}

#[test]
fn rotation_is_followed_and_the_old_file_keeps_being_read() {
    let e = Env::new();
    let p = e.path("e.jsonl");
    let old = e.path("e.jsonl.1");
    // The real producer: one descriptor held open for the whole run.
    let producer = Sink::file(p.to_str().unwrap()).unwrap();
    producer.emit(&live("r1"));
    let mut c = Follow::start(&[p.to_str().unwrap()]);
    c.ready();
    c.wait_lines(1);
    // Rotate: the path now names a new file, but the producer keeps writing
    // to the old inode, including the run's final event.
    std::fs::rename(&p, &old).unwrap();
    append(&p, &line(&nested("other", "r1", None)));
    c.wait_lines(2);
    producer.emit(&event("r1", None, State::Stalled, None));
    c.wait_lines(3);
    producer.emit(&done("r1"));
    let (code, out, err) = c.finish();
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(states(&out), ["progressing", "progressing", "stalled", "done"], "{out}");
    assert!(out.lines().nth(1).unwrap().starts_with("  progressing"), "{out}");
}

#[test]
fn rotation_is_noticed_while_the_old_file_is_still_busy() {
    let e = Env::new();
    let p = e.path("e.jsonl");
    let old = e.path("e.jsonl.1");
    let producer = Arc::new(Sink::file(p.to_str().unwrap()).unwrap());
    producer.emit(&live("r1"));
    let mut c = Follow::start(&["--timeout", "30s", p.to_str().unwrap()]);
    c.ready();
    c.wait_lines(1);
    // The producer never pauses for long: the old inode makes progress at every poll.
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let busy = {
        let (producer, stop) = (producer.clone(), stop.clone());
        spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                producer.emit(&live("r1"));
                sleep(Duration::from_millis(2));
            }
        })
    };
    std::fs::rename(&p, &old).unwrap();
    // The locked run's verdict lands on the NEW file; the old one only gets noise.
    append(&p, &line(&done("r1")));
    let t0 = Instant::now();
    let (code, out, err) = c.finish();
    stop.store(true, Ordering::SeqCst);
    busy.join().unwrap();
    assert_eq!(code, Some(0), "{err}");
    assert!(out.contains("done exit"), "{out}");
    assert!(
        t0.elapsed() < Duration::from_secs(10),
        "the replacement was starved: {:?}",
        t0.elapsed()
    );
}

#[test]
fn a_line_over_a_mebibyte_is_skipped_with_one_warning() {
    let e = Env::new();
    let p = e.path("e.jsonl");
    let mut big = String::from("{\"run_id\":\"r\",\"pad\":\"");
    big.push_str(&"x".repeat(2 * 1024 * 1024));
    big.push_str("\"}\n");
    append(&p, &line(&live("r")));
    append(&p, &big);
    append(&p, &line(&done("r")));
    let (code, out, err) = Follow::start(&["--timeout", "20s", p.to_str().unwrap()]).finish();
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(out.lines().count(), 2, "{out}");
    assert_eq!(err.matches("discarding a line longer than").count(), 1, "{err}");
    // An endless line with no newline neither grows memory without bound nor hangs the follow.
    let q = e.path("endless.jsonl");
    append(&q, &line(&live("r")));
    {
        let mut f = OpenOptions::new().append(true).open(&q).unwrap();
        let chunk = vec![b'y'; 1 << 20];
        for _ in 0..20 {
            f.write_all(&chunk).unwrap();
        }
    }
    let mut c = Follow::start(&["--timeout", "20s", q.to_str().unwrap()]);
    c.ready();
    c.wait_lines(1);
    append(&q, &format!("\n{}", line(&done("r"))));
    let (code, out, err) = c.finish();
    assert_eq!(code, Some(0), "{err}");
    assert!(out.contains("done exit"), "{out}");
}

#[test]
fn new_skips_the_rest_of_a_line_it_lands_in() {
    let e = Env::new();
    let p = e.path("e.jsonl");
    let old = line(&done("old"));
    append(&p, &old[..old.len() - 15]); // writer is mid-line
    let mut c = Follow::start(&["--new", p.to_str().unwrap()]);
    c.ready();
    append(&p, &old[old.len() - 15..]);
    append(&p, &line(&done("new")));
    let (code, out, err) = c.finish();
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(out, "done exit info exit=0 - new says hi\n");
    assert!(!err.contains("malformed"), "{err}");
}

#[test]
fn timeout_exits_3_with_a_note() {
    let e = Env::new();
    let p = e.path("e.jsonl");
    append(&p, &line(&live("r")));
    let (code, out, err) = Follow::start(&["--timeout", "700ms", p.to_str().unwrap()]).finish();
    assert_eq!(code, Some(3), "{err}");
    assert!(out.starts_with("progressing"), "{out}");
    assert!(err.contains("timed out"), "{err}");
    // A file that never appears times out too.
    let (code, _, _) = Follow::start(&["--timeout", "500ms", e.path("never").to_str().unwrap()]).finish();
    assert_eq!(code, Some(3));
}

#[test]
fn timeout_is_enforced_against_a_sustained_backlog() {
    let e = Env::new();
    let p = e.path("backlog.jsonl");
    // Plenty of cheap non-final rows: draining them takes far longer than
    // the deadline.
    let rows = "{\"run_id\":\"r\",\"state\":\"progressing\"}\n".repeat(300_000);
    std::fs::write(&p, rows).unwrap();
    let mut c = Follow::start(&["--timeout", "1ms", p.to_str().unwrap()]);
    c.ready();
    sleep(Duration::from_millis(30));
    // The final row lands well after the deadline. The outer watchdog is
    // `finish`'s bounded wait.
    append(&p, &line(&done("r")));
    let (code, out, err) = c.finish();
    assert_eq!(code, Some(3), "{err}");
    assert!(!out.contains("done exit"), "an event past the deadline was consumed");
    assert!(err.contains("timed out"), "{err}");
}

#[test]
fn fifos_and_directories_are_usage_errors_not_hangs() {
    let e = Env::new();
    let fifo = e.path("pipe");
    nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::from_bits_truncate(0o600)).unwrap();
    for target in [fifo, e.dir.path().to_path_buf()] {
        let (code, out, err) = Follow::start(&["--timeout", "30s", target.to_str().unwrap()]).finish();
        assert_eq!(code, Some(2), "{err}");
        assert_eq!(out, "");
        assert!(err.contains("regular file"), "{err}");
    }
}

#[test]
fn timeout_does_not_fire_when_the_run_finishes() {
    let e = Env::new();
    let p = e.path("e.jsonl");
    append(&p, &line(&done("r")));
    let (code, _, err) = Follow::start(&["--timeout", "30s", p.to_str().unwrap()]).finish();
    assert_eq!(code, Some(0), "{err}");
}

#[test]
fn help_documents_exit_codes_and_limits() {
    let r = run({
        let mut c = Command::new(BIN);
        c.args(["follow", "--help"]);
        c
    });
    let h = r.out();
    assert!(h.contains("--timeout") && h.contains("Exit codes"), "{h}");
    assert!(h.contains("stopped") && h.contains("regular file"), "{h}");
}

#[test]
fn usage_errors_exit_2() {
    for args in [&["follow"][..], &["follow", "--bogus", "x"], &["follow", "--version"]] {
        let r = run({
            let mut c = Command::new(BIN);
            c.args(args);
            c
        });
        assert_eq!(r.status.code(), Some(2), "{args:?}");
    }
}

#[test]
fn guide_prints_the_embedded_guide() {
    let r = run({
        let mut c = Command::new(BIN);
        c.arg("guide");
        c
    });
    assert_eq!(r.status.code(), Some(0));
    assert_eq!(r.out(), include_str!("../docs/agent-guide.md"));
    assert!(r.out().contains("agent-feedback"));
}
