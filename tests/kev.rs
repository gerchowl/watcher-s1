//! Real System One (Kev) checks. Opt-in: they run only when the test-only
//! WATCHER_S1_KEV_URL is set (deliberately NOT the shared SYSTEMONE_URL,
//! which fleet hosts export for everyday use), e.g.
//!
//!   just test-kev url=http://<kev-host>:8023/v1/systemone
//!
//! Everything lives in ONE test so the calls are strictly sequential: the
//! Kev host is wedge-prone and must never see a burst. No endpoint is
//! compiled in or committed as a default.

mod common;
use common::*;
use serde_json::json;
use std::io::Write;
use std::process::Stdio;

#[test]
fn real_system_one_sequential() {
    let Some(url) = std::env::var("WATCHER_S1_KEV_URL").ok().filter(|u| !u.is_empty()) else {
        eprintln!("WATCHER_S1_KEV_URL unset: skipping the real System One checks");
        return;
    };
    // A tail that ENDS in an error: the measured fused score clears 0.8.
    let failing = "   Compiling foo v0.1.0 (/x)\nerror[E0425]: cannot find value `cfg` in this scope\n  --> src/main.rs:12:5\n   |\n12 |     cfg.run();\n   |     ^^^ not found in this scope\n\nerror: could not compile `foo` (bin \"foo\") due to 1 previous error\n";
    let clean = "running 3 tests\ntest parse ... ok\ntest roundtrip ... ok\ntest edge ... ok\n\ntest result: ok. 3 passed; 0 failed; 0 ignored\n";
    // A failing test-runner summary: the old fused default scored it ~0.45
    // (missed); the logistic set scores it ~0.89 (measured 2026-10-05).
    let summary = "running 3 tests\ntest parse ... ok\ntest roundtrip ... FAILED\ntest edge ... ok\n\nfailures:\n    roundtrip\n\ntest result: FAILED. 2 passed; 1 failed; 0 ignored\n\nerror: test failed, to rerun pass `--lib`\n";

    // 1. Wrapper, exit 0 with a failing tail (the masked-failure path).
    let e = Env::new();
    let f = std::fs::write(e.path("out.txt"), failing);
    f.unwrap();
    let r = run({
        let mut c = e.cmd();
        c.env("SYSTEMONE_URL", &url)
            .args(["-q", "--silence", "0", "--", "cat"])
            .arg(e.path("out.txt"));
        c
    });
    assert_eq!(r.status.code(), Some(0));
    let ev = last(&e.events()).clone();
    eprintln!("wrapper failing-tail event: state={} s1={}", ev["state"], ev["s1"]);
    let fused = ev["s1"]["fused"].as_f64().expect("System One answered");
    assert!(fused >= 0.8, "fused {fused} on a clearly failing tail");
    assert_eq!(ev["reason"], "masked_failure");

    // Ranking: a failing test summary scores above a clean one.
    let fused_of = |out: &str| {
        let e = Env::new();
        std::fs::write(e.path("o.txt"), out).unwrap();
        run({
            let mut c = e.cmd();
            c.env("SYSTEMONE_URL", &url)
                .args(["-q", "--silence", "0", "--", "sh", "-c"]);
            c.arg(format!("cat {}; exit 1", e.path("o.txt").display()));
            c
        });
        last(&e.events())["s1"]["fused"].as_f64().expect("System One answered")
    };
    let (f_summary, f_clean) = (fused_of(summary), fused_of(clean));
    eprintln!("fused: failing summary {f_summary:.3}, clean summary {f_clean:.3}");
    assert!(f_summary >= 0.8, "failing test summary must flag: {f_summary}");
    assert!(f_clean < 0.5, "clean summary must not flag: {f_clean}");

    // 2. Judge hook on a piped command whose output is clean: no output.
    let judge = |cmd: &str, out: &str| {
        let mut c = std::process::Command::new(BIN);
        c.env("SYSTEMONE_URL", &url)
            .env("WATCHER_S1_STATE_DIR", e.path("state"))
            .args(["judge", "--posttooluse"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped());
        let mut ch = c.spawn().unwrap();
        let input = json!({
            "hook_event_name": "PostToolUse", "tool_name": "Bash",
            "tool_input": {"command": cmd},
            "tool_response": {"stdout": out, "stderr": "", "interrupted": false, "isImage": false}
        });
        ch.stdin
            .take()
            .unwrap()
            .write_all(input.to_string().as_bytes())
            .unwrap();
        let o = ch.wait_with_output().unwrap();
        assert!(o.status.success());
        String::from_utf8_lossy(&o.stdout).into_owned()
    };
    let quiet = judge("cargo test 2>&1 | tail -20", clean);
    let summary_out = judge("cargo test 2>&1 | tail -20", summary);
    eprintln!("judge on a failing test summary: {summary_out:?}");
    assert!(summary_out.contains("exit 0 came from the pipe"));
    eprintln!("judge on clean output: {quiet:?}");
    assert!(quiet.is_empty());

    // 3. Judge hook on the failing output: flagged with evidence.
    let flagged = judge("cargo build 2>&1 | tail -20", failing);
    eprintln!("judge on failing output: {flagged}");
    assert!(flagged.contains("exit 0 came from the pipe"));
}
