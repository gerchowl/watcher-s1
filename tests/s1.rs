//! Tier 2 against a fake local System One: event-time calls only, one
//! request per state, ordered endpoints, breaker, fail-open; and the
//! PostToolUse judge.

mod common;
use common::*;
use serde_json::{Value, json};
use std::io::Write;
use std::process::Stdio;
use std::time::Duration;

fn cfg(e: &Env, urls: &[&str], extra: &str) -> std::path::PathBuf {
    let list = urls.iter().map(|u| format!("{u:?}")).collect::<Vec<_>>().join(", ");
    e.config(&format!("[systemone]\nurls = [{list}]\ntimeout_s = 0.5\n{extra}\n"))
}

fn wrap(e: &Env, config: &std::path::Path, script: &str) -> Run {
    run({
        let mut c = e.cmd();
        c.arg("--config")
            .arg(config)
            .args(["-q", "--silence", "0", "--", "sh", "-c", script]);
        c
    })
}

#[test]
fn exit_zero_with_failing_tail_is_a_masked_failure() {
    let s1 = FakeS1::start(Reply::Answers {
        failing: 0.95,
        clean_done: 0.05,
    });
    let e = Env::new();
    let r = wrap(
        &e,
        &cfg(&e, &[&s1.url], ""),
        "echo 'error[E0425]: cannot find value'; exit 0",
    );
    assert_eq!(r.status.code(), Some(0), "the verdict never changes the exit code");
    let f = last(&e.events()).clone();
    assert_eq!(
        (f["state"].as_str(), f["reason"].as_str()),
        (Some("failing"), Some("masked_failure"))
    );
    assert_eq!(f["severity"], "warn");
    assert_eq!(f["s1"]["endpoint"], s1.url);
    assert!((f["s1"]["fused"].as_f64().unwrap() - 0.95).abs() < 1e-9);
    assert_eq!(f["s1"]["failing"], 0.95);
    assert_eq!(f["s1"]["clean_done"], 0.05);
    assert_eq!(s1.hits(), 1);
}

#[test]
fn one_request_carries_every_question_and_the_measured_state() {
    let s1 = FakeS1::start(Reply::Answers {
        failing: 0.1,
        clean_done: 0.9,
    });
    let e = Env::new();
    wrap(&e, &cfg(&e, &[&s1.url], ""), "echo 'FAILED step'; exit 0");
    let bodies = s1.bodies.lock().unwrap().clone();
    assert_eq!(bodies.len(), 1);
    let b = &bodies[0];
    let q = b["questions"].as_object().unwrap();
    assert_eq!(q.keys().collect::<Vec<_>>(), ["failing", "clean_done"]);
    assert_eq!(q["failing"]["type"], "noul");
    assert_eq!(
        q["clean_done"]["criteria"]["true"],
        "the output ends with the work completed and nothing left broken"
    );
    let state = b["state"].as_str().unwrap();
    assert!(
        state.starts_with(
            "Command: sh -c 'echo '\\''FAILED step'\\''; exit 0'\n(The exit status is not shown.)\nLast output:\n"
        ),
        "{state:?}"
    );
    assert!(state.ends_with("FAILED step\n"));
    // Below threshold: done, but the verdict rides along.
    let f = last(&e.events()).clone();
    assert_eq!(f["state"], "done");
    assert!(f["s1"].is_object());
}

#[test]
fn clean_exit_zero_makes_no_call() {
    let s1 = FakeS1::start(Reply::Answers {
        failing: 0.9,
        clean_done: 0.1,
    });
    let e = Env::new();
    wrap(&e, &cfg(&e, &[&s1.url], ""), "echo all good; exit 0");
    assert_eq!(s1.hits(), 0);
    let f = last(&e.events()).clone();
    assert_eq!(f["state"], "done");
    assert_eq!(f["s1"], Value::Null);
}

#[test]
fn non_zero_exit_is_judged_for_evidence() {
    let s1 = FakeS1::start(Reply::Answers {
        failing: 0.8,
        clean_done: 0.3,
    });
    let e = Env::new();
    let r = wrap(&e, &cfg(&e, &[&s1.url], ""), "echo boom; exit 2");
    assert_eq!(r.status.code(), Some(2));
    let f = last(&e.events()).clone();
    assert_eq!(f["state"], "failing");
    assert_eq!(f["severity"], "error");
    assert!(f["s1"]["fused"].is_number());
}

