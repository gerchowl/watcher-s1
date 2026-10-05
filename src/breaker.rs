//! Per-endpoint circuit breaker, persisted so it holds across processes.
//!
//! A watcher calls System One a handful of times and the PostToolUse judge
//! runs once per Bash call, so an in-memory breaker would never trip. The
//! state lives in `$XDG_STATE_HOME/watcher-s1/breaker.json` (default
//! `~/.local/state/...`; `WATCHER_S1_STATE_DIR` overrides). It is
//! best-effort: an unreadable or unwritable file degrades to "closed".
//!
//! Closed: count consecutive failures; at `fails` the breaker opens for
//! `cooldown`. After the cooldown exactly one caller, across all processes,
//! claims the trial call (half-open): claiming re-arms the cooldown, so
//! everyone else keeps skipping; a success closes the breaker, a failure
//! reopens it. Read-modify-write runs under an `flock` on `breaker.lock`.

use crate::config::BreakerConfig;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct Entry {
    fails: u32,
    /// Unix seconds until which the endpoint is skipped (0 = closed).
    open_until: f64,
}

pub struct Breaker {
    path: Option<PathBuf>,
    cfg: BreakerConfig,
    now: Box<dyn Fn() -> f64 + Send + Sync>,
}

fn unix_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

pub fn default_state_dir() -> Option<PathBuf> {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    if let Some(d) = env("WATCHER_S1_STATE_DIR") {
        return Some(PathBuf::from(d));
    }
    env("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| env("HOME").map(|h| Path::new(&h).join(".local/state")))
        .map(|d| d.join("watcher-s1"))
}

impl Breaker {
    pub fn new(state_dir: Option<PathBuf>, cfg: BreakerConfig) -> Self {
        Self {
            path: state_dir.map(|d| d.join("breaker.json")),
            cfg,
            now: Box::new(unix_now),
        }
    }

    #[cfg(test)]
    fn with_clock(state_dir: &Path, cfg: BreakerConfig, now: impl Fn() -> f64 + Send + Sync + 'static) -> Self {
        Self {
            path: Some(state_dir.join("breaker.json")),
            cfg,
            now: Box::new(now),
        }
    }

    fn load(&self) -> BTreeMap<String, Entry> {
        self.path
            .as_ref()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    /// Run `f` on the state under an exclusive lock (best effort: if the
    /// lock cannot be had within ~200 ms, run unlocked rather than stall).
    fn locked<T>(&self, f: impl FnOnce(&mut BTreeMap<String, Entry>) -> (T, bool)) -> T {
        let lock = self.path.as_ref().and_then(|p| {
            let dir = p.parent()?;
            std::fs::create_dir_all(dir).ok()?;
            let file = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(false)
                .open(dir.join("breaker.lock"))
                .ok()?;
            use std::os::fd::AsRawFd;
            for _ in 0..20 {
                if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                    return Some(file); // released when dropped (closed)
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            None
        });
        let mut m = self.load();
        let (out, dirty) = f(&mut m);
        if dirty {
            self.store(&m);
        }
        drop(lock);
        out
    }

    fn store(&self, m: &BTreeMap<String, Entry>) {
        let Some(p) = &self.path else { return };
        let Some(dir) = p.parent() else { return };
        if std::fs::create_dir_all(dir).is_err() {
            return;
        }
        let tmp = dir.join(format!(".breaker.{}.tmp", std::process::id()));
        if let Ok(b) = serde_json::to_vec(m)
            && std::fs::write(&tmp, b).is_ok()
            && std::fs::rename(&tmp, p).is_err()
        {
            let _ = std::fs::remove_file(&tmp);
        }
    }

    /// May we call `url` now? Closed: yes. Open: no, unless the cooldown is
    /// over and we are the one caller that claims the half-open trial.
    pub fn acquire(&self, url: &str) -> bool {
        let now = (self.now)();
        let cooldown = self.cfg.cooldown.as_secs_f64();
        let fails = self.cfg.fails;
        self.locked(|m| match m.get_mut(url) {
            None => (true, false),
            Some(e) if e.fails < fails => (true, false),
            Some(e) if e.open_until > now => (false, false),
            Some(e) => {
                e.open_until = now + cooldown; // claim the trial
                (true, true)
            }
        })
    }

    pub fn record(&self, url: &str, ok: bool) {
        let now = (self.now)();
        let cooldown = self.cfg.cooldown.as_secs_f64();
        let fails = self.cfg.fails;
        self.locked(|m| {
            if ok {
                return ((), m.remove(url).is_some());
            }
            let e = m.entry(url.to_string()).or_default();
            e.fails = e.fails.saturating_add(1);
            if e.fails >= fails {
                e.open_until = now + cooldown;
            }
            ((), true)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    #[test]
    fn opens_after_n_failures_and_half_opens_after_cooldown() {
        let t = tempfile::tempdir().unwrap();
        let clock = Arc::new(AtomicU64::new(1000));
        let c = clock.clone();
        let cfg = BreakerConfig {
            fails: 3,
            cooldown: Duration::from_secs(600),
        };
        let b = Breaker::with_clock(t.path(), cfg.clone(), move || c.load(Ordering::SeqCst) as f64);
        let u = "http://a/x";
        for _ in 0..2 {
            b.record(u, false);
            assert!(b.acquire(u));
        }
        b.record(u, false);
        assert!(!b.acquire(u), "open after 3 consecutive failures");
        assert!(b.acquire("http://other/x"), "per endpoint");

        // A second process sees the same state.
        let c2 = clock.clone();
        let b2 = Breaker::with_clock(t.path(), cfg, move || c2.load(Ordering::SeqCst) as f64);
        assert!(!b2.acquire(u));

        clock.store(1601, Ordering::SeqCst);
        assert!(b.acquire(u), "half-open after cooldown: one trial call");
        assert!(!b2.acquire(u), "only one caller gets the trial");
        b.record(u, false);
        assert!(!b.acquire(u), "a half-open failure reopens");
        clock.store(2300, Ordering::SeqCst);
        assert!(b.acquire(u));
        b.record(u, true);
        assert!(b2.acquire(u), "success closed it for everyone");
        b.record(u, false);
        assert!(b.acquire(u), "success reset the count");
    }

    #[test]
    fn unwritable_state_degrades_to_closed() {
        let b = Breaker::new(
            Some(PathBuf::from("/proc/definitely/not/writable")),
            BreakerConfig::default(),
        );
        for _ in 0..5 {
            b.record("http://a/x", false);
        }
        assert!(b.acquire("http://a/x"));
    }
}
