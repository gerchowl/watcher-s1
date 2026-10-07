//! Wrap one command: run it in its own process group (under a PTY by
//! default), tee its output through unchanged, watch it (tiers 0–2), emit
//! sideband events, and report its exit truthfully.

use crate::detect;
use crate::event::{BlockedProc, ENV_PARENT, Event, Exit, Heartbeat, ProcInfo, RunInfo, Severity, Sink, State};
use crate::outbox::Outbox;
use crate::probe::{Prober, Sample, sample_tree};
use crate::questions::Surface;
use crate::ring::{self, LineTracker, Ring};
use crate::s1::{Client, Verdict};
use nix::sys::signal::{Signal, kill};
use nix::sys::termios::{self, LocalFlags, OutputFlags, SetArg, Termios};
use nix::unistd::Pid;
use std::io::Write;
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

pub struct Options {
    pub argv: Vec<String>,
    pub pty: bool,
    /// Output-silence threshold; `None` disables the timer.
    pub silence: Option<Duration>,
    /// Hard wall-clock limit; the whole group gets TERM, then KILL.
    pub timeout: Option<Duration>,
    pub kill_grace: Duration,
    /// Quiet time after which a prompt-shaped last line counts as waiting.
    pub prompt_after: Duration,
    /// Process-state sampling interval while the output is quiet.
    pub sample_every: Duration,
    /// A blocked (D/U) or unprobeable tree this long raises `stalled`.
    pub blocked_after: Duration,
    pub probe_timeout: Duration,
    /// Cancel an unanswered prompt after this long (SIGINT, then TERM and
    /// KILL with `kill_grace` between); `None` = only report it.
    pub prompt_cancel: Option<Duration>,
    /// Emit a `heartbeat` status event every this long (monotonic ticks).
    pub heartbeat: Option<Duration>,
    /// Attach a System One verdict to each heartbeat.
    pub heartbeat_s1: bool,
    pub evidence_bytes: usize,
    pub s1: Option<Arc<Client>>,
    pub sink: Arc<Sink>,
    pub quiet: bool,
}

/// How the child ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Code(i32),
    Signal(i32),
}

impl Outcome {
    pub fn exit(self) -> Exit {
        match self {
            Outcome::Code(c) => Exit {
                code: Some(c),
                signal: None,
            },
            Outcome::Signal(s) => Exit {
                code: None,
                signal: Some(s),
            },
        }
    }
}

pub use crate::diag::log;

// ---------------------------------------------------------------------------
// Signals: handlers only write the signal number to a self-pipe; the poll
// loop does the work.

static SIG_PIPE_W: AtomicI32 = AtomicI32::new(-1);

extern "C" fn on_signal(sig: libc::c_int) {
    let saved = nix::errno::Errno::last_raw();
    let fd = SIG_PIPE_W.load(Ordering::Relaxed);
    if fd >= 0 {
        let b = sig as u8;
        unsafe {
            libc::write(fd, &b as *const u8 as *const libc::c_void, 1);
        }
    }
    nix::errno::Errno::set_raw(saved);
}

const FORWARDED: &[libc::c_int] = &[
    libc::SIGINT,
    libc::SIGTERM,
    libc::SIGHUP,
    libc::SIGQUIT,
    libc::SIGUSR1,
    libc::SIGUSR2,
];

fn install_signals() -> std::io::Result<OwnedFd> {
    let (r, w) = nix::unistd::pipe().map_err(std::io::Error::from)?;
    for fd in [&r, &w] {
        set_nonblocking(fd.as_raw_fd());
        set_cloexec(fd.as_raw_fd());
    }
    SIG_PIPE_W.store(w.as_raw_fd(), Ordering::Relaxed);
    std::mem::forget(w); // lives for the process
    for &s in FORWARDED.iter().chain(&[libc::SIGCHLD, libc::SIGWINCH]) {
        unsafe {
            // An inherited SIG_IGN (nohup, `cmd &` in sh) is the caller's
            // decision: keep ignoring it, and so never forward it. exec keeps
            // SIG_IGN, so the child ignores it too, exactly as without us.
            let mut old: libc::sigaction = std::mem::zeroed();
            libc::sigaction(s, std::ptr::null(), &mut old);
            if old.sa_sigaction == libc::SIG_IGN && FORWARDED.contains(&s) {
                continue;
            }
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = on_signal as *const () as usize;
            sa.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut sa.sa_mask);
            libc::sigaction(s, &sa, std::ptr::null_mut());
        }
    }
    Ok(r)
}

fn set_nonblocking(fd: RawFd) {
    unsafe {
        let fl = libc::fcntl(fd, libc::F_GETFL);
        libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK);
    }
}

fn set_cloexec(fd: RawFd) {
    unsafe {
        libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
    }
}

fn isatty(fd: RawFd) -> bool {
    unsafe { libc::isatty(fd) == 1 }
}

fn winsize_of(fd: RawFd) -> Option<libc::winsize> {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let ok = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) } == 0;
    (ok && ws.ws_col > 0 && ws.ws_row > 0).then_some(ws)
}

fn our_winsize() -> libc::winsize {
    winsize_of(1)
        .or_else(|| winsize_of(0))
        .or_else(|| winsize_of(2))
        .unwrap_or(libc::winsize {
            ws_row: 50,
            ws_col: 160,
            ws_xpixel: 0,
            ws_ypixel: 0,
        })
}

/// Puts our tty stdin into raw mode for the life of the value.
struct RawMode {
    saved: Termios,
}

impl RawMode {
    fn enter() -> Option<Self> {
        let fd = unsafe { BorrowedFd::borrow_raw(0) };
        let saved = termios::tcgetattr(fd).ok()?;
        let mut raw = saved.clone();
        termios::cfmakeraw(&mut raw);
        // Raw input, but keep output processing: the child's "\n" (no CRLF
        // on the PTY side) must still become "\r\n" on the real terminal,
        // as must our own log and event lines.
        raw.output_flags.insert(OutputFlags::OPOST | OutputFlags::ONLCR);
        termios::tcsetattr(fd, SetArg::TCSANOW, &raw).ok()?;
        Some(Self { saved })
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let fd = unsafe { BorrowedFd::borrow_raw(0) };
        let _ = termios::tcsetattr(fd, SetArg::TCSANOW, &self.saved);
    }
}

// ---------------------------------------------------------------------------
// Spawning

struct Spawned {
    child: Child,
    /// Output fds we read: (fd, is_stderr). PTY: the master only.
    outputs: Vec<(OwnedFd, bool)>,
    /// Where forwarded stdin goes (the PTY master), when interactive.
    input: Option<RawFd>,
    /// `--pipe` with our terminal handed to the child's group.
    tty_handed: bool,
}

/// Is stdin a terminal whose foreground group is ours?
fn owns_terminal() -> bool {
    isatty(0) && unsafe { libc::tcgetpgrp(0) == libc::getpgrp() }
}

fn with_sigttou_ignored(f: impl FnOnce()) {
    unsafe {
        let old = libc::signal(libc::SIGTTOU, libc::SIG_IGN);
        f();
        libc::signal(libc::SIGTTOU, old);
    }
}