#[test]
fn silence_threshold_is_judged_once() {
    let s1 = FakeS1::start(Reply::Answers {
        failing: 0.9,
        clean_done: 0.1,
    });
    let e = Env::new();
    let config = cfg(&e, &[&s1.url], "");
    run({
        let mut c = e.cmd();
        c.arg("--config").arg(&config).args(["-q", "--silence", "300ms", "--"]);
        c.args(["sh", "-c", "echo 'Error: disk full'; sleep 1.5; echo recovered"]);
        c
    });
    let ev = e.events();
    // Silence + failing tail -> `failing/silence`; then output resumes.
    assert_eq!(states(&ev)[..2], ["failing/silence", "progressing/resumed"]);
    assert!(ev[0]["s1"]["fused"].as_f64().unwrap() >= 0.8);
    assert_eq!(ev[0]["dedup_key"].as_str().unwrap().split(':').nth(2), Some("failing"));
    // One call at the silence event, one at exit (tail still has "Error:").
    assert_eq!(s1.hits(), 2);
}

#[test]
fn first_healthy_endpoint_wins() {
    let good = FakeS1::start(Reply::Answers {
        failing: 0.9,
        clean_done: 0.1,
    });
    let e = Env::new();
    let dead = dead_url();
    wrap(&e, &cfg(&e, &[&dead, &good.url], ""), "echo 'error: x'; exit 1");
    assert_eq!(last(&e.events())["s1"]["endpoint"], good.url);
}

#[test]
fn a_hung_endpoint_fails_open_within_the_timeout() {
    let hang = FakeS1::start(Reply::Hang);
    let e = Env::new();
    let r = wrap(&e, &cfg(&e, &[&hang.url], ""), "echo 'error: x'; exit 4");
    assert_eq!(r.status.code(), Some(4));
    assert!(r.elapsed < Duration::from_secs(3), "{:?}", r.elapsed);
    let f = last(&e.events()).clone();
    assert_eq!(f["state"], "failing");
    assert_eq!(f["s1"], Value::Null);
}

#[test]
fn http_errors_fail_open() {
    let s1 = FakeS1::start(Reply::Status(500));
    let e = Env::new();
    let r = wrap(&e, &cfg(&e, &[&s1.url], ""), "echo 'error: x'; exit 0");
    assert_eq!(r.status.code(), Some(0));
    assert_eq!(last(&e.events())["state"], "done");
    assert_eq!(last(&e.events())["s1"], Value::Null);
}

#[test]
fn breaker_opens_per_endpoint_across_runs() {
    let bad = FakeS1::start(Reply::Status(503));
    let e = Env::new();
    let config = cfg(&e, &[&bad.url], "breaker = { fails = 2, cooldown_s = 600 }");
    for _ in 0..4 {
        wrap(&e, &config, "echo 'error: x'; exit 1");
    }
    assert_eq!(
        bad.hits(),
        2,
        "the breaker should stop calls after 2 consecutive failures"
    );
    // A different endpoint is unaffected.
    let good = FakeS1::start(Reply::Answers {
        failing: 0.9,
        clean_done: 0.1,
    });
    let config = cfg(&e, &[&bad.url, &good.url], "breaker = { fails = 2, cooldown_s = 600 }");
    wrap(&e, &config, "echo 'error: x'; exit 1");
    assert_eq!(bad.hits(), 2);
    assert_eq!(last(&e.events())["s1"]["endpoint"], good.url);
}

#[test]
fn env_url_enables_the_tier() {
    let s1 = FakeS1::start(Reply::Answers {
        failing: 0.9,
        clean_done: 0.1,
    });
    let e = Env::new();
    run({
        let mut c = e.cmd();
        c.env("SYSTEMONE_URL", &s1.url)
            .args(["-q", "--", "sh", "-c", "echo 'error: x'; exit 1"]);
        c
    });
    assert_eq!(last(&e.events())["s1"]["endpoint"], s1.url);
}

#[test]
fn cli_url_beats_env() {
    let a = FakeS1::start(Reply::Answers {
        failing: 0.9,
        clean_done: 0.1,
    });
    let b = FakeS1::start(Reply::Answers {
        failing: 0.9,
        clean_done: 0.1,
    });
    let e = Env::new();
    run({
        let mut c = e.cmd();
        c.env("SYSTEMONE_URL", &a.url)
            .args(["-q", "--s1-url", &b.url, "--", "sh", "-c", "echo 'error: x'; exit 1"]);
        c
    });
    assert_eq!((a.hits(), b.hits()), (0, 1));
}

#[test]
fn custom_question_file_and_threshold() {
    let s1 = FakeS1::start(Reply::Answers {
        failing: 0.6,
        clean_done: 0.4,
    });
    let e = Env::new();
    std::fs::write(
        e.path("q.toml"),
        r#"
threshold = 0.5
[score]
positive = ["failing"]
[questions.failing]
type = "noul"
instructions = "Did it fail?"
criteria = { true = "yes", false = "no" }
"#,
    )
    .unwrap();
    let config = cfg(&e, &[&s1.url], "questions = \"q.toml\"");
    wrap(&e, &config, "echo 'error: x'; exit 0");
    let f = last(&e.events()).clone();
    assert_eq!(f["reason"], "masked_failure", "0.6 >= custom threshold 0.5");
    assert_eq!(
        s1.bodies.lock().unwrap()[0]["questions"]["failing"]["instructions"],
        "Did it fail?"
    );
}

