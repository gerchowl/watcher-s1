//! The state directory behind `watcher-s1 mcp`: one directory per run under
//! `runs/<id>/` holding `events.jsonl`, `output.log`, `meta.json` and a
//! `cursor`. Everything here is synchronous and knows nothing about MCP; the
//! directory is the source of truth, so a run started by an earlier server
//! process is just as visible as one started by this one.

use crate::cli::{parse_duration, parse_heartbeat};
use crate::event::rfc3339;
use crate::follow::{Event, EventReader};
use nix::sys::signal::{Signal, kill, killpg};
use nix::unistd::Pid;
use rmcp::schemars::{self, JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fs::{self, File};
use std::io;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Runs older than this are removed when a server starts.
pub const MAX_AGE: Duration = Duration::from_secs(7 * 24 * 3600);

pub const EVENTS: &str = "events.jsonl";
pub const LOG: &str = "output.log";
const META: &str = "meta.json";
const CURSOR: &str = "cursor";

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// A duration option as an agent writes it: seconds as a number, or the CLI
/// form (`90s`, `5m`, `2h`).
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum Dur {
    Secs(f64),
    Text(String),
}

impl Dur {
    /// The CLI spelling, validated.
    fn arg(&self, name: &str, parse: fn(&str) -> Result<Duration, String>) -> Result<String, String> {
        let text = match self {
            Dur::Secs(s) => format!("{s}s"),
            Dur::Text(t) => t.clone(),
        };
        parse(&text).map_err(|e| format!("{name}: {e}"))?;
        Ok(text)
    }
}

/// What `watch_start` takes.
#[derive(Debug, Clone, Default)]
pub struct StartOpts {
    pub cmd: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub silence: Option<Dur>,
    pub timeout: Option<Dur>,
    pub heartbeat: Option<Dur>,
    /// `Some(false)` passes `--no-s1`; otherwise the CLI default applies.
    pub s1: Option<bool>,
}

/// `meta.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Meta {
    pub id: String,
    pub cmd: Vec<String>,
    pub cwd: PathBuf,
    /// RFC 3339 and unix milliseconds of the start.
    pub started: String,
    pub started_ms: u64,
    pub watcher_pid: u32,
    /// The watcher-s1 flags the run was started with.
    pub options: Value,
    pub events: PathBuf,
    pub log: PathBuf,
}

/// Where a run stands, derived from its events and its watcher process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Running,
    /// The final event was written.
    Finished,
    /// The watcher is gone and left no final event (killed outright).
    Lost,
}

pub struct Runs {
    root: PathBuf,
}

/// Ids are `<unix ms>-<n>`: never a path.
fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

impl Runs {
    /// `state_dir/runs`, created if absent.
    pub fn open(state_dir: &Path) -> io::Result<Self> {
        let root = std::path::absolute(state_dir)?.join("runs");
        fs::create_dir_all(&root)?;
        Ok(Runs { root })
    }

    pub fn dir(&self, id: &str) -> PathBuf {
        self.root.join(id)
    }

    /// Remove runs started more than `max_age` ago whose watcher is gone.
    /// Returns how many went. Runs still alive are kept however old.
    pub fn prune(&self, max_age: Duration) -> usize {
        let cutoff = now_ms().saturating_sub(max_age.as_millis() as u64);
        let mut n = 0;
        for id in self.ids() {
            let Ok(meta) = self.meta(&id) else { continue };
            if meta.started_ms < cutoff && !pid_alive(meta.watcher_pid) && fs::remove_dir_all(self.dir(&id)).is_ok() {
                n += 1;
            }
        }
        n
    }

    fn ids(&self) -> Vec<String> {
        let Ok(rd) = fs::read_dir(&self.root) else {
            return vec![];
        };
        rd.filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter(|n| valid_id(n))
            .collect()
    }