fn spawn(opts: &Options, run_id: &str, interactive: bool) -> std::io::Result<Spawned> {
    let mut cmd = Command::new(&opts.argv[0]);
    cmd.args(&opts.argv[1..]).env(ENV_PARENT, run_id);
    if !opts.pty {
        cmd.stdin(Stdio::inherit())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // The child leads its own group. If we own the terminal, hand it the
        // foreground (as a shell would), or its first read of the tty stops
        // it with SIGTTIN. Done in the child too, so there is no race.
        let take_tty = owns_terminal();
        unsafe {
            cmd.pre_exec(move || {
                if libc::setpgid(0, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if take_tty {
                    libc::signal(libc::SIGTTOU, libc::SIG_IGN);
                    libc::tcsetpgrp(0, libc::getpid());
                    libc::signal(libc::SIGTTOU, libc::SIG_DFL);
                }
                Ok(())
            });
        }
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                // exec failed after the child may already have taken the
                // terminal: take it back, or the shell is stranded.
                if take_tty {
                    with_sigttou_ignored(|| unsafe {
                        libc::tcsetpgrp(0, libc::getpgrp());
                    });
                }
                return Err(e);
            }
        };
        let out: OwnedFd = child.stdout.take().expect("piped").into();
        let err: OwnedFd = child.stderr.take().expect("piped").into();
        for fd in [&out, &err] {
            set_nonblocking(fd.as_raw_fd());
        }
        if take_tty {
            with_sigttou_ignored(|| unsafe {
                libc::tcsetpgrp(0, child.id() as libc::pid_t);
            });
        }
        return Ok(Spawned {
            child,
            outputs: vec![(out, false), (err, true)],
            input: None,
            tty_handed: take_tty,
        });
    }

    let ws = our_winsize();
    let pty = nix::pty::openpty(None::<&nix::pty::Winsize>, None::<&Termios>).map_err(std::io::Error::from)?;
    let (master, slave) = (pty.master, pty.slave);
    unsafe { libc::ioctl(slave.as_raw_fd(), libc::TIOCSWINSZ, &ws) };
    // The child's "\n" reaches us as "\n" (no CRLF translation), so the
    // tee is byte-faithful for files and pipes; a terminal on our side
    // translates it (RawMode keeps OPOST|ONLCR for exactly this).
    if let Ok(mut t) = termios::tcgetattr(&slave) {
        t.output_flags.remove(OutputFlags::ONLCR);
        if !interactive {
            t.local_flags.remove(LocalFlags::ECHO);
        }
        let _ = termios::tcsetattr(&slave, SetArg::TCSANOW, &t);
    }
    set_cloexec(master.as_raw_fd());
    let stdin = if interactive {
        Stdio::from(slave.try_clone()?)
    } else {
        Stdio::inherit()
    };
    cmd.stdin(stdin)
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave));
    unsafe {
        cmd.pre_exec(|| {
            // New session: the child leads its own process group (pgid ==
            // pid) and takes the PTY (its stdout) as controlling terminal.
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::ioctl(1, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = cmd.spawn()?;
    drop(cmd); // closes our copies of the slave, so EOF/EIO arrives on exit
    set_nonblocking(master.as_raw_fd());
    let input = interactive.then(|| master.as_raw_fd());
    Ok(Spawned {
        child,
        outputs: vec![(master, false)],
        input,
        tty_handed: false,
    })
}

// ---------------------------------------------------------------------------
// The watch loop

struct Watch<'a> {
    opts: &'a Options,
    run: RunInfo,
    ring: Ring,
    /// The last non-empty line, tracked apart from the (evicting) ring.
    last_line: LineTracker,
    /// Event transport: every event leaves through the writer thread.
    out: Outbox,
    start: Instant,
    last_output: Instant,
    /// Warn-level events emitted in the current quiet episode.
    episode_warned: bool,
    stall_done: bool,
    prompt_checked: bool,
    prompt_emitted: Option<String>,
    blocked_emitted: bool,
    prober: Prober,
    last_sample_at: Option<Instant>,
    blocked_since: Option<Instant>,
    last_proc: Option<ProcInfo>,
    pending_stall: Option<(Receiver<Result<Verdict, String>>, Event)>,
    /// Newlines seen in the child's output so far.
    lines: u64,
    /// Heartbeat bookkeeping: the next tick, and the counters at the last one.
    next_heartbeat: Option<Instant>,
    hb_bytes: u64,
    hb_lines: u64,
    /// The heartbeat awaiting its System One verdict (at most one).
    pending_heartbeats: HbQueue<Event, Verdict>,
    timeout_fired: bool,
    kill_at: Option<Instant>,
    prompt_since: Option<Instant>,
    cancel_fired: bool,
    term_at: Option<Instant>,
}

impl<'a> Watch<'a> {
    fn new(opts: &'a Options, run: RunInfo) -> Self {
        let now = Instant::now();
        Watch {
            opts,
            run,
            ring: Ring::new(ring::DEFAULT_CAPACITY),
            last_line: LineTracker::default(),
            out: Outbox::start(opts.sink.clone(), opts.quiet),
            start: now,
            last_output: now,
            episode_warned: false,
            stall_done: false,
            prompt_checked: false,
            prompt_emitted: None,
            blocked_emitted: false,
            prober: Prober::new(opts.probe_timeout),
            last_sample_at: None,
            blocked_since: None,
            last_proc: None,
            pending_stall: None,
            lines: 0,
            next_heartbeat: opts.heartbeat.map(|d| later(now, d)),
            hb_bytes: 0,
            hb_lines: 0,
            pending_heartbeats: HbQueue::default(),
            timeout_fired: false,
            kill_at: None,
            prompt_since: None,
            cancel_fired: false,
            term_at: None,
        }
    }

    /// Did we (timeout or prompt cancel) kill the group?
    fn we_killed(&self) -> bool {
        self.timeout_fired || self.cancel_fired
    }

    fn evidence(&self) -> String {
        ring::tail(&self.ring.text(), self.opts.evidence_bytes).to_string()
    }

    fn emit(&mut self, ev: &Event) {
        if ev.severity >= Severity::Warn {
            self.episode_warned = true;
        }
        self.enqueue(ev.clone());
    }

    /// The only way a non-heartbeat event reaches the outbox. A heartbeat
    /// still awaiting its System One verdict was created earlier, so it is
    /// released first (fail-open if the verdict is not in): the file order
    /// is always the creation order, and a stale `stalled` heartbeat can
    /// never land after the `resumed` that superseded it.
    fn enqueue(&mut self, ev: Event) {
        for (hb, res) in self.pending_heartbeats.release() {
            self.emit_heartbeat(hb, res);
        }
        self.out.send(ev);
    }

    /// Take in bytes that were already there (log attach): they count for
    /// the evidence and the last line, not as new output.
    fn prime(&mut self, data: &[u8]) {
        self.ring.push(data);
        self.last_line.feed(data);
        self.hb_bytes = self.ring.total_bytes();
    }

    fn on_output(&mut self, data: &[u8]) {
        self.ring.push(data);
        self.last_line.feed(data);
        self.lines += data.iter().filter(|&&b| b == b'\n').count() as u64;
        self.last_output = Instant::now();
        if self.episode_warned {
            let ev = self
                .run
                .event(State::Progressing, Severity::Info, "resumed", self.evidence());
            self.enqueue(ev);
        }
        self.episode_warned = false;
        self.stall_done = false;
        self.prompt_checked = false;
        self.prompt_emitted = None;
        self.prompt_since = None;
        self.blocked_emitted = false;
        self.blocked_since = None;
        self.last_sample_at = None;
        self.pending_stall = None;
    }

    /// The state a heartbeat reports: the open episode, else `progressing`.
    fn episode_state(&self) -> State {
        if self.prompt_emitted.is_some() {
            State::WaitingOnInput
        } else if self.stall_done || self.blocked_emitted {
            State::Stalled
        } else {
            State::Progressing
        }
    }

    /// Ask System One about the current tail on a worker thread, so a slow
    /// endpoint never stalls the timers. Same breaker, deadline and fail-open
    /// path wherever it is called from.
    fn judge_async(&self, client: &Arc<Client>) -> Receiver<Result<Verdict, String>> {
        let (tx, rx) = mpsc::channel();
        let client = client.clone();
        let cmd = self.run.cmd.clone();
        let tail = ring::tail(&self.ring.text(), client.questions.tail_bytes).to_string();
        let budget = s1_budget(&client);
        std::thread::spawn(move || {
            let _ = tx.send(client.judge(&cmd, &tail, budget));
        });
        rx
    }

    /// One heartbeat event: progress since the previous one. It reads the
    /// output counters but touches neither them nor the silence timer or the
    /// episode flags: a heartbeat is our output, not the child's.
    fn heartbeat_event(&mut self, now: Instant) -> Event {
        let last_line = self.last_line.last_line();
        let (bytes, lines) = (self.ring.total_bytes(), self.lines);
        let hb = Heartbeat {
            elapsed_ms: now.duration_since(self.start).as_millis() as u64,
            bytes_since_last: bytes - self.hb_bytes,
            lines_since_last: lines - self.hb_lines,
            last_line,
            heartbeats_dropped: None,
        };
        (self.hb_bytes, self.hb_lines) = (bytes, lines);
        self.run.heartbeat(self.episode_state(), self.evidence(), hb)
    }

    /// Emit a heartbeat when its tick is due; release verdicts that arrived.
    /// Liveness comes first: a verdict never delays a heartbeat (see `HbQueue`).
    fn heartbeat(&mut self, now: Instant) {
        let mut out = Vec::new();
        if let (Some(every), Some(at)) = (self.opts.heartbeat, self.next_heartbeat)
            && now >= at
        {
            self.next_heartbeat = Some(next_tick(self.start, every, now));
            let ev = self.heartbeat_event(now);
            match self.opts.s1.clone().filter(|_| self.opts.heartbeat_s1) {
                Some(client) => {
                    let mut q = std::mem::take(&mut self.pending_heartbeats);
                    out = q.tick(ev, || self.judge_async(&client));
                    self.pending_heartbeats = q;
                }
                None => self.out.send_heartbeat(ev),
            }
        } else {
            out = self.pending_heartbeats.poll();
        }
        for (ev, res) in out {
            self.emit_heartbeat(ev, res);
        }
    }

    /// Attach a verdict (or fail open without one); the state never changes.
    fn emit_heartbeat(&mut self, mut ev: Event, res: Result<Verdict, String>) {
        match res {
            Ok(v) => ev.s1 = Some(v),
            Err(e) => log(self.opts.quiet, &format!("System One unavailable, failing open: {e}")),
        }
        self.out.send_heartbeat(ev);
    }

    /// Before the final event (or on a signal exit): release the pending
    /// heartbeat, waiting at most `HB_FLUSH_BOUND` for its verdict, else
    /// failing open. Never blocks the final event for a System One budget.
    fn flush_heartbeats(&mut self) {
        for (ev, res) in self.pending_heartbeats.flush(HB_FLUSH_BOUND) {
            self.emit_heartbeat(ev, res);
        }
    }

    fn proc_info(&self, now: Instant) -> Option<ProcInfo> {
        self.last_proc.clone().map(|mut p| {
            p.blocked_for_s = self.blocked_since.map(|t| now.duration_since(t).as_secs()).unwrap_or(0);
            p
        })
    }

    /// Tier 0/1/2 timers; called every tick.
    fn tick(&mut self, now: Instant) {
        let quiet = now.duration_since(self.last_output);

        // Tier 1: a prompt-shaped unterminated last line, once quiet.
        if !self.prompt_checked && quiet >= self.opts.prompt_after {
            self.prompt_checked = true;
            if let Some(p) = detect::prompt(&self.ring.text()) {
                let mut ev = self
                    .run
                    .event(State::WaitingOnInput, Severity::Warn, "prompt", self.evidence());
                ev.prompt = Some(p.clone());
                self.emit(&ev);
                self.prompt_emitted = Some(p);
                self.prompt_since = Some(now);
            }
        }

        // --on-prompt cancel: an unanswered prompt is cancelled, never
        // answered. SIGINT first (what a person's Ctrl-C would do), then the
        // TERM/KILL escalation.
        if let (Some(after), Some(since)) = (self.opts.prompt_cancel, self.prompt_since)
            && !self.cancel_fired
            && !self.timeout_fired
            && self.run.pgid > 0
            && now.duration_since(since) >= after
        {
            self.cancel_fired = true;
            let _ = kill(Pid::from_raw(-self.run.pgid), Signal::SIGINT);
            log(
                self.opts.quiet,
                &format!(
                    "prompt unanswered for {after:?}: SIGINT to process group {}",
                    self.run.pgid
                ),
            );
            self.term_at = Some(later(now, self.opts.kill_grace));
        }

        // Tier 0: process-state sampler while quiet.
        if self.run.pid > 0
            && quiet >= self.opts.sample_every
            && self
                .last_sample_at
                .is_none_or(|t| now.duration_since(t) >= self.opts.sample_every)
        {
            self.last_sample_at = Some(now);
            let (root, pgid) = (self.run.pid, self.run.pgid);
            self.prober.start(move || sample_tree(root, pgid));
        }
        if let Some(s) = self.prober.poll()
            && quiet >= self.opts.sample_every
        {
            // (A sample that lands after output resumed is stale: dropped.)
            self.on_sample(s, now);
        }
        if !self.blocked_emitted
            && self.prompt_emitted.is_none()
            && self
                .blocked_since
                .is_some_and(|t| now.duration_since(t) >= self.opts.blocked_after)
        {
            self.blocked_emitted = true;
            let mut ev = self
                .run
                .event(State::Stalled, Severity::Warn, "blocked", self.evidence());
            ev.proc = self.proc_info(now);
            self.emit(&ev);
        }

        self.heartbeat(now);

        // Tier 0 silence threshold, judged by System One when configured.
        if let Some(limit) = self.opts.silence
            && !self.stall_done
            && quiet >= limit
        {
            self.stall_done = true;
            if self.prompt_emitted.is_none() {
                let mut ev = self
                    .run
                    .event(State::Stalled, Severity::Warn, "silence", self.evidence());
                ev.proc = self.proc_info(now);
                match &self.opts.s1 {
                    Some(client) => self.pending_stall = Some((self.judge_async(client), ev)),
                    None => self.emit(&ev),
                }
            }
        }
        if let Some((rx, _)) = &self.pending_stall
            && let Ok(res) = rx.try_recv()
        {
            let (_, mut ev) = self.pending_stall.take().expect("pending");
            match res {
                Ok(v) => {
                    if v.fused >= self.opts.s1.as_ref().map_or(1.0, |c| c.threshold(Surface::Wrap)) {
                        ev.state = State::Failing;
                        ev.dedup_key = crate::event::dedup_key(&self.run.host, &self.run.cmd, State::Failing);
                    }
                    ev.s1 = Some(v);
                }
                Err(e) => log(self.opts.quiet, &format!("System One unavailable, failing open: {e}")),
            }
            self.emit(&ev);
        }
    }

    fn on_sample(&mut self, s: Sample, now: Instant) {
        let (probe, blocked) = match s {
            Sample::Ok(ps) => (
                "ok",
                ps.into_iter()
                    .filter(|p| p.blocked())
                    .map(|p| BlockedProc {
                        pid: p.pid,
                        state: p.state,
                        wchan: p.wchan,
                        comm: p.comm,
                    })
                    .collect::<Vec<_>>(),
            ),
            Sample::Timeout => ("timeout", vec![]),
            Sample::Error(e) => {
                log(self.opts.quiet, &format!("process probe failed: {e}"));
                ("error", vec![])
            }
        };
        let suspect = probe == "timeout" || !blocked.is_empty();
        if suspect {
            self.blocked_since.get_or_insert(now);
        } else {
            self.blocked_since = None;
        }
        self.last_proc = Some(ProcInfo {
            probe: probe.into(),
            blocked,
            blocked_for_s: 0,
        });
    }

    /// Hard timeout: TERM the whole group, KILL after the grace.
    fn enforce_timeout(&mut self, now: Instant) {
        if let Some(limit) = self.opts.timeout
            && !self.timeout_fired
            && now.duration_since(self.start) >= limit
        {
            self.timeout_fired = true;
            let _ = kill(Pid::from_raw(-self.run.pgid), Signal::SIGTERM);
            log(
                self.opts.quiet,
                &format!("timeout after {:?}: SIGTERM to process group {}", limit, self.run.pgid),
            );
            self.kill_at = Some(later(now, self.opts.kill_grace));
        }
        if let Some(at) = self.term_at
            && now >= at
        {
            self.term_at = None;
            let _ = kill(Pid::from_raw(-self.run.pgid), Signal::SIGTERM);
            log(
                self.opts.quiet,
                &format!("still running: SIGTERM to process group {}", self.run.pgid),
            );
            self.kill_at = Some(later(now, self.opts.kill_grace));
        }
        if let Some(at) = self.kill_at
            && now >= at
        {
            self.kill_at = None;
            let _ = kill(Pid::from_raw(-self.run.pgid), Signal::SIGKILL);
            log(
                self.opts.quiet,
                &format!("grace expired: SIGKILL to process group {}", self.run.pgid),
            );
        }
    }
}

/// The writer thread for our own stdout/stderr, behind a bounded queue
/// (8 chunks of at most 64 KiB: enough to keep the child streaming, small
/// enough that the final flush to a slow reader stays short).
struct Tee {
    tx: mpsc::SyncSender<(RawFd, Vec<u8>)>,
    handle: std::thread::JoinHandle<()>,
}

impl Tee {
    fn start() -> Self {
        let (tx, rx) = mpsc::sync_channel::<(RawFd, Vec<u8>)>(8);
        let handle = std::thread::spawn(move || {
            for (fd, data) in rx {
                write_all_fd(fd, &data);
            }
        });
        Tee { tx, handle }
    }

    /// Queue without blocking; a full queue hands the chunk back.
    fn try_send(&self, chunk: (RawFd, Vec<u8>)) -> Result<(), (RawFd, Vec<u8>)> {
        match self.tx.try_send(chunk) {
            Ok(()) => Ok(()),
            Err(mpsc::TrySendError::Full(c)) => Err(c),
            Err(mpsc::TrySendError::Disconnected(_)) => Ok(()), // writer gone: drop
        }
    }

    /// The drain-time send: before the exit just `try_send`; after it, wait
    /// for room until `cap` (processes may still be writing), or with no cap
    /// for as long as it takes, unless a stop signal arrives.
    fn send_draining(
        &self,
        chunk: (RawFd, Vec<u8>),
        draining: bool,
        cap: Option<Instant>,
        sig_r: RawFd,
    ) -> Result<(), Stop> {
        if !draining {
            return self.try_send(chunk).map_err(Stop::Full);
        }
        if let Some(cap) = cap {
            return self.send_until(chunk, cap).map_err(Stop::Full);
        }
        let mut chunk = chunk;
        loop {
            match self.try_send(chunk) {
                Ok(()) => return Ok(()),
                Err(back) => {
                    chunk = back;
                    if stop_signalled(sig_r, 20) {
                        return Err(Stop::Signal);
                    }
                }
            }
        }
    }

    /// Queue, waiting for room until `deadline`; past it the chunk comes back.
    fn send_until(&self, mut chunk: (RawFd, Vec<u8>), deadline: Instant) -> Result<(), (RawFd, Vec<u8>)> {
        loop {
            match self.try_send(chunk) {
                Ok(()) => return Ok(()),
                Err(back) if Instant::now() >= deadline => return Err(back),
                Err(back) => {
                    chunk = back;
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
        }
    }

    /// Flush everything queued, like a process blocking on its last write,
    /// but give up when INT/TERM/HUP/QUIT arrives on the signal pipe: being
    /// told to stop beats delivering the rest to a reader that is not reading.
    /// (We still exit like the child did: the signal stopped the tee, not the
    /// child.)
    fn finish(self, sig_r: RawFd) {
        drop(self.tx);
        while !self.handle.is_finished() {
            if stop_signalled(sig_r, 20) {
                return; // the writer thread dies with the process
            }
        }
        let _ = self.handle.join();
    }
}

/// Why a drain-time send gave up.
enum Stop {
    /// No room before the cap: the chunk comes back.
    Full((RawFd, Vec<u8>)),
    /// A stop signal arrived.
    Signal,
}

/// Wait up to `ms` for the signal pipe; true if INT/TERM/HUP/QUIT arrived.
fn stop_signalled(sig_r: RawFd, ms: i32) -> bool {
    let mut p = libc::pollfd {
        fd: sig_r,
        events: libc::POLLIN,
        revents: 0,
    };
    unsafe { libc::poll(&mut p, 1, ms) };
    let mut sb = [0u8; 64];
    let mut stop = false;
    while let Some(n) = read_some(sig_r, &mut sb).filter(|n| *n > 0) {
        stop |= sb[..n].iter().any(|&s| {
            matches!(
                s as libc::c_int,
                libc::SIGINT | libc::SIGTERM | libc::SIGHUP | libc::SIGQUIT
            )
        });
    }
    stop
}

/// Is any process of group `pgid` still alive?
fn group_alive(pgid: i32) -> bool {
    pgid > 0 && unsafe { libc::kill(-pgid, 0) } == 0
}

/// Has `pid` exited? Looks without reaping (WNOWAIT), so the zombie keeps its
/// pid, and with it the process group id, reserved until we decide to reap.
fn exited_unreaped(pid: i32) -> bool {
    unsafe {
        let mut info: libc::siginfo_t = std::mem::zeroed();
        let r = libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        );
        r == 0 && siginfo_pid(&info) != 0
    }
}

#[cfg(target_os = "linux")]
unsafe fn siginfo_pid(i: &libc::siginfo_t) -> libc::pid_t {
    unsafe { i.si_pid() }
}

#[cfg(not(target_os = "linux"))]
unsafe fn siginfo_pid(i: &libc::siginfo_t) -> libc::pid_t {
    i.si_pid
}

/// The first heartbeat tick `start + k·every` (k >= 1) after `now`: a loop
/// that was blocked past several ticks emits one heartbeat, not a burst.
fn next_tick(start: Instant, every: Duration, now: Instant) -> Instant {
    let k = now.duration_since(start).as_nanos() / every.as_nanos().max(1) + 1;
    later(start, every.saturating_mul(u32::try_from(k).unwrap_or(u32::MAX)))
}

fn later(now: Instant, d: Duration) -> Instant {
    now.checked_add(d).unwrap_or(now + Duration::from_secs(86400))
}

/// The whole-call budget for one System One judgement.
pub fn s1_budget(c: &Client) -> Duration {
    c.cfg.timeout * c.cfg.urls.len().max(1) as u32
}

fn write_all_fd(fd: RawFd, mut data: &[u8]) {
    while !data.is_empty() {
        let n = unsafe { libc::write(fd, data.as_ptr() as *const libc::c_void, data.len()) };
        if n > 0 {
            data = &data[n as usize..];
        } else if n < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
            continue;
        } else if n < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::WouldBlock {
            let mut p = libc::pollfd {
                fd,
                events: libc::POLLOUT,
                revents: 0,
            };
            unsafe { libc::poll(&mut p, 1, 100) };
        } else {
            return; // closed or broken: drop our copy, keep watching
        }
    }
}

/// Non-blocking write: bytes accepted (possibly 0), `None` if the fd is gone.
fn write_some(fd: RawFd, data: &[u8]) -> Option<usize> {
    loop {
        let n = unsafe { libc::write(fd, data.as_ptr() as *const libc::c_void, data.len()) };
        if n >= 0 {
            return Some(n as usize);
        }
        match std::io::Error::last_os_error().kind() {
            std::io::ErrorKind::Interrupted => continue,
            std::io::ErrorKind::WouldBlock => return Some(0),
            _ => return None,
        }
    }
}

/// Read what is available. `None` = EOF/EIO (the writer side is gone).
fn read_some(fd: RawFd, buf: &mut [u8]) -> Option<usize> {
    loop {
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n > 0 {
            return Some(n as usize);
        }
        if n == 0 {
            return None;
        }
        match std::io::Error::last_os_error().kind() {
            std::io::ErrorKind::Interrupted => continue,
            std::io::ErrorKind::WouldBlock => return Some(0),
            _ => return None, // EIO on a PTY master once the slave closed
        }
    }
}

pub fn make_run_info(argv: &[String], pid: i32) -> RunInfo {
    let host = crate::event::hostname();
    RunInfo {
        run_id: format!("{host}:{}:{}", std::process::id(), crate::event::unix_ms()),
        host,
        cmd: crate::event::shell_join(argv),
        pid,
        pgid: pid,
        caused_by: std::env::var(ENV_PARENT).ok().filter(|v| !v.is_empty()),
    }
}

/// Run and watch the command; returns how the child ended. Never changes
/// the outcome: the verdict only goes to the sideband.
pub fn run(opts: Options) -> Outcome {
    let interactive = opts.pty && isatty(0);
    let sig_r = match install_signals() {
        Ok(r) => r,
        Err(e) => {
            log(opts.quiet, &format!("cannot install signal handlers: {e}"));
            return Outcome::Code(125);
        }
    };
    let mut run = make_run_info(&opts.argv, 0);
    let mut sp = match spawn(&opts, &run.run_id, interactive) {
        Ok(s) => s,
        Err(e) => {
            // Mirror the shell: 127 not found, 126 found but not runnable.
            let code = if e.kind() == std::io::ErrorKind::NotFound {
                127
            } else {
                126
            };
            crate::diag::error(&format!("cannot run {}: {e}", opts.argv[0]));
            let mut ev = run.event(State::Failing, Severity::Error, "exit", format!("cannot run: {e}"));
            ev.exit = Some(Outcome::Code(code).exit());
            opts.sink.emit(&ev);
            return Outcome::Code(code);
        }
    };
    let pid = sp.child.id() as i32;
    run.pid = pid;
    run.pgid = pid;
    let raw = if interactive { RawMode::enter() } else { None };

    let mut w = Watch::new(&opts, run);
    // Our own stdout/stderr are written by a separate thread, so a stalled
    // reader downstream can never stall the timers (--timeout included).
    let tee = Tee::start();
    // A chunk the writer queue had no room for: we stop reading the child
    // (natural backpressure) until it fits, but keep ticking.
    let mut held: Option<(RawFd, Vec<u8>)> = None;

    let mut buf = vec![0u8; 64 * 1024];
    let mut open: Vec<bool> = sp.outputs.iter().map(|_| true).collect();
    let mut stdin_open = sp.input.is_some();
    let mut status = None;
    let mut drain_until: Option<Instant> = None;
    // Hard stop for the drain: a grandchild that keeps writing after the
    // leader exited (`sh -c 'yes &'`) must not keep us alive forever.
    let mut drain_cap: Option<Instant> = None;
    // INT/TERM/HUP/QUIT while draining to a stalled reader: stop teeing.
    let mut aborted = false;
    // Forwarded keystrokes the PTY has not accepted yet. While non-empty we
    // wait for POLLOUT on the master instead of reading more stdin, so a
    // big paste can never block the loop (and with it the output drain).
    let mut pending: Vec<u8> = Vec::new();

    loop {
        // After the child exited nothing needs the loop to stay responsive
        // (the timers are done), so the drain flushes with blocking sends and
        // never stops reading because the queue is full.
        let draining = status.is_some();
        // While draining: does anything of the child's group still live (and
        // so may keep writing)? Only then does the 3 s cap apply; the output
        // an exited group left behind is finite and is always teed in full.
        let writers_left = draining && group_alive(pid);
        let cap = if writers_left { drain_cap } else { None };
        if let Some(h) = held.take() {
            held = match tee.send_draining(h, draining, cap, sig_r.as_raw_fd()) {
                Ok(()) => None,
                Err(Stop::Full(back)) => Some(back),
                Err(Stop::Signal) => {
                    aborted = true;
                    None
                }
            };
        }
        // Poll set: signal pipe, open outputs (unless backpressured), stdin
        // when forwarding.
        let mut fds = vec![libc::pollfd {
            fd: sig_r.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        }];
        for (i, (fd, _)) in sp.outputs.iter().enumerate() {
            if open[i] && held.is_none() {
                fds.push(libc::pollfd {
                    fd: fd.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                });
            }
        }
        let stdin_slot = (stdin_open && status.is_none() && pending.is_empty()).then(|| {
            fds.push(libc::pollfd {
                fd: 0,
                events: libc::POLLIN,
                revents: 0,
            });
            fds.len() - 1
        });
        let pending_slot = match (sp.input, pending.is_empty()) {
            (Some(input), false) => {
                fds.push(libc::pollfd {
                    fd: input,
                    events: libc::POLLOUT,
                    revents: 0,
                });
                Some(fds.len() - 1)
            }
            _ => None,
        };
        unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, 100) };
        // An output fd the kernel calls invalid would otherwise spin.
        for (i, (fd, _)) in sp.outputs.iter().enumerate() {
            if fds
                .iter()
                .any(|p| p.fd == fd.as_raw_fd() && p.revents & libc::POLLNVAL != 0)
            {
                open[i] = false;
            }
        }

        // Signals.
        let mut sb = [0u8; 64];
        while let Some(n) = read_some(sig_r.as_raw_fd(), &mut sb).filter(|n| *n > 0) {
            for &s in &sb[..n] {
                let s = s as libc::c_int;
                if s == libc::SIGWINCH {
                    if let Some(input) = sp.input {
                        let ws = our_winsize();
                        unsafe { libc::ioctl(input, libc::TIOCSWINSZ, &ws) };
                    }
                } else if FORWARDED.contains(&s)
                    && let Ok(sig) = Signal::try_from(s)
                {
                    let _ = kill(Pid::from_raw(-w.run.pgid), sig);
                }
            }
        }

        // Output: into the ring, and to the writer to tee through unchanged.
        let mut got_output = false;
        'outputs: for (i, (fd, is_err)) in sp.outputs.iter().enumerate() {
            while open[i] && held.is_none() {
                // The drain is bounded even while data keeps coming (a
                // grandchild writing faster than our reader consumes).
                if cap.is_some_and(|c| Instant::now() >= c) {
                    break 'outputs;
                }
                match read_some(fd.as_raw_fd(), &mut buf) {
                    Some(0) => break,
                    Some(n) => {
                        got_output = true;
                        w.on_output(&buf[..n]);
                        let chunk = (if *is_err { 2 } else { 1 }, buf[..n].to_vec());
                        match tee.send_draining(chunk, draining, cap, sig_r.as_raw_fd()) {
                            Ok(()) => {}
                            Err(Stop::Full(back)) => {
                                held = Some(back);
                                break 'outputs;
                            }
                            Err(Stop::Signal) => {
                                aborted = true;
                                break 'outputs;
                            }
                        }
                    }
                    None => open[i] = false,
                }
            }
        }

        // Stdin forwarding (interactive PTY only). Read ONLY when poll says
        // stdin is ready: our tty stdin stays blocking (it is shared with
        // the shell), and a blocking read here would freeze the loop.
        if let (Some(slot), Some(_)) = (stdin_slot, sp.input) {
            let re = fds[slot].revents;
            if re & libc::POLLIN != 0 {
                match read_some(0, &mut buf) {
                    Some(0) => {}
                    Some(n) => pending.extend_from_slice(&buf[..n]),
                    None => stdin_open = false,
                }
            } else if re & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
                stdin_open = false;
            }
        }
        if let Some(input) = sp.input
            && !pending.is_empty()
            && (pending_slot.is_none_or(|i| fds[i].revents != 0))
        {
            match write_some(input, &pending) {
                Some(n) => {
                    pending.drain(..n);
                }
                None => pending.clear(), // the PTY is gone
            }
        }

        let now = Instant::now();
        if status.is_none() {
            if exited_unreaped(pid) {
                // The leader is a zombie: its pid, and so the group id, cannot
                // be reused until we reap it. Kill what we started killing
                // NOW, before the reap, so the signal cannot reach a stranger.
                if w.we_killed() {
                    let _ = kill(Pid::from_raw(-pid), Signal::SIGKILL);
                }
                let st = match sp.child.wait() {
                    Ok(st) => st,
                    Err(_) => break,
                };
                status = Some(st);
                // Grandchildren may hold the output open: drain briefly.
                drain_until = Some(now + Duration::from_millis(300));
                drain_cap = Some(now + Duration::from_secs(3));
            } else {
                w.enforce_timeout(now);
                w.tick(now);
            }
        }
        w.out.pump();
        // Leave once every output is closed, or once the grace for new data
        // is over AND a full read pass found nothing, or at the 3 s cap. So
        // the output a child leaves behind is teed even behind a slow reader,
        // and a grandchild that writes forever is cut off at the cap.
        if status.is_some()
            && (open.iter().all(|o| !o)
                || (drain_until.is_some_and(|d| now >= d) && !got_output && held.is_none())
                || aborted
                || (writers_left && drain_cap.is_some_and(|d| now >= d)))
        {
            break;
        }
    }

    // Only output from processes still writing after the 3 s cap, or output
    // left when we were told to stop, is ever dropped, and never silently.
    if let Some((_, h)) = held.take() {
        log(
            opts.quiet,
            &format!(
                "drain stopped ({}): {} bytes of output not teed",
                if aborted {
                    "signal"
                } else {
                    "3 s cap, processes still writing"
                },
                h.len()
            ),
        );
    }
    if !aborted {
        tee.finish(sig_r.as_raw_fd());
    }
    let Some(st) = status else {
        log(opts.quiet, "lost the child's exit status");
        return Outcome::Code(1);
    };
    let outcome = match (st.code(), st.signal()) {
        (Some(c), _) => Outcome::Code(c),
        (None, Some(s)) => Outcome::Signal(s),
        _ => Outcome::Code(1),
    };
    drop(raw);
    if sp.tty_handed {
        with_sigttou_ignored(|| unsafe {
            libc::tcsetpgrp(0, libc::getpgrp());
        });
    }
    w.flush_heartbeats();
    let ev = final_event(&w, outcome);
    // Everything queued first, then the final event; may block on a sink
    // nobody reads (see `outbox`).
    w.out.finish(Some(ev));
    outcome
}

