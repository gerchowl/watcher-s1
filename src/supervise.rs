//! Wrap one command: run it in its own process group (under a PTY by
//! default), tee its output through unchanged, watch it (tiers 0–2), emit
//! sideband events, and report its exit truthfully.

use crate::detect;
use crate::event::{BlockedProc, ENV_PARENT, Event, Exit, ProcInfo, RunInfo, Severity, Sink, State};
use crate::probe::{Prober, Sample, sample_tree};
use crate::ring::{self, Ring};
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
    pub evidence_bytes: usize,
    pub s1: Option<Arc<Client>>,
    pub sink: Sink,
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

pub fn log(quiet: bool, msg: &str) {
    if !quiet {
        let _ = writeln!(std::io::stderr(), "watcher-s1 (log): {msg}");
    }
}

// ---------------------------------------------------------------------------
// Signals: handlers only write the signal number to a self-pipe; the poll
// loop does the work.

static SIG_PIPE_W: AtomicI32 = AtomicI32::new(-1);

extern "C" fn on_signal(sig: libc::c_int) {
    let fd = SIG_PIPE_W.load(Ordering::Relaxed);
    if fd >= 0 {
        let b = sig as u8;
        unsafe {
            libc::write(fd, &b as *const u8 as *const libc::c_void, 1);
        }
    }
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
        let mut child = cmd.spawn()?;
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
    // does its own translation.
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
    timeout_fired: bool,
    kill_at: Option<Instant>,
}

impl<'a> Watch<'a> {
    fn evidence(&self) -> String {
        ring::tail(&self.ring.text(), self.opts.evidence_bytes).to_string()
    }

    fn emit(&mut self, ev: &Event) {
        if ev.severity >= Severity::Warn {
            self.episode_warned = true;
        }
        self.opts.sink.emit(ev);
    }

