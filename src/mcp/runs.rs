//! The state directory behind `watcher-s1 mcp`: one directory per run under
//! `runs/<id>/` holding `events.jsonl`, `output.log`, `meta.json`, the
//! read state (`cursor`, `summary`, `cursor.lock`) and, while the watcher
//! lives, its control socket. Everything here is synchronous and knows
//! nothing about MCP (callers run it off the async runtime: see `mod.rs`).
//! The directory is the source of truth, so a run started by an earlier
//! server process is just as visible as one started by this one.
//!
//! Trust model: the state directory belongs to the user, but nothing inside
//! it is believed. The run id is the directory entry's name and the only
//! thing paths derive from (a `meta.json` that names another id, or other
//! paths, is rejected or ignored); state files are opened `O_NOFOLLOW` and
//! `O_NONBLOCK`, must be regular files and are size-bounded; whether a
//! watcher lives, and every signal to it, goes through its control socket
//! (`crate::control`), never through a pid or process group read from a file.

use crate::cli::{parse_duration, parse_heartbeat};
use crate::control::{self, ClientError, MAX_SOCKET_PATH};
use crate::event::rfc3339;
use crate::follow::{Event, EventReader, ReadPos};
use rmcp::schemars::{self, JsonSchema};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Runs older than this are removed by the pruner (at start, then hourly).
pub const MAX_AGE: Duration = Duration::from_secs(7 * 24 * 3600);
/// `meta.json`, `cursor` and `summary` are read up to this many bytes.
pub const MAX_STATE_FILE: u64 = 64 * 1024;
/// Events one `watch_wait` hands out at most.
pub const MAX_EVENTS: usize = 100;
/// Serialized bytes (JSON plus compact line) one `watch_wait` hands out at most.
pub const MAX_RESPONSE_BYTES: usize = 256 * 1024;
/// A string or object field of an event larger than this is clipped before it
/// is handed out or stored.
const MAX_FIELD: usize = 8 * 1024;
/// How long `start` waits for the watcher to come up.
const START_WAIT: Duration = Duration::from_secs(5);
/// A run whose meta still says `starting` is not `lost` for this long.
const STARTUP_GRACE_MS: u64 = 10_000;
/// Wall-clock bound for one control-socket request.
const PROBE_TIMEOUT: Duration = Duration::from_millis(1500);
/// A cursor transaction waits this long for the per-run lock.
const LOCK_WAIT: Duration = Duration::from_secs(10);
/// How long a status summary scans before it reports `partial`.
const SUMMARY_BUDGET: Duration = Duration::from_millis(250);

pub const EVENTS: &str = "events.jsonl";
pub const LOG: &str = "output.log";
const META: &str = "meta.json";
const CURSOR: &str = "cursor";
const SUMMARY: &str = "summary";
const LOCK: &str = "cursor.lock";
const SOCKET: &str = "control.sock";

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

fn default_status() -> String {
    "running".into()
}

/// `meta.json`. Informational except for the id check: paths are derived
/// from the run directory, never read from here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Meta {
    pub id: String,
    pub cmd: Vec<String>,
    pub cwd: PathBuf,
    /// RFC 3339 and unix milliseconds of the start.
    pub started: String,
    pub started_ms: u64,
    /// The watcher's pid, for information only; nothing signals it.
    pub watcher_pid: u32,
    /// The watcher-s1 flags the run was started with.
    pub options: Value,
    /// Where the run's files are (filled in from the run directory on load).
    #[serde(default)]
    pub events: PathBuf,
    #[serde(default)]
    pub log: PathBuf,
    /// `starting` (written before the watcher is launched), `running`, or
    /// `failed` (the watcher could not be started; see `error`).
    #[serde(default = "default_status")]
    pub status: String,
    #[serde(default)]
    pub error: Option<String>,
}

/// Where a run stands, from its events and its control socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Running,
    /// Just launched; the watcher is not answering yet.
    Starting,
    /// The final event was written.
    Finished,
    /// The watcher is gone and left no final event (killed outright).
    Lost,
    /// The watcher could not be started.
    Failed,
}

impl Phase {
    /// Is a watcher (probably) there to wait on?
    pub fn live(self) -> bool {
        matches!(self, Phase::Running | Phase::Starting)
    }
}

/// What a control-socket probe found.
#[derive(Debug, Clone)]
pub enum Liveness {
    /// Answered `status` (the reply), or is there but did not answer in time (`Null`).
    Alive(Value),
    /// Nothing is listening.
    Gone,
}

impl Liveness {
    pub fn alive(&self) -> bool {
        matches!(self, Liveness::Alive(_))
    }
}

/// Which events a cursor transaction hands out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Take {
    /// Everything unseen, heartbeats included.
    Next,
    /// Edge events up to and including the final one; heartbeats are counted.
    Final,
}

/// The result of one cursor transaction.
#[derive(Debug, Default)]
pub struct Taken {
    pub events: Vec<Event>,
    pub heartbeats_skipped: usize,
    /// Capped by [`MAX_EVENTS`] / [`MAX_RESPONSE_BYTES`] (or the read budget): call again.
    pub more: bool,
    /// There is something to answer with (as opposed to keep waiting).
    pub ready: bool,
    /// The final event was handed out by an earlier call and is repeated.
    pub already_seen: bool,
    /// The final event is among `events`.
    pub final_seen: bool,
}

/// An event kept in the summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Stored {
    pub seq: u64,
    pub event: Value,
    pub line: String,
}