/// Passive mode: watch a growing log file (no child, no exit). Silence,
/// prompts and the System One silence judgement work as in wrap mode; the
/// process sampler has no tree to sample. The file is followed across
/// truncation and rotation (a new inode at the same path). Nothing is
/// teed. Runs until INT/TERM/HUP/QUIT, then leaves by that signal.
pub fn run_log(opts: Options, path: &std::path::Path) -> Outcome {
    use std::io::{Read, Seek, SeekFrom};
    use std::os::unix::fs::MetadataExt;

    let sig_r = match install_signals() {
        Ok(r) => r,
        Err(e) => {
            log(opts.quiet, &format!("cannot install signal handlers: {e}"));
            return Outcome::Code(125);
        }
    };
    let mut run = make_run_info(&[], 0);
    run.cmd = format!("--log {}", path.display());
    run.pgid = 0;
    let mut w = Watch::new(&opts, run);

    // Open (or wait for) the file; prime the ring with its recent tail so a
    // prompt already sitting there is seen, without emitting anything for it.
    let open_at_end = |w: &mut Watch<'_>| -> Option<(std::fs::File, u64, u64)> {
        let mut f = std::fs::File::open(path).ok()?;
        let meta = f.metadata().ok()?;
        let len = meta.len();
        let start = len.saturating_sub(ring::DEFAULT_CAPACITY as u64);
        let mut prime = Vec::new();
        f.seek(SeekFrom::Start(start)).ok()?;
        Read::by_ref(&mut f).take(len - start).read_to_end(&mut prime).ok()?;
        w.prime(&prime);
        Some((f, len, meta.ino()))
    };
    let mut file = open_at_end(&mut w);
    if file.is_none() {
        log(
            opts.quiet,
            &format!("{}: not readable yet; waiting for it", path.display()),
        );
    }
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let mut p = libc::pollfd {
            fd: sig_r.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        unsafe { libc::poll(&mut p, 1, 200) };
        let mut sb = [0u8; 64];
        while let Some(n) = read_some(sig_r.as_raw_fd(), &mut sb).filter(|n| *n > 0) {
            if let Some(&s) = sb[..n].iter().find(|&&s| FORWARDED.contains(&(s as libc::c_int)))
                && matches!(
                    s as libc::c_int,
                    libc::SIGINT | libc::SIGTERM | libc::SIGHUP | libc::SIGQUIT
                )
            {
                // Pending heartbeats are emitted fail-open, not dropped.
                w.flush_heartbeats();
                w.out.finish(None);
                return Outcome::Signal(s as libc::c_int);
            }
        }

        match &mut file {
            None => file = open_at_end(&mut w),
            Some((f, offset, ino)) => {
                // Finish what the current handle holds first: lines written
                // to the old file just before a rename still belong to it.
                drain_file(f, offset, &mut w, &mut buf);
                // Rotation (new inode) or truncation: continue at offset 0.
                if let Ok(meta) = std::fs::metadata(path)
                    && (meta.ino() != *ino || meta.len() < *offset)
                    && let Ok(nf) = std::fs::File::open(path)
                {
                    *f = nf;
                    *offset = 0;
                    *ino = meta.ino();
                    drain_file(f, offset, &mut w, &mut buf);
                }
            }
        }
        w.tick(Instant::now());
        w.out.pump();
    }
}