    pub fn meta(&self, id: &str) -> Result<Meta, String> {
        if !valid_id(id) {
            return Err(format!("invalid run id {id:?}"));
        }
        let raw = fs::read_to_string(self.dir(id).join(META)).map_err(|e| format!("unknown run {id:?} ({e})"))?;
        serde_json::from_str(&raw).map_err(|e| format!("run {id:?} has a damaged meta.json ({e})"))
    }

    /// Runs, newest first.
    pub fn list(&self) -> Vec<Meta> {
        let mut all: Vec<Meta> = self.ids().iter().filter_map(|id| self.meta(id).ok()).collect();
        all.sort_by(|a, b| b.started_ms.cmp(&a.started_ms).then_with(|| b.id.cmp(&a.id)));
        all
    }

    /// Start `watcher-s1 --pipe --events ... -- cmd` detached: its own
    /// session, stdin from /dev/null, output to `output.log`.
    pub fn start(&self, opts: &StartOpts) -> Result<Meta, String> {
        if opts.cmd.is_empty() || opts.cmd[0].is_empty() {
            return Err("cmd must be a non-empty argv array".into());
        }
        let cwd = match &opts.cwd {
            Some(c) => std::path::absolute(c).map_err(|e| format!("cwd: {e}"))?,
            None => std::env::current_dir().map_err(|e| format!("cwd: {e}"))?,
        };
        if !cwd.is_dir() {
            return Err(format!("cwd {} is not a directory", cwd.display()));
        }
        let mut flags: Vec<String> = Vec::new();
        let mut options = serde_json::Map::new();
        for (name, v, parse) in [
            (
                "silence",
                &opts.silence,
                parse_duration as fn(&str) -> Result<Duration, String>,
            ),
            ("timeout", &opts.timeout, parse_duration),
            ("heartbeat", &opts.heartbeat, parse_heartbeat),
        ] {
            if let Some(d) = v {
                let text = d.arg(name, parse)?;
                flags.extend([format!("--{name}"), text.clone()]);
                options.insert(name.into(), json!(text));
            }
        }
        if opts.s1 == Some(false) {
            flags.push("--no-s1".into());
        }
        options.insert("s1".into(), json!(opts.s1.unwrap_or(true)));

        let (id, dir) = self.fresh_dir().map_err(|e| format!("state dir: {e}"))?;
        let events = dir.join(EVENTS);
        let log_path = dir.join(LOG);
        let log = File::create(&log_path).map_err(|e| format!("{}: {e}", log_path.display()))?;
        let log_err = log.try_clone().map_err(|e| e.to_string())?;
        let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
        let mut cmd = Command::new(exe);
        cmd.args(["--pipe", "--quiet", "--events"])
            .arg(&events)
            .args(&flags)
            .arg("--")
            .args(&opts.cmd)
            .current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(log)
            .stderr(log_err);
        // SAFETY: setsid is async-signal-safe and touches no shared state.
        unsafe {
            cmd.pre_exec(|| nix::unistd::setsid().map(|_| ()).map_err(io::Error::from));
        }
        let mut child = cmd.spawn().map_err(|e| format!("cannot start watcher-s1: {e}"))?;
        let watcher_pid = child.id();
        // Reap it when it ends, so it never lingers as a zombie that looks alive.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        let started_ms = now_ms();
        let meta = Meta {
            id,
            cmd: opts.cmd.clone(),
            cwd,
            started: rfc3339(started_ms as u128),
            started_ms,
            watcher_pid,
            options: Value::Object(options),
            events,
            log: log_path,
        };
        let text = serde_json::to_string_pretty(&meta).map_err(|e| e.to_string())?;
        fs::write(dir.join(META), text).map_err(|e| format!("meta.json: {e}"))?;
        Ok(meta)
    }

    /// Create `runs/<unix ms>-<n>`; `create_dir` is the uniqueness check.
    fn fresh_dir(&self) -> io::Result<(String, PathBuf)> {
        let ms = now_ms();
        for n in 0..10_000u32 {
            let id = format!("{ms}-{n}");
            let dir = self.dir(&id);
            match fs::create_dir(&dir) {
                Ok(()) => return Ok((id, dir)),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::other("no free run id"))
    }

    /// How many events the caller has been given (see `watch_wait`).
    pub fn cursor(&self, id: &str) -> u64 {
        fs::read_to_string(self.dir(id).join(CURSOR))
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0)
    }

