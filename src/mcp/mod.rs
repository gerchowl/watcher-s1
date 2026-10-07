//! `watcher-s1 mcp`: the supervisor as an MCP server over stdio.
//!
//! The only async code in the crate lives here (a current-thread tokio
//! runtime, built in [`serve`]); the wrapper itself stays synchronous. Runs
//! are detached `watcher-s1 --pipe --events ... --control ...` processes
//! whose state lives in the state directory ([`runs`]), so they outlive this
//! server and any later server can wait on them. stdout is protocol only.
//!
//! Nothing here may block the runtime thread: every filesystem or socket
//! operation runs on the blocking pool (`blocking`, with a wall-clock
//! bound), so a ping is answered while a `watch_stop` or `watch_wait` is in
//! progress. Whether a watcher lives, and stopping it, go through its
//! control socket only.

pub mod runs;

use crate::GUIDE;
use crate::event::rfc3339;
use crate::follow::{Event, EventReader, ReadPos};
use rmcp::{
    ErrorData as McpError, Peer, RoleServer, ServerHandler, ServiceExt,
    handler::server::wrapper::Parameters,
    model::{
        CallToolResult, ContentBlock, CustomNotification, Implementation, ListResourcesResult, PaginatedRequestParams,
        ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult, Resource, ResourceContents,
        ServerCapabilities, ServerConfig, ServerNotification,
    },
    schemars::{self, JsonSchema},
    service::RequestContext,
    tool, tool_handler, tool_router,
};
use runs::{Dur, Liveness, Meta, Phase, Runs, StartOpts, Stored, Take};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::sleep;

pub const GUIDE_URI: &str = "watcher-s1://guide";
/// The notification Claude Code listens for on a channel server.
pub const CHANNEL_METHOD: &str = "notifications/claude/channel";
/// The capability key that makes a server a channel.
pub const CHANNEL_CAPABILITY: &str = "claude/channel";

/// Poll interval while waiting on a file (the run's events land by append).
const POLL: Duration = Duration::from_millis(100);
/// Seconds `watch_wait` waits when the caller names no timeout.
const DEFAULT_WAIT_S: f64 = 60.0;
/// Upper bound for one `watch_wait`; wait again for longer.
const MAX_WAIT_S: f64 = 3600.0;
/// Grace between TERM to the job's group and SIGKILL.
const DEFAULT_GRACE_S: f64 = 5.0;
/// How long to wait for the final event after the grace is over.
const KILL_SETTLE: Duration = Duration::from_secs(10);
/// Wall-clock bound for one blocking operation on the state directory.
const IO_BOUND: Duration = Duration::from_secs(15);
/// How often completed runs are pruned while the server lives.
const PRUNE_EVERY: Duration = Duration::from_secs(3600);
/// Test hook: seconds between prunes, overriding [`PRUNE_EVERY`].
const PRUNE_ENV: &str = "WATCHER_S1_MCP_PRUNE_SECS";

#[derive(Debug, Deserialize, JsonSchema)]
pub struct StartParams {
    /// The command as an argv array, e.g. ["nix", "build", ".#foo"]. Run directly, not through a shell:
    /// use ["sh", "-c", "..."] for shell syntax.
    pub cmd: Vec<String>,
    /// Working directory (default: the server's).
    pub cwd: Option<PathBuf>,
    /// Quiet time before `stalled`: seconds or "5m" (default 300s; 0 disables).
    pub silence: Option<Dur>,
    /// Hard limit: seconds or "2h". The job's process group gets TERM, then KILL. Without it nothing bounds the
    /// run or its output.log (which grows as the job writes).
    pub timeout: Option<Dur>,
    /// Emit a `heartbeat` event this often (at least 1s). Heartbeats count as events for `until: "next"`; `final` skips them.
    pub heartbeat: Option<Dur>,
    /// Allow the System One tier (default true; it only runs when an endpoint is configured). false skips it.
    pub s1: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Until {
    /// Return as soon as there are events you have not been given yet.
    Next,
    /// Return when the run's final event is in (earlier unseen edge events come with it).
    Final,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WaitParams {
    /// The run id from watch_start / watch_list.
    pub id: String,
    /// "next" or "final".
    pub until: Until,
    /// Give up after this many seconds (default 60, at most 3600); call again to keep waiting.
    pub timeout_s: Option<f64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct IdParams {
    /// The run id from watch_start / watch_list.
    pub id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct StopParams {
    /// The run id from watch_start / watch_list.
    pub id: String,
    /// Only TERM (the default) is accepted: the supervisor sends it to the job's process group and escalates to
    /// SIGKILL by itself.
    pub signal: Option<String>,
    /// Seconds the job gets after TERM before its process group is SIGKILLed (default 5).
    pub grace_s: Option<f64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListParams {
    /// At most this many runs, newest first (default 50).
    pub limit: Option<usize>,
}

/// An error a tool reports to the model (`isError`), not a protocol error.
fn fail(msg: impl std::fmt::Display) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(msg.to_string())])
}

fn reply(v: Value) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(v.to_string())])
}