/// Read `f` to its current end, feeding the watch.
fn drain_file(f: &mut std::fs::File, offset: &mut u64, w: &mut Watch<'_>, buf: &mut [u8]) {
    use std::io::Read;
    loop {
        match f.read(buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                *offset += n as u64;
                w.on_output(&buf[..n]);
            }
        }
    }
}

fn final_event(w: &Watch<'_>, outcome: Outcome) -> Event {
    let text = w.ring.text();
    let evidence = ring::tail(&text, w.opts.evidence_bytes).to_string();
    let (mut state, mut severity, reason) = match outcome {
        _ if w.timeout_fired => (State::Failing, Severity::Error, "timeout"),
        _ if w.cancel_fired => (State::Failing, Severity::Error, "prompt_cancelled"),
        Outcome::Code(0) => (State::Done, Severity::Info, "exit"),
        Outcome::Code(_) => (State::Failing, Severity::Error, "exit"),
        Outcome::Signal(_) => (State::Failing, Severity::Error, "signal"),
    };
    // Tier 2 at exit, once per run: for a failure it adds evidence and
    // confidence; for exit 0 it is the only detector of a masked failure.
    // (A regex pre-filter here caught only 34 % of real failures; see
    // docs/eval/questions-spike.md.)
    let mut verdict = None;
    let mut reason = reason;
    if let Some(client) = &w.opts.s1
        && !text.trim().is_empty()
    {
        let tail = ring::tail(&text, client.questions.tail_bytes);
        match client.judge(&w.run.cmd, tail, s1_budget(client)) {
            Ok(v) => {
                if state == State::Done && v.fused >= client.threshold(Surface::Wrap) {
                    state = State::Failing;
                    severity = Severity::Warn;
                    reason = "masked_failure";
                }
                verdict = Some(v);
            }
            Err(e) => log(w.opts.quiet, &format!("System One unavailable, failing open: {e}")),
        }
    }
    let mut ev = w.run.event(state, severity, reason, evidence);
    ev.exit = Some(outcome.exit());
    ev.s1 = verdict;
    ev.proc = w
        .proc_info(Instant::now())
        .filter(|p| p.probe != "ok" || !p.blocked.is_empty());
    ev
}

