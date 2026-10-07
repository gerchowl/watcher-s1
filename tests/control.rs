//! The supervisor's hidden `--control PATH` socket, through the real binary.
//!
//! Waits poll observable facts (the socket appearing, a readiness file, the
//! process exiting) under a deadline; jobs announce readiness by writing a
//! file after their `trap` is installed.

mod common;
use common::*;
use serde_json::json;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};
use watcher_s1::control::{self, ClientError};

const BOUND: Duration = Duration::from_secs(20);
const T: Duration = Duration::from_secs(5);

fn until(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + BOUND;
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn wait_exit(c: &mut Child) -> std::process::ExitStatus {
    let deadline = Instant::now() + BOUND;
    loop {
        if let Some(st) = c.try_wait().unwrap() {
            return st;
        }
        assert!(Instant::now() < deadline, "the watcher did not exit");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Start a watcher with a control socket and wait until it answers.
fn start(env: &Env, sock: &Path, extra: &[&str], job: &[&str]) -> Child {
    let mut c = env.cmd();
    c.args(["--pipe", "--quiet", "--no-s1", "--control"])
        .arg(sock)
        .args(extra);
    c.arg("--").args(job);
    let child = c.stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
    until("the control socket to answer", || control::status(sock, T).is_ok());
    child
}

#[test]
fn status_reports_the_job_and_stop_ends_it_with_reason_stopped() {
    let env = Env::new();
    let sock = env.path("c.sock");
    let mut w = start(&env, &sock, &[], &["sleep", "60"]);
    assert_eq!(std::fs::metadata(&sock).unwrap().permissions().mode() & 0o777, 0o600);
    let st = control::status(&sock, T).unwrap();
    let pid = st["pid"].as_i64().unwrap();
    assert!(pid > 1 && st["pgid"] == st["pid"], "{st}");
    assert!(alive(pid as i32));
    assert_eq!(st["state"], "progressing");
    assert!(st["elapsed_ms"].as_u64().is_some());
    control::stop(&sock, Duration::from_secs(5), T).unwrap();
    let status = wait_exit(&mut w);
    // Truthful: the watcher leaves the way the job did (TERM).
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(status.signal(), Some(15), "{status:?}");
    let ev = env.events();
    let fin = last(&ev);
    assert_eq!(
        (fin["reason"].as_str(), fin["state"].as_str()),
        (Some("stopped"), Some("failing")),
        "{fin}"
    );
    assert_eq!(fin["exit"], json!({"code": null, "signal": 15}));
    assert!(!sock.exists(), "the socket is removed on exit");
    assert!(!alive(pid as i32));
    assert!(matches!(control::status(&sock, T), Err(ClientError::Unreachable(_))));
}

#[test]
fn stop_runs_the_timeout_escalation_including_the_pre_reap_group_kill() {
    let env = Env::new();
    let sock = env.path("c.sock");
    let ready = env.path("ready");
    let pidfile = env.path("gc.pid");
    // The leader dies on TERM; its same-group child ignores it (and records
    // its pid only once the trap is installed).
    let script = format!(
        "sh -c \"trap '' TERM; echo \\$\\$ > '{}'; touch '{}'; while :; do sleep 0.2; done\" & while :; do sleep 0.2; done",
        pidfile.display(),
        ready.display()
    );
    let mut w = start(&env, &sock, &["--silence", "0"], &["sh", "-c", &script]);
    until("the grandchild's trap", || ready.exists());
    let gc: i32 = std::fs::read_to_string(&pidfile).unwrap().trim().parse().unwrap();
    assert!(alive(gc));
    let t0 = Instant::now();
    control::stop(&sock, Duration::from_secs(30), T).unwrap();
    wait_exit(&mut w);
    assert!(
        t0.elapsed() < Duration::from_secs(15),
        "waited out the grace although the leader was gone"
    );
    until("the TERM-ignoring grandchild to die", || !alive(gc));
    assert_eq!(last(&env.events())["reason"], "stopped");
}

#[test]
fn stop_escalates_to_kill_after_the_grace() {
    let env = Env::new();
    let sock = env.path("c.sock");
    let ready = env.path("ready");
    let script = format!("trap '' TERM; touch '{}'; while :; do sleep 0.2; done", ready.display());
    let mut w = start(&env, &sock, &["--silence", "0"], &["sh", "-c", &script]);
    until("the trap", || ready.exists());
    let t0 = Instant::now();
    control::stop(&sock, Duration::from_millis(800), T).unwrap();
    wait_exit(&mut w);
    assert!(t0.elapsed() >= Duration::from_millis(700), "no grace");
    let fin = last(&env.events()).clone();
    assert_eq!(
        (fin["reason"].as_str(), fin["exit"]["signal"].as_i64()),
        (Some("stopped"), Some(9)),
        "{fin}"
    );
}

#[test]
fn a_repeated_stop_is_harmless() {
    let env = Env::new();
    let sock = env.path("c.sock");
    let mut w = start(
        &env,
        &sock,
        &["--timeout", "2s", "--kill-grace", "1s"],
        &["sleep", "60"],
    );
    control::stop(&sock, Duration::from_secs(5), T).unwrap();
    let _ = control::stop(&sock, Duration::ZERO, T); // may race the exit; must not matter
    wait_exit(&mut w);
    assert_eq!(last(&env.events())["reason"], "stopped");
}

#[test]
fn a_hung_control_client_never_delays_the_timeout() {
    let env = Env::new();
    let sock = env.path("c.sock");
    let mut w = start(
        &env,
        &sock,
        &["--timeout", "2s", "--kill-grace", "1s"],
        &["sleep", "60"],
    );
    // Connections that say nothing, or never finish a line.
    let _idle = UnixStream::connect(&sock).unwrap();
    let mut slow = UnixStream::connect(&sock).unwrap();
    std::io::Write::write_all(&mut slow, b"{\"op\":\"sta").unwrap();
    let t0 = Instant::now();
    wait_exit(&mut w);
    assert!(t0.elapsed() < Duration::from_secs(8), "{:?}", t0.elapsed());
    assert_eq!(last(&env.events())["reason"], "timeout");
}

#[test]
fn a_path_that_is_not_a_stale_socket_stops_the_run_before_it_starts() {
    let env = Env::new();
    let marker = env.path("ran");
    let script = format!("touch '{}'", marker.display());
    // A regular file there: refused, nothing runs, the file is untouched.
    let file = env.path("precious");
    std::fs::write(&file, "keep").unwrap();
    let r = run({
        let mut c = env.cmd();
        c.args(["--pipe", "--no-s1", "--control"])
            .arg(&file)
            .args(["--", "sh", "-c", &script]);
        c
    });
    assert_eq!(r.status.code(), Some(125), "{}", r.stderr);
    assert!(r.stderr.contains("control socket"), "{}", r.stderr);
    assert!(!marker.exists() && std::fs::read_to_string(&file).unwrap() == "keep");

    // A live socket (another watcher): refused too.
    let sock = env.path("c.sock");
    let mut first = start(&env, &sock, &[], &["sleep", "60"]);
    let r = run({
        let mut c = env.cmd_bare();
        c.args(["--pipe", "--no-s1", "--control"])
            .arg(&sock)
            .args(["--", "sh", "-c", &script]);
        c
    });
    assert_eq!(r.status.code(), Some(125), "{}", r.stderr);
    assert!(!marker.exists());
    control::stop(&sock, Duration::from_secs(5), T).unwrap();
    wait_exit(&mut first);
}

#[test]
fn a_stale_socket_left_by_a_crash_is_replaced() {
    let env = Env::new();
    let sock = env.path("c.sock");
    drop(std::os::unix::net::UnixListener::bind(&sock).unwrap()); // the file stays, nobody listens
    assert!(sock.exists());
    let mut w = start(&env, &sock, &[], &["sleep", "60"]);
    control::stop(&sock, Duration::from_secs(5), T).unwrap();
    wait_exit(&mut w);
    assert!(!sock.exists());
}

#[test]
fn log_mode_answers_status_without_a_child_and_stops_on_request() {
    let env = Env::new();
    let sock = env.path("c.sock");
    let log = env.path("app.log");
    std::fs::write(&log, "hello\n").unwrap();
    let mut c = env.cmd();
    c.args(["--quiet", "--no-s1", "--silence", "0", "--control"])
        .arg(&sock)
        .arg("--log")
        .arg(&log);
    let mut w = c.stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
    until("the control socket", || control::status(&sock, T).is_ok());
    let st = control::status(&sock, T).unwrap();
    assert!(
        st["pid"].is_null() && st["pgid"].is_null(),
        "no child in --log mode: {st}"
    );
    assert_eq!(st["state"], "progressing");
    control::stop(&sock, Duration::from_secs(1), T).unwrap();
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(wait_exit(&mut w).signal(), Some(15));
    assert!(!sock.exists());
}
