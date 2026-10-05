//! Tier 0 process-state sampler. Finds processes of the watched tree in
//! uninterruptible sleep: Linux `D` (with `/proc/<pid>/wchan`), darwin `U`
//! (via `ps`, which has no wchan).
//!
//! Every sample runs on a worker thread under a wall-clock timeout, because
//! the probe itself can hang on a wedged kernel (darwin `ps` has). A sample
//! that times out reports `Timeout` = "state unknown, maybe wedged", and no
//! new sample starts until the stuck one returns, so threads never pile up.

use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq)]
pub struct ProcState {
    pub pid: i32,
    pub ppid: i32,
    pub pgid: i32,
    pub state: String,
    pub comm: String,
    pub wchan: Option<String>,
}

impl ProcState {
    /// Uninterruptible sleep: Linux `D`, darwin `U`.
    pub fn blocked(&self) -> bool {
        matches!(self.state.chars().next(), Some('D' | 'U'))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Sample {
    Ok(Vec<ProcState>),
    Timeout,
    Error(String),
}

/// The processes of the tree rooted at `root` plus everything in `pgid`.
pub fn select_tree(all: &[ProcState], root: i32, pgid: i32) -> Vec<ProcState> {
    let mut keep: Vec<i32> = vec![root];
    let mut i = 0;
    while i < keep.len() {
        let p = keep[i];
        for c in all.iter().filter(|c| c.ppid == p && c.pid != p) {
            if !keep.contains(&c.pid) {
                keep.push(c.pid);
            }
        }
        i += 1;
    }
    all.iter()
        .filter(|p| keep.contains(&p.pid) || p.pgid == pgid)
        .cloned()
        .collect()
}

/// Parse a Linux `/proc/<pid>/stat` line.
pub fn parse_proc_stat(s: &str) -> Option<ProcState> {
    let open = s.find('(')?;
    let close = s.rfind(')')?;
    let pid = s[..open].trim().parse().ok()?;
    let comm = s[open + 1..close].to_string();
    let mut f = s[close + 1..].split_whitespace();
    let state = f.next()?.to_string();
    let ppid = f.next()?.parse().ok()?;
    let pgid = f.next()?.parse().ok()?;
    Some(ProcState {
        pid,
        ppid,
        pgid,
        state,
        comm,
        wchan: None,
    })
}

/// Parse `ps -A -o pid=,ppid=,pgid=,stat=,comm=` output.
pub fn parse_ps(out: &str) -> Vec<ProcState> {
    out.lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            let pid = f.next()?.parse().ok()?;
            let ppid = f.next()?.parse().ok()?;
            let pgid = f.next()?.parse().ok()?;
            let state = f.next()?.to_string();
            let comm = f.collect::<Vec<_>>().join(" ");
            Some(ProcState {
                pid,
                ppid,
                pgid,
                state,
                comm,
                wchan: None,
            })
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn sample_all() -> Result<Vec<ProcState>, String> {
    let mut out = Vec::new();
    for e in std::fs::read_dir("/proc").map_err(|e| e.to_string())?.flatten() {
        let name = e.file_name();
        let Some(pid) = name.to_str().and_then(|n| n.parse::<i32>().ok()) else {
            continue;
        };
        // Only stat and wchan: neither takes the target's mm lock, unlike
        // cmdline/environ, which can hang on a wedged process.
        if let Ok(s) = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            && let Some(p) = parse_proc_stat(&s)
        {
            out.push(p);
        }
    }
    Ok(out)
}

#[cfg(target_os = "linux")]
fn enrich(ps: &mut [ProcState]) {
    for p in ps.iter_mut().filter(|p| p.blocked()) {
        p.wchan = std::fs::read_to_string(format!("/proc/{}/wchan", p.pid))
            .ok()
            .map(|w| w.trim().to_string())
            .filter(|w| !w.is_empty() && w != "0");
    }
}

#[cfg(not(target_os = "linux"))]
fn sample_all() -> Result<Vec<ProcState>, String> {
    let mut child = std::process::Command::new("ps")
        .args(["-A", "-o", "pid=,ppid=,pgid=,stat=,comm="])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("ps: {e}"))?;
    let mut stdout = child.stdout.take().ok_or("ps: no stdout")?;
    let mut out = String::new();
    std::io::Read::read_to_string(&mut stdout, &mut out).map_err(|e| format!("ps: {e}"))?;
    let _ = child.wait();
    Ok(parse_ps(&out))
}

#[cfg(not(target_os = "linux"))]
fn enrich(_: &mut [ProcState]) {}

/// One blocking sample of the tree (call it via [`Prober`]).
pub fn sample_tree(root: i32, pgid: i32) -> Sample {
    match sample_all() {
        Ok(all) => {
            let mut t = select_tree(&all, root, pgid);
            enrich(&mut t);
            Sample::Ok(t)
        }
        Err(e) => Sample::Error(e),
    }
}

pub struct Prober {
    timeout: Duration,
    /// The running sample; the worker reports how long it took, so a slow
    /// sample is a timeout no matter when the loop gets round to polling.
    inflight: Option<(Receiver<(Sample, Duration)>, Instant)>,
    timed_out: bool,
}

impl Prober {
    pub fn new(timeout: Duration) -> Self {
        Self {
            timeout,
            inflight: None,
            timed_out: false,
        }
    }

    /// Start a sample unless one is already running (or stuck).
    pub fn start(&mut self, f: impl FnOnce() -> Sample + Send + 'static) {
        if self.inflight.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let spawned = std::thread::Builder::new().name("probe".into()).spawn(move || {
            let t0 = Instant::now();
            let s = f();
            let _ = tx.send((s, t0.elapsed()));
        });
        if spawned.is_ok() {
            self.inflight = Some((rx, Instant::now()));
            self.timed_out = false;
        }
    }

    /// Non-blocking. `Some(Timeout)` once per stuck sample; the stuck worker
    /// keeps the slot until it returns.
    pub fn poll(&mut self) -> Option<Sample> {
        let (rx, started) = self.inflight.as_ref()?;
        match rx.try_recv() {
            Ok((s, took)) => {
                self.inflight = None;
                if self.timed_out {
                    None
                } else if took > self.timeout {
                    Some(Sample::Timeout)
                } else {
                    Some(s)
                }
            }
            Err(TryRecvError::Empty) if !self.timed_out && started.elapsed() >= self.timeout => {
                self.timed_out = true;
                Some(Sample::Timeout)
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                self.inflight = None;
                Some(Sample::Error("probe thread died".into()))
            }
        }
    }

    /// Block (up to the timeout) for a fresh sample.
    pub fn sample_now(&mut self, f: impl FnOnce() -> Sample + Send + 'static) -> Sample {
        self.start(f);
        loop {
            if let Some(s) = self.poll() {
                return s;
            }
            if self.timed_out {
                return Sample::Timeout;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stat_with_odd_comm() {
        let p = parse_proc_stat("1234 (we ird) (x)) D 1 1234 1234 0 -1 4194560").unwrap();
        assert_eq!(
            (p.pid, p.comm.as_str(), p.state.as_str(), p.ppid, p.pgid),
            (1234, "we ird) (x)", "D", 1, 1234)
        );
        assert!(p.blocked());
    }

    #[test]
    fn parses_ps() {
        let v = parse_ps("  10     1    10 Ss   bash\n  11    10    10 U+   nix build\n");
        assert_eq!(v.len(), 2);
        assert!(v[1].blocked());
        assert_eq!(v[1].comm, "nix build");
    }

    #[test]
    fn selects_descendants_and_group() {
        let mk = |pid, ppid, pgid| ProcState {
            pid,
            ppid,
            pgid,
            state: "S".into(),
            comm: "x".into(),
            wchan: None,
        };
        let all = vec![
            mk(1, 0, 1),
            mk(10, 1, 10),
            mk(11, 10, 10),
            mk(12, 11, 99),
            mk(13, 1, 10),
            mk(14, 1, 14),
        ];
        let mut got: Vec<i32> = select_tree(&all, 10, 10).iter().map(|p| p.pid).collect();
        got.sort();
        assert_eq!(got, [10, 11, 12, 13]);
    }

    #[test]
    fn a_hung_probe_times_out_once_and_blocks_new_ones() {
        let mut p = Prober::new(Duration::from_millis(50));
        let s = p.sample_now(|| {
            std::thread::sleep(Duration::from_millis(400));
            Sample::Ok(vec![])
        });
        assert_eq!(s, Sample::Timeout);
        // The stuck worker still holds the slot: a new start is a no-op.
        p.start(|| Sample::Error("must not run".into()));
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(p.poll(), None, "the late result of a timed-out sample is dropped");
        assert_eq!(p.sample_now(|| Sample::Ok(vec![])), Sample::Ok(vec![]));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn samples_our_own_tree() {
        let me = std::process::id() as i32;
        let pg = nix::unistd::getpgrp().as_raw();
        match sample_tree(me, pg) {
            Sample::Ok(v) => assert!(v.iter().any(|p| p.pid == me)),
            other => panic!("{other:?}"),
        }
    }
}