#[test]
fn config_subcommand_reports_sources() {
    let e = Env::new();
    let r = run({
        let mut c = std::process::Command::new(BIN);
        c.env("XDG_CONFIG_HOME", e.path("xdg"))
            .env("SYSTEMONE_URL", "http://env:1/x")
            .args(["config", "--s1-timeout", "2"]);
        c
    });
    let v: Value = serde_json::from_str(&r.out()).unwrap();
    assert_eq!(v["urls"], json!(["http://env:1/x"]));
    assert_eq!(v["urls_source"], "env SYSTEMONE_URL");
    assert_eq!(v["timeout_s"], 2.0);
    assert_eq!(v["timeout_source"], "cli");
    assert_eq!(v["questions_ok"], true);
}

// --- judge --posttooluse -----------------------------------------------------

fn judge(e: &Env, url: Option<&str>, input: &Value) -> Run {
    let mut c = std::process::Command::new(BIN);
    c.env_remove("SYSTEMONE_URL")
        .env("XDG_CONFIG_HOME", e.path("xdg"))
        .env("WATCHER_S1_STATE_DIR", e.path("state"))
        .args(["judge", "--posttooluse"]);
    if let Some(u) = url {
        c.args(["--s1-url", u]);
    }
    c.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let t0 = std::time::Instant::now();
    let mut ch = c.spawn().unwrap();
    ch.stdin
        .take()
        .unwrap()
        .write_all(input.to_string().as_bytes())
        .unwrap();
    let o = ch.wait_with_output().unwrap();
    Run {
        status: o.status,
        stdout: o.stdout,
        stderr: String::from_utf8_lossy(&o.stderr).into(),
        elapsed: t0.elapsed(),
    }
}

fn bash_input(cmd: &str, stdout: &str) -> Value {
    json!({
        "session_id": "s", "transcript_path": "/tmp/t.jsonl", "cwd": "/tmp",
        "permission_mode": "default", "hook_event_name": "PostToolUse",
        "tool_name": "Bash", "tool_use_id": "toolu_1", "duration_ms": 10,
        "tool_input": {"command": cmd, "description": "d", "timeout": 120000},
        "tool_response": {"stdout": stdout, "stderr": "", "interrupted": false, "isImage": false}
    })
}

const FAILING_OUT: &str = "running 2 tests\ntest a ... ok\ntest b ... FAILED\n\nfailures:\n    b\n\ntest result: FAILED. 1 passed; 1 failed\nerror: test failed, to rerun pass `--lib`\n";

#[test]
fn judge_flags_a_masked_pipe() {
    let s1 = FakeS1::start(Reply::Answers {
        failing: 0.97,
        clean_done: 0.02,
    });
    let e = Env::new();
    let r = judge(
        &e,
        Some(&s1.url),
        &bash_input("cargo test 2>&1 | tail -20", FAILING_OUT),
    );
    assert!(r.status.success());
    let v: Value = serde_json::from_slice(&r.stdout).unwrap();
    assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PostToolUse");
    let ctx = v["hookSpecificOutput"]["additionalContext"].as_str().unwrap();
    assert!(ctx.contains("exit 0 came from the pipe"), "{ctx}");
    assert!(ctx.contains("error: test failed, to rerun pass `--lib`"), "{ctx}");
    assert_eq!(s1.hits(), 1);
    assert!(
        s1.bodies.lock().unwrap()[0]["state"]
            .as_str()
            .unwrap()
            .starts_with("Command: cargo test 2>&1 | tail -20\n")
    );
}

#[test]
fn judge_is_silent_below_threshold_unpiped_or_without_endpoint() {
    let low = FakeS1::start(Reply::Answers {
        failing: 0.4,
        clean_done: 0.6,
    });
    let e = Env::new();
    for (url, cmd) in [
        (Some(low.url.as_str()), "cargo test | tail"),
        (Some(low.url.as_str()), "cargo test"),
        (None, "cargo test | tail"),
    ] {
        let r = judge(&e, url, &bash_input(cmd, FAILING_OUT));
        assert!(r.status.success());
        assert!(r.stdout.is_empty(), "{cmd}: {}", r.out());
        assert!(r.stderr.is_empty(), "{cmd}: {}", r.stderr);
    }
    assert_eq!(low.hits(), 1, "only the piped command is judged");
}

#[test]
fn judge_fails_open_inside_its_budget() {
    let hang = FakeS1::start(Reply::Hang);
    let e = Env::new();
    let r = judge(&e, Some(&hang.url), &bash_input("make 2>&1 | tail", FAILING_OUT));
    assert!(r.status.success());
    assert!(r.stdout.is_empty());
    assert!(r.elapsed < Duration::from_millis(3200), "{:?}", r.elapsed);
}