fn event_json(e: &Event) -> Value {
    let mut v = e.event.clone();
    if let Value::Object(m) = &mut v {
        m.insert("seq".into(), json!(e.seq));
    }
    v
}

fn stored_json(s: &Stored) -> Value {
    let mut v = s.event.clone();
    if let Value::Object(m) = &mut v {
        m.insert("seq".into(), json!(s.seq));
    }
    v
}

/// Run `f` on the blocking pool, bounded by `limit` of wall-clock time. The
/// runtime thread never waits on the filesystem or a socket; a stuck
/// operation costs a pool thread, not the server.
async fn blocking<T: Send + 'static>(limit: Duration, f: impl FnOnce() -> T + Send + 'static) -> Result<T, String> {
    match tokio::time::timeout(limit, tokio::task::spawn_blocking(f)).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(format!("internal error: {e}")),
        Err(_) => Err(format!(
            "timed out after {limit:?}: the state directory or the watcher is not responding"
        )),
    }
}

/// Only TERM exists: the supervisor owns the escalation.
fn check_signal(name: &str) -> Result<(), String> {
    let bare = name.trim().to_ascii_uppercase();
    match bare.strip_prefix("SIG").unwrap_or(&bare) {
        "TERM" => Ok(()),
        "KILL" => Err(
            "never SIGKILL a run directly; watch_stop sends TERM and the supervisor escalates to SIGKILL on \
                       the job's process group by itself"
                .into(),
        ),
        other => Err(format!(
            "unsupported signal {other:?}: watch_stop sends TERM (the only option)"
        )),
    }
}

fn is_heartbeat(e: &Event) -> bool {
    e.event["reason"] == "heartbeat"
}

/// Edge events: where an agent should look. Everything but heartbeats and a
/// non-final `progressing` (output resumed).
pub fn is_edge(e: &Event) -> bool {
    !is_heartbeat(e)
        && (e.is_final
            || matches!(
                e.event["state"].as_str(),
                Some("stalled" | "waiting_on_input" | "failing")
            ))
}

/// `notifications/claude/channel` for an edge event: the compact follow line
/// as content; `run_id` is the id the tools take (not the event's own).
pub fn channel_notification(run_id: &str, e: &Event) -> CustomNotification {
    let s = |k: &str| e.event[k].as_str().unwrap_or("-").to_owned();
    CustomNotification::new(
        CHANNEL_METHOD,
        Some(json!({
            "content": e.line,
            "meta": {"run_id": run_id, "state": s("state"), "reason": s("reason")},
        })),
    )
}

#[derive(Clone)]
pub struct Watcher {
    runs: Arc<Runs>,
    channel: bool,
}

impl Watcher {
    pub fn new(runs: Arc<Runs>, channel: bool) -> Self {
        Watcher { runs, channel }
    }

