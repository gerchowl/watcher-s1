//! The wrapper end to end: exit fidelity, process-group kill, tier 0/1
//! timers, sideband events.

mod common;
use common::*;
use std::os::unix::process::ExitStatusExt;
use std::time::Duration;

#[test]
fn exit_code_passes_through_with_a_failing_event() {
    let e = Env::new();
    let r = run({
        let mut c = e.cmd();
        c.args(["--no-s1", "--", "sh", "-c", "echo out; echo err >&2; exit 3"]);
        c
    });
    assert_eq!(r.status.code(), Some(3));
    let ev = e.events();
    assert_eq!(ev.len(), 1);
    let f = last(&ev);
    assert_eq!(f["state"], "failing");
    assert_eq!(f["severity"], "error");
    assert_eq!(f["exit"]["code"], 3);
    assert_eq!(f["exit"]["signal"], serde_json::Value::Null);
    assert_eq!(f["pid"], f["pgid"]);
    assert!(f["evidence_tail"].as_str().unwrap().contains("err"));
    assert_eq!(f["cmd"], "sh -c 'echo out; echo err >&2; exit 3'");
}

#[test]
fn exit_zero_is_done() {
    let e = Env::new();
    let r = run({
        let mut c = e.cmd();
        c.args(["--no-s1", "--", "true"]);
        c
    });
    assert!(r.status.success());
    assert_eq!(states(&e.events()), ["done/exit"]);
}

#[test]
fn death_by_signal_is_re_raised() {
    for (sig, name) in [(libc::SIGTERM, "TERM"), (libc::SIGKILL, "KILL"), (libc::SIGINT, "INT")] {
        for mode in [None, Some("--pipe")] {
            let e = Env::new();
            let r = run({
                let mut c = e.cmd();
                c.args(mode)
                    .args(["--no-s1", "-q", "--", "sh", "-c", &format!("kill -{name} $$")]);
                c
            });
            assert_eq!(r.status.signal(), Some(sig), "{name} {mode:?}: {:?}", r.status);
            let f = e.events().pop().unwrap();
            assert_eq!(f["exit"]["signal"], sig);
            assert_eq!(f["exit"]["code"], serde_json::Value::Null);
            assert_eq!(f["reason"], "signal");
        }
    }
}

#[test]
fn shell_sees_128_plus_n() {
    let e = Env::new();
    let r = run({
        let mut c = std::process::Command::new("sh");
        c.arg("-c")
            .arg(format!(
                "{BIN} --no-s1 -q --events {} -- sh -c 'kill -TERM $$'; echo rc=$?",
                e.path("ev").display()
            ))
            .env_remove("SYSTEMONE_URL");
        c
    });
    assert!(r.out().contains("rc=143"), "{}", r.out());
}

#[test]
fn not_found_is_127() {
    let e = Env::new();
    let r = run({
        let mut c = e.cmd();
        c.args(["--no-s1", "-q", "--", "/nonexistent/definitely-not-here"]);
        c
    });
    assert_eq!(r.status.code(), Some(127));
    assert_eq!(e.events().pop().unwrap()["exit"]["code"], 127);
}

#[test]
fn output_is_teed_unchanged() {
    let script = r#"printf 'a\nb\r\nc\001\377'; printf 'E\n' >&2"#;
    // --pipe keeps the streams apart and the bytes exact.
    let e = Env::new();
    let r = run({
        let mut c = e.cmd();
        c.args(["--no-s1", "-q", "--pipe", "--", "sh", "-c", script]);
        c
    });
    assert_eq!(r.stdout, b"a\nb\r\nc\x01\xff");
    assert_eq!(r.stderr, "E\n");
    // Under a PTY both streams share the terminal; no \n -> \r\n rewrite.
    let e = Env::new();
    let r = run({
        let mut c = e.cmd();
        c.args(["--no-s1", "-q", "--", "sh", "-c", script]);
        c
    });
    assert_eq!(r.stdout, b"a\nb\r\nc\x01\xffE\n");
}

