//! `watcher-s1 mcp`: the supervisor as an MCP server over stdio.
//!
//! The only async code in the crate lives here (a current-thread tokio
//! runtime, built in [`serve`]); the wrapper itself stays synchronous. Runs
//! are detached `watcher-s1 --pipe --events ...` processes whose state lives
//! in the state directory ([`runs`]), so they outlive this server and any
//! later server can wait on them. stdout is protocol only.

pub mod runs;

use crate::GUIDE;
use crate::event::rfc3339;
use crate::follow::{Event, EventReader};
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
use runs::{Dur, Meta, Phase, Runs, StartOpts};
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
/// Events returned by one `until: "next"`; the rest stay unseen for the next call.
const MAX_BATCH: usize = 100;
/// Grace between TERM to the watcher and SIGKILL to the job's group.
const DEFAULT_GRACE_S: f64 = 5.0;
/// How long to wait for the final event after the group SIGKILL.
const KILL_SETTLE: Duration = Duration::from_secs(10);

#[derive(Debug, Deserialize, JsonSchema)]
pub struct StartParams {
    /// The command as an argv array, e.g. ["nix", "build", ".#foo"]. Run directly, not through a shell:
    /// use ["sh", "-c", "..."] for shell syntax.
    pub cmd: Vec<String>,
    /// Working directory (default: the server's).
    pub cwd: Option<PathBuf>,
    /// Quiet time before `stalled`: seconds or "5m" (default 300s; 0 disables).
    pub silence: Option<Dur>,
    /// Hard limit: seconds or "2h". The job's process group gets TERM, then KILL.
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
    /// Signal for the watcher, which forwards it to the job's process group: TERM (default), INT, HUP or QUIT.
    pub signal: Option<String>,
    /// Seconds to wait for the job to end before SIGKILL goes to its process group (default 5).
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
    pub fn new(runs: Runs, channel: bool) -> Self {
        Watcher {
            runs: Arc::new(runs),
            channel,
        }
    }

    /// Push each edge event of a run this server started, until it ends.
    fn spawn_channel(&self, peer: Peer<RoleServer>, meta: &Meta) {
        let (id, path) = (meta.id.clone(), meta.events.clone());
        let pid = meta.watcher_pid;
        tokio::spawn(async move {
            let mut reader = EventReader::new(path);
            loop {
                let alive = runs::pid_alive(pid);
                let batch = reader.poll().unwrap_or_default();
                for e in batch.iter().filter(|e| is_edge(e)) {
                    let n = ServerNotification::CustomNotification(channel_notification(&id, e));
                    if let Err(err) = peer.send_notification(n).await {
                        eprintln!("watcher-s1 mcp: channel push for {id} failed: {err}");
                        return;
                    }
                }
                if reader.is_done() || (!alive && batch.is_empty()) {
                    return;
                }
                sleep(POLL).await;
            }
        });
    }

    /// Block (without spinning) until `until` is satisfied or `timeout`.
    async fn wait(&self, meta: &Meta, until: &Until, timeout: Duration) -> Result<Value, String> {
        let deadline = Instant::now() + timeout;
        let cursor = self.runs.cursor(&meta.id);
        let mut reader = EventReader::new(&meta.events);
        let mut seen: Vec<Event> = Vec::new();
        loop {
            // Liveness before the read: a final event is written before the
            // watcher exits, so "dead and no final" is never a race.
            let alive = runs::pid_alive(meta.watcher_pid);
            seen.extend(reader.poll().map_err(|e| format!("{}: {e}", meta.events.display()))?);
            let pending: Vec<&Event> = seen.iter().filter(|e| e.seq > cursor).collect();
            let final_ev = seen.iter().find(|e| e.is_final);
            let ready = match until {
                // A finished run has nothing more to say: answer with the verdict.
                Until::Next => !pending.is_empty() || final_ev.is_some(),
                Until::Final => final_ev.is_some(),
            };
            let lost = !alive && final_ev.is_none() && !ready;
            let timed_out = Instant::now() >= deadline;
            if ready || lost || timed_out {
                return Ok(self.wait_result(meta, until, &seen, cursor, ready, lost));
            }
            sleep(POLL.min(deadline.saturating_duration_since(Instant::now()))).await;
        }
    }