    /// Push each edge event of a run this server started, until it ends.
    fn spawn_channel(&self, peer: Peer<RoleServer>, meta: &Meta) {
        let (runs, meta) = (self.runs.clone(), meta.clone());
        tokio::spawn(async move {
            let mut reader = Some(EventReader::resume(&meta.events, ReadPos::default()));
            loop {
                let (r, id, rs) = (
                    reader.take().expect("put back each round"),
                    meta.id.clone(),
                    runs.clone(),
                );
                let step = blocking(IO_BOUND, move || {
                    let live = rs.probe(&id);
                    let mut r = r;
                    let polled = r.poll();
                    (r, live, polled)
                })
                .await;
                let Ok((r, live, polled)) = step else { return };
                reader = Some(r);
                let polled = polled.unwrap_or_default();
                for e in polled.events.iter().filter(|e| is_edge(e)) {
                    let n = ServerNotification::CustomNotification(channel_notification(&meta.id, e));
                    if let Err(err) = peer.send_notification(n).await {
                        eprintln!("watcher-s1 mcp: channel push for {} failed: {err}", meta.id);
                        return;
                    }
                }
                let r = reader.as_ref().expect("just set");
                if r.is_done() || (!live.alive() && polled.events.is_empty() && !polled.more) {
                    return;
                }
                if polled.more {
                    tokio::task::yield_now().await;
                } else {
                    sleep(POLL).await;
                }
            }
        });
    }

    /// `meta.json` of a run, read on the blocking pool.
    async fn meta(&self, id: &str) -> Result<Meta, String> {
        let (runs, id) = (self.runs.clone(), id.to_owned());
        blocking(IO_BOUND, move || runs.meta(&id)).await?
    }

    /// The run's phase: the final event decides, wherever it sits in the file.
    async fn phase_of(&self, meta: &Meta, final_seen: bool, live: &Liveness) -> Phase {
        let mut final_seen = final_seen;
        if !final_seen && !live.alive() {
            let (runs, m) = (self.runs.clone(), meta.clone());
            final_seen = matches!(
                blocking(IO_BOUND, move || runs.summary(&m)).await,
                Ok(Ok(s)) if s.final_ev.is_some()
            );
        }
        self.runs.phase(meta, final_seen, live)
    }

    /// Block (without spinning) until `until` is satisfied or `timeout`.
    /// Each round is one cursor transaction on the blocking pool; the
    /// deadline is checked between rounds, whatever the producer does.
    async fn wait(&self, meta: &Meta, until: &Until, timeout: Duration) -> Result<Value, String> {
        let deadline = Instant::now() + timeout;
        let mode = match until {
            Until::Next => Take::Next,
            Until::Final => Take::Final,
        };
        loop {
            let (runs, m) = (self.runs.clone(), meta.clone());
            // Liveness before the read: a final event is written before the
            // watcher exits, so "gone and no final" is never a race.
            let (live, taken) = blocking(IO_BOUND, move || {
                let live = runs.probe(&m.id);
                (live, runs.take(&m, mode))
            })
            .await?;
            let taken = taken?;
            let phase = self.phase_of(meta, taken.final_seen, &live).await;
            let lost = !taken.ready && !phase.live() && phase != Phase::Finished;
            if taken.ready || lost || Instant::now() >= deadline {
                return Ok(wait_reply(meta, &taken, phase, lost));
            }
            if taken.more {
                tokio::task::yield_now().await;
            } else {
                sleep(POLL.min(deadline.saturating_duration_since(Instant::now()))).await;
            }
        }
    }

