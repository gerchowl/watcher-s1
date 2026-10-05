//! Real System One (Kev) checks. Opt-in: they run only when SYSTEMONE_URL
//! is set, e.g.
//!
//!   SYSTEMONE_URL=http://sage.tail22bd7c.ts.net:8023/v1/systemone \
//!     cargo test --test kev -- --nocapture
//!
//! Everything lives in ONE test so the calls are strictly sequential: the
//! Kev host is wedge-prone and must never see a burst. No endpoint is
//! compiled in or committed as a default.

mod common;
use common::*;

#[test]
fn real_system_one_sequential() {
    let Some(url) = std::env::var("SYSTEMONE_URL").ok().filter(|u| !u.is_empty()) else {
        eprintln!("SYSTEMONE_URL unset: skipping the real System One checks");
        return;
    };
    // A tail that ENDS in an error: the measured fused score clears 0.8.
    let failing = "   Compiling foo v0.1.0 (/x)\nerror[E0425]: cannot find value `cfg` in this scope\n  --> src/main.rs:12:5\n   |\n12 |     cfg.run();\n   |     ^^^ not found in this scope\n\nerror: could not compile `foo` (bin \"foo\") due to 1 previous error\n";
    let clean = "running 3 tests\ntest parse ... ok\ntest roundtrip ... ok\ntest edge ... ok\n\ntest result: ok. 3 passed; 0 failed; 0 ignored\n";
    // A test summary reads as "reached its end" to `clean_done`, so its fused
    // score sits near 0.5 (measured 2026-10-05): ranked, but not flagged.
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
    assert!(f_summary > f_clean + 0.2);
}