#[test]
fn judge_survives_garbage_input() {
    let mut c = std::process::Command::new(BIN);
    c.args(["judge", "--posttooluse"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());
    let mut ch = c.spawn().unwrap();
    ch.stdin.take().unwrap().write_all(b"not json").unwrap();
    let o = ch.wait_with_output().unwrap();
    assert!(o.status.success());
    assert!(o.stdout.is_empty());
}

// --- https -------------------------------------------------------------------

/// A one-shot TLS System One on localhost with a fresh self-signed cert.
/// Returns (url, path of the cert PEM to trust via SSL_CERT_FILE).
fn tls_fake(e: &Env, reply_body: &'static str, hang: bool) -> (String, std::path::PathBuf) {
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    use std::io::{Read, Write};
    let ck = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let pem = e.path("ca.pem");
    std::fs::write(&pem, ck.cert.pem()).unwrap();
    let certs = vec![CertificateDer::from(ck.cert.der().to_vec())];
    let key = PrivateKeyDer::try_from(ck.signing_key.serialize_der()).unwrap();
    let provider = std::sync::Arc::new(rustls::crypto::ring::default_provider());
    let cfg = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .unwrap();
    let cfg = std::sync::Arc::new(cfg);
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("https://localhost:{}/v1/systemone", l.local_addr().unwrap().port());
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            if hang {
                std::thread::sleep(Duration::from_secs(30));
                drop(s);
                continue;
            }
            let conn = rustls::ServerConnection::new(cfg.clone()).unwrap();
            let mut tls = rustls::StreamOwned::new(conn, s);
            let mut req = Vec::new();
            let mut buf = [0u8; 8192];
            while let Ok(n) = tls.read(&mut buf) {
                if n == 0 {
                    break;
                }
                req.extend_from_slice(&buf[..n]);
                if let Some(h) = req.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&req[..h]).to_ascii_lowercase();
                    let len: usize = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:").map(|v| v.trim().parse().unwrap_or(0)))
                        .unwrap_or(0);
                    if req.len() >= h + 4 + len {
                        break;
                    }
                }
            }
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{reply_body}",
                reply_body.len()
            );
            let _ = tls.write_all(resp.as_bytes());
            let _ = tls.flush();
            tls.conn.send_close_notify();
            let _ = tls.flush();
        }
    });
    (url, pem)
}

const FAILING_ANSWERS: &str =
    r#"{"answers":{"failing":{"type":"noul","noul":0.9},"clean_done":{"type":"noul","noul":0.1}}}"#;

#[test]
fn https_endpoint_with_a_trusted_private_ca() {
    let e = Env::new();
    let (url, pem) = tls_fake(&e, FAILING_ANSWERS, false);
    let r = run({
        let mut c = e.cmd();
        c.env("SSL_CERT_FILE", &pem).args([
            "-q",
            "--silence",
            "0",
            "--s1-url",
            &url,
            "--",
            "sh",
            "-c",
            "echo 'error: x'; exit 1",
        ]);
        c
    });
    assert_eq!(r.status.code(), Some(1));
    let f = last(&e.events()).clone();
    assert_eq!(f["s1"]["endpoint"], url, "{f}");
    assert!((f["s1"]["fused"].as_f64().unwrap() - 0.9).abs() < 1e-9);
}

#[test]
fn https_with_an_untrusted_cert_fails_open() {
    let e = Env::new();
    let (url, _pem) = tls_fake(&e, FAILING_ANSWERS, false);
    let r = run({
        let mut c = e.cmd();
        c.env_remove("SSL_CERT_FILE").args([
            "-q",
            "--silence",
            "0",
            "--s1-url",
            &url,
            "--",
            "sh",
            "-c",
            "echo 'error: x'; exit 1",
        ]);
        c
    });
    assert_eq!(r.status.code(), Some(1));
    assert_eq!(last(&e.events())["s1"], Value::Null);
}

#[test]
fn a_hung_tls_handshake_respects_the_deadline() {
    let e = Env::new();
    let (url, pem) = tls_fake(&e, FAILING_ANSWERS, true);
    let r = run({
        let mut c = e.cmd();
        c.env("SSL_CERT_FILE", &pem).args([
            "-q",
            "--silence",
            "0",
            "--s1-url",
            &url,
            "--s1-timeout",
            "0.5",
            "--",
            "sh",
            "-c",
            "echo 'error: x'; exit 1",
        ]);
        c
    });
    assert_eq!(r.status.code(), Some(1));
    assert!(r.elapsed < Duration::from_secs(3), "{:?}", r.elapsed);
    assert_eq!(last(&e.events())["s1"], Value::Null);
}