    /// Ask the supervisor to stop the job (TERM, KILL after the grace, KILL
    /// of the group again before the leader is reaped) and wait for the
    /// final event.
    async fn stop(&self, meta: &Meta, grace: Duration) -> Result<Value, String> {
        let (runs, m) = (self.runs.clone(), meta.clone());
        let (live, sum) = blocking(IO_BOUND, move || (runs.probe(&m.id), runs.summary(&m))).await?;
        if sum?.final_ev.is_some() {
            return Err(format!("run {} already finished", meta.id));
        }
        let phase = self.runs.phase(meta, false, &live);
        if !phase.live() {
            return Err(format!("run {}: the watcher process is gone", meta.id));
        }
        let (runs, id) = (self.runs.clone(), meta.id.clone());
        blocking(IO_BOUND, move || runs.stop(&id, grace)).await??;
        let began = Instant::now();
        let deadline = began + grace + KILL_SETTLE;
        let final_ev = loop {
            let (runs, m) = (self.runs.clone(), meta.clone());
            let (live, sum) = blocking(IO_BOUND, move || (runs.probe(&m.id), runs.summary(&m))).await?;
            let fin = sum?.final_ev;
            if fin.is_some() || !live.alive() || Instant::now() >= deadline {
                break fin;
            }
            sleep(POLL).await;
        };
        Ok(json!({
            "id": meta.id,
            "signal": "TERM",
            "escalated_to_sigkill": final_ev.as_ref().is_some_and(|f| f.event["exit"]["signal"] == 9),
            "pid": live_field(&live, "pid"),
            "pgid": live_field(&live, "pgid"),
            "ended": final_ev.is_some(),
            "waited_ms": began.elapsed().as_millis() as u64,
            "final": final_ev.as_ref().map(stored_json),
            "line": final_ev.as_ref().map(|f| f.line.clone()),
        }))
    }
}

fn live_field(live: &Liveness, key: &str) -> Value {
    match live {
        Liveness::Alive(v) => v[key].clone(),
        Liveness::Gone => Value::Null,
    }
}

/// The reply of `watch_wait`.
fn wait_reply(meta: &Meta, t: &runs::Taken, phase: Phase, lost: bool) -> Value {
    let mut v = json!({
        "id": meta.id,
        "state": phase,
        "timed_out": !t.ready && !lost,
        "events": t.events.iter().map(event_json).collect::<Vec<_>>(),
        "lines": t.events.iter().map(|e| e.line.clone()).collect::<Vec<_>>(),
    });
    if t.heartbeats_skipped > 0 {
        v["heartbeats_skipped"] = json!(t.heartbeats_skipped);
    }
    if t.already_seen {
        v["already_seen"] = json!(true);
    }
    if t.more {
        v["more"] = json!(true);
    }
    if lost {
        v["note"] = json!(match phase {
            Phase::Failed => "the watcher could not be started (see watch_status)",
            _ => "the watcher process is gone and wrote no final event (it was killed outright)",
        });
    }
    v
}

/// `watch_status` / `watch_list` row. Blocking: call on the pool.
fn describe(runs: &Runs, meta: &Meta) -> Value {
    let live = runs.probe(&meta.id);
    let (sum, err) = match runs.summary(meta) {
        Ok(s) => (s, None),
        Err(e) => (runs::Summary::default(), Some(e)),
    };
    let phase = runs.phase(meta, sum.final_ev.is_some(), &live);
    let end_ms = if phase.live() {
        runs::now_ms()
    } else {
        runs.last_write_ms(meta).unwrap_or_else(runs::now_ms)
    };
    let last = sum.last.as_ref();
    let mut v = json!({
        "id": meta.id,
        "cmd": meta.cmd,
        "cwd": meta.cwd,
        "started": meta.started,
        "state": phase,
        "alive": live.alive(),
        "watcher_pid": meta.watcher_pid,
        "job_pid": live_field(&live, "pid"),
        "job_pgid": live_field(&live, "pgid"),
        "elapsed_s": end_ms.saturating_sub(meta.started_ms) as f64 / 1000.0,
        "event_state": last.map(|e| e.event["state"].clone()),
        "exit": sum.final_ev.as_ref().map(|e| e.event["exit"].clone()),
        "events": sum.events,
        "last_event": last.map(stored_json),
        "line": last.map(|e| e.line.clone()),
        "events_path": meta.events,
        "log_path": meta.log,
    });
    if sum.partial {
        v["partial"] = json!(true);
    }
    if let Some(e) = err.or_else(|| meta.error.clone()) {
        v["error"] = json!(e);
    }
    v
}

