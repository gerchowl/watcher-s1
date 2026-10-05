//! Command-line surface.

use clap::{Args, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;
use std::time::Duration;

/// Parse `300s`, `5m`, `1.5h`, `250ms` or a bare number of seconds.
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    let (num, mult) = if let Some(n) = s.strip_suffix("ms") {
        (n, 0.001)
    } else if let Some(n) = s.strip_suffix('s') {
        (n, 1.0)
    } else if let Some(n) = s.strip_suffix('m') {
        (n, 60.0)
    } else if let Some(n) = s.strip_suffix('h') {
        (n, 3600.0)
    } else {
        (s, 1.0)
    };
    let v: f64 = num
        .trim()
        .parse()
        .map_err(|_| format!("invalid duration {s:?} (try 300s, 5m, 1h)"))?;
    if !v.is_finite() || v < 0.0 {
        return Err(format!("invalid duration {s:?}"));
    }
    Duration::try_from_secs_f64(v * mult).map_err(|_| format!("duration {s:?} is out of range"))
}

#[derive(Debug, Parser)]
#[command(
    name = "watcher-s1",
    version,
    about = "Run a command and watch it: stalls, prompts and failures go to a JSON sideband; the exit status stays the child's.",
    args_conflicts_with_subcommands = true,
    subcommand_negates_reqs = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub sub: Option<Sub>,
    #[command(flatten)]
    pub wrap: WrapArgs,
}

#[derive(Debug, Subcommand)]
pub enum Sub {
    /// Judge a finished command (Claude Code hook mode).
    Judge(JudgeArgs),
    /// Print the resolved System One configuration and where each value came from.
    Config(S1Args),
}

#[derive(Debug, Args)]
pub struct JudgeArgs {
    /// Read a Claude Code PostToolUse hook payload on stdin.
    #[arg(long, required = true)]
    pub posttooluse: bool,
    #[command(flatten)]
    pub s1: S1Args,
}

#[derive(Debug, Args, Clone, Default)]
pub struct S1Args {
    /// System One endpoint (repeatable, ordered). Overrides SYSTEMONE_URL and config files.
    #[arg(long = "s1-url", value_name = "URL")]
    pub s1_url: Vec<String>,
    /// Per-call System One timeout in seconds.
    #[arg(long = "s1-timeout", value_name = "SECS")]
    pub s1_timeout: Option<f64>,
    /// Use this config file instead of the XDG / /etc search.
    #[arg(long, value_name = "FILE")]
    pub config: Option<PathBuf>,
    /// Turn the System One tier off regardless of configuration.
    #[arg(long = "no-s1")]
    pub no_s1: bool,
}

impl S1Args {
    pub fn to_cli(&self) -> crate::config::CliS1 {
        crate::config::CliS1 {
            urls: self.s1_url.clone(),
            timeout_s: self.s1_timeout,
            config: self.config.clone(),
            disabled: self.no_s1,
        }
    }
}

