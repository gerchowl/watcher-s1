//! `watcher-s1 follow` and `watcher-s1 guide` through the real binary.

mod common;
use common::*;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

fn ev(run: &str, parent: Option<&str>, state: &str, exit: Option<i32>) -> String {
    serde_json::json!({
        "run_id": run, "caused_by": parent, "state": state, "reason": "exit", "severity": "info",
        "exit": exit.map(|c| serde_json::json!({"code": c, "signal": null})),
        "s1": null, "evidence_tail": format!("{run} says hi\n"),
    })
    .to_string()
        + "\n"
}

fn append(p: &Path, s: &str) {
    let mut f = OpenOptions::new().create(true).append(true).open(p).unwrap();
    f.write_all(s.as_bytes()).unwrap();
}

fn follow(args: &[&str]) -> Child {
    Command::new(BIN)
        .arg("follow")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

/// Wait up to `secs` for the child to exit; None means it is still running.
fn exit_within(c: &mut Child, secs: u64) -> Option<std::process::ExitStatus> {
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(secs) {
        if let Some(s) = c.try_wait().unwrap() {
            return Some(s);
        }
        sleep(Duration::from_millis(50));
    }
    None
}

fn finish(mut c: Child) -> (std::process::ExitStatus, String) {
    let st = exit_within(&mut c, 10).expect("follow did not exit");
    let out = c.wait_with_output().unwrap();
    (st, String::from_utf8_lossy(&out.stdout).into_owned())
}

#[test]
fn waits_for_a_file_that_appears_late() {
    let e = Env::new();
    let p = e.path("late.jsonl");
    let mut c = follow(&[p.to_str().unwrap()]);
    sleep(Duration::from_millis(500));
    assert!(c.try_wait().unwrap().is_none(), "must wait, not fail");
    append(&p, &ev("r1", None, "done", Some(0)));
    let (st, out) = finish(c);
    assert_eq!(st.code(), Some(0));
    assert_eq!(out, "done exit info exit=0 - r1 says hi\n");
}

#[test]
fn buffers_a_partial_last_line() {
    let e = Env::new();
    let p = e.path("e.jsonl");
    let line = ev("r1", None, "done", Some(0));
    let (a, b) = line.split_at(30);
    append(&p, a);
    let mut c = follow(&[p.to_str().unwrap()]);
    sleep(Duration::from_millis(600));
    assert!(c.try_wait().unwrap().is_none(), "a half line is not an event");
    append(&p, b);
    let (st, out) = finish(c);
    assert_eq!(st.code(), Some(0));
    assert!(out.starts_with("done exit info"), "{out}");
}

#[test]
fn nested_final_event_does_not_stop_it() {
    let e = Env::new();
    let p = e.path("e.jsonl");
    append(&p, &ev("outer", None, "progressing", None));
    append(&p, "this is not json\n");
    append(&p, &ev("inner", Some("outer"), "done", Some(0)));
    let mut c = follow(&[p.to_str().unwrap()]);
    assert!(exit_within(&mut c, 1).is_none(), "inner's final event must not end it");
    append(&p, &ev("outer", None, "failing", Some(3)));
    let out = c.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    let (out, err) = (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    );
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 3, "{out}");
    assert!(lines[1].starts_with("  done"), "nested run is indented: {out}");
    assert!(lines[2].starts_with("failing exit info exit=3"), "{out}");
    assert_eq!(err.matches("malformed").count(), 1, "{err}");
}

#[test]
fn new_ignores_a_prior_completed_run() {
    let e = Env::new();
    let p = e.path("e.jsonl");
    append(&p, &ev("old", None, "done", Some(0)));
    let mut c = follow(&["--new", p.to_str().unwrap()]);
    assert!(exit_within(&mut c, 1).is_none(), "the old run must be ignored");
    append(&p, &ev("new", None, "progressing", None));
    append(&p, &ev("new", None, "done", Some(0)));
    let (st, out) = finish(c);
    assert_eq!(st.code(), Some(0));
    assert_eq!(out.lines().count(), 2, "{out}");
    assert!(!out.contains("old says"), "{out}");
}

fn overwrite(p: &Path, s: &str) {
    std::fs::write(p, s).unwrap();
}

fn stderr_of(c: Child) -> (Option<i32>, String, String) {
    let o = c.wait_with_output().unwrap();
    (
        o.status.code(),
        String::from_utf8_lossy(&o.stdout).into_owned(),
        String::from_utf8_lossy(&o.stderr).into_owned(),
    )
}