#[test]
fn child_runs_under_a_pty_by_default() {
    let e = Env::new();
    let r = run({
        let mut c = e.cmd();
        c.args(["--no-s1", "-q", "--", "sh", "-c", "test -t 1 && echo TTY || echo NOTTY"]);
        c
    });
    assert_eq!(r.out().trim(), "TTY");
    let r = run({
        let mut c = e.cmd();
        c.args([
            "--no-s1",
            "-q",
            "--pipe",
            "--",
            "sh",
            "-c",
            "test -t 1 && echo TTY || echo NOTTY",
        ]);
        c
    });
    assert_eq!(r.out().trim(), "NOTTY");
}

#[test]
fn stdin_is_passed_through_when_not_a_tty() {
    let e = Env::new();
    let mut c = e.cmd();
    c.args(["--no-s1", "-q", "--", "sh", "-c", "wc -l"])
        .stdin(std::process::Stdio::piped());
    let mut child = c.stdout(std::process::Stdio::piped()).spawn().unwrap();
    use std::io::Write;
    child.stdin.take().unwrap().write_all(b"1\n2\n3\n").unwrap();
    let o = child.wait_with_output().unwrap();
    assert_eq!(String::from_utf8_lossy(&o.stdout).trim(), "3");
}

fn grandchild_test(mode: Option<&str>) {
    let e = Env::new();
    let pidfile = e.path("gc.pid");
    // The grandchild ignores nothing special; the child waits on it.
    let script = format!("sleep 300 & echo $! > {}; wait", pidfile.display());
    let r = run({
        let mut c = e.cmd();
        c.args(mode).args([
            "--no-s1",
            "-q",
            "--timeout",
            "1s",
            "--kill-grace",
            "1s",
            "--",
            "sh",
            "-c",
            &script,
        ]);
        c
    });
    assert_eq!(r.status.signal(), Some(libc::SIGTERM), "{:?}", r.status);
    assert!(r.elapsed < Duration::from_secs(10));
    let gc: i32 = std::fs::read_to_string(&pidfile).unwrap().trim().parse().unwrap();
    let t0 = std::time::Instant::now();
    while alive(gc) && t0.elapsed() < Duration::from_secs(3) {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!alive(gc), "grandchild {gc} survived the process-group kill");
    let f = e.events().pop().unwrap();
    assert_eq!(
        (f["state"].as_str(), f["reason"].as_str()),
        (Some("failing"), Some("timeout"))
    );
}

#[test]
fn timeout_kills_the_whole_group_under_pty() {
    grandchild_test(None);
}

#[test]
fn timeout_kills_the_whole_group_with_pipes() {
    grandchild_test(Some("--pipe"));
}

#[test]
fn timeout_escalates_to_kill_when_term_is_ignored() {
    let e = Env::new();
    let r = run({
        let mut c = e.cmd();
        c.args(["--no-s1", "-q", "--timeout", "500ms", "--kill-grace", "500ms", "--"])
            .args(["sh", "-c", "trap '' TERM; while :; do sleep 0.1; done"]);
        c
    });
    assert_eq!(r.status.signal(), Some(libc::SIGKILL));
    assert!(r.elapsed < Duration::from_secs(5), "{:?}", r.elapsed);
}

#[test]
fn silence_timer_fires_then_resumes() {
    let e = Env::new();
    let r = run({
        let mut c = e.cmd();
        c.args([
            "--no-s1",
            "-q",
            "--silence",
            "700ms",
            "--",
            "sh",
            "-c",
            "echo start; sleep 2; echo end",
        ]);
        c
    });
    assert!(r.status.success());
    let ev = e.events();
    assert_eq!(states(&ev), ["stalled/silence", "progressing/resumed", "done/exit"]);
    assert_eq!(ev[0]["severity"], "warn");
    assert_eq!(ev[0]["exit"], serde_json::Value::Null);
    assert!(ev[0]["evidence_tail"].as_str().unwrap().contains("start"));
}