    pub fn set_cursor(&self, id: &str, seq: u64) {
        let _ = fs::write(self.dir(id).join(CURSOR), seq.to_string());
    }

    /// Every event of the run so far.
    pub fn events(&self, meta: &Meta) -> io::Result<Vec<Event>> {
        EventReader::new(&meta.events).poll()
    }

    pub fn phase(&self, meta: &Meta, events: &[Event]) -> Phase {
        if events.iter().any(|e| e.is_final) {
            Phase::Finished
        } else if pid_alive(meta.watcher_pid) {
            Phase::Running
        } else {
            Phase::Lost
        }
    }

    /// When the run stopped producing events: the events file's mtime.
    pub fn last_write_ms(&self, meta: &Meta) -> Option<u64> {
        let m = fs::metadata(&meta.events).ok()?.modified().ok()?;
        Some(m.duration_since(UNIX_EPOCH).ok()?.as_millis() as u64)
    }
}

/// Is a process with this pid there (signal 0)?
pub fn pid_alive(pid: u32) -> bool {
    match kill(Pid::from_raw(pid as i32), None) {
        Ok(()) => true,
        Err(nix::errno::Errno::EPERM) => true,
        Err(_) => false,
    }
}

/// `TERM`, `SIGINT`, ... for the signals `watch_stop` may send to the watcher.
/// KILL is refused: the watcher cannot forward it and would orphan the job.
pub fn parse_signal(name: &str) -> Result<Signal, String> {
    let bare = name.trim().to_ascii_uppercase();
    match bare.strip_prefix("SIG").unwrap_or(&bare) {
        "TERM" => Ok(Signal::SIGTERM),
        "INT" => Ok(Signal::SIGINT),
        "HUP" => Ok(Signal::SIGHUP),
        "QUIT" => Ok(Signal::SIGQUIT),
        "KILL" => Err("never SIGKILL the watcher (it would orphan the job); \
                       watch_stop escalates to SIGKILL on the job's process group by itself"
            .into()),
        other => Err(format!("unsupported signal {other:?} (use TERM, INT, HUP or QUIT)")),
    }
}

/// The job's process group: from the events (every event carries `pgid`),
/// else the group of the watcher's child, found with `ps` (a job that has
/// produced no event yet).
pub fn job_pgid(meta: &Meta, events: &[Event]) -> Option<i32> {
    let from_events = events.iter().rev().find_map(|e| e.event["pgid"].as_i64());
    from_events.map(|p| p as i32).or_else(|| child_pgid(meta.watcher_pid))
}

fn child_pgid(watcher: u32) -> Option<i32> {
    let out = Command::new("ps")
        .args(["-A", "-o", "pid=,ppid=,pgid="])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).lines().find_map(|l| {
        let mut it = l.split_whitespace().map(|f| f.parse::<i64>().ok());
        let (_pid, ppid, pgid) = (it.next()??, it.next()??, it.next()??);
        (ppid == watcher as i64 && pgid > 1).then_some(pgid as i32)
    })
}

pub fn signal_watcher(meta: &Meta, sig: Signal) -> Result<(), String> {
    kill(Pid::from_raw(meta.watcher_pid as i32), sig)
        .map_err(|e| format!("signalling watcher {}: {e}", meta.watcher_pid))
}