#[test]
fn truncation_resets_the_partial_buffer() {
    let e = Env::new();
    let p = e.path("e.jsonl");
    // A long first run, cut off mid-line, then the file is reused.
    let old = ev("old", None, "progressing", None);
    append(&p, &old[..old.len() - 10]);
    let mut c = follow(&[p.to_str().unwrap()]);
    sleep(Duration::from_millis(600));
    assert!(c.try_wait().unwrap().is_none());
    // Truncate in place to something shorter, then write a full run.
    overwrite(&p, "");
    sleep(Duration::from_millis(600));
    append(&p, &ev("new", None, "done", Some(0)));
    let (code, out, err) = stderr_of(c);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(out, "done exit info exit=0 - new says hi\n");
    assert!(!err.contains("malformed"), "stale partial line leaked: {err}");
}

#[test]
fn truncated_and_regrown_between_polls_is_noticed() {
    let e = Env::new();
    let p = e.path("e.jsonl");
    append(&p, &ev("old", None, "progressing", None));
    let mut c = follow(&[p.to_str().unwrap()]);
    sleep(Duration::from_millis(600));
    // Same inode, longer than before, different head.
    // Same run (it is locked on), but a different head and longer.
    let long = ev("old", None, "done", Some(0)).replace("old says hi", &"x".repeat(200));
    overwrite(&p, &long);
    let (st, out) = {
        let st = exit_within(&mut c, 10).expect("follow did not exit");
        (
            st,
            String::from_utf8_lossy(&c.wait_with_output().unwrap().stdout).into_owned(),
        )
    };
    assert_eq!(st.code(), Some(0));
    assert!(out.contains("done exit"), "{out}");
}

#[test]
fn rotation_is_followed() {
    let e = Env::new();
    let p = e.path("e.jsonl");
    let old = e.path("e.jsonl.1");
    append(&p, &ev("r1", None, "progressing", None));
    let c = follow(&[p.to_str().unwrap()]);
    sleep(Duration::from_millis(600));
    // The run's last lines land in the old file, which is then rotated away
    // and a new file takes its place: the old tail must still be read.
    append(&p, &ev("r1", None, "stalled", None));
    std::fs::rename(&p, &old).unwrap();
    sleep(Duration::from_millis(100));
    append(&p, &ev("r1", None, "done", Some(0)));
    let (st, out) = finish(c);
    assert_eq!(st.code(), Some(0));
    let states: Vec<&str> = out.lines().map(|l| l.split(' ').next().unwrap()).collect();
    assert_eq!(states, ["progressing", "stalled", "done"], "{out}");
}

#[test]
fn new_skips_the_rest_of_a_line_it_lands_in() {
    let e = Env::new();
    let p = e.path("e.jsonl");
    let old = ev("old", None, "done", Some(0));
    append(&p, &old[..old.len() - 15]); // writer is mid-line
    let c = follow(&["--new", p.to_str().unwrap()]);
    sleep(Duration::from_millis(600));
    append(&p, &old[old.len() - 15..]);
    append(&p, &ev("new", None, "done", Some(0)));
    let (code, out, err) = stderr_of(c);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(out, "done exit info exit=0 - new says hi\n");
    assert!(!err.contains("malformed"), "{err}");
}

#[test]
fn timeout_exits_3_with_a_note() {
    let e = Env::new();
    let p = e.path("e.jsonl");
    append(&p, &ev("r", None, "progressing", None));
    let t0 = Instant::now();
    let c = follow(&["--timeout", "700ms", p.to_str().unwrap()]);
    let (code, out, err) = stderr_of(c);
    assert_eq!(code, Some(3), "{err}");
    assert!(t0.elapsed() < Duration::from_secs(5));
    assert!(out.starts_with("progressing"), "{out}");
    assert_eq!(err.lines().count(), 1, "{err}");
    assert!(err.contains("timed out"), "{err}");
    // A file that never appears times out too.
    let c = follow(&["--timeout", "500ms", e.path("never").to_str().unwrap()]);
    assert_eq!(stderr_of(c).0, Some(3));
}

#[test]
fn timeout_does_not_fire_when_the_run_finishes() {
    let e = Env::new();
    let p = e.path("e.jsonl");
    append(&p, &ev("r", None, "done", Some(0)));
    let c = follow(&["--timeout", "30s", p.to_str().unwrap()]);
    assert_eq!(stderr_of(c).0, Some(0));
}

#[test]
fn help_documents_exit_codes() {
    let r = run({
        let mut c = Command::new(BIN);
        c.args(["follow", "--help"]);
        c
    });
    let h = r.out();
    assert!(h.contains("--timeout") && h.contains("Exit codes"), "{h}");
}

#[test]
fn usage_errors_exit_2() {
    let r = run({
        let mut c = Command::new(BIN);
        c.args(["follow"]);
        c
    });
    assert_eq!(r.status.code(), Some(2));
    let r = run({
        let mut c = Command::new(BIN);
        c.args(["follow", "--bogus", "x"]);
        c
    });
    assert_eq!(r.status.code(), Some(2));
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