    fn on_output(&mut self, data: &[u8]) {
        self.ring.push(data);
        self.last_output = Instant::now();
        if self.episode_warned {
            let ev = self
                .run
                .event(State::Progressing, Severity::Info, "resumed", self.evidence());
            self.opts.sink.emit(&ev);
        }
        self.episode_warned = false;
        self.stall_done = false;
        self.prompt_checked = false;
        self.prompt_emitted = None;
        self.blocked_emitted = false;
        self.blocked_since = None;
        self.last_sample_at = None;
        self.pending_stall = None;
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
            }
        }

        // Tier 0: process-state sampler while quiet.
        if quiet >= self.opts.sample_every
            && self
                .last_sample_at
                .is_none_or(|t| now.duration_since(t) >= self.opts.sample_every)
        {
            self.last_sample_at = Some(now);
            let (root, pgid) = (self.run.pid, self.run.pgid);
            self.prober.start(move || sample_tree(root, pgid));
        }
        if let Some(s) = self.prober.poll() {
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
                    Some(client) => {
                        let (tx, rx) = mpsc::channel();
                        let client = client.clone();
                        let cmd = self.run.cmd.clone();
                        let tail = ring::tail(&self.ring.text(), client.questions.tail_bytes).to_string();
                        let budget = s1_budget(&client);
                        std::thread::spawn(move || {
                            let _ = tx.send(client.judge(&cmd, &tail, budget));
                        });
                        self.pending_stall = Some((rx, ev));
                    }
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
                    if v.fused >= self.opts.s1.as_ref().map_or(1.0, |c| c.threshold()) {
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
            log(
                self.opts.quiet,
                &format!("timeout after {:?}: SIGTERM to process group {}", limit, self.run.pgid),
            );
            let _ = kill(Pid::from_raw(-self.run.pgid), Signal::SIGTERM);
            self.kill_at = Some(now + self.opts.kill_grace);
        }
        if let Some(at) = self.kill_at
            && now >= at
        {
            self.kill_at = None;
            log(
                self.opts.quiet,
                &format!("grace expired: SIGKILL to process group {}", self.run.pgid),
            );
            let _ = kill(Pid::from_raw(-self.run.pgid), Signal::SIGKILL);
        }
    }
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
            let _ = writeln!(
                std::io::stderr(),
                "watcher-s1 (error): cannot run {}: {e}",
                opts.argv[0]
            );
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

    let now = Instant::now();
    let mut w = Watch {
        opts: &opts,
        run,
        ring: Ring::new(ring::DEFAULT_CAPACITY),
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
        timeout_fired: false,
        kill_at: None,
    };

    let mut buf = vec![0u8; 64 * 1024];
    let mut open: Vec<bool> = sp.outputs.iter().map(|_| true).collect();
    let mut stdin_open = sp.input.is_some();
    let mut status = None;
    let mut drain_until: Option<Instant> = None;

    loop {
        // Poll set: signal pipe, open outputs, stdin when forwarding.
        let mut fds = vec![libc::pollfd {
            fd: sig_r.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        }];
        for (i, (fd, _)) in sp.outputs.iter().enumerate() {
            if open[i] {
                fds.push(libc::pollfd {
                    fd: fd.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                });
            }
        }
        if stdin_open && status.is_none() {
            fds.push(libc::pollfd {
                fd: 0,
                events: libc::POLLIN,
                revents: 0,
            });
        }
        unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, 100) };

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

        // Output: tee through unchanged, then into the ring.
        for (i, (fd, is_err)) in sp.outputs.iter().enumerate() {
            while open[i] {
                match read_some(fd.as_raw_fd(), &mut buf) {
                    Some(0) => break,
                    Some(n) => {
                        write_all_fd(if *is_err { 2 } else { 1 }, &buf[..n]);
                        w.on_output(&buf[..n]);
                    }
                    None => open[i] = false,
                }
            }
        }

        // Stdin forwarding (interactive PTY only).
        if stdin_open
            && status.is_none()
            && let Some(input) = sp.input
        {
            match read_some(0, &mut buf) {
                Some(0) => {}
                Some(n) => write_all_fd(input, &buf[..n]),
                None => stdin_open = false,
            }
        }

        let now = Instant::now();
        if status.is_none() {
            if let Ok(Some(st)) = sp.child.try_wait() {
                status = Some(st);
                // Grandchildren may hold the output open: drain briefly.
                drain_until = Some(now + Duration::from_millis(300));
            } else {
                w.enforce_timeout(now);
                w.tick(now);
            }
        }
        if status.is_some() && (open.iter().all(|o| !o) || drain_until.is_some_and(|d| now >= d)) {
            break;
        }
    }

    let st = status.expect("loop exits only after the child");
    if w.timeout_fired {
        // The whole group goes, grandchildren included.
        let _ = kill(Pid::from_raw(-w.run.pgid), Signal::SIGKILL);
    }
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
    let ev = final_event(&w, outcome);
    opts.sink.emit(&ev);
    outcome
}

fn final_event(w: &Watch<'_>, outcome: Outcome) -> Event {
    let text = w.ring.text();
    let evidence = ring::tail(&text, w.opts.evidence_bytes).to_string();
    let (mut state, mut severity, reason) = match outcome {
        _ if w.timeout_fired => (State::Failing, Severity::Error, "timeout"),
        Outcome::Code(0) => (State::Done, Severity::Info, "exit"),
        Outcome::Code(_) => (State::Failing, Severity::Error, "exit"),
        Outcome::Signal(_) => (State::Failing, Severity::Error, "signal"),
    };
    // Tier 2 at exit: always for a failure (evidence + confidence), and for
    // exit 0 only when the tail trips the weak error panel.
    let mut verdict = None;
    let mut reason = reason;
    if let Some(client) = &w.opts.s1
        && (state != State::Done || detect::looks_failing(&text))
    {
        let tail = ring::tail(&text, client.questions.tail_bytes);
        match client.judge(&w.run.cmd, tail, s1_budget(client)) {
            Ok(v) => {
                if state == State::Done && v.fused >= client.threshold() {
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