/// Leave the way the child left: same exit code, or the same signal
/// re-raised with the default action (so callers see 128+n).
pub fn exit_like(outcome: Outcome) -> ! {
    // Everything queued for stderr (diagnostics, events on the stderr sink).
    crate::diag::drain();
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    match outcome {
        Outcome::Code(c) => std::process::exit(c),
        Outcome::Signal(s) => unsafe {
            // No second core file: the child already dumped one if it would.
            let zero = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            libc::setrlimit(libc::RLIMIT_CORE, &zero);
            libc::signal(s, libc::SIG_DFL);
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, s);
            libc::sigprocmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());
            libc::raise(s);
            // Signals whose default is to ignore (or stop) land here.
            std::process::exit(128 + s)
        },
    }
}

/// How long the final event (or a signal exit) waits for a heartbeat verdict.
const HB_FLUSH_BOUND: Duration = Duration::from_millis(250);

/// Heartbeats waiting for their System One verdict. Liveness beats verdicts,
/// and a request is never forgotten while it runs:
///  - at most ONE System One request is in flight for heartbeats, tracked by
///    its receiver for as long as the worker lives, whether or not an event
///    still awaits its verdict;
///  - if it is still pending when the next tick is due, the awaiting event is
///    emitted with `s1: null` (fail-open) and that tick's heartbeat goes out
///    at once with `s1: null`; the receiver is kept, and no new request
///    starts until the worker returns (its late verdict is discarded) or
///    disconnects, so a slow endpoint never sees overlapping requests nor
///    delays or queues up heartbeats;
///  - a ready verdict is released as soon as it is seen; a worker that died
///    (channel disconnected) is released fail-open, never waited for.
struct HbQueue<E, V> {
    /// The worker's receiver and, while it is within its tick, the heartbeat
    /// awaiting the verdict (`None` once that event went out overdue).
    inflight: Option<Inflight<E, V>>,
}

