//! Tier 1: deterministic text detectors. Prompts are text, so a regex on
//! the unterminated last line finds them (no model needed). The error panel
//! is a weak signal: it only decides whether an exit-0 tail is worth a
//! System One call.

use regex::{Regex, RegexSet};
use std::sync::OnceLock;

const PROMPTS: &[&str] = &[
    r"(?i)pass(word|phrase)\b[^\n]{0,80}[:?]\s*$",
    r"(?i)\bpassphrase\b",
    r"(?i)\[\s*y\s*/\s*n\s*\]",
    r"(?i)\(\s*y(es)?\s*/\s*n(o)?\s*(/\s*\[?fingerprint\]?)?\s*\)",
    r"(?i)authenticity of host",
    r"(?i)\bpress\b.{0,40}\b(key|enter|return)\b",
    r"(?i)\b(username|login|user name)\b[^\n]{0,80}:\s*$",
    r"(?i)\b(verification code|one-time (pass)?code|otp|2fa code|pin)\b[^\n]{0,40}:\s*$",
    r"(?i)\b(continue|proceed|overwrite|replace)\b[^\n]{0,60}\?\s*$",
];

/// How much of the text before the prompt line can carry the prompt's
/// context (ssh prints "authenticity of host" on an earlier line).
const PROMPT_CONTEXT: usize = 512;

fn prompt_set() -> &'static RegexSet {
    static S: OnceLock<RegexSet> = OnceLock::new();
    S.get_or_init(|| RegexSet::new(PROMPTS).expect("prompt regexes compile"))
}

/// The prompt text if `text` (cleaned output) ends waiting on input: the
/// last line is unterminated and it, or the few lines before it, matches.
pub fn prompt(text: &str) -> Option<String> {
    let line = crate::ring::last_line(text);
    let l = line.trim();
    if l.is_empty() {
        return None;
    }
    if prompt_set().is_match(line) {
        return Some(l.to_string());
    }
    let ctx = crate::ring::tail(text, PROMPT_CONTEXT);
    // Context-only matches need a prompt-shaped last line (ends in ? or :).
    if (l.ends_with('?') || l.ends_with(':')) && prompt_set().is_match(ctx) {
        return Some(l.to_string());
    }
    None
}

const ERRORS: &[&str] = &[
    r"(?m)^\s*error(\[E\d+\])?:",
    r"(?m)^\s*(fatal|FATAL)( error)?:",
    r"test result: FAILED",
    r"(?m)^(FAILED|FAIL)\b|\bFAILED\b",
    r"panicked at",
    r"Traceback \(most recent call last\)",
    r"(?m)^\s*[A-Za-z_.]*(Error|Exception):",
    r"npm ERR!",
    r"(?i)segmentation fault|core dumped|bus error",
    r"(?i)command not found",
    r"error: builder for .* failed",
    r"(?i)\b(build|compilation|tests?) failed\b",
    r"(?i)exit(ed)? (with )?(code|status) [1-9]",
    r"(?m)^\s*E\s{2,}",
];

fn error_regexes() -> &'static [Regex] {
    static S: OnceLock<Vec<Regex>> = OnceLock::new();
    S.get_or_init(|| {
        ERRORS
            .iter()
            .map(|r| Regex::new(r).expect("error regexes compile"))
            .collect()
    })
}

/// Lines of `text` that hit the error panel (weak; for gating only).
pub fn error_lines(text: &str) -> Vec<String> {
    text.lines()
        .filter(|l| error_regexes().iter().any(|r| r.is_match(l)))
        .map(|l| l.trim().chars().take(200).collect())
        .collect()
}

pub fn looks_failing(text: &str) -> bool {
    text.lines().any(|l| error_regexes().iter().any(|r| r.is_match(l)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_prompts_on_the_last_line() {
        for p in [
            "[sudo] password for lars: ",
            "Password:",
            "Enter passphrase for key '/home/x/.ssh/id_ed25519': ",
            "Proceed? [y/N] ",
            "Do you want to continue? [Y/n]",
            "Are you sure you want to continue connecting (yes/no/[fingerprint])? ",
            "Press any key to continue",
            "Press ENTER to continue...",
            "Username for 'https://github.com': ",
            "Overwrite existing file? ",
        ] {
            assert!(prompt(&format!("some output\n{p}")).is_some(), "missed {p:?}");
        }
    }

    #[test]
    fn ssh_host_key_prompt_via_context() {
        let t = "The authenticity of host 'x (1.2.3.4)' can't be established.\nED25519 key fingerprint is SHA256:abc.\nType it:";
        assert!(prompt(t).is_some());
    }

    #[test]
    fn ignores_terminated_and_ordinary_lines() {
        assert_eq!(prompt("Password: \n"), None);
        assert_eq!(prompt("Compiling foo v0.1.0"), None);
        assert_eq!(prompt("building '/nix/store/abc.drv'..."), None);
        assert_eq!(prompt("password reset done, all good"), None);
        assert_eq!(prompt(""), None);
    }

    #[test]
    fn error_panel_is_a_weak_net() {
        assert!(looks_failing("error[E0425]: cannot find value `x`"));
        assert!(looks_failing("test result: FAILED. 1 passed; 1 failed"));
        assert!(looks_failing("thread 'main' panicked at src/main.rs:2:5"));
        assert!(looks_failing(
            "Traceback (most recent call last):\n  File \"x\"\nValueError: bad"
        ));
        assert!(looks_failing(
            "error: builder for '/nix/store/x.drv' failed with exit code 1"
        ));
        assert!(!looks_failing("test result: ok. 12 passed; 0 failed"));
        assert!(!looks_failing("Finished `release` profile in 3.2s"));
        assert_eq!(error_lines("ok\nerror: boom\nok").len(), 1);
    }
}
