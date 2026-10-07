//! Event transport off the supervisor loop. A sink write can block for as
//! long as a reader does not read (a full `--events-fd` pipe, a full stderr
//! pipe), and the loop that enforces `--timeout`, forwards signals and ticks
//! heartbeats must never wait on that. Every event therefore goes through a
//! bounded channel to one writer thread, which keeps per-sink order.
//!
//! Overload policy (the loop never blocks on it):
//!  - heartbeats are informational: when the channel is full the heartbeat is
//!    dropped and counted, and the next one that gets through carries the
//!    count as `heartbeats_dropped`;
//!  - other events wait in a bounded overflow queue that is retried on every
//!    loop iteration; beyond its bound the oldest is dropped (with a stderr
//!    diagnostic unless quiet);
//!  - the final event is sent last, blocking, then the writer is joined: like
//!    any process writing to a full pipe, the watcher may block at exit on a
//!    sink nobody reads.

use crate::event::{Event, Sink};
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::thread::JoinHandle;

/// Events the writer queue holds before the overload policy applies.
pub const CHANNEL_CAP: usize = 256;
/// Non-heartbeat events kept in-loop while the channel is full.
pub const OVERFLOW_CAP: usize = 1024;

pub struct Outbox {
    tx: Option<SyncSender<Event>>,
    join: Option<JoinHandle<()>>,
    overflow: VecDeque<Event>,
    overflow_cap: usize,
    dropped_heartbeats: u64,
    /// Whether this overflow episode already printed its diagnostic.
    warned: bool,
    quiet: bool,
}

impl Outbox {
    /// A writer thread delivering to `sink`.
    pub fn start(sink: Arc<Sink>, quiet: bool) -> Self {
        Self::with_deliver(move |ev| sink.emit(&ev), CHANNEL_CAP, OVERFLOW_CAP, quiet)
    }

    pub fn with_deliver(
        mut deliver: impl FnMut(Event) + Send + 'static,
        channel_cap: usize,
        overflow_cap: usize,
        quiet: bool,
    ) -> Self {
        let (tx, rx) = mpsc::sync_channel::<Event>(channel_cap);
        let join = std::thread::spawn(move || rx.into_iter().for_each(&mut deliver));
        Self {
            tx: Some(tx),
            join: Some(join),
            overflow: VecDeque::new(),
            overflow_cap,
            dropped_heartbeats: 0,
            warned: false,
            quiet,
        }
    }

    /// Retry the overflow queue; called on every loop iteration. Never blocks.
    pub fn pump(&mut self) {
        let Some(tx) = &self.tx else { return };
        while let Some(ev) = self.overflow.pop_front() {
            match tx.try_send(ev) {
                Ok(()) => {}
                Err(TrySendError::Full(ev)) => {
                    self.overflow.push_front(ev);
                    return;
                }
                Err(TrySendError::Disconnected(_)) => {
                    self.overflow.clear();
                    return;
                }
            }
        }
        self.warned = false;
    }

    /// Queue a non-final, non-heartbeat event. Never blocks.
    pub fn send(&mut self, ev: Event) {
        self.pump();
        if !self.overflow.is_empty() {
            return self.park(ev);
        }
        if let Some(tx) = &self.tx
            && let Err(TrySendError::Full(ev)) = tx.try_send(ev)
        {
            self.park(ev);
        }
    }

    /// Queue a heartbeat, or drop and count it when the sink cannot keep up.
    /// Never blocks.
    pub fn send_heartbeat(&mut self, mut ev: Event) {
        self.pump();
        let behind = self.dropped_heartbeats;
        if let Some(hb) = &mut ev.heartbeat {
            hb.heartbeats_dropped = (behind > 0).then_some(behind);
        }
        // Behind queued overflow means behind in order too: drop, don't jump.
        let sent = self.overflow.is_empty()
            && match &self.tx {
                Some(tx) => match tx.try_send(ev) {
                    Ok(()) => true,
                    Err(TrySendError::Full(_)) => false,
                    Err(TrySendError::Disconnected(_)) => return,
                },
                None => return,
            };
        self.dropped_heartbeats = if sent { 0 } else { behind + 1 };
    }

    fn park(&mut self, ev: Event) {
        self.overflow.push_back(ev);
        while self.overflow.len() > self.overflow_cap {
            self.overflow.pop_front();
            if !self.warned && !self.quiet {
                self.warned = true;
                let _ = writeln!(
                    std::io::stderr(),
                    "watcher-s1: the event sink is not keeping up; dropping the oldest queued events"
                );
            }
        }
    }

    /// Deliver everything still queued, then `last` (if any), then stop the
    /// writer. Blocks for as long as the sink does not accept the data.
    pub fn finish(&mut self, last: Option<Event>) {
        if let Some(tx) = self.tx.take() {
            for ev in self.overflow.drain(..).chain(last) {
                if tx.send(ev).is_err() {
                    break;
                }
            }
        }
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }

    #[cfg(test)]
    fn overflow_len(&self) -> usize {
        self.overflow.len()
    }
}

use std::io::Write as _;

