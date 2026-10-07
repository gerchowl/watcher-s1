//! The one stderr writer. A write to a stderr whose reader does not read
//! blocks, and the supervisor loop (hard timeout, signal forwarding, ticks)
//! must never wait on that. Everything the watcher itself prints to stderr
//! therefore goes through ONE writer thread behind a bounded queue, which
//! also serialises the two producers that share the stream:
//!
//!  - diagnostics (`watcher-s1 (log): ...`): [`log`] never blocks; when the
//!    queue is full the line is dropped and counted, and the next line that
//!    gets through is preceded by a note with the count;
//!  - events on the default stderr sink: they arrive from the outbox writer
//!    thread (never the loop) via [`line()`](fn@line), which may block for room. Events
//!    are not dropped here: the outbox in front of it applies its own policy.
//!
//! [`drain`] waits until everything queued so far was written. It runs once,
//! at exit, and may block on a stalled reader, like the final flush of the
//! event outbox (documented there).

use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, OnceLock};

/// Lines the writer queue holds before diagnostics are dropped.
pub const QUEUE_CAP: usize = 256;

enum Msg {
    Line(String),
    Barrier(mpsc::Sender<()>),
}

/// A bounded queue in front of a writer thread owning `out`.
pub struct Diag {
    tx: SyncSender<Msg>,
    dropped: Arc<AtomicU64>,
}

impl Diag {
    pub fn start(mut out: impl Write + Send + 'static, cap: usize) -> Self {
        let (tx, rx) = mpsc::sync_channel::<Msg>(cap);
        let dropped = Arc::new(AtomicU64::new(0));
        let d = dropped.clone();
        std::thread::spawn(move || {
            for msg in rx {
                match msg {
                    Msg::Line(l) => {
                        let n = d.swap(0, Ordering::Relaxed);
                        if n > 0 {
                            let _ = writeln!(
                                out,
                                "watcher-s1 (log): {n} diagnostic line(s) dropped: stderr is not keeping up"
                            );
                        }
                        let _ = out.write_all(l.as_bytes());
                        let _ = out.flush();
                    }
                    Msg::Barrier(ack) => {
                        let _ = ack.send(());
                    }
                }
            }
        });
        Diag { tx, dropped }
    }

    /// Queue a line without ever blocking; a full queue drops and counts it.
    pub fn try_line(&self, line: String) {
        if let Err(TrySendError::Full(_)) = self.tx.try_send(Msg::Line(line)) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Queue a line, waiting for room. Never call this from the loop.
    pub fn line(&self, line: String) {
        let _ = self.tx.send(Msg::Line(line));
    }

    /// Block until everything queued before this call was written.
    pub fn drain(&self) {
        let (ack_tx, ack_rx) = mpsc::channel();
        if self.tx.send(Msg::Barrier(ack_tx)).is_ok() {
            let _ = ack_rx.recv();
        }
    }
}

static STDERR: OnceLock<Diag> = OnceLock::new();

fn stderr() -> &'static Diag {
    STDERR.get_or_init(|| Diag::start(std::io::stderr(), QUEUE_CAP))
}

/// A supervisor diagnostic. Never blocks; silent when `quiet`.
pub fn log(quiet: bool, msg: &str) {
    if !quiet {
        stderr().try_line(format!("watcher-s1 (log): {msg}\n"));
    }
}

/// A diagnostic that is not subject to `--quiet` (a failure to run at all).
pub fn error(msg: &str) {
    stderr().try_line(format!("watcher-s1 (error): {msg}\n"));
}

/// One already-terminated line for the shared stderr stream, in order with
/// the diagnostics. May block for room: for writer threads only.
pub fn line(text: String) {
    stderr().line(text);
}

/// Wait until everything queued for stderr was written (exit path only).
pub fn drain() {
    if let Some(d) = STDERR.get() {
        d.drain();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::mpsc as m;
    use std::time::{Duration, Instant};

    /// A writer that blocks on its first write until the gate is released.
    struct Gated {
        gate: Option<m::Receiver<()>>,
        got: Arc<Mutex<Vec<u8>>>,
    }
    impl Write for Gated {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            if let Some(g) = self.gate.take() {
                let _ = g.recv();
            }
            self.got.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn text(got: &Arc<Mutex<Vec<u8>>>) -> String {
        String::from_utf8(got.lock().unwrap().clone()).unwrap()
    }

    #[test]
    fn a_stalled_stderr_never_blocks_a_diagnostic_and_overflow_is_counted() {
        let (gate_tx, gate_rx) = m::channel();
        let got = Arc::new(Mutex::new(Vec::new()));
        let d = Diag::start(
            Gated {
                gate: Some(gate_rx),
                got: got.clone(),
            },
            2,
        );
        let t = Instant::now();
        for i in 0..50 {
            d.try_line(format!("l{i}\n"));
        }
        assert!(t.elapsed() < Duration::from_secs(1), "try_line must not block");
        gate_tx.send(()).unwrap();
        d.try_line("last\n".into()); // may or may not fit; drain decides
        d.drain();
        let s = text(&got);
        assert!(s.contains("l0\n"), "{s}");
        assert!(s.contains("dropped"), "the drop is reported: {s}");
    }

    #[test]
    fn events_and_diagnostics_are_serialised_in_order() {
        let got = Arc::new(Mutex::new(Vec::new()));
        let d = Diag::start(
            Gated {
                gate: None,
                got: got.clone(),
            },
            8,
        );
        d.line("event 1\n".into());
        d.try_line("diag\n".into());
        d.line("event 2\n".into());
        d.drain();
        assert_eq!(text(&got), "event 1\ndiag\nevent 2\n");
    }
}