    /// Build the reply and advance the caller's cursor past what it was given.
    fn wait_result(&self, meta: &Meta, until: &Until, seen: &[Event], cursor: u64, ready: bool, lost: bool) -> Value {
        let pending: Vec<&Event> = seen.iter().filter(|e| e.seq > cursor).collect();
        let (mut out, skipped, upto): (Vec<&Event>, usize, u64) = match until {
            Until::Next => {
                let batch: Vec<&Event> = pending.into_iter().take(MAX_BATCH).collect();
                let upto = batch.last().map_or(cursor, |e| e.seq);
                (batch, 0, upto)
            }
            // The final verdict is the point: edge events come with it, heartbeats are counted.
            Until::Final if ready => {
                let hb = pending.iter().filter(|e| is_heartbeat(e)).count();
                let upto = pending.last().map_or(cursor, |e| e.seq);
                (pending.into_iter().filter(|e| !is_heartbeat(e)).collect(), hb, upto)
            }
            Until::Final => (Vec::new(), 0, cursor),
        };
        let mut already_seen = false;
        if out.is_empty()
            && ready
            && let Some(f) = seen.iter().find(|e| e.is_final)
        {
            // An earlier call handed the final event out already.
            out.push(f);
            already_seen = true;
        }
        self.runs.set_cursor(&meta.id, upto);
        let phase = if seen.iter().any(|e| e.is_final) {
            Phase::Finished
        } else if lost {
            Phase::Lost
        } else {
            Phase::Running
        };
        let mut v = json!({
            "id": meta.id,
            "state": phase,
            "timed_out": !ready && !lost,
            "events": out.iter().map(|e| event_json(e)).collect::<Vec<_>>(),
            "lines": out.iter().map(|e| e.line.clone()).collect::<Vec<_>>(),
        });
        if skipped > 0 {
            v["heartbeats_skipped"] = json!(skipped);
        }
        if already_seen {
            v["already_seen"] = json!(true);
        }
        if lost {
            v["note"] = json!("the watcher process is gone and wrote no final event (it was killed outright)");
        }
        v
    }

    async fn stop(&self, meta: &Meta, sig: nix::sys::signal::Signal, grace: Duration) -> Result<Value, String> {
        let events = self.runs.events(meta).map_err(|e| e.to_string())?;
        match self.runs.phase(meta, &events) {
            Phase::Finished => return Err(format!("run {} already finished", meta.id)),
            Phase::Lost => return Err(format!("run {}: the watcher process is gone", meta.id)),
            Phase::Running => {}
        }
        // Resolve the group first: after a quick TERM exit there is nothing to ask.
        let pgid = self.pgid(meta, events).await;
        runs::signal_watcher(meta, sig)?;
        let mut escalated = false;
        if !self.ended_within(meta, grace).await {
            let pgid = pgid.ok_or("the job did not end and its process group is unknown; not escalating")?;
            runs::kill_job_group(pgid)?;
            escalated = true;
            self.ended_within(meta, KILL_SETTLE).await;
        }
        let events = self.runs.events(meta).map_err(|e| e.to_string())?;
        let last = events.iter().find(|e| e.is_final);
        Ok(json!({
            "id": meta.id,
            "signal": format!("{sig:?}"),
            "escalated_to_sigkill": escalated,
            "pgid": pgid,
            "ended": last.is_some(),
            "final": last.map(event_json),
            "line": last.map(|e| e.line.clone()),
        }))
    }