impl Drop for Outbox {
    fn drop(&mut self) {
        self.finish(None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Heartbeat, RunInfo, Severity, State};
    use std::sync::{Mutex, mpsc as std_mpsc};

    fn run() -> RunInfo {
        RunInfo {
            host: "h".into(),
            run_id: "h:1:1".into(),
            cmd: "x".into(),
            pid: 1,
            pgid: 1,
            caused_by: None,
        }
    }

    fn ev(reason: &str) -> Event {
        run().event(State::Stalled, Severity::Warn, reason, String::new())
    }

    fn hb(n: u64) -> Event {
        run().heartbeat(
            State::Progressing,
            String::new(),
            Heartbeat {
                elapsed_ms: n,
                bytes_since_last: 0,
                lines_since_last: 0,
                last_line: None,
                heartbeats_dropped: None,
            },
        )
    }

    /// A box whose writer blocks until the returned gate is opened.
    #[allow(clippy::type_complexity)]
    fn gated(cap: usize, overflow: usize) -> (Outbox, std_mpsc::Sender<()>, Arc<Mutex<Vec<Event>>>) {
        let (gate_tx, gate_rx) = std_mpsc::channel::<()>();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s = seen.clone();
        let mut first = true;
        let b = Outbox::with_deliver(
            move |e| {
                if first {
                    first = false;
                    let _ = gate_rx.recv(); // stalled reader, until released
                }
                s.lock().unwrap().push(e);
            },
            cap,
            overflow,
            true,
        );
        (b, gate_tx, seen)
    }

    fn reasons(seen: &Arc<Mutex<Vec<Event>>>) -> Vec<String> {
        seen.lock().unwrap().iter().map(|e| e.reason.clone()).collect()
    }

    #[test]
    fn a_stalled_sink_never_blocks_the_sender() {
        let (mut b, gate, seen) = gated(2, 3);
        let t = std::time::Instant::now();
        for i in 0..50 {
            b.send_heartbeat(hb(i));
            b.send(ev("silence"));
        }
        assert!(t.elapsed() < std::time::Duration::from_secs(1));
        assert!(b.overflow_len() <= 3, "bounded");
        gate.send(()).unwrap();
        b.finish(Some(ev("exit")));
        let r = reasons(&seen);
        assert_eq!(r.last().map(String::as_str), Some("exit"), "final event last");
    }

    #[test]
    fn dropped_heartbeats_are_counted_on_the_next_one_through() {
        let (mut b, gate, seen) = gated(1, 8);
        // The writer holds #0; #1 fills the channel; the rest are dropped.
        b.send_heartbeat(hb(0));
        std::thread::sleep(std::time::Duration::from_millis(100));
        for i in 1..=5 {
            b.send_heartbeat(hb(i));
        }
        gate.send(()).unwrap();
        // Let the writer drain, then one more gets through with the count.
        std::thread::sleep(std::time::Duration::from_millis(100));
        b.send_heartbeat(hb(99));
        b.finish(None);
        let v = seen.lock().unwrap();
        let counts: Vec<Option<u64>> = v
            .iter()
            .map(|e| e.heartbeat.as_ref().unwrap().heartbeats_dropped)
            .collect();
        // #0 and #1 delivered clean, 2..=5 dropped (4), then the count rides on 99.
        assert_eq!(v.len(), 3, "{counts:?}");
        assert_eq!(counts, [None, None, Some(4)]);
        assert_eq!(v[2].heartbeat.as_ref().unwrap().elapsed_ms, 99);
    }

    #[test]
    fn other_events_keep_order_through_the_overflow_queue() {
        let (mut b, gate, seen) = gated(1, 100);
        for i in 0..10 {
            b.send(ev(&format!("e{i}")));
        }
        gate.send(()).unwrap();
        b.finish(Some(ev("exit")));
        let want: Vec<String> = (0..10).map(|i| format!("e{i}")).chain(["exit".into()]).collect();
        assert_eq!(reasons(&seen), want);
    }

    #[test]
    fn a_heartbeat_never_jumps_the_overflow_queue() {
        let (mut b, gate, seen) = gated(1, 100);
        b.send(ev("a")); // writer takes it and stalls
        std::thread::sleep(std::time::Duration::from_millis(100));
        b.send(ev("b")); // channel
        b.send(ev("c")); // overflow
        gate.send(()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(100));
        // "c" is still parked (or only just moved on): the heartbeat is
        // dropped or delivered after it, never ahead of it.
        b.send_heartbeat(hb(1));
        b.finish(None);
        let r = reasons(&seen);
        assert_eq!(r[..3], ["a", "b", "c"], "{r:?}");
        assert!(r.len() <= 4);
    }

    #[test]
    fn overflow_drops_the_oldest_beyond_its_bound() {
        let (mut b, gate, seen) = gated(1, 3);
        b.send(ev("e0"));
        std::thread::sleep(std::time::Duration::from_millis(100));
        for i in 1..=8 {
            b.send(ev(&format!("e{i}")));
        }
        assert_eq!(b.overflow_len(), 3);
        gate.send(()).unwrap();
        b.finish(Some(ev("exit")));
        // e0 (in the writer), e1 (channel), the newest three, then the final.
        assert_eq!(reasons(&seen), ["e0", "e1", "e6", "e7", "e8", "exit"]);
    }
}
