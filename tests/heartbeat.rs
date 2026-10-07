//! `--heartbeat DUR`: periodic status events on the monotonic clock, in the
//! same sink as every other event, never counted as child output.
//! Timing assertions leave a few hundred ms of slack for slow CI runners.

mod common;
use common::*;
use serde_json::Value;
use std::io::Write as _;
use std::os::unix::process::ExitStatusExt;
use std::time::Duration;

fn heartbeats(ev: &[Value]) -> Vec<&Value> {
    ev.iter().filter(|e| e["reason"] == "heartbeat").collect()
}

fn cfg(e: &Env, url: &str) -> std::path::PathBuf {
    e.config(&format!("[systemone]\nurls = [{url:?}]\ntimeout_s = 0.5\n"))
}

#[test]
fn three_heartbeats_then_the_final_event() {
    let e = Env::new();
    let r = run({
        let mut c = e.cmd();
        c.args(["--no-s1", "-q", "--heartbeat", "2s", "--", "sleep", "7"]);
        c
    });
    assert!(r.status.success(), "the exit code is the child's");
    let ev = e.events();
    assert_eq!(
        states(&ev),
        [
            "progressing/heartbeat",
            "progressing/heartbeat",
            "progressing/heartbeat",
            "done/exit"
        ]
    );
    for (i, h) in heartbeats(&ev).into_iter().enumerate() {
        assert_eq!(h["severity"], "info");
        assert_eq!(h["exit"], Value::Null);
        assert_eq!(h["s1"], Value::Null);
        assert_eq!(h["last_line"], Value::Null, "no output yet");
        assert_eq!(
            (h["bytes_since_last"].as_u64(), h["lines_since_last"].as_u64()),
            (Some(0), Some(0))
        );
        let at = h["elapsed_ms"].as_u64().unwrap() as i64;
        let want = 2000 * (i as i64 + 1);
        assert!((at - want).abs() < 600, "tick {i} at {at} ms, wanted ~{want}");
        // One key for all of a job's heartbeats, distinct from the final's.
        assert!(h["dedup_key"].as_str().unwrap().contains(":heartbeat:"));
        assert_eq!(h["dedup_key"], ev[0]["dedup_key"]);
    }
    assert_ne!(ev[3]["dedup_key"], ev[0]["dedup_key"]);
}

#[test]
fn heartbeats_carry_progress_and_a_clean_last_line() {
    let e = Env::new();
    let r = run({
        let mut c = e.cmd();
        // 300 x's in red, then blank lines: the line is cleaned and cut.
        c.args([
            "--no-s1",
            "-q",
            "--heartbeat",
            "1s",
            "--",
            "sh",
            "-c",
            "printf 'one\\n'; printf '\\033[31m'; head -c 300 /dev/zero | tr '\\0' x; \
             printf '\\033[0m\\n\\n  \\n'; sleep 1.6; echo more; sleep 1.1; exit 3",
        ]);
        c
    });
    assert_eq!(r.status.code(), Some(3), "the exit code is the child's");
    let ev = e.events();
    let hb = heartbeats(&ev);
    assert!(hb.len() >= 2, "{:?}", states(&ev));
    assert_eq!(hb[0]["last_line"], "x".repeat(200));
    assert_eq!(hb[0]["lines_since_last"], 4);
    assert_eq!(hb[0]["bytes_since_last"], 318, "raw bytes, escapes included");
    // Counters are per interval, not cumulative.
    assert_eq!(hb[1]["lines_since_last"], 1);
    assert_eq!(hb[1]["last_line"], "more");
    assert_eq!(last(&ev)["reason"], "exit");
}

#[test]
fn a_heartbeat_during_silence_carries_stalled_and_resets_nothing() {
    let e = Env::new();
    run({
        let mut c = e.cmd();
        c.args([
            "--no-s1",
            "-q",
            "--silence",
            "600ms",
            "--heartbeat",
            "1s",
            "--",
            "sh",
            "-c",
            "echo start; sleep 2.5; echo end",
        ]);
        c
    });
    let ev = e.events();
    let st = states(&ev);
    // The heartbeats (our output) neither reset the silence timer nor
    // trigger `resumed`: exactly one of each, and `resumed` only at `end`.
    assert_eq!(st.iter().filter(|s| *s == "stalled/silence").count(), 1, "{st:?}");
    assert_eq!(st.iter().filter(|s| *s == "progressing/resumed").count(), 1, "{st:?}");
    let resumed = st.iter().position(|s| s == "progressing/resumed").unwrap();
    let hb = heartbeats(&ev);
    assert!(!hb.is_empty());
    let first = &hb[0];
    assert_eq!(
        (first["state"].as_str(), first["severity"].as_str()),
        (Some("stalled"), Some("info"))
    );
    assert_eq!(first["last_line"], "start");
    let at = ev.iter().position(|e| std::ptr::eq(e, *first)).unwrap();
    assert!(at < resumed, "{st:?}");
    assert_eq!(last(&ev)["state"], "done");
}

#[test]
fn a_heartbeat_during_a_prompt_carries_waiting_on_input() {
    let e = Env::new();
    run({
        let mut c = e.cmd();
        c.args([
            "--no-s1",
            "-q",
            "--prompt-after",
            "300ms",
            "--heartbeat",
            "1s",
            "--",
            "sh",
            "-c",
            "printf 'Continue? [y/N] '; sleep 1.6",
        ]);
        c
    });
    let ev = e.events();
    let hb = heartbeats(&ev);
    assert_eq!(hb[0]["state"], "waiting_on_input");
    assert_eq!(hb[0]["severity"], "info");
}