/// SIGKILL the job's whole process group. Refuses pgid <= 1 and the
/// watcher's own group.
pub fn kill_job_group(pgid: i32) -> Result<(), String> {
    if pgid <= 1 || pgid == nix::unistd::getpgrp().as_raw() {
        return Err(format!("refusing to kill process group {pgid}"));
    }
    killpg(Pid::from_raw(pgid), Signal::SIGKILL).map_err(|e| format!("killing group {pgid}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(id: &str, started_ms: u64, pid: u32) -> Meta {
        Meta {
            id: id.into(),
            cmd: vec!["x".into()],
            cwd: "/".into(),
            started: rfc3339(started_ms as u128),
            started_ms,
            watcher_pid: pid,
            options: json!({}),
            events: PathBuf::new(),
            log: PathBuf::new(),
        }
    }

    fn put(runs: &Runs, m: &Meta) {
        fs::create_dir_all(runs.dir(&m.id)).unwrap();
        fs::write(runs.dir(&m.id).join(META), serde_json::to_string(m).unwrap()).unwrap();
    }

    #[test]
    fn ids_cannot_escape_the_runs_dir() {
        let d = tempfile::tempdir().unwrap();
        let runs = Runs::open(d.path()).unwrap();
        for bad in ["", "..", "../x", "a/b", "a b", "."] {
            assert!(runs.meta(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn prune_removes_old_ended_runs_only() {
        let d = tempfile::tempdir().unwrap();
        let runs = Runs::open(d.path()).unwrap();
        let week = MAX_AGE.as_millis() as u64;
        let dead = i32::MAX as u32; // no such pid
        put(&runs, &meta("old-dead", now_ms() - week - 1000, dead));
        put(&runs, &meta("old-alive", now_ms() - week - 1000, std::process::id()));
        put(&runs, &meta("new-dead", now_ms() - 1000, dead));
        assert_eq!(runs.prune(MAX_AGE), 1);
        let left: Vec<String> = runs.list().into_iter().map(|m| m.id).collect();
        assert_eq!(left, ["new-dead", "old-alive"]);
    }

    #[test]
    fn list_is_newest_first_and_survives_junk() {
        let d = tempfile::tempdir().unwrap();
        let runs = Runs::open(d.path()).unwrap();
        put(&runs, &meta("a", 1, 1));
        put(&runs, &meta("b", 3, 1));
        put(&runs, &meta("c", 2, 1));
        fs::create_dir_all(runs.dir("broken")).unwrap();
        fs::write(runs.dir("broken").join(META), "{").unwrap();
        fs::write(runs.dir("stray-file"), "").unwrap();
        let ids: Vec<String> = runs.list().into_iter().map(|m| m.id).collect();
        assert_eq!(ids, ["b", "c", "a"]);
    }

    #[test]
    fn cursor_round_trips_and_defaults_to_zero() {
        let d = tempfile::tempdir().unwrap();
        let runs = Runs::open(d.path()).unwrap();
        put(&runs, &meta("r", 1, 1));
        assert_eq!(runs.cursor("r"), 0);
        runs.set_cursor("r", 7);
        assert_eq!(runs.cursor("r"), 7);
    }

    #[test]
    fn signals_and_durations_are_validated() {
        assert_eq!(parse_signal("term").unwrap(), Signal::SIGTERM);
        assert_eq!(parse_signal("SIGINT").unwrap(), Signal::SIGINT);
        assert!(parse_signal("KILL").unwrap_err().contains("never SIGKILL"));
        assert!(parse_signal("USR1").is_err());
        assert_eq!(Dur::Secs(90.0).arg("t", parse_duration).unwrap(), "90s");
        assert_eq!(Dur::Text("5m".into()).arg("t", parse_duration).unwrap(), "5m");
        assert!(Dur::Text("soon".into()).arg("t", parse_duration).is_err());
        assert!(Dur::Secs(0.5).arg("heartbeat", parse_heartbeat).is_err());
    }

    #[test]
    fn our_own_pid_is_alive_and_a_bogus_one_is_not() {
        assert!(pid_alive(std::process::id()));
        assert!(!pid_alive(i32::MAX as u32));
    }

    #[test]
    fn kill_refuses_dangerous_groups() {
        assert!(kill_job_group(0).is_err());
        assert!(kill_job_group(1).is_err());
        assert!(kill_job_group(nix::unistd::getpgrp().as_raw()).is_err());
    }
}