#[tool_router]
impl Watcher {
    #[tool(
        description = "Run a command under watcher-s1, detached: it survives this server and the session. Returns the run id and the events/log paths. Wait for it with watch_wait."
    )]
    async fn watch_start(
        &self,
        Parameters(p): Parameters<StartParams>,
        peer: Peer<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let opts = StartOpts {
            cmd: p.cmd,
            cwd: p.cwd,
            silence: p.silence,
            timeout: p.timeout,
            heartbeat: p.heartbeat,
            s1: p.s1,
        };
        let runs = self.runs.clone();
        let started = blocking(Duration::from_secs(30), move || runs.start(&opts))
            .await
            .and_then(|r| r);
        Ok(match started {
            Ok(meta) => {
                if self.channel {
                    self.spawn_channel(peer, &meta);
                }
                reply(json!({
                    "id": meta.id,
                    "events_path": meta.events,
                    "log_path": meta.log,
                    "watcher_pid": meta.watcher_pid,
                    "started": meta.started,
                    "cmd": meta.cmd,
                    "state": "running",
                }))
            }
            Err(e) => fail(e),
        })
    }

    #[tool(
        description = "Block until the run's next unseen event (until: next) or its final event (until: final), or timeout_s. Returns events as JSON plus the compact follow lines. Works for runs started by an earlier server. Check timed_out; the final event carries the exit code and the verdict."
    )]
    async fn watch_wait(&self, Parameters(p): Parameters<WaitParams>) -> Result<CallToolResult, McpError> {
        let secs = p.timeout_s.unwrap_or(DEFAULT_WAIT_S);
        if !secs.is_finite() || secs < 0.0 {
            return Ok(fail("timeout_s must be a non-negative number"));
        }
        let meta = match self.meta(&p.id).await {
            Ok(m) => m,
            Err(e) => return Ok(fail(e)),
        };
        let timeout = Duration::from_secs_f64(secs.min(MAX_WAIT_S));
        Ok(match self.wait(&meta, &p.until, timeout).await {
            Ok(v) => reply(v),
            Err(e) => fail(e),
        })
    }

    #[tool(description = "The last event of a run, how long it has run, and whether its watcher process is alive.")]
    async fn watch_status(&self, Parameters(p): Parameters<IdParams>) -> Result<CallToolResult, McpError> {
        let meta = match self.meta(&p.id).await {
            Ok(m) => m,
            Err(e) => return Ok(fail(e)),
        };
        let runs = self.runs.clone();
        Ok(match blocking(IO_BOUND, move || describe(&runs, &meta)).await {
            Ok(v) => reply(v),
            Err(e) => fail(e),
        })
    }

    #[tool(
        description = "Stop a run: its supervisor sends TERM to the job's whole process group, SIGKILLs the group after grace_s (default 5) and again before it reaps the leader, so descendants that ignore TERM die too; the final event has reason stopped and the real signal exit. Talks to the supervisor over its control socket; never signals a pid."
    )]
    async fn watch_stop(&self, Parameters(p): Parameters<StopParams>) -> Result<CallToolResult, McpError> {
        if let Err(e) = check_signal(p.signal.as_deref().unwrap_or("TERM")) {
            return Ok(fail(e));
        }
        let grace = p.grace_s.unwrap_or(DEFAULT_GRACE_S);
        if !grace.is_finite() || grace < 0.0 {
            return Ok(fail("grace_s must be a non-negative number"));
        }
        let meta = match self.meta(&p.id).await {
            Ok(m) => m,
            Err(e) => return Ok(fail(e)),
        };
        Ok(
            match self.stop(&meta, Duration::from_secs_f64(grace.min(MAX_WAIT_S))).await {
                Ok(v) => reply(v),
                Err(e) => fail(e),
            },
        )
    }

    #[tool(description = "Runs in the state directory, newest first, with their state (running, finished, lost).")]
    async fn watch_list(&self, Parameters(p): Parameters<ListParams>) -> Result<CallToolResult, McpError> {
        let limit = p.limit.unwrap_or(50);
        let runs = self.runs.clone();
        Ok(
            match blocking(IO_BOUND * 4, move || {
                runs.list()
                    .iter()
                    .take(limit)
                    .map(|m| describe(&runs, m))
                    .collect::<Vec<_>>()
            })
            .await
            {
                Ok(all) => reply(json!({"runs": all})),
                Err(e) => fail(e),
            },
        )
    }
}