#[test]
fn silence_zero_disables_the_timer() {
    let e = Env::new();
    run({
        let mut c = e.cmd();
        c.args(["--no-s1", "-q", "--silence", "0", "--", "sh", "-c", "sleep 1.2"]);
        c
    });
    assert_eq!(states(&e.events()), ["done/exit"]);
}

#[test]
fn prompt_on_stdout_is_waiting_on_input() {
    let e = Env::new();
    run({
        let mut c = e.cmd();
        c.args([
            "--no-s1",
            "-q",
            "--prompt-after",
            "300ms",
            "--silence",
            "0",
            "--timeout",
            "2s",
            "--kill-grace",
            "200ms",
        ])
        .args([
            "--",
            "sh",
            "-c",
            "echo working; printf 'Proceed with install? [y/N] '; sleep 30",
        ]);
        c
    });
    let ev = e.events();
    assert_eq!(ev[0]["state"], "waiting_on_input");
    assert_eq!(ev[0]["prompt"], "Proceed with install? [y/N]");
    assert_eq!(last(&ev)["reason"], "timeout");
}

#[test]
fn prompt_on_dev_tty_is_seen_under_the_pty() {
    // sudo/ssh write prompts to /dev/tty, not stdout; only a PTY sees them.
    let e = Env::new();
    let r = run({
        let mut c = e.cmd();
        c.args([
            "--no-s1",
            "-q",
            "--prompt-after",
            "300ms",
            "--silence",
            "0",
            "--timeout",
            "2s",
            "--kill-grace",
            "200ms",
        ])
        .args([
            "--",
            "sh",
            "-c",
            "printf 'Enter passphrase for key /k: ' > /dev/tty; sleep 30",
        ]);
        c
    });
    assert!(r.out().contains("Enter passphrase"));
    let ev = e.events();
    assert_eq!(ev[0]["state"], "waiting_on_input", "{ev:?}");
}

#[test]
fn caused_by_chains_nested_watchers() {
    let e = Env::new();
    let inner_events = e.path("inner.jsonl");
    let r = run({
        let mut c = e.cmd();
        c.args(["--no-s1", "-q", "--", BIN, "--no-s1", "-q", "--events"])
            .arg(&inner_events)
            .args(["--", "sh", "-c", "echo parent=$WATCHER_S1_PARENT; exit 5"]);
        c
    });
    assert_eq!(r.status.code(), Some(5));
    let outer = last(&e.events()).clone();
    let inner = last(&read_events(&inner_events)).clone();
    assert_eq!(outer["caused_by"], serde_json::Value::Null);
    assert_eq!(inner["caused_by"], outer["run_id"]);
    assert!(
        r.out()
            .contains(&format!("parent={}", inner["run_id"].as_str().unwrap()))
    );
}

#[test]
fn inherited_parent_env_sets_caused_by() {
    let e = Env::new();
    run({
        let mut c = e.cmd();
        c.env("WATCHER_S1_PARENT", "otherhost:1:2")
            .args(["--no-s1", "--", "true"]);
        c
    });
    assert_eq!(last(&e.events())["caused_by"], "otherhost:1:2");
}

#[test]
fn events_default_to_prefixed_stderr_lines() {
    let r = run({
        let mut c = std::process::Command::new(BIN);
        c.env_remove("SYSTEMONE_URL")
            .args(["--no-s1", "-q", "--", "sh", "-c", "exit 2"]);
        c
    });
    assert_eq!(r.status.code(), Some(2));
    let line = r
        .stderr
        .lines()
        .find(|l| l.starts_with("watcher-s1: {"))
        .expect(&r.stderr);
    let v: serde_json::Value = serde_json::from_str(line.strip_prefix("watcher-s1: ").unwrap()).unwrap();
    assert_eq!(v["exit"]["code"], 2);
}