#[derive(Debug, Args)]
pub struct WrapArgs {
    /// Use plain pipes instead of a PTY (stdout and stderr stay separate).
    #[arg(long)]
    pub pipe: bool,
    /// Output-silence threshold; 0 disables.
    #[arg(long, value_name = "DUR", default_value = "300s", value_parser = parse_duration)]
    pub silence: Duration,
    /// Hard limit: SIGTERM the whole process group, then SIGKILL after --kill-grace.
    #[arg(long, value_name = "DUR", value_parser = parse_duration)]
    pub timeout: Option<Duration>,
    /// Grace between SIGTERM and SIGKILL on --timeout.
    #[arg(long, value_name = "DUR", default_value = "10s", value_parser = parse_duration)]
    pub kill_grace: Duration,
    /// Quiet time before a prompt-shaped last line counts as waiting on input.
    #[arg(long, value_name = "DUR", default_value = "5s", value_parser = parse_duration)]
    pub prompt_after: Duration,
    /// Process-state sampling interval while the output is quiet.
    #[arg(long, value_name = "DUR", default_value = "10s", value_parser = parse_duration)]
    pub sample_every: Duration,
    /// Raise `stalled` when the tree stays blocked (D/U state) or unprobeable this long.
    #[arg(long, value_name = "DUR", default_value = "60s", value_parser = parse_duration)]
    pub blocked_after: Duration,
    /// Wall-clock limit for one process-state probe.
    #[arg(long, value_name = "DUR", default_value = "2s", value_parser = parse_duration)]
    pub probe_timeout: Duration,
    /// What to do about a prompt nobody answers: `wait` (only report it) or
    /// `cancel` (SIGINT the group after --prompt-cancel-after, then TERM and
    /// KILL). Never answers the prompt.
    #[arg(long, value_enum, default_value_t = OnPrompt::Wait)]
    pub on_prompt: OnPrompt,
    /// With `--on-prompt cancel`: how long a prompt may wait unanswered.
    #[arg(long, value_name = "DUR", default_value = "60s", value_parser = parse_duration)]
    pub prompt_cancel_after: Duration,
    /// Passive mode: watch a growing log FILE instead of running a command
    /// (silence, prompts and System One at the silence threshold; no exit).
    #[arg(long, value_name = "FILE", conflicts_with_all = ["pipe", "timeout", "on_prompt"])]
    pub log: Option<PathBuf>,
    /// Bytes of output tail carried in each event.
    #[arg(long, value_name = "N", default_value_t = 1500)]
    pub evidence_bytes: usize,
    /// Append events as JSON lines to FILE.
    #[arg(long, value_name = "FILE", conflicts_with = "events_fd")]
    pub events: Option<PathBuf>,
    /// Write events as JSON lines to an inherited file descriptor.
    #[arg(long, value_name = "N")]
    pub events_fd: Option<i32>,
    /// No `watcher-s1 (log):` diagnostics on stderr (events still flow).
    #[arg(long, short)]
    pub quiet: bool,
    #[command(flatten)]
    pub s1: S1Args,
    /// The command to run, after `--`.
    #[arg(last = true, required_unless_present = "log", value_name = "CMD")]
    pub cmd: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OnPrompt {
    Wait,
    Cancel,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration("300s").unwrap(), Duration::from_secs(300));
        assert_eq!(parse_duration("5m").unwrap(), Duration::from_secs(300));
        assert_eq!(parse_duration("1.5h").unwrap(), Duration::from_secs(5400));
        assert_eq!(parse_duration("250ms").unwrap(), Duration::from_millis(250));
        assert_eq!(parse_duration("7").unwrap(), Duration::from_secs(7));
        assert!(parse_duration("-1s").is_err());
        assert!(parse_duration("soon").is_err());
        assert!(parse_duration("1e20").is_err());
    }

    #[test]
    fn wrap_needs_double_dash() {
        let c = Cli::try_parse_from(["watcher-s1", "--silence", "1m", "--", "ls", "-la"]).unwrap();
        assert!(c.sub.is_none());
        assert_eq!(c.wrap.cmd, ["ls", "-la"]);
        assert_eq!(c.wrap.silence, Duration::from_secs(60));
        assert!(Cli::try_parse_from(["watcher-s1"]).is_err());
        let c = Cli::try_parse_from(["watcher-s1", "--log", "/var/log/x.log"]).unwrap();
        assert!(c.wrap.cmd.is_empty() && c.wrap.log.is_some());
        assert!(Cli::try_parse_from(["watcher-s1", "--log", "x", "--timeout", "1s"]).is_err());
        let c = Cli::try_parse_from(["watcher-s1", "--on-prompt", "cancel", "--", "x"]).unwrap();
        assert_eq!(c.wrap.on_prompt, OnPrompt::Cancel);
        // A command that looks like a flag still belongs to the child.
        let c = Cli::try_parse_from(["watcher-s1", "--", "judge", "--posttooluse"]).unwrap();
        assert_eq!(c.wrap.cmd, ["judge", "--posttooluse"]);
    }

    #[test]
    fn judge_subcommand() {
        let c = Cli::try_parse_from(["watcher-s1", "judge", "--posttooluse", "--s1-url", "http://h:1/x"]).unwrap();
        match c.sub {
            Some(Sub::Judge(j)) => assert_eq!(j.s1.s1_url, ["http://h:1/x"]),
            other => panic!("{other:?}"),
        }
    }
}