impl<E, V> Default for HbQueue<E, V> {
    fn default() -> Self {
        Self { inflight: None }
    }
}

type Inflight<E, V> = (Receiver<Result<V, String>>, Option<E>);
type Released<E, V> = Vec<(E, Result<V, String>)>;

impl<E, V> HbQueue<E, V> {
    /// Retire the worker if it returned or died; release the event awaiting
    /// its verdict, if there still is one.
    fn poll(&mut self) -> Released<E, V> {
        let Some((rx, _)) = &self.inflight else {
            return Vec::new();
        };
        let res = match rx.try_recv() {
            Ok(r) => r,
            Err(mpsc::TryRecvError::Empty) => return Vec::new(),
            Err(mpsc::TryRecvError::Disconnected) => Err("judge worker died".into()),
        };
        match self.inflight.take() {
            Some((_, Some(ev))) => vec![(ev, res)],
            _ => Vec::new(),
        }
    }

    /// A later event is about to be enqueued: release the awaiting heartbeat
    /// now (its verdict if ready, else fail-open), so it goes out first. The
    /// worker stays registered: it still counts as the one request in flight.
    fn release(&mut self) -> Released<E, V> {
        let mut out = self.poll();
        if let Some((_, awaiting)) = &mut self.inflight
            && let Some(ev) = awaiting.take()
        {
            out.push((ev, Err("superseded by a later event".into())));
        }
        out
    }