#[test]
fn events_fd_sink() {
    let e = Env::new();
    let out = e.path("fd.jsonl");
    let r = run({
        let mut c = std::process::Command::new("sh");
        c.arg("-c")
            .arg(format!("{BIN} --no-s1 -q --events-fd 3 -- true 3>{}", out.display()))
            .env_remove("SYSTEMONE_URL");
        c
    });
    assert!(r.status.success(), "{}", r.stderr);
    assert_eq!(states(&read_events(&out)), ["done/exit"]);
}

#[test]
fn tier_off_is_logged_once() {
    let e = Env::new();
    let r = run({
        let mut c = e.cmd();
        c.args(["--silence", "300ms", "--", "sh", "-c", "sleep 0.8; exit 1"]);
        c
    });
    assert_eq!(r.status.code(), Some(1));
    assert_eq!(r.stderr.matches("System One tier off").count(), 1, "{}", r.stderr);
}

#[test]
fn healthy_quiet_tree_is_not_blocked() {
    let e = Env::new();
    let r = run({
        let mut c = e.cmd();
        c.args([
            "--no-s1",
            "-q",
            "--sample-every",
            "100ms",
            "--blocked-after",
            "300ms",
            "--silence",
            "0",
        ])
        .args(["--", "sh", "-c", "sleep 1"]);
        c
    });
    assert!(r.status.success());
    assert_eq!(states(&e.events()), ["done/exit"]);
}

#[test]
fn a_probe_that_cannot_finish_counts_as_maybe_wedged() {
    // A zero probe timeout makes every sample "timeout" (state unknown,
    // maybe wedged), which is exactly what a hung `ps` looks like.
    let e = Env::new();
    let r = run({
        let mut c = e.cmd();
        c.args([
            "--no-s1",
            "-q",
            "--sample-every",
            "100ms",
            "--blocked-after",
            "300ms",
            "--silence",
            "0",
        ])
        .args(["--probe-timeout", "0s", "--", "sh", "-c", "sleep 1.5"]);
        c
    });
    assert!(r.status.success());
    let ev = e.events();
    assert_eq!(states(&ev)[0], "stalled/blocked", "{ev:?}");
    assert_eq!(ev[0]["proc"]["probe"], "timeout");
    assert!(ev[0]["proc"]["blocked_for_s"].as_u64().is_some());
}

#[test]
fn inherited_sighup_ignore_is_respected() {
    // nohup: the caller ignores SIGHUP; a HUP must not kill the child.
    use std::os::unix::process::CommandExt;
    let e = Env::new();
    let mut c = e.cmd();
    c.args(["--no-s1", "-q", "--", "sh", "-c", "sleep 1; echo survived"])
        .stdout(std::process::Stdio::piped());
    unsafe {
        c.pre_exec(|| {
            libc::signal(libc::SIGHUP, libc::SIG_IGN);
            Ok(())
        });
    }
    let child = c.spawn().unwrap();
    std::thread::sleep(Duration::from_millis(300));
    unsafe { libc::kill(child.id() as i32, libc::SIGHUP) };
    let o = child.wait_with_output().unwrap();
    assert!(o.status.success(), "{:?}", o.status);
    assert!(String::from_utf8_lossy(&o.stdout).contains("survived"));
}

/// Re-exec target for the blocked-state test, not a test on its own: with
/// WATCHER_S1_VFORK_HOLD set it vforks a child that sleeps before exiting.
/// Until the child exits, the vfork parent sits in uninterruptible wait
/// (Linux `D`), a real kernel-level block.
#[test]
fn helper_vfork_hold() {
    let Some(secs) = std::env::var("WATCHER_S1_VFORK_HOLD")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
    else {
        return;
    };
    #[allow(deprecated)]
    unsafe {
        if libc::vfork() == 0 {
            // Only async-signal-safe syscalls in a vfork child.
            libc::sleep(secs);
            libc::_exit(0);
        }
    }
}