#[test]
fn no_system_one_request_unless_heartbeat_s1() {
    let s1 = FakeS1::start(Reply::Answers {
        failing: 0.1,
        clean_done: 0.9,
    });
    let script = "echo working; sleep 2.5";
    let args = ["-q", "--silence", "0", "--heartbeat", "1s"];

    // Without --heartbeat-s1 only the final event consults System One.
    let e = Env::new();
    let c = cfg(&e, &s1.url);
    run({
        let mut cmd = e.cmd();
        cmd.arg("--config").arg(&c).args(args).args(["--", "sh", "-c", script]);
        cmd
    });
    let ev = e.events();
    assert!(heartbeats(&ev).len() >= 2, "{:?}", states(&ev));
    assert!(heartbeats(&ev).iter().all(|h| h["s1"].is_null()));
    assert_eq!(s1.hits(), 1, "only the final event");

    // With it: one request per tick (and the final's), the verdict attached,
    // the state untouched.
    let before = s1.hits();
    let e = Env::new();
    let c = cfg(&e, &s1.url);
    run({
        let mut cmd = e.cmd();
        cmd.arg("--config")
            .arg(&c)
            .args(args)
            .arg("--heartbeat-s1")
            .args(["--", "sh", "-c", script]);
        cmd
    });
    let ev = e.events();
    let hb = heartbeats(&ev);
    assert!(hb.len() >= 2, "{:?}", states(&ev));
    assert_eq!(s1.hits() - before, hb.len() + 1, "one per tick, plus the final");
    for h in &hb {
        assert_eq!(h["s1"]["endpoint"], s1.url);
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
    run({
        let mut cmd = e.cmd();
        cmd.arg("--config").arg(&c).args([
            "-q",
            "--silence",
            "0",
            "--heartbeat",
            "1s",
            "--heartbeat-s1",
            "--",
            "sh",
            "-c",
            "echo 'FAILED step'; sleep 1.5; exit 0",
        ]);
        cmd
    });
    let ev = e.events();
    let h = heartbeats(&ev)[0];
    assert!(h["s1"]["fused"].as_f64().unwrap() > 0.5);
    assert_eq!(
        (h["state"].as_str(), h["severity"].as_str()),
        (Some("progressing"), Some("info"))
    );
}

#[test]
fn heartbeat_s1_fails_open() {
    let e = Env::new();
    let c = cfg(&e, &dead_url());
    let r = run({
        let mut cmd = e.cmd();
        cmd.arg("--config").arg(&c).args([
            "-q",
            "--silence",
            "0",
            "--heartbeat",
            "1s",
            "--heartbeat-s1",
            "--",
            "sh",
            "-c",
            "echo x; sleep 1.5",
        ]);
        cmd
    });
    assert!(r.status.success());
    let ev = e.events();
    let hb = heartbeats(&ev);
    assert_eq!(hb.len(), 1, "{:?}", states(&ev));
    assert_eq!(hb[0]["s1"], Value::Null);
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
fn a_blocked_loop_yields_one_heartbeat_not_a_burst() {
    let e = Env::new();
    let mut c = e.cmd();
    c.args(["--no-s1", "-q", "--heartbeat", "1s", "--", "sleep", "7"]);
    let mut child = c.spawn().unwrap();
    let pid = child.id() as i32;
    std::thread::sleep(Duration::from_millis(1500)); // tick at 1s
    unsafe { libc::kill(pid, libc::SIGSTOP) };
    std::thread::sleep(Duration::from_millis(3700)); // ticks 2..5 are missed, resume mid-interval
    unsafe { libc::kill(pid, libc::SIGCONT) };
    assert!(child.wait().unwrap().success());
    let ev = e.events();
    let at: Vec<u64> = heartbeats(&ev)
        .iter()
        .map(|h| h["elapsed_ms"].as_u64().unwrap())
        .collect();
    assert!(at.len() >= 3, "{at:?}");
    // A burst would put several heartbeats within milliseconds of each other.
    for w in at.windows(2) {
        assert!(w[1] - w[0] >= 400, "burst after the blocked loop: {at:?}");
    }
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
    std::thread::sleep(Duration::from_millis(300));
    let mut f = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
    f.write_all(b"step 1 done\n").unwrap();
    std::thread::sleep(Duration::from_millis(2200));
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    let o = child.wait_with_output().unwrap();
    assert_eq!(o.status.signal(), Some(libc::SIGTERM));
    let ev = e.events();
    let hb = heartbeats(&ev);
    assert!(hb.len() >= 2, "{:?}", states(&ev));
    assert_eq!(states(&ev).iter().filter(|s| *s != "progressing/heartbeat").count(), 0);
    assert!(hb[0]["cmd"].as_str().unwrap().starts_with("--log "));
    assert_eq!(hb[0]["last_line"], "step 1 done");
    // What was already in the file at attach is not "since last".
    assert_eq!(
        (hb[0]["bytes_since_last"].as_u64(), hb[0]["lines_since_last"].as_u64()),
        (Some(12), Some(1))
    );
    assert_eq!(hb[1]["bytes_since_last"], 0);
    let at = hb[0]["elapsed_ms"].as_u64().unwrap() as i64;
    assert!((at - 1000).abs() < 600, "{at}");
}