/// The MCP `instructions`: what a model needs without reading the guide.
fn instructions(channel: bool) -> String {
    let mut s = String::from(
        "watcher-s1 supervises long commands. watch_start runs one detached (it outlives this server) and returns \
         an id; watch_wait {id, until: \"final\"} blocks until it ends and returns the exit code and verdict; \
         watch_status, watch_list and watch_stop manage runs. Events react per the guide, resource watcher-s1://guide.",
    );
    if channel {
        s.push_str(
            " Edge events of runs started by this server (stalled, waiting_on_input, failing, final) also arrive as \
             <channel source=\"watcher-s1\" run_id=... state=... reason=...> lines; run_id is the id the tools take.",
        );
    }
    s
}

#[tool_handler]
impl ServerHandler for Watcher {
    fn get_info(&self) -> ServerConfig {
        let mut caps = ServerCapabilities::builder().enable_tools().enable_resources().build();
        if self.channel {
            let mut exp = std::collections::BTreeMap::new();
            exp.insert(CHANNEL_CAPABILITY.to_string(), serde_json::Map::new());
            caps.experimental = Some(exp);
        }
        ServerConfig::new(caps)
            .with_server_info(Implementation::new("watcher-s1", env!("CARGO_PKG_VERSION")))
            .with_instructions(instructions(self.channel))
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        let guide = Resource::new(GUIDE_URI, "guide")
            .with_title("watcher-s1 agent guide")
            .with_description("When and how to run commands under watcher-s1, and how to react to its events.")
            .with_mime_type("text/markdown")
            .with_size(GUIDE.len() as u64);
        Ok(ListResourcesResult::with_all_items(vec![guide]))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, McpError> {
        if request.uri != GUIDE_URI {
            return Err(McpError::resource_not_found(
                format!("unknown resource {}", request.uri),
                Some(json!({"uri": request.uri})),
            ));
        }
        Ok(ReadResourceResult::new(vec![
            ResourceContents::text(GUIDE, GUIDE_URI).with_mime_type("text/markdown"),
        ])
        .into())
    }
}

/// Serve on stdio until the client disconnects. Returns the process exit code.
pub fn serve(state_dir: PathBuf, channel: bool) -> i32 {
    let runs = match Runs::open(&state_dir) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("watcher-s1 mcp: state dir {}: {e}", state_dir.display());
            return 1;
        }
    };
    let runs = Arc::new(runs);
    let pruned = runs.prune(runs::MAX_AGE);
    eprintln!(
        "watcher-s1 mcp: serving on stdio (state dir {}, channel {}, pruned {pruned} old run(s), {})",
        state_dir.display(),
        if channel { "on" } else { "off" },
        rfc3339(runs::now_ms() as u128)
    );
    let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("watcher-s1 mcp: cannot start the async runtime: {e}");
            return 1;
        }
    };
    rt.block_on(async move {
        // Completed runs are pruned for as long as the server lives (never a
        // run whose watcher still answers).
        let pruner = runs.clone();
        let every = std::env::var(PRUNE_ENV)
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|s| *s > 0)
            .map_or(PRUNE_EVERY, Duration::from_secs);
        tokio::spawn(async move {
            loop {
                sleep(every).await;
                let r = pruner.clone();
                match blocking(IO_BOUND * 8, move || r.prune(runs::MAX_AGE)).await {
                    Ok(n) if n > 0 => eprintln!("watcher-s1 mcp: pruned {n} old run(s)"),
                    Ok(_) => {}
                    Err(e) => eprintln!("watcher-s1 mcp: prune: {e}"),
                }
            }
        });
        let service = match Watcher::new(runs, channel).serve(rmcp::transport::stdio()).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("watcher-s1 mcp: {e}");
                return 1;
            }
        };
        match service.waiting().await {
            Ok(_) => 0,
            Err(e) => {
                eprintln!("watcher-s1 mcp: {e}");
                1
            }
        }
    })
}