/// A compact picture of a run's events, kept incrementally.
#[derive(Debug, Clone, Default)]
pub struct Summary {
    pub events: u64,
    pub last: Option<Stored>,
    pub final_ev: Option<Stored>,
    /// The scan hit its time budget; call again for the rest.
    pub partial: bool,
}

#[derive(Serialize, Deserialize, Default)]
struct CursorState {
    pos: ReadPos,
    final_given: bool,
}

#[derive(Serialize, Deserialize, Default)]
struct SummaryState {
    pos: ReadPos,
    last: Option<Stored>,
    final_ev: Option<Stored>,
}

pub struct Runs {
    root: PathBuf,
}

/// Ids are `<unix ms>-<n>`: never a path.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Read a state file: no symlink as the last component, never blocks (a
/// FIFO is opened nonblocking, then refused), regular files only, at most
/// `max` bytes.
pub fn read_state_file(path: &Path, max: u64) -> io::Result<Vec<u8>> {
    let f = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)?;
    let m = f.metadata()?;
    if !m.is_file() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a regular file"));
    }
    if m.len() > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("larger than {max} bytes"),
        ));
    }
    let mut buf = Vec::new();
    f.take(max + 1).read_to_end(&mut buf)?;
    if buf.len() as u64 > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("larger than {max} bytes"),
        ));
    }
    Ok(buf)
}

/// Replace `path` atomically (temp file in the same directory, then rename),
/// so a reader or a crash never sees half a file.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    static N: AtomicU64 = AtomicU64::new(0);
    let tmp = path.with_extension(format!(
        "tmp{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let res = (|| {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&tmp)?;
        f.write_all(bytes)?;
        fs::rename(&tmp, path)
    })();
    if res.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    res
}

fn load_json<T: DeserializeOwned + Default>(path: &Path) -> T {
    match read_state_file(path, MAX_STATE_FILE) {
        Ok(b) => serde_json::from_slice(&b).unwrap_or_default(),
        Err(_) => T::default(),
    }
}

fn store_json<T: Serialize>(path: &Path, v: &T) -> io::Result<()> {
    write_atomic(path, &serde_json::to_vec(v).map_err(io::Error::other)?)
}

/// Hold an exclusive `flock` on `f`, retrying without ever blocking forever.
fn lock_exclusive(f: &File, limit: Duration) -> io::Result<()> {
    let deadline = Instant::now() + limit;
    loop {
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        if !matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted) {
            return Err(e);
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "the run's cursor lock is busy"));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Clip what an event carries so one cannot make a reply or a state file
/// large: a top-level string or object field over `MAX_FIELD` (8 KiB) bytes is cut
/// (strings) or replaced by a marker (everything else).
pub fn shrink_event(v: &mut Value) {
    let Value::Object(m) = v else { return };
    for (_, f) in m.iter_mut() {
        let size = match f {
            Value::String(s) => s.len(),
            Value::Object(_) | Value::Array(_) => f.to_string().len(),
            _ => 0,
        };
        if size <= MAX_FIELD {
            continue;
        }
        *f = match f {
            Value::String(s) => {
                let cut = (0..=MAX_FIELD).rev().find(|&i| s.is_char_boundary(i)).unwrap_or(0);
                Value::String(format!("{}...[clipped, {size} bytes]", &s[..cut]))
            }
            _ => Value::String(format!("[omitted, {size} bytes]")),
        };
    }
}

fn clip_line(line: &mut String) {
    if line.len() > 1024 {
        let cut = (0..=1024).rev().find(|&i| line.is_char_boundary(i)).unwrap_or(0);
        line.truncate(cut);
        line.push_str("...");
    }
}

fn is_heartbeat(e: &Event) -> bool {
    e.event["reason"] == "heartbeat"
}

fn stored(e: &Event) -> Stored {
    let mut event = e.event.clone();
    shrink_event(&mut event);
    let mut line = e.line.clone();
    clip_line(&mut line);
    Stored {
        seq: e.seq,
        event,
        line,
    }
}

fn fnv(s: &str) -> u32 {
    s.bytes()
        .fold(0x811c9dc5u32, |h, b| (h ^ u32::from(b)).wrapping_mul(0x01000193))
}

/// Make `dir` (mode 0700) or accept an existing one only if it is a real
/// directory, ours, and closed to others.
fn private_dir(dir: &Path) -> io::Result<()> {
    match fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            let m = fs::symlink_metadata(dir)?;
            let me = unsafe { libc::geteuid() };
            if m.is_dir() && m.uid() == me && m.permissions().mode() & 0o077 == 0 {
                Ok(())
            } else {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("{} is not a private directory of ours", dir.display()),
                ))
            }
        }
        Err(e) => Err(e),
    }
}

