//! `watcher-s1 judge --posttooluse`: a Claude Code PostToolUse hook for
//! Bash. PostToolUse fires only for successful calls, so every input here is
//! an exit-0 command. When that command pipes into a filter (`| tail`,
//! `| grep`, ...), the 0 is the filter's, not the producer's; if System One
//! then reads the output as an unrecovered failure, tell the agent.
//!
//! Hard rules: at most ~3 s end to end, fail open, never block the tool.
//! Anything unexpected is a silent no-op with exit 0.

use crate::detect;
use crate::ring;
use crate::s1::Client;
use serde_json::{Value, json};
use std::io::Read;
use std::time::{Duration, Instant};

/// Wall-clock budget for the whole hook, enforced by a watchdog.
pub const HARD_BUDGET: Duration = Duration::from_millis(2900);

/// Filters whose exit status says nothing about their input's producer.
const FILTERS: &[&str] = &[
    "tail", "head", "grep", "egrep", "fgrep", "rg", "ag", "sed", "awk", "gawk", "mawk", "cut", "sort", "uniq", "wc",
    "tee", "less", "more", "cat", "jq", "yq", "column", "tr", "fold", "nl", "fmt", "ts", "paste", "strings",
    "ansi2txt", "col", "bat", "ccze",
];

/// Split a shell command into top-level statements, each a list of pipe
/// stages. Quotes, escapes, `$(...)`, backticks and subshell parentheses
/// are kept opaque. Not a full parser; good enough to find `| tail`.
pub fn pipelines(cmd: &str) -> Vec<Vec<String>> {
    let mut stmts: Vec<Vec<String>> = vec![vec![String::new()]];
    let cs: Vec<char> = cmd.chars().collect();
    let (mut i, mut depth) = (0, 0i32);
    let (mut sq, mut dq, mut bq) = (false, false, false);
    let push = |stmts: &mut Vec<Vec<String>>, c: char| stmts.last_mut().unwrap().last_mut().unwrap().push(c);
    while i < cs.len() {
        let c = cs[i];
        let next = cs.get(i + 1).copied();
        if sq {
            sq = c != '\'';
        } else if c == '\\' {
            push(&mut stmts, c);
            if let Some(n) = next {
                push(&mut stmts, n);
            }
            i += 2;
            continue;
        } else if dq {
            dq = c != '"';
        } else if bq {
            bq = c != '`';
        } else {
            match c {
                '\'' => sq = true,
                '"' => dq = true,
                '`' => bq = true,
                '(' => depth += 1,
                ')' => depth -= 1,
                '|' if depth == 0 && next == Some('|') => {
                    stmts.push(vec![String::new()]);
                    i += 2;
                    continue;
                }
                '|' if depth == 0 => {
                    stmts.last_mut().unwrap().push(String::new());
                    i += if next == Some('&') { 2 } else { 1 };
                    continue;
                }
                '&' if depth == 0 && next == Some('&') => {
                    stmts.push(vec![String::new()]);
                    i += 2;
                    continue;
                }
                ';' | '\n' | '&' if depth == 0 => {
                    stmts.push(vec![String::new()]);
                    i += 1;
                    continue;
                }
                _ => {}
            }
        }
        push(&mut stmts, c);
        i += 1;
    }
    stmts
        .into_iter()
        .map(|s| s.into_iter().map(|p| p.trim().to_string()).collect::<Vec<_>>())
        .filter(|s| s.iter().any(|p| !p.is_empty()))
        .collect()
}

/// The program a stage runs, skipping `VAR=x` prefixes and wrappers.
fn program(stage: &str) -> Option<&str> {
    stage
        .split_whitespace()
        .find(|w| !w.contains('=') && !matches!(*w, "command" | "exec" | "nice" | "stdbuf" | "-oL" | "-o0" | "-eL"))
        .map(|w| w.rsplit('/').next().unwrap_or(w))
}

/// Does `cmd` hide a producer's exit status behind a filter stage?
pub fn masks_exit(cmd: &str) -> bool {
    if cmd.contains("pipefail") {
        return false;
    }
    pipelines(cmd).iter().any(|stages| {
        stages.len() >= 2
            && stages
                .last()
                .and_then(|s| program(s))
                .is_some_and(|p| FILTERS.contains(&p))
    })
}

/// One line of evidence: the last error-panel hit, else the last line.
pub fn evidence_line(text: &str) -> String {
    let line = detect::error_lines(text)
        .pop()
        .or_else(|| {
            text.lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .map(|l| l.trim().to_string())
        })
        .unwrap_or_default();
    let mut l: String = line.chars().take(200).collect();
    if line.chars().count() > 200 {
        l.push('…');
    }
    l
}

