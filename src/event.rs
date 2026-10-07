//! The sideband event: one JSON object per line, versioned by
//! `event.schema.json` (schema 1). watcher-s1 owns no alerting policy; a
//! gateway consumes these and decides what reaches a human.

use crate::s1::Verdict;
use serde::Serialize;
use std::fs::File;
use std::io::Write;
use std::os::fd::{FromRawFd, OwnedFd};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

pub const SCHEMA_VERSION: u32 = 1;
pub const SCHEMA_JSON: &str = include_str!("../event.schema.json");
/// Line prefix on the default stderr sink.
pub const STDERR_PREFIX: &str = "watcher-s1: ";
/// The env var that carries a parent watcher's `run_id` to its children.
pub const ENV_PARENT: &str = "WATCHER_S1_PARENT";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Progressing,
    Stalled,
    WaitingOnInput,
    Failing,
    Done,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Exit {
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

/// Process-state probe result (tier 0), attached when a sample exists.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProcInfo {
    /// `ok`, `timeout` (the probe itself hung: maybe wedged) or `error`.
    pub probe: String,
    /// Processes in uninterruptible sleep (Linux `D`, darwin `U`).
    pub blocked: Vec<BlockedProc>,
    /// The blocked condition has held for this long (seconds).
    pub blocked_for_s: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BlockedProc {
    pub pid: i32,
    pub state: String,
    pub wchan: Option<String>,
    pub comm: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Event {
    pub schema: u32,
    pub ts: String,
    pub host: String,
    pub source: &'static str,
    pub run_id: String,
    pub cmd: String,
    pub pid: i32,
    pub pgid: i32,
    pub state: State,
    pub reason: String,
    pub exit: Option<Exit>,
    pub severity: Severity,
    pub dedup_key: String,
    pub caused_by: Option<String>,
    pub evidence_tail: String,
    pub s1: Option<Verdict>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proc: Option<ProcInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// Heartbeat-only fields, flattened into the event.
    #[serde(flatten)]
    pub heartbeat: Option<Heartbeat>,
}

/// What a `heartbeat` event adds: progress since the previous one.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Heartbeat {
    /// Since start (since attach in `--log` mode).
    pub elapsed_ms: u64,
    pub bytes_since_last: u64,
    pub lines_since_last: u64,
    /// Last non-empty line, ANSI stripped, at most [`LAST_LINE_MAX`] chars.
    pub last_line: Option<String>,
}

pub const LAST_LINE_MAX: usize = 200;

/// Run-scoped fields every event of one watcher shares.
#[derive(Debug, Clone)]
pub struct RunInfo {
    pub host: String,
    pub run_id: String,
    pub cmd: String,
    pub pid: i32,
    pub pgid: i32,
    pub caused_by: Option<String>,
}

impl RunInfo {
    pub fn event(&self, state: State, severity: Severity, reason: &str, evidence_tail: String) -> Event {
        Event {
            schema: SCHEMA_VERSION,
            ts: rfc3339_now(),
            host: self.host.clone(),
            source: "watcher-s1",
            run_id: self.run_id.clone(),
            cmd: self.cmd.clone(),
            pid: self.pid,
            pgid: self.pgid,
            state,
            reason: reason.to_string(),
            exit: None,
            severity,
            dedup_key: dedup_key(&self.host, &self.cmd, state),
            caused_by: self.caused_by.clone(),
            evidence_tail,
            s1: None,
            proc: None,
            prompt: None,
            heartbeat: None,
        }
    }

    /// A periodic status event: the current episode `state`, always `info`
    /// (the edge event already alerted), and one dedup key for all of a job's
    /// heartbeats whatever their state.
    pub fn heartbeat(&self, state: State, evidence_tail: String, hb: Heartbeat) -> Event {
        let mut ev = self.event(state, Severity::Info, "heartbeat", evidence_tail);
        ev.dedup_key = dedup_key_segment(&self.host, &self.cmd, "heartbeat");
        ev.heartbeat = Some(hb);
        ev
    }
}

/// Stable across runs of the same command on the same host, so a gateway
/// can fold repeats: `watcher-s1:<host>:<state>:<fnv1a64(cmd)>`.
pub fn dedup_key(host: &str, cmd: &str, state: State) -> String {
    let state = serde_json::to_value(state)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default();
    dedup_key_segment(host, cmd, &state)
}

/// [`dedup_key`] with an explicit state segment (`heartbeat`).
pub fn dedup_key_segment(host: &str, cmd: &str, segment: &str) -> String {
    format!("watcher-s1:{host}:{segment}:{:016x}", fnv1a64(cmd.as_bytes()))
}

fn fnv1a64(b: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for x in b {
        h ^= *x as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

pub fn hostname() -> String {
    nix::unistd::gethostname()
        .ok()
        .and_then(|h| h.into_string().ok())
        .map(|h| h.split('.').next().unwrap_or(&h).to_string())
        .unwrap_or_else(|| "unknown".into())
}

/// Quote argv for display the way a POSIX shell would read it back.
pub fn shell_join(argv: &[String]) -> String {
    argv.iter().map(|a| shell_quote(a)).collect::<Vec<_>>().join(" ")
}

fn shell_quote(a: &str) -> String {
    let safe = !a.is_empty()
        && a.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_./=:,+@%^".contains(&b));
    if safe {
        a.to_string()
    } else {
        format!("'{}'", a.replace('\'', r"'\''"))
    }
}

pub fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

pub fn rfc3339_now() -> String {
    rfc3339(unix_ms())
}

/// UTC RFC 3339 with milliseconds, from Unix milliseconds.
pub fn rfc3339(ms: u128) -> String {
    let secs = (ms / 1000) as i64;
    let (days, sod) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    // Howard Hinnant's civil_from_days.
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        sod / 3600,
        sod % 3600 / 60,
        sod % 60,
        ms % 1000
    )
}

/// Where events go.
pub enum Sink {
    /// One line per event on stderr, prefixed `watcher-s1: `.
    Stderr,
    /// Plain JSONL to a file (appended) or an inherited fd.
    Writer(Mutex<File>),
}

impl Sink {
    pub fn file(path: &str) -> std::io::Result<Self> {
        let f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Sink::Writer(Mutex::new(f)))
    }

    /// Duplicate `fd` so the caller's descriptor stays theirs.
    pub fn fd(fd: i32) -> std::io::Result<Self> {
        let dup = nix::fcntl::fcntl(
            unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) },
            nix::fcntl::FcntlArg::F_DUPFD_CLOEXEC(3),
        )
        .map_err(std::io::Error::from)?;
        Ok(Sink::Writer(Mutex::new(File::from(unsafe {
            OwnedFd::from_raw_fd(dup)
        }))))
    }

    /// Emit one event as one write. Errors are swallowed: the sideband must
    /// never take down the wrapped command.
    pub fn emit(&self, ev: &Event) {
        let Ok(json) = serde_json::to_string(ev) else { return };
        match self {
            Sink::Stderr => {
                let line = format!("{STDERR_PREFIX}{json}\n");
                let _ = std::io::stderr().lock().write_all(line.as_bytes());
            }
            Sink::Writer(f) => {
                let line = format!("{json}\n");
                if let Ok(mut f) = f.lock() {
                    let _ = f.write_all(line.as_bytes());
                    let _ = f.flush();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Event {
        let run = RunInfo {
            host: "vm-dev".into(),
            run_id: "vm-dev:42:1".into(),
            cmd: "cargo test".into(),
            pid: 42,
            pgid: 42,
            caused_by: Some("vm-dev:7:1".into()),
        };
        let mut e = run.event(State::Failing, Severity::Error, "exit", "boom\n".into());
        e.exit = Some(Exit {
            code: Some(3),
            signal: None,
        });
        e.s1 = Some(Verdict {
            endpoint: "http://h:1/v1/systemone".into(),
            failing: Some(0.9),
            clean_done: Some(0.1),
            fused: 0.9,
            latency_ms: 500,
        });
        e.proc = Some(ProcInfo {
            probe: "ok".into(),
            blocked: vec![BlockedProc {
                pid: 43,
                state: "D".into(),
                wchan: Some("nfs_wait".into()),
                comm: "nix".into(),
            }],
            blocked_for_s: 30,
        });
        e
    }

    fn validator() -> jsonschema::Validator {
        let schema: serde_json::Value = serde_json::from_str(SCHEMA_JSON).unwrap();
        jsonschema::validator_for(&schema).unwrap()
    }

    #[test]
    fn events_validate_against_the_schema() {
        let v = validator();
        let full = serde_json::to_value(sample()).unwrap();
        assert!(
            v.is_valid(&full),
            "{:?}",
            v.iter_errors(&full).map(|e| e.to_string()).collect::<Vec<_>>()
        );
        let mut minimal = sample();
        minimal.s1 = None;
        minimal.exit = None;
        minimal.caused_by = None;
        minimal.proc = None;
        minimal.state = State::Stalled;
        let m = serde_json::to_value(minimal).unwrap();
        assert!(v.is_valid(&m));
        assert_eq!(m["s1"], serde_json::Value::Null);
        assert_eq!(m["exit"], serde_json::Value::Null);
    }

    #[test]
    fn schema_rejects_drift() {
        let v = validator();
        let mut e = serde_json::to_value(sample()).unwrap();
        e["state"] = "exploded".into();
        assert!(!v.is_valid(&e));
        let mut e = serde_json::to_value(sample()).unwrap();
        e.as_object_mut().unwrap().remove("dedup_key");
        assert!(!v.is_valid(&e));
        let mut e = serde_json::to_value(sample()).unwrap();
        e["schema"] = 2.into();
        assert!(!v.is_valid(&e));
    }

    #[test]
    fn dedup_key_is_stable_and_state_scoped() {
        let a = dedup_key("h", "cargo test", State::Failing);
        assert_eq!(a, dedup_key("h", "cargo test", State::Failing));
        assert!(a.starts_with("watcher-s1:h:failing:"));
        assert_ne!(a, dedup_key("h", "cargo test", State::Stalled));
        assert_ne!(a, dedup_key("h", "cargo build", State::Failing));
    }

    #[test]
    fn heartbeat_events_validate_and_share_one_key() {
        let run = RunInfo {
            host: "h".into(),
            run_id: "h:1:1".into(),
            cmd: "make".into(),
            pid: 1,
            pgid: 1,
            caused_by: None,
        };
        let hb = |last_line| Heartbeat {
            elapsed_ms: 5,
            bytes_since_last: 1,
            lines_since_last: 1,
            last_line,
        };
        let a = run.heartbeat(State::Progressing, String::new(), hb(None));
        let b = run.heartbeat(State::Stalled, String::new(), hb(Some("x".into())));
        assert_eq!(a.dedup_key, b.dedup_key);
        assert!(a.dedup_key.starts_with("watcher-s1:h:heartbeat:"));
        assert_eq!((a.severity, a.reason.as_str()), (Severity::Info, "heartbeat"));
        let v = validator();
        for e in [a, b] {
            let j = serde_json::to_value(e).unwrap();
            assert!(v.is_valid(&j), "{j}");
        }
        let j = serde_json::to_value(run.heartbeat(State::Progressing, String::new(), hb(None))).unwrap();
        assert_eq!(j["last_line"], serde_json::Value::Null);
        assert_eq!(j["elapsed_ms"], 5);
        // Other events carry none of the heartbeat fields.
        let j = serde_json::to_value(run.event(State::Done, Severity::Info, "exit", String::new())).unwrap();
        assert!(j.get("elapsed_ms").is_none());
    }

    #[test]
    fn rfc3339_formats_utc() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(rfc3339(1_791_207_377_123), "2026-10-05T13:36:17.123Z");
        assert_eq!(rfc3339(951_782_400_000), "2000-02-29T00:00:00.000Z");
    }

    #[test]
    fn shell_join_round_trips_through_sh() {
        let argv: Vec<String> = ["echo", "a b", "it's", "", "x=1"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let joined = shell_join(&argv);
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("printf '%s|' {}", &joined[5..]))
            .output()
            .unwrap();
        assert_eq!(String::from_utf8(out.stdout).unwrap(), "a b|it's||x=1|");
    }
}