#[cfg(target_os = "linux")]
#[test]
fn a_really_blocked_process_raises_stalled_blocked() {
    let e = Env::new();
    let me = std::env::current_exe().unwrap();
    let r = run({
        let mut c = e.cmd();
        c.env("WATCHER_S1_VFORK_HOLD", "3")
            .args([
                "--no-s1",
                "-q",
                "--sample-every",
                "200ms",
                "--blocked-after",
                "500ms",
                "--silence",
                "0",
                "--",
            ])
            .arg(me)
            .args(["--exact", "helper_vfork_hold", "--test-threads=1", "-q"]);
        c
    });
    assert!(r.status.success(), "{:?} {}", r.status, r.stderr);
    let ev = e.events();
    let blocked = ev
        .iter()
        .find(|x| x["reason"] == "blocked")
        .unwrap_or_else(|| panic!("{ev:?}"));
    assert_eq!(blocked["state"], "stalled");
    assert_eq!(blocked["proc"]["probe"], "ok");
    let procs = blocked["proc"]["blocked"].as_array().unwrap();
    assert!(
        procs.iter().any(|p| p["state"].as_str().unwrap().starts_with('D')),
        "{procs:?}"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn a_really_blocked_process_raises_stalled_blocked_on_darwin() {
    let e = Env::new();
    let me = std::env::current_exe().unwrap();
    let r = run({
        let mut c = e.cmd();
        c.env("WATCHER_S1_VFORK_HOLD", "4")
            .args([
                "--no-s1",
                "-q",
                "--sample-every",
                "300ms",
                "--blocked-after",
                "800ms",
                "--silence",
                "0",
                "--",
            ])
            .arg(me)
            .args(["--exact", "helper_vfork_hold", "--test-threads=1", "-q"]);
        c
    });
    assert!(r.status.success(), "{:?} {}", r.status, r.stderr);
    let ev = e.events();
    let blocked = ev
        .iter()
        .find(|x| x["reason"] == "blocked")
        .unwrap_or_else(|| panic!("{ev:?}"));
    let procs = blocked["proc"]["blocked"].as_array().unwrap();
    assert!(
        procs.iter().any(|p| p["state"].as_str().unwrap().starts_with('U')),
        "{procs:?}"
    );
}

#[test]
fn a_stalled_stdout_reader_does_not_stall_the_timeout() {
    // Nobody reads our stdout: the pipe fills after ~64 KiB. The timeout
    // must still fire (it used to block behind the write).
    use std::io::BufRead;
    let e = Env::new();
    let mut c = e.cmd();
    c.args(["--no-s1", "--timeout", "1s", "--kill-grace", "300ms", "--"])
        .args(["sh", "-c", "head -c 150000 /dev/zero | tr '\\\\0' x; sleep 30"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let t0 = std::time::Instant::now();
    let mut child = c.spawn().unwrap();
    let stderr = child.stderr.take().unwrap();
    let mut saw = false;
    for line in std::io::BufReader::new(stderr).lines().map_while(Result::ok) {
        if line.contains("timeout after") {
            saw = true;
            break;
        }
    }
    assert!(saw, "no timeout log line");
    assert!(t0.elapsed() < Duration::from_secs(5), "{:?}", t0.elapsed());
    // Now drain stdout so the watcher can finish its writes and exit.
    let mut out = child.stdout.take().unwrap();
    let mut sink = Vec::new();
    std::io::Read::read_to_end(&mut out, &mut sink).unwrap();
    let st = child.wait().unwrap();
    assert_eq!(st.signal(), Some(libc::SIGTERM), "{st:?}");
    assert_eq!(sink.len(), 150_000, "output lost");
    assert_eq!(last(&e.events())["reason"], "timeout");
}

#[test]
fn on_prompt_cancel_interrupts_an_unanswered_prompt() {
    let e = Env::new();
    let r = run({
        let mut c = e.cmd();
        c.args(["--no-s1", "-q", "--silence", "0", "--prompt-after", "200ms"])
            .args([
                "--on-prompt",
                "cancel",
                "--prompt-cancel-after",
                "300ms",
                "--kill-grace",
                "300ms",
                "--",
            ])
            .args([
                "sh",
                "-c",
                "printf 'Password: ' > /dev/tty; read x < /dev/tty; echo answered",
            ]);
        c
    });
    assert!(r.status.signal().is_some(), "{:?}", r.status);
    assert!(r.elapsed < Duration::from_secs(4), "{:?}", r.elapsed);
    assert!(!r.out().contains("answered"));
    let ev = e.events();
    assert_eq!(states(&ev)[0], "waiting_on_input/prompt");
    assert_eq!(last(&ev)["reason"], "prompt_cancelled", "{ev:?}");
}

#[test]
fn on_prompt_cancel_escalates_when_sigint_is_ignored() {
    let e = Env::new();
    let r = run({
        let mut c = e.cmd();
        c.args(["--no-s1", "-q", "--silence", "0", "--prompt-after", "200ms"])
            .args([
                "--on-prompt",
                "cancel",
                "--prompt-cancel-after",
                "200ms",
                "--kill-grace",
                "300ms",
                "--",
            ])
            .args([
                "sh",
                "-c",
                "trap '' INT; printf 'Continue? [y/N] ' > /dev/tty; read x < /dev/tty",
            ]);
        c
    });
    assert_eq!(r.status.signal(), Some(libc::SIGTERM), "{:?}", r.status);
    assert_eq!(last(&e.events())["reason"], "prompt_cancelled");
}

#[test]
fn default_on_prompt_only_reports() {
    let e = Env::new();
    let r = run({
        let mut c = e.cmd();
        c.args([
            "--no-s1",
            "-q",
            "--silence",
            "0",
            "--prompt-after",
            "200ms",
            "--timeout",
            "1500ms",
        ])
        .args([
            "--kill-grace",
            "200ms",
            "--",
            "sh",
            "-c",
            "printf 'Password: ' > /dev/tty; read x < /dev/tty",
        ]);
        c
    });
    assert_eq!(
        last(&e.events())["reason"],
        "timeout",
        "the prompt was left alone until the timeout"
    );
    let _ = r;
}

#[test]
fn log_mode_watches_a_growing_file() {
    use std::io::Write as _;
    let e = Env::new();
    let log = e.path("job.log");
    std::fs::write(&log, "old content\n").unwrap();
    let mut c = e.cmd();
    c.args([
        "--no-s1",
        "-q",
        "--silence",
        "600ms",
        "--prompt-after",
        "300ms",
        "--log",
    ])
    .arg(&log)
    .stdout(std::process::Stdio::piped());
    let child = c.spawn().unwrap();
    let append = |s: &str| {
        let mut f = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
        f.write_all(s.as_bytes()).unwrap();
    };
    std::thread::sleep(Duration::from_millis(300));
    append("step 1 done\n");
    std::thread::sleep(Duration::from_millis(1000)); // > --silence: stalled
    append("step 2 done\n"); // resumed
    std::thread::sleep(Duration::from_millis(300));
    // Rotation: a new file at the same path is followed.
    std::fs::rename(&log, e.path("job.log.1")).unwrap();
    std::fs::write(&log, "Overwrite existing deployment? [y/N] ").unwrap();
    std::thread::sleep(Duration::from_millis(900));
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    let o = child.wait_with_output().unwrap();
    assert_eq!(o.status.signal(), Some(libc::SIGTERM));
    assert!(o.stdout.is_empty(), "log mode must not tee");
    let ev = e.events();
    let st = states(&ev);
    assert!(
        st.starts_with(&["stalled/silence".to_string(), "progressing/resumed".to_string()]),
        "{st:?}"
    );
    let prompt = ev
        .iter()
        .find(|x| x["state"] == "waiting_on_input")
        .unwrap_or_else(|| panic!("{st:?}"));
    assert_eq!(prompt["prompt"], "Overwrite existing deployment? [y/N]");
    assert_eq!(prompt["pid"], 0);
    assert!(prompt["cmd"].as_str().unwrap().starts_with("--log "));
}