/// The decision, separated from I/O for tests. Returns the hook's JSON
/// output, or `None` for the silent no-op.
pub fn decide(input: &Value, client: Option<&Client>, budget: Duration) -> Option<Value> {
    if input.get("tool_name")?.as_str()? != "Bash" {
        return None;
    }
    let ti = input.get("tool_input")?;
    let cmd = ti.get("command")?.as_str()?;
    if ti.get("run_in_background").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let tr = input.get("tool_response")?;
    if tr.get("interrupted").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    if !masks_exit(cmd) {
        return None;
    }
    let client = client?;
    let stdout = tr.get("stdout").and_then(Value::as_str).unwrap_or("");
    let stderr = tr.get("stderr").and_then(Value::as_str).unwrap_or("");
    let mut out = ring::clean(stdout.as_bytes());
    if !stderr.trim().is_empty() {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&ring::clean(stderr.as_bytes()));
    }
    if out.trim().is_empty() {
        return None;
    }
    let tail = ring::tail(&out, client.questions.tail_bytes);
    let v = client.judge(cmd, tail, budget).ok()?;
    if v.fused < client.threshold() {
        return None;
    }
    let msg = format!(
        "watcher-s1: exit 0 came from the pipe; the output shows an unrecovered failure: {} \
         (System One fused score {:.2} >= {:.2}). Re-run without the filter, or with `set -o pipefail`, before trusting this result.",
        evidence_line(tail),
        v.fused,
        client.threshold()
    );
    Some(json!({
        "hookSpecificOutput": {
            "hookEventName": "PostToolUse",
            "additionalContext": msg,
        }
    }))
}

/// Run the hook: stdin JSON in, optional JSON out, always exit 0.
pub fn run_posttooluse(client: Option<Client>) -> ! {
    let t0 = Instant::now();
    // Watchdog: whatever hangs (stdin, DNS, the endpoint), we leave on time.
    std::thread::spawn(|| {
        std::thread::sleep(HARD_BUDGET);
        std::process::exit(0);
    });
    let mut raw = String::new();
    if std::io::stdin().take(8 << 20).read_to_string(&mut raw).is_err() {
        std::process::exit(0);
    }
    let Ok(input) = serde_json::from_str::<Value>(&raw) else {
        std::process::exit(0)
    };
    let left = HARD_BUDGET
        .saturating_sub(t0.elapsed())
        .saturating_sub(Duration::from_millis(150));
    let budget = client
        .as_ref()
        .map_or(left, |c| left.min(crate::supervise::s1_budget(c)));
    if let Some(out) = decide(&input, client.as_ref(), budget) {
        println!("{out}");
    }
    std::process::exit(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_masking_pipes() {
        for c in [
            "cargo test 2>&1 | tail -20",
            "cd x && cargo test | grep -E 'test result'",
            "nix build .#x 2>&1 | tail -5; echo done",
            "FOO=1 make |& tee build.log",
            "pytest -q | head -50",
            "just test 2>&1 | /usr/bin/tail -n 30",
        ] {
            assert!(masks_exit(c), "{c}");
        }
    }

    #[test]
    fn ignores_unmasked_commands() {
        for c in [
            "cargo test",
            "cargo test || true",
            "echo 'a | tail' && ls",
            "set -o pipefail; cargo test | tail",
            "ls | xargs rm",
            "git log --format='%h|%s' -5",
            "echo $(ls | tail -1)",
        ] {
            assert!(!masks_exit(c), "{c}");
        }
    }

    #[test]
    fn pipelines_split_statements_and_stages() {
        let p = pipelines("a | b && c ; d | e | f");
        assert_eq!(p, vec![vec!["a", "b"], vec!["c"], vec!["d", "e", "f"]]);
        assert_eq!(pipelines("echo 'x|y' | tail"), vec![vec!["echo 'x|y'", "tail"]]);
    }

    #[test]
    fn evidence_prefers_error_lines() {
        let t = "running 3 tests\nerror: test failed, to rerun pass `--lib`\nfinished\n";
        assert_eq!(evidence_line(t), "error: test failed, to rerun pass `--lib`");
        assert_eq!(evidence_line("a\nb\n\n"), "b");
    }

    #[test]
    fn noop_without_endpoint_or_pipe() {
        let input = json!({
            "tool_name": "Bash",
            "tool_input": {"command": "cargo test | tail"},
            "tool_response": {"stdout": "error: boom", "stderr": "", "interrupted": false, "isImage": false}
        });
        assert_eq!(decide(&input, None, Duration::from_secs(1)), None);
        let mut other = input.clone();
        other["tool_name"] = "Write".into();
        assert_eq!(decide(&other, None, Duration::from_secs(1)), None);
    }
}