    /// A tick is due with heartbeat `ev`: returns what to emit now, in order.
    /// `spawn` starts the System One request, only if none is in flight.
    fn tick(&mut self, ev: E, spawn: impl FnOnce() -> Receiver<Result<V, String>>) -> Released<E, V> {
        let mut out = self.poll();
        match &mut self.inflight {
            None => self.inflight = Some((spawn(), Some(ev))),
            Some((_, awaiting)) => {
                if let Some(old) = awaiting.take() {
                    out.push((old, Err("verdict not ready by the next tick".into())));
                }
                out.push((ev, Err("previous request still in flight".into())));
            }
        }
        out
    }

    /// Release the awaiting event, waiting at most `bound`, else fail open.
    /// A stale worker (its event already out) is never waited for.
    fn flush(&mut self, bound: Duration) -> Released<E, V> {
        let Some((rx, Some(ev))) = self.inflight.take() else {
            return Vec::new();
        };
        let res = match rx.recv_timeout(bound) {
            Ok(r) => r,
            Err(mpsc::RecvTimeoutError::Timeout) => Err("timed out".into()),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err("judge worker died".into()),
        };
        vec![(ev, res)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Q = HbQueue<u32, u32>;
    type Chan = (mpsc::Sender<Result<u32, String>>, Receiver<Result<u32, String>>);

    fn chan() -> Chan {
        mpsc::channel()
    }

    #[test]
    fn a_later_event_releases_the_awaiting_heartbeat_first() {
        // silence -> resumed with a delayed verdict: the heartbeat (created
        // before `resumed`) must leave the queue before `resumed` is enqueued.
        let mut q = Q::default();
        let (tx, rx) = chan();
        assert!(q.tick(1, || rx).is_empty(), "held for its verdict");
        let out = q.release();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, 1);
        assert!(out[0].1.is_err(), "fail-open: no verdict yet");
        assert!(q.release().is_empty(), "nothing left to release");
        // The request is still in flight: the next tick must not spawn another.
        let out = q.tick(2, || panic!("no second request while one is in flight"));
        assert_eq!(out.len(), 1);
        assert!(out[0].1.is_err());
        // The late verdict for the released heartbeat is discarded.
        tx.send(Ok(7)).unwrap();
        assert!(q.poll().is_empty());
    }

    #[test]
    fn a_ready_verdict_is_attached_when_released_by_a_later_event() {
        let mut q = Q::default();
        let (tx, rx) = chan();
        assert!(q.tick(1, || rx).is_empty());
        tx.send(Ok(5)).unwrap();
        assert_eq!(q.release(), vec![(1, Ok(5))]);
    }

    #[test]
    fn a_slow_head_never_delays_the_next_heartbeat() {
        let mut q = Q::default();
        let (tx1, rx1) = chan();
        assert!(q.tick(1, || rx1).is_empty(), "held for its verdict");
        assert!(q.poll().is_empty(), "not ready: still held");
        // Next tick, verdict still missing: head out null, tick 2 out at once.
        let out = q.tick(2, || unreachable!("one request in flight at most"));
        assert_eq!(
            out.iter().map(|(e, r)| (*e, r.is_ok())).collect::<Vec<_>>(),
            [(1, false), (2, false)]
        );
        // The worker is still running (tx1 alive) across several more ticks:
        // each goes out null at once and no request is ever started.
        for n in 3..=8 {
            let out = q.tick(n, || unreachable!("the first request has not returned"));
            assert_eq!(out.len(), 1);
            assert_eq!((out[0].0, out[0].1.is_ok()), (n, false));
            assert!(q.poll().is_empty());
        }
        // It finally returns: its late verdict belongs to no event any more.
        tx1.send(Ok(1)).unwrap();
        assert!(q.poll().is_empty());
        assert!(q.inflight.is_none());
        // Only now may the next tick start a fresh request.
        let (tx9, rx9) = chan();
        assert!(q.tick(9, || rx9).is_empty());
        tx9.send(Ok(7)).unwrap();
        let out = q.poll();
        assert_eq!(out.len(), 1);
        assert_eq!((out[0].0, out[0].1.clone()), (9, Ok(7)));
    }

    #[test]
    fn a_stale_worker_that_dies_frees_the_slot() {
        let mut q = Q::default();
        let (tx1, rx1) = chan();
        q.tick(1, || rx1);
        q.tick(2, || unreachable!());
        drop(tx1);
        let (_tx3, rx3) = chan();
        // Tick 3 retires the dead worker (nothing awaits it) and starts anew.
        assert!(q.tick(3, || rx3).is_empty());
        assert_eq!(q.inflight.as_ref().and_then(|p| p.1), Some(3));
    }

    #[test]
    fn a_ready_head_is_released_on_the_next_tick_which_starts_a_new_request() {
        let mut q = Q::default();
        let (tx1, rx1) = chan();
        q.tick(1, || rx1);
        tx1.send(Ok(5)).unwrap();
        let (_tx2, rx2) = chan();
        let out = q.tick(2, || rx2);
        assert_eq!(out.len(), 1);
        assert_eq!((out[0].0, out[0].1.clone()), (1, Ok(5)));
        assert_eq!(q.inflight.as_ref().and_then(|p| p.1), Some(2));
    }

    #[test]
    fn a_disconnected_worker_fails_open_without_blocking() {
        let mut q = Q::default();
        let (tx, rx) = chan();
        q.tick(1, || rx);
        drop(tx); // the worker panicked
        let out = q.poll();
        assert_eq!(out.len(), 1);
        assert!(out[0].1.is_err());

        let (tx, rx) = chan();
        q.tick(2, || rx);
        drop(tx);
        let t = Instant::now();
        let out = q.flush(Duration::from_secs(5));
        assert!(
            t.elapsed() < Duration::from_secs(1),
            "disconnect must not wait out the bound"
        );
        assert_eq!(out.len(), 1);
        assert!(out[0].1.is_err());
    }

    #[test]
    fn flush_is_bounded_for_a_slow_worker() {
        let mut q = Q::default();
        let (_tx, rx) = chan();
        q.tick(1, || rx);
        let t = Instant::now();
        let out = q.flush(Duration::from_millis(50));
        assert!(t.elapsed() < Duration::from_secs(1));
        assert!(out[0].1.is_err());
        assert!(q.flush(Duration::from_secs(5)).is_empty());
    }

    #[test]
    fn heartbeat_ticks_stay_on_the_grid_and_never_burst() {
        let t0 = Instant::now();
        let every = Duration::from_secs(2);
        let at = |s: u64| t0 + Duration::from_secs(s);
        // Just after a tick: the next grid point, not "every after now".
        assert_eq!(next_tick(t0, every, at(2) + Duration::from_millis(30)), at(4));
        // The loop was blocked across ticks 4, 6 and 8: one heartbeat, then 10.
        assert_eq!(next_tick(t0, every, at(8) + Duration::from_millis(500)), at(10));
        // Exactly on a tick counts as that tick having fired.
        assert_eq!(next_tick(t0, every, at(6)), at(8));
    }
}