impl Runs {
    /// `state_dir/runs`, created (0700) if absent.
    pub fn open(state_dir: &Path) -> io::Result<Self> {
        let state = std::path::absolute(state_dir)?;
        fs::create_dir_all(&state)?;
        let root = state.join("runs");
        match fs::DirBuilder::new().mode(0o700).create(&root) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
        Ok(Runs { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn dir(&self, id: &str) -> PathBuf {
        self.root.join(id)
    }

    /// The run directory of a valid id, which must be a real directory (not a symlink).
    fn checked_dir(&self, id: &str) -> Result<PathBuf, String> {
        if !valid_id(id) {
            return Err(format!("invalid run id {id:?}"));
        }
        let dir = self.dir(id);
        match fs::symlink_metadata(&dir) {
            Ok(m) if m.is_dir() => Ok(dir),
            Ok(_) => Err(format!("run {id:?} is not a directory")),
            Err(e) => Err(format!("unknown run {id:?} ({e})")),
        }
    }

    /// Where the run's control socket is: in the run directory when the path
    /// fits `sun_path`, else under a private directory in `$XDG_RUNTIME_DIR`
    /// (or `/tmp`) keyed by the run id and the state directory.
    pub fn control_path(&self, id: &str) -> PathBuf {
        let inside = self.dir(id).join(SOCKET);
        if inside.as_os_str().len() <= MAX_SOCKET_PATH {
            return inside;
        }
        let key = fnv(&self.root.to_string_lossy());
        short_dir().join(format!("{key:08x}-{id}.sock"))
    }

    /// Runs older than `max_age` whose watcher is gone are removed; returns
    /// how many. A run with a watcher that answers is kept however old.
    pub fn prune(&self, max_age: Duration) -> usize {
        let cutoff = now_ms().saturating_sub(max_age.as_millis() as u64);
        let mut n = 0;
        for id in self.ids() {
            let Ok(meta) = self.meta(&id) else { continue };
            if meta.started_ms < cutoff && !self.probe(&id).alive() && fs::remove_dir_all(self.dir(&id)).is_ok() {
                let _ = fs::remove_file(self.control_path(&id)); // the short-path socket, if any
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

    /// `meta.json` of run `id`. The directory-entry id is authoritative: a
    /// file naming another id is refused, and the events/log paths come from
    /// the directory.
    pub fn meta(&self, id: &str) -> Result<Meta, String> {
        let dir = self.checked_dir(id)?;
        let raw = read_state_file(&dir.join(META), MAX_STATE_FILE).map_err(|e| format!("unknown run {id:?} ({e})"))?;
        let mut meta: Meta =
            serde_json::from_slice(&raw).map_err(|e| format!("run {id:?} has a damaged meta.json ({e})"))?;
        if meta.id != id {
            return Err(format!(
                "run {id:?} has a meta.json that names run {:?}; refusing it",
                meta.id
            ));
        }
        meta.events = dir.join(EVENTS);
        meta.log = dir.join(LOG);
        Ok(meta)
    }

    fn write_meta(&self, meta: &Meta) -> io::Result<()> {
        let text = serde_json::to_vec_pretty(meta).map_err(io::Error::other)?;
        write_atomic(&self.dir(&meta.id).join(META), &text)
    }

    /// Runs, newest first.
    pub fn list(&self) -> Vec<Meta> {
        let mut all: Vec<Meta> = self.ids().iter().filter_map(|id| self.meta(id).ok()).collect();
        all.sort_by(|a, b| b.started_ms.cmp(&a.started_ms).then_with(|| b.id.cmp(&a.id)));
        all
    }

    /// Ask the run's watcher for its status over the control socket.
    pub fn probe(&self, id: &str) -> Liveness {
        match control::status(&self.control_path(id), PROBE_TIMEOUT) {
            Ok(v) => Liveness::Alive(v),
            Err(ClientError::Unreachable(_)) => Liveness::Gone,
            // Something accepted the connection but did not answer sensibly:
            // a process is there, so it is not gone.
            Err(_) => Liveness::Alive(Value::Null),
        }
    }

    /// Tell the run's watcher to stop with `grace`.
    pub fn stop(&self, id: &str, grace: Duration) -> Result<(), String> {
        control::stop(&self.control_path(id), grace, PROBE_TIMEOUT).map_err(|e| e.to_string())
    }

    pub fn phase(&self, meta: &Meta, final_seen: bool, live: &Liveness) -> Phase {
        if final_seen {
            Phase::Finished
        } else if live.alive() {
            Phase::Running
        } else if meta.status == "failed" {
            Phase::Failed
        } else if meta.status == "starting" && now_ms().saturating_sub(meta.started_ms) < STARTUP_GRACE_MS {
            Phase::Starting
        } else {
            Phase::Lost
        }
    }

    /// Start `watcher-s1 --pipe --events ... --control ... -- cmd` detached:
    /// its own session, stdin from /dev/null, output to `output.log`.
    ///
    /// The run is recorded (status `starting`) before anything is launched,
    /// and updated after; whatever happens, a launched watcher is reachable
    /// through its id. Waits (up to a few seconds) until the watcher answers
    /// on its control socket, has written an event, or has died.
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
        let control = self.control_path(&id);
        let abandon = |why: String| {
            let _ = fs::remove_dir_all(&dir);
            why
        };
        if control.parent() != Some(dir.as_path())
            && let Some(parent) = control.parent()
        {
            private_dir(parent).map_err(|e| abandon(format!("control socket directory: {e}")))?;
        }
        if control.as_os_str().len() > MAX_SOCKET_PATH {
            return Err(abandon(
                "the state directory path is too long for a control socket".into(),
            ));
        }
        let started_ms = now_ms();
        let mut meta = Meta {
            id,
            cmd: opts.cmd.clone(),
            cwd: cwd.clone(),
            started: rfc3339(started_ms as u128),
            started_ms,
            watcher_pid: 0,
            options: Value::Object(options),
            events: dir.join(EVENTS),
            log: dir.join(LOG),
            status: "starting".into(),
            error: None,
        };
        // Recorded before launch: if this fails, nothing has been started.
        self.write_meta(&meta).map_err(|e| abandon(format!("meta.json: {e}")))?;

        let fail = |meta: &mut Meta, runs: &Runs, why: String| {
            meta.status = "failed".into();
            meta.error = Some(why.clone());
            let _ = runs.write_meta(meta);
            format!("{why} (run {} is marked failed)", meta.id)
        };
        let log = match File::create(&meta.log) {
            Ok(l) => l,
            Err(e) => {
                let why = format!("{}: {e}", meta.log.display());
                return Err(fail(&mut meta, self, why));
            }
        };
        let log_err = log.try_clone().map_err(|e| fail(&mut meta, self, e.to_string()))?;
        let exe = std::env::current_exe().map_err(|e| fail(&mut meta, self, format!("current_exe: {e}")))?;
        let mut cmd = Command::new(exe);
        cmd.args(["--pipe", "--quiet", "--events"])
            .arg(&meta.events)
            .arg("--control")
            .arg(&control)
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
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                let why = format!("cannot start watcher-s1: {e}");
                return Err(fail(&mut meta, self, why));
            }
        };
        meta.watcher_pid = child.id();
        meta.status = "running".into();
        let exited = Arc::new(AtomicBool::new(false));
        let flag = exited.clone();
        // Reap it when it ends, so it never lingers as a zombie.
        std::thread::spawn(move || {
            let _ = child.wait();
            flag.store(true, Ordering::SeqCst);
        });
        if let Err(e) = self.write_meta(&meta) {
            // The watcher runs but the record could not be completed: end it
            // rather than leave something we cannot describe (it stays
            // reachable by id either way).
            let _ = self.stop(&meta.id, Duration::ZERO);
            return Err(format!(
                "meta.json: {e}; the run {} was started and told to stop",
                meta.id
            ));
        }
        self.await_ready(&meta, &exited)
            .map_err(|why| fail(&mut meta, self, why))?;
        Ok(meta)
    }

    /// Until the watcher answers, has written an event, or has exited.
    fn await_ready(&self, meta: &Meta, exited: &AtomicBool) -> Result<(), String> {
        let deadline = Instant::now() + START_WAIT;
        let mut checked_after_exit = false;
        loop {
            if self.probe(&meta.id).alive() || fs::symlink_metadata(&meta.events).is_ok_and(|m| m.len() > 0) {
                return Ok(());
            }
            if exited.load(Ordering::SeqCst) {
                // One more look: it may have finished between the checks.
                if checked_after_exit {
                    let log = read_state_file(&meta.log, MAX_STATE_FILE).unwrap_or_default();
                    let text = String::from_utf8_lossy(&log);
                    let tail: String = text
                        .chars()
                        .rev()
                        .take(500)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect();
                    return Err(format!("the watcher exited before it started: {}", tail.trim()));
                }
                checked_after_exit = true;
                continue;
            }
            if Instant::now() >= deadline {
                return Ok(()); // slow, not dead: later calls see what it becomes
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Create `runs/<unix ms>-<n>`; `create_dir` is the uniqueness check.
    fn fresh_dir(&self) -> io::Result<(String, PathBuf)> {
        let ms = now_ms();
        for n in 0..10_000u32 {
            let id = format!("{ms}-{n}");
            let dir = self.dir(&id);
            match fs::DirBuilder::new().mode(0o700).create(&dir) {
                Ok(()) => return Ok((id, dir)),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::other("no free run id"))
    }

    /// Run `f` under the run's exclusive lock (`flock` on `cursor.lock`):
    /// serialized across tasks and across server processes.
    fn locked<T>(&self, id: &str, f: impl FnOnce(&Path) -> Result<T, String>) -> Result<T, String> {
        let dir = self.checked_dir(id)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(dir.join(LOCK))
            .map_err(|e| format!("{LOCK}: {e}"))?;
        if !lock.metadata().map_err(|e| e.to_string())?.is_file() {
            return Err(format!("{LOCK} is not a regular file"));
        }
        lock_exclusive(&lock, LOCK_WAIT).map_err(|e| format!("{LOCK}: {e}"))?;
        f(&dir) // the lock is released when `lock` drops
    }

    /// One cursor transaction: read what the caller has not been given, take
    /// it (within the caps) and move the cursor past it, atomically with
    /// respect to every other caller of this run. Errors, including a
    /// cursor that cannot be saved, are returned and nothing is handed out.
    pub fn take(&self, meta: &Meta, mode: Take) -> Result<Taken, String> {
        self.locked(&meta.id, |dir| {
            let mut cur: CursorState = load_json(&dir.join(CURSOR));
            if cur.final_given {
                // The verdict is repeated, flagged, instead of leaving the caller hanging.
                let fin = self.summary_inner(meta, dir)?.final_ev;
                return Ok(Taken {
                    events: fin.map(restore).into_iter().collect(),
                    ready: true,
                    already_seen: true,
                    final_seen: true,
                    ..Taken::default()
                });
            }
            let mut reader = EventReader::resume(&meta.events, cur.pos.clone());
            let polled = reader.poll().map_err(|e| format!("{EVENTS}: {e}"))?;
            let mut t = Taken {
                more: polled.more,
                ..Taken::default()
            };
            let mut bytes = 0usize;
            // The position to save: the end of the last event consumed.
            let mut consumed: Option<(u64, u64)> = None;
            // Final mode: consumed through the heartbeat-only prefix, so a
            // long history of heartbeats is not reread by every call.
            let mut prefix: Option<(u64, u64)> = None;
            let mut prefix_open = true;
            let (mut skipped_total, mut skipped_prefix, mut skipped_consumed) = (0usize, 0usize, 0usize);
            let mut capped = false;
            for mut e in polled.events {
                if mode == Take::Final && is_heartbeat(&e) {
                    skipped_total += 1;
                    if prefix_open {
                        prefix = Some((e.end, e.seq));
                        skipped_prefix = skipped_total;
                    }
                    continue;
                }
                prefix_open = false;
                shrink_event(&mut e.event);
                clip_line(&mut e.line);
                let size = e.event.to_string().len() + e.line.len();
                if !t.events.is_empty() && (t.events.len() >= MAX_EVENTS || bytes + size > MAX_RESPONSE_BYTES) {
                    capped = true;
                    break;
                }
                bytes += size;
                consumed = Some((e.end, e.seq));
                skipped_consumed = skipped_total;
                let fin = e.is_final;
                t.events.push(e);
                if fin {
                    t.final_seen = true;
                    break;
                }
            }
            t.more |= capped;
            let advance;
            match mode {
                Take::Next => {
                    t.ready = !t.events.is_empty();
                    advance = consumed;
                }
                Take::Final if t.final_seen => {
                    t.ready = true;
                    t.heartbeats_skipped = skipped_total;
                    advance = consumed;
                }
                Take::Final if capped && !t.events.is_empty() => {
                    t.ready = true;
                    t.heartbeats_skipped = skipped_consumed;
                    advance = consumed;
                }
                Take::Final => {
                    // Not there yet: keep the edge events for the call that
                    // reaches the verdict; only the heartbeat prefix is spent.
                    t.events.clear();
                    t.heartbeats_skipped = skipped_prefix;
                    advance = prefix;
                }
            }
            let at = reader.position();
            // Nothing handed out and no edge event passed over: everything the
            // reader got through (junk, other runs' lines, a discarded
            // oversized line) is spent, so the next call starts after it.
            let spent_all = t.events.is_empty() && prefix_open && !t.final_seen;
            let new = match (advance, at.clone()) {
                _ if spent_all => at,
                (Some((end, seq)), Some(at)) => Some(ReadPos {
                    offset: end,
                    seq,
                    skip: false,
                    ..at
                }),
                _ => None,
            };
            if let Some(new) = new
                && new != cur.pos
            {
                cur.pos = new;
                cur.final_given |= t.final_seen;
                store_json(&dir.join(CURSOR), &cur).map_err(|e| format!("cannot save the cursor: {e}"))?;
            }
            Ok(t)
        })
    }

    /// The incremental summary of the run's events (last event, count,
    /// final): reads only what is new since the previous call.
    pub fn summary(&self, meta: &Meta) -> Result<Summary, String> {
        self.locked(&meta.id, |dir| self.summary_inner(meta, dir))
    }

    fn summary_inner(&self, meta: &Meta, dir: &Path) -> Result<Summary, String> {
        let path = dir.join(SUMMARY);
        let mut st: SummaryState = load_json(&path);
        let mut partial = false;
        if st.final_ev.is_none() {
            let mut reader = EventReader::resume(&meta.events, st.pos.clone());
            let began = Instant::now();
            loop {
                let p = reader.poll().map_err(|e| format!("{EVENTS}: {e}"))?;
                for e in &p.events {
                    st.last = Some(stored(e));
                    if e.is_final {
                        st.final_ev = Some(stored(e));
                    }
                }
                if reader.is_done() || !p.more {
                    break;
                }
                if began.elapsed() >= SUMMARY_BUDGET {
                    partial = true;
                    break;
                }
            }
            if let Some(pos) = reader.position()
                && pos != st.pos
            {
                st.pos = pos;
                store_json(&path, &st).map_err(|e| format!("cannot save the summary: {e}"))?;
            }
        }
        Ok(Summary {
            events: st.pos.seq,
            last: st.last,
            final_ev: st.final_ev,
            partial,
        })
    }

    /// When the run stopped producing events: the events file's mtime.
    pub fn last_write_ms(&self, meta: &Meta) -> Option<u64> {
        let m = fs::symlink_metadata(&meta.events).ok()?;
        if !m.is_file() {
            return None;
        }
        Some(m.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_millis() as u64)
    }
}

/// An event rebuilt from its stored copy (for repeating the verdict).
fn restore(s: Stored) -> Event {
    Event {
        seq: s.seq,
        is_final: true,
        event: s.event,
        line: s.line,
        end: 0,
    }
}

/// `$XDG_RUNTIME_DIR/watcher-s1` or `/tmp/watcher-s1-<uid>`: where control
/// sockets go when the run directory's path is too long for `sun_path`.
fn short_dir() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from) {
        Some(d) if d.is_absolute() => d.join("watcher-s1"),
        _ => PathBuf::from(format!("/tmp/watcher-s1-{}", unsafe { libc::geteuid() })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{ControlServer, StatusReply};

    fn meta(id: &str, started_ms: u64) -> Meta {
        Meta {
            id: id.into(),
            cmd: vec!["x".into()],
            cwd: "/".into(),
            started: rfc3339(started_ms as u128),
            started_ms,
            watcher_pid: 1,
            options: json!({}),
            events: PathBuf::new(),
            log: PathBuf::new(),
            status: "running".into(),
            error: None,
        }
    }

    fn put(runs: &Runs, m: &Meta) {
        fs::create_dir_all(runs.dir(&m.id)).unwrap();
        fs::write(runs.dir(&m.id).join(META), serde_json::to_string(m).unwrap()).unwrap();
    }

    /// A control socket for run `id` that answers `status` until dropped.
    struct Fake {
        stop: Arc<AtomicBool>,
        t: Option<std::thread::JoinHandle<()>>,
    }

    impl Fake {
        fn new(runs: &Runs, id: &str) -> Fake {
            let mut s = ControlServer::bind(&runs.control_path(id)).unwrap();
            let stop = Arc::new(AtomicBool::new(false));
            let flag = stop.clone();
            let t = std::thread::spawn(move || {
                while !flag.load(Ordering::SeqCst) {
                    s.service(&|| StatusReply {
                        pid: Some(5),
                        pgid: Some(5),
                        state: "progressing",
                        elapsed_ms: 1,
                    });
                    std::thread::sleep(Duration::from_millis(5));
                }
            });
            Fake { stop, t: Some(t) }
        }
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            self.t.take().unwrap().join().unwrap();
        }
    }

    fn ev(run: &str, state: &str, reason: &str, exit: Option<i32>) -> String {
        json!({
            "run_id": run, "state": state, "reason": reason, "severity": "info",
            "exit": exit.map(|c| json!({"code": c, "signal": null})), "evidence_tail": "t\n",
        })
        .to_string()
            + "\n"
    }

    fn append(runs: &Runs, id: &str, text: &str) {
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(runs.dir(id).join(EVENTS))
            .unwrap();
        f.write_all(text.as_bytes()).unwrap();
    }

    fn runs_with(id: &str) -> (tempfile::TempDir, Runs, Meta) {
        let d = tempfile::tempdir().unwrap();
        let runs = Runs::open(d.path()).unwrap();
        put(&runs, &meta(id, 1));
        let m = runs.meta(id).unwrap();
        (d, runs, m)
    }

    #[test]
    fn ids_cannot_escape_the_runs_dir() {
        let d = tempfile::tempdir().unwrap();
        let runs = Runs::open(d.path()).unwrap();
        for bad in ["", "..", "../x", "a/b", "a b", ".", "/abs", "/etc/passwd"] {
            assert!(runs.meta(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_meta_naming_another_id_is_refused_and_its_paths_ignored() {
        let d = tempfile::tempdir().unwrap();
        let runs = Runs::open(d.path()).unwrap();
        for forged in ["../outside", "/tmp/abs", "other"] {
            let mut m = meta("stale", 1);
            m.id = forged.into();
            put(
                &runs,
                &Meta {
                    id: "stale".into(),
                    ..m.clone()
                },
            );
            fs::write(runs.dir("stale").join(META), serde_json::to_string(&m).unwrap()).unwrap();
            let e = runs.meta("stale").unwrap_err();
            assert!(e.contains("refusing"), "{forged}: {e}");
        }
        // Paths in the file are not believed either.
        let mut m = meta("good", 1);
        m.events = "/etc/passwd".into();
        m.log = "/etc/shadow".into();
        put(&runs, &m);
        let got = runs.meta("good").unwrap();
        assert_eq!(got.events, runs.dir("good").join(EVENTS));
        assert_eq!(got.log, runs.dir("good").join(LOG));
    }

    #[test]
    fn symlinked_run_dirs_and_state_files_are_refused() {
        let d = tempfile::tempdir().unwrap();
        let runs = Runs::open(d.path()).unwrap();
        let outside = d.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join(META), serde_json::to_string(&meta("link", 1)).unwrap()).unwrap();
        std::os::unix::fs::symlink(&outside, runs.dir("link")).unwrap();
        assert!(runs.meta("link").unwrap_err().contains("not a directory"));
        assert!(runs.list().is_empty());
        // A symlinked meta.json inside a real run directory.
        fs::create_dir_all(runs.dir("real")).unwrap();
        std::os::unix::fs::symlink(outside.join(META), runs.dir("real").join(META)).unwrap();
        assert!(runs.meta("real").is_err());
        // A symlinked cursor is not followed (nor written through).
        let (_d2, runs2, m) = runs_with("r");
        let target = _d2.path().join("target");
        fs::write(&target, "DO NOT OVERWRITE").unwrap();
        std::os::unix::fs::symlink(&target, runs2.dir("r").join(CURSOR)).unwrap();
        append(&runs2, "r", &ev("r", "progressing", "resumed", None));
        runs2.take(&m, Take::Next).unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "DO NOT OVERWRITE");
    }

    #[test]
    fn a_fifo_meta_does_not_block_and_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let runs = Runs::open(d.path()).unwrap();
        fs::create_dir_all(runs.dir("fifo")).unwrap();
        nix::unistd::mkfifo(
            &runs.dir("fifo").join(META),
            nix::sys::stat::Mode::from_bits_truncate(0o600),
        )
        .unwrap();
        let t0 = Instant::now();
        assert!(runs.meta("fifo").is_err());
        assert!(runs.list().is_empty());
        assert_eq!(runs.prune(Duration::ZERO), 0);
        assert!(t0.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn oversized_state_files_are_refused() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("big");
        fs::write(&p, vec![b'x'; 100]).unwrap();
        assert!(read_state_file(&p, 99).is_err());
        assert_eq!(read_state_file(&p, 100).unwrap().len(), 100);
    }

    #[test]
    fn prune_removes_old_unanswered_runs_only() {
        let d = tempfile::tempdir().unwrap();
        let runs = Runs::open(d.path()).unwrap();
        let week = MAX_AGE.as_millis() as u64;
        put(&runs, &meta("old-dead", now_ms() - week - 1000));
        put(&runs, &meta("old-alive", now_ms() - week - 1000));
        put(&runs, &meta("new-dead", now_ms() - 1000));
        let _alive = Fake::new(&runs, "old-alive");
        assert_eq!(runs.prune(MAX_AGE), 1);
        let left: Vec<String> = runs.list().into_iter().map(|m| m.id).collect();
        assert_eq!(left, ["new-dead", "old-alive"]);
    }

    #[test]
    fn list_is_newest_first_and_survives_junk() {
        let d = tempfile::tempdir().unwrap();
        let runs = Runs::open(d.path()).unwrap();
        put(&runs, &meta("a", 1));
        put(&runs, &meta("b", 3));
        put(&runs, &meta("c", 2));
        fs::create_dir_all(runs.dir("broken")).unwrap();
        fs::write(runs.dir("broken").join(META), "{").unwrap();
        fs::write(runs.dir("stray-file"), "").unwrap();
        let ids: Vec<String> = runs.list().into_iter().map(|m| m.id).collect();
        assert_eq!(ids, ["b", "c", "a"]);
    }

    #[test]
    fn long_state_dirs_get_a_short_control_path_that_fits_sun_path() {
        let d = tempfile::tempdir().unwrap();
        let deep = d.path().join("x".repeat(90));
        let runs = Runs::open(&deep).unwrap();
        let p = runs.control_path("1-0");
        assert!(p.as_os_str().len() <= MAX_SOCKET_PATH, "{}", p.display());
        assert!(!p.starts_with(runs.dir("1-0")));
        let short = Runs::open(&d.path().join("s")).unwrap().control_path("1-0");
        assert!(short.ends_with("runs/1-0/control.sock"), "{}", short.display());
    }

    #[test]
    fn take_next_hands_out_each_event_once_and_persists_the_position() {
        let (_d, runs, m) = runs_with("r");
        append(
            &runs,
            "r",
            &(ev("r", "progressing", "heartbeat", None) + &ev("r", "stalled", "silence", None)),
        );
        let a = runs.take(&m, Take::Next).unwrap();
        assert_eq!((a.events.len(), a.ready, a.more), (2, true, false));
        assert_eq!(a.events[1].seq, 2);
        let b = runs.take(&m, Take::Next).unwrap();
        assert!(!b.ready && b.events.is_empty(), "nothing is delivered twice");
        append(&runs, "r", &ev("r", "done", "exit", Some(0)));
        let c = runs.take(&m, Take::Next).unwrap();
        assert_eq!((c.events.len(), c.events[0].seq, c.final_seen), (1, 3, true));
        // The verdict is repeated, flagged.
        let d = runs.take(&m, Take::Next).unwrap();
        assert_eq!((d.already_seen, d.ready, d.events[0].is_final), (true, true, true));
        let e = runs.take(&m, Take::Final).unwrap();
        assert!(e.already_seen);
    }

    #[test]
    fn take_final_skips_heartbeats_and_waits_for_the_verdict() {
        let (_d, runs, m) = runs_with("r");
        append(&runs, "r", &ev("r", "progressing", "heartbeat", None).repeat(3));
        append(&runs, "r", &ev("r", "stalled", "silence", None));
        let a = runs.take(&m, Take::Final).unwrap();
        assert!(!a.ready && a.events.is_empty(), "no final yet");
        append(&runs, "r", &ev("r", "progressing", "heartbeat", None));
        append(&runs, "r", &ev("r", "done", "exit", Some(0)));
        let b = runs.take(&m, Take::Final).unwrap();
        assert!(b.ready && b.final_seen);
        let reasons: Vec<&str> = b.events.iter().map(|e| e.event["reason"].as_str().unwrap()).collect();
        assert_eq!(reasons, ["silence", "exit"]);
        assert_eq!(
            b.heartbeats_skipped,
            1 + 3 - 3,
            "the first 3 were spent by the earlier call"
        );
    }

    #[test]
    fn responses_are_capped_and_the_rest_comes_next_call() {
        let (_d, runs, m) = runs_with("r");
        append(&runs, "r", &ev("r", "stalled", "silence", None).repeat(MAX_EVENTS + 20));
        let a = runs.take(&m, Take::Next).unwrap();
        assert_eq!((a.events.len(), a.more), (MAX_EVENTS, true));
        let b = runs.take(&m, Take::Next).unwrap();
        assert_eq!((b.events.len(), b.events[0].seq), (20, MAX_EVENTS as u64 + 1));
        assert!(!b.more);
        // Bytes cap too: a few big events.
        let (_d, runs, m) = runs_with("big");
        let big = json!({"run_id": "big", "state": "stalled", "reason": "silence", "exit": null,
            "a": "x".repeat(8000), "b": "y".repeat(8000), "c": "z".repeat(8000), "d": "w".repeat(8000)})
        .to_string()
            + "\n";
        append(&runs, "big", &big.repeat(40));
        let a = runs.take(&m, Take::Next).unwrap();
        let bytes: usize = a.events.iter().map(|e| e.event.to_string().len()).sum();
        assert!(a.more && bytes <= MAX_RESPONSE_BYTES, "{bytes}");
        assert!(a.events.len() < 40);
    }

    #[test]
    fn huge_fields_are_clipped() {
        let mut v = json!({"s": "é".repeat(MAX_FIELD), "o": {"k": "v".repeat(MAX_FIELD + 1)}, "n": 5, "small": "ok"});
        shrink_event(&mut v);
        assert!(v["s"].as_str().unwrap().len() < MAX_FIELD + 64);
        assert!(v["o"].as_str().unwrap().starts_with("[omitted"));
        assert_eq!((v["n"].as_i64(), v["small"].as_str()), (Some(5), Some("ok")));
    }

    #[test]
    fn concurrent_takes_get_distinct_events_across_threads() {
        let (_d, runs, m) = runs_with("r");
        let runs = Arc::new(runs);
        append(
            &runs,
            "r",
            &(1..=60)
                .map(|i| ev("r", "stalled", &format!("silence{i}"), None))
                .collect::<String>(),
        );
        let mut hs = Vec::new();
        for _ in 0..6 {
            let (runs, m) = (runs.clone(), m.clone());
            hs.push(std::thread::spawn(move || {
                let mut mine = Vec::new();
                loop {
                    let t = runs.take(&m, Take::Next).unwrap();
                    if !t.ready {
                        return mine;
                    }
                    mine.extend(t.events.iter().map(|e| e.seq));
                }
            }));
        }
        let mut all: Vec<u64> = hs.into_iter().flat_map(|h| h.join().unwrap()).collect();
        all.sort();
        assert_eq!(all, (1..=60).collect::<Vec<u64>>(), "each event exactly once");
    }

    #[test]
    fn an_unsavable_cursor_is_an_error_and_hands_nothing_out() {
        let (_d, runs, m) = runs_with("r");
        append(&runs, "r", &ev("r", "stalled", "silence", None));
        // A directory where the cursor goes: the rename cannot succeed.
        fs::create_dir(runs.dir("r").join(CURSOR)).unwrap();
        let e = runs.take(&m, Take::Next).unwrap_err();
        assert!(e.contains("cursor"), "{e}");
    }

    #[test]
    fn the_summary_is_incremental_and_bounded_in_size() {
        let (_d, runs, m) = runs_with("r");
        append(&runs, "r", &ev("r", "progressing", "heartbeat", None).repeat(500));
        let s = runs.summary(&m).unwrap();
        assert_eq!((s.events, s.last.as_ref().unwrap().seq), (500, 500));
        let before = fs::read(runs.dir("r").join(SUMMARY)).unwrap().len();
        append(&runs, "r", &ev("r", "progressing", "heartbeat", None).repeat(500));
        let s = runs.summary(&m).unwrap();
        assert_eq!(s.events, 1000);
        let after = fs::read(runs.dir("r").join(SUMMARY)).unwrap().len();
        assert!(
            after < before + 64,
            "the summary file does not grow with history ({before} -> {after})"
        );
        append(&runs, "r", &ev("r", "done", "exit", Some(2)));
        let s = runs.summary(&m).unwrap();
        assert_eq!(s.final_ev.unwrap().event["exit"]["code"], 2);
    }

    #[test]
    fn the_summary_converges_on_a_history_larger_than_one_budget() {
        let (_d, runs, m) = runs_with("r");
        // Far more than one 250 ms scan reads in a debug build.
        let n = 400_000;
        let line = ev("r", "progressing", "heartbeat", None);
        append(&runs, "r", &line.repeat(n));
        let (mut last, mut calls) = (0, 0);
        loop {
            let s = runs.summary(&m).unwrap();
            calls += 1;
            assert!(s.events > last || !s.partial, "call {calls} made no progress at {last}");
            last = s.events;
            if !s.partial {
                assert_eq!(s.events, n as u64);
                assert_eq!(s.last.unwrap().seq, n as u64);
                break;
            }
            assert!(calls < 10_000, "no convergence");
        }
        // Settled: the next call is complete at once and the offset sticks.
        let again = runs.summary(&m).unwrap();
        assert!(!again.partial && again.events == n as u64);
    }

    #[test]
    fn a_replaced_events_file_voids_the_saved_position() {
        let (_d, runs, m) = runs_with("r");
        append(&runs, "r", &ev("r", "stalled", "silence", None).repeat(3));
        assert_eq!(runs.take(&m, Take::Next).unwrap().events.len(), 3);
        fs::remove_file(runs.dir("r").join(EVENTS)).unwrap();
        append(&runs, "r", &ev("r2", "stalled", "silence", None).repeat(2));
        assert_eq!(runs.take(&m, Take::Next).unwrap().events.len(), 2);
    }

    #[test]
    fn probe_reports_gone_alive_and_unresponsive() {
        let (_d, runs, _m) = runs_with("r");
        assert!(!runs.probe("r").alive());
        {
            let _f = Fake::new(&runs, "r");
            let Liveness::Alive(v) = runs.probe("r") else { panic!() };
            assert_eq!(v["pgid"], 5);
        }
        assert!(!runs.probe("r").alive(), "the socket goes with its owner");
        // Bound but never serviced: the connection is accepted by the kernel, no answer.
        let _idle = ControlServer::bind(&runs.control_path("r")).unwrap();
        let t0 = Instant::now();
        assert!(runs.probe("r").alive());
        assert!(t0.elapsed() < Duration::from_secs(4));
    }

    #[test]
    fn phases() {
        let (_d, runs, mut m) = runs_with("r");
        let gone = Liveness::Gone;
        let up = Liveness::Alive(Value::Null);
        assert_eq!(runs.phase(&m, true, &gone), Phase::Finished);
        assert_eq!(runs.phase(&m, false, &up), Phase::Running);
        assert_eq!(runs.phase(&m, false, &gone), Phase::Lost);
        m.status = "starting".into();
        m.started_ms = now_ms();
        assert_eq!(runs.phase(&m, false, &gone), Phase::Starting);
        m.started_ms = 1;
        assert_eq!(runs.phase(&m, false, &gone), Phase::Lost);
        m.status = "failed".into();
        assert_eq!(runs.phase(&m, false, &gone), Phase::Failed);
    }

    #[test]
    fn durations_are_validated() {
        assert_eq!(Dur::Secs(90.0).arg("t", parse_duration).unwrap(), "90s");
        assert_eq!(Dur::Text("5m".into()).arg("t", parse_duration).unwrap(), "5m");
        assert!(Dur::Text("soon".into()).arg("t", parse_duration).is_err());
        assert!(Dur::Secs(0.5).arg("heartbeat", parse_heartbeat).is_err());
    }
}