    /// The job's process group; a job that was only just spawned has no
    /// event and no child yet, so look for a moment.
    async fn pgid(&self, meta: &Meta, mut events: Vec<Event>) -> Option<i32> {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(p) = runs::job_pgid(meta, &events) {
                return Some(p);
            }
            if Instant::now() >= deadline || events.iter().any(|e| e.is_final) {
                return None;
            }
            sleep(POLL).await;
            events = self.runs.events(meta).unwrap_or_default();
        }
    }

    /// Has the run's final event arrived (or its watcher died) within `d`?
    async fn ended_within(&self, meta: &Meta, d: Duration) -> bool {
        let deadline = Instant::now() + d;
        loop {
            let alive = runs::pid_alive(meta.watcher_pid);
            let events = self.runs.events(meta).unwrap_or_default();
            if events.iter().any(|e| e.is_final) {
                return true;
            }
            if !alive || Instant::now() >= deadline {
                return !alive;
            }
            sleep(POLL).await;
        }
    }

    fn summary(&self, meta: &Meta) -> Value {
        let events = self.runs.events(meta).unwrap_or_default();
        let phase = self.runs.phase(meta, &events);
        let last = events.last();
        let final_ev = events.iter().find(|e| e.is_final);
        let end_ms = match phase {
            Phase::Running => runs::now_ms(),
            _ => self.runs.last_write_ms(meta).unwrap_or_else(runs::now_ms),
        };
        json!({
            "id": meta.id,
            "cmd": meta.cmd,
            "cwd": meta.cwd,
            "started": meta.started,
            "state": phase,
            "alive": runs::pid_alive(meta.watcher_pid),
            "watcher_pid": meta.watcher_pid,
            "elapsed_s": end_ms.saturating_sub(meta.started_ms) as f64 / 1000.0,
            "event_state": last.map(|e| e.event["state"].clone()),
            "exit": final_ev.map(|e| e.event["exit"].clone()),
            "events": events.len(),
            "last_event": last.map(event_json),
            "line": last.map(|e| e.line.clone()),
            "events_path": meta.events,
            "log_path": meta.log,
        })
    }
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
        Ok(match self.runs.start(&opts) {
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
        let meta = match self.runs.meta(&p.id) {
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
        Ok(match self.runs.meta(&p.id) {
            Ok(meta) => reply(self.summary(&meta)),
            Err(e) => fail(e),
        })
    }

    #[tool(
        description = "Stop a run: TERM (or signal) goes to the watcher, which forwards it to the job's process group; if the job has not ended after grace_s (default 5), the job's process group gets SIGKILL and the watcher reports the signal exit. Never kills the watcher itself."
    )]
    async fn watch_stop(&self, Parameters(p): Parameters<StopParams>) -> Result<CallToolResult, McpError> {
        let sig = match runs::parse_signal(p.signal.as_deref().unwrap_or("TERM")) {
            Ok(s) => s,
            Err(e) => return Ok(fail(e)),
        };
        let grace = p.grace_s.unwrap_or(DEFAULT_GRACE_S);
        if !grace.is_finite() || grace < 0.0 {
            return Ok(fail("grace_s must be a non-negative number"));
        }
        let meta = match self.runs.meta(&p.id) {
            Ok(m) => m,
            Err(e) => return Ok(fail(e)),
        };
        Ok(
            match self
                .stop(&meta, sig, Duration::from_secs_f64(grace.min(MAX_WAIT_S)))
                .await
            {
                Ok(v) => reply(v),
                Err(e) => fail(e),
            },
        )
    }

    #[tool(description = "Runs in the state directory, newest first, with their state (running, finished, lost).")]
    async fn watch_list(&self, Parameters(p): Parameters<ListParams>) -> Result<CallToolResult, McpError> {
        let limit = p.limit.unwrap_or(50);
        let all: Vec<Value> = self.runs.list().iter().take(limit).map(|m| self.summary(m)).collect();
        Ok(reply(json!({"runs": all})))
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
