//! `watcher-s1 follow FILE`: tail an events file, one compact line per
//! event, and return when the run it locked onto finishes.
//!
//! The file may be shared with nested watchers and reused across runs, so the
//! first `run_id` seen is the run; only its final event (non-null `exit`)
//! ends the follow.
//!
//! Limits, by design: only regular files are followed (a FIFO or device is
//! refused, never opened blocking); the `--timeout` deadline is checked
//! between events, but a write to a stdout whose reader has stopped can block
//! until the reader resumes, and the deadline cannot fire meanwhile.

use serde::Deserialize;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::{Duration, Instant};

const POLL: Duration = Duration::from_millis(200);
/// Characters of the evidence tail shown on a line.
const TAIL_CHARS: usize = 80;
/// Bytes read per batch; the deadline is checked between events inside it.
const CHUNK: usize = 64 * 1024;
/// Rotated-away files still polled for late writes by their producer.
const MAX_OLD_FILES: usize = 16;

/// The fields `follow` acts on, validated before any state changes. Unknown
/// fields are accepted.
#[derive(Deserialize)]
struct Identity {
    run_id: String,
    #[serde(default)]
    caused_by: Option<String>,
    /// null/absent while the run is live; an object once final.
    #[serde(default)]
    exit: Option<Map<String, Value>>,
}

impl Identity {
    fn project(ev: &Value) -> Result<Self, String> {
        let id = Self::deserialize(ev).map_err(|e| e.to_string())?;
        if id.run_id.is_empty() {
            return Err("empty run_id".into());
        }
        if let Some(exit) = &id.exit {
            for k in ["code", "signal"] {
                if !exit.get(k).is_none_or(|v| v.is_null() || v.is_i64()) {
                    return Err(format!("exit.{k} is not an integer"));
                }
            }
        }
        Ok(id)
    }
}

/// Per-file line assembly: bytes of an unfinished line.
#[derive(Default)]
pub struct LineBuf {
    /// Bytes after the last newline, held until the line completes.
    partial: Vec<u8>,
    /// Drop bytes up to and including the next newline (a line we joined
    /// mid-way).
    skipping: bool,
}

impl LineBuf {
    /// The file restarted: a half-read line from the old content is void.
    pub fn reset(&mut self) {
        self.partial.clear();
        self.skipping = false;
    }

    /// Discard through the next newline before parsing anything.
    pub fn skip_partial_line(&mut self) {
        self.partial.clear();
        self.skipping = true;
    }
}

/// What feeding a chunk led to.
#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    More,
    /// The locked run's final event has been printed.
    Done,
    /// The deadline passed before the next event; unread events stay buffered.
    Late,
}

/// Run state shared by every file being followed.
#[derive(Default)]
pub struct Follower {
    /// The run being followed: the first valid `run_id` seen.
    locked: Option<String>,
    /// run_id -> caused_by, for indenting nested runs.
    parents: HashMap<String, Option<String>>,
}

/// One valid event, as `Follower` hands it to a consumer.
pub struct Seen<'a> {
    pub event: &'a Value,
    /// Belongs to the run this follower locked onto (the first valid `run_id`).
    pub ours: bool,
    /// Ours, and its `exit` is set: the run is over.
    pub is_final: bool,
    /// Length of the `caused_by` chain above the event's run.
    pub depth: usize,
}

impl Follower {
    /// Consume a chunk, writing one compact line per complete event to `out`.
    /// The deadline is checked before each event.
    pub fn feed(
        &mut self,
        lb: &mut LineBuf,
        chunk: &[u8],
        deadline: Option<Instant>,
        out: &mut impl Write,
    ) -> io::Result<Step> {
        self.feed_with(lb, chunk, deadline, &mut |s| {
            writeln!(out, "{}{}", "  ".repeat(s.depth), format_event(s.event))?;
            out.flush()
        })
    }

    /// Consume a chunk, calling `on_event` for every complete valid event
    /// (malformed lines are skipped with a note on stderr). Stops after the
    /// locked run's final event (`Step::Done`) or at the deadline
    /// (`Step::Late`); the rest of the chunk stays unread.
    pub fn feed_with(
        &mut self,
        lb: &mut LineBuf,
        mut chunk: &[u8],
        deadline: Option<Instant>,
        on_event: &mut dyn FnMut(Seen<'_>) -> io::Result<()>,
    ) -> io::Result<Step> {
        if lb.skipping {
            match chunk.iter().position(|&b| b == b'\n') {
                Some(i) => {
                    chunk = &chunk[i + 1..];
                    lb.skipping = false;
                }
                None => return Ok(Step::More),
            }
        }
        lb.partial.extend_from_slice(chunk);
        while let Some(nl) = lb.partial.iter().position(|&b| b == b'\n') {
            if deadline.is_some_and(|d| Instant::now() >= d) {
                return Ok(Step::Late);
            }
            let raw: Vec<u8> = lb.partial.drain(..=nl).collect();
            let line = String::from_utf8_lossy(&raw[..nl]);
            if line.trim().is_empty() {
                continue;
            }
            let parsed = serde_json::from_str::<Value>(&line)
                .map_err(|e| e.to_string())
                .and_then(|ev| Identity::project(&ev).map(|id| (ev, id)));
            match parsed {
                Ok((ev, id)) => {
                    if self.event(&ev, id, on_event)? {
                        return Ok(Step::Done);
                    }
                }
                Err(why) => eprintln!(
                    "watcher-s1 follow: skipping a malformed line ({why}): {}",
                    clip(&line, TAIL_CHARS)
                ),
            }
        }
        Ok(Step::More)
    }

    fn event(
        &mut self,
        ev: &Value,
        id: Identity,
        on_event: &mut dyn FnMut(Seen<'_>) -> io::Result<()>,
    ) -> io::Result<bool> {
        self.parents.entry(id.run_id.clone()).or_insert(id.caused_by);
        let locked = self.locked.get_or_insert_with(|| id.run_id.clone());
        let ours = *locked == id.run_id;
        let is_final = ours && id.exit.is_some();
        on_event(Seen {
            event: ev,
            ours,
            is_final,
            depth: self.depth(&id.run_id),
        })?;
        Ok(is_final)
    }

    /// Length of the `caused_by` chain above `run_id` (cycle-safe).
    fn depth(&self, run_id: &str) -> usize {
        let mut depth = 0;
        let mut cur = run_id;
        while let Some(Some(parent)) = self.parents.get(cur) {
            depth += 1;
            if depth > self.parents.len() {
                break;
            }
            cur = parent;
        }
        depth
    }
}

fn str_of<'a>(ev: &'a Value, key: &str) -> &'a str {
    ev[key].as_str().unwrap_or("-")
}

fn clip(s: &str, max: usize) -> String {
    let mut it = s.chars();
    let head: String = it.by_ref().take(max).collect();
    if it.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

/// `elapsed=12.3s last_line` for a heartbeat; its own `last_line` stands in
/// for the evidence tail.
fn heartbeat_tail(ev: &Value) -> String {
    let elapsed = ev["elapsed_ms"]
        .as_u64()
        .map_or(String::new(), |ms| format!("elapsed={:.1}s", ms as f64 / 1000.0));
    let last = ev["last_line"]
        .as_str()
        .map_or(String::new(), |l| clip(l.trim(), TAIL_CHARS));
    format!("{elapsed} {last}").trim().to_owned()
}

/// `state reason severity exit s1 tail`
pub fn format_event(ev: &Value) -> String {
    let exit = match (ev["exit"]["code"].as_i64(), ev["exit"]["signal"].as_i64()) {
        (Some(c), _) => format!("exit={c}"),
        (None, Some(s)) => format!("signal={s}"),
        _ => "-".into(),
    };
    let s1 = ev["s1"]["fused"].as_f64().map_or("-".into(), |f| format!("s1={f:.2}"));
    let tail = if str_of(ev, "reason") == "heartbeat" {
        heartbeat_tail(ev)
    } else {
        ev["evidence_tail"]
            .as_str()
            .and_then(|t| t.lines().rev().find(|l| !l.trim().is_empty()))
            .map_or(String::new(), |l| clip(l.trim(), TAIL_CHARS))
    };
    format!(
        "{} {} {} {exit} {s1} {tail}",
        str_of(ev, "state"),
        str_of(ev, "reason"),
        str_of(ev, "severity")
    )
    .trim_end()
    .to_owned()
}

/// How a follow ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The locked run's final event arrived.
    Done,
    /// `timeout` elapsed first.
    TimedOut,
}

/// Bytes of the file head remembered to notice a truncate-and-regrow.
const HEAD_LEN: usize = 64;

/// Open `path` for following without ever blocking: `O_NONBLOCK` keeps a FIFO
/// open from waiting for a writer, and anything but a regular file is
/// refused (`InvalidInput`).
fn open_regular(path: &Path) -> io::Result<File> {
    let f = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)?;
    if !f.metadata()?.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a regular file (a FIFO, device or directory cannot be followed)",
        ));
    }
    Ok(f)
}

/// Read exactly `buf.len()` bytes at `off`; `None` if the file is shorter
/// (it was truncated under us).
fn read_exact_or_short(f: &File, buf: &mut [u8], off: u64) -> io::Result<Option<()>> {
    match f.read_exact_at(buf, off) {
        Ok(()) => Ok(Some(())),
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(None),
        Err(e) => Err(e),
    }
}

/// One open file plus what is needed to notice it was reset in place.
struct Tail {
    f: File,
    pos: u64,
    /// The first bytes of the file (up to `HEAD_LEN`) as read. A file
    /// truncated and regrown past `pos` between two polls keeps its inode and
    /// is longer than `pos`, but its head differs, which this catches. It is
    /// a heuristic: a file rewritten in place with identical first
    /// `HEAD_LEN` bytes (or grown without ever dropping below `pos`) is not
    /// noticed, so reusing a file in place is only reliable when the new
    /// content starts differently (events carry a fresh `run_id` and `ts`) or
    /// the file is shorter at some poll. Use a fresh file per run.
    head: Vec<u8>,
    lines: LineBuf,
}

/// Result of polling one file once.
enum Poll {
    Idle,
    Read,
    Done,
    Late,
}

impl Tail {
    /// Start at `pos`; with a nonzero `pos` the cached head is filled from the
    /// file now, not from later reads.
    fn open(f: File, pos: u64) -> io::Result<Self> {
        let mut t = Tail {
            f,
            pos,
            head: Vec::new(),
            lines: LineBuf::default(),
        };
        if !t.sync_head()? {
            t.restart();
        }
        Ok(t)
    }

    /// The file went back to its start: forget everything about the old one.
    fn restart(&mut self) {
        self.pos = 0;
        self.head.clear();
        self.lines.reset();
    }

    /// Extend the cached head up to `min(pos, HEAD_LEN)`, independent of
    /// which chunk was just read. False if the file is shorter than that
    /// (truncated concurrently).
    fn sync_head(&mut self) -> io::Result<bool> {
        let want = (self.pos as usize).min(HEAD_LEN);
        let have = self.head.len();
        if have >= want {
            return Ok(true);
        }
        let mut more = vec![0u8; want - have];
        if read_exact_or_short(&self.f, &mut more, have as u64)?.is_none() {
            return Ok(false);
        }
        self.head.extend_from_slice(&more);
        Ok(true)
    }

    /// True if the file shrank or its head changed: it was reused in place.
    fn was_reset(&mut self) -> io::Result<bool> {
        if self.f.metadata()?.len() < self.pos {
            return Ok(true);
        }
        let n = self.head.len().min(self.pos as usize);
        if n > 0 {
            let mut now = vec![0u8; n];
            // Shorter than the head again: truncated since the length check.
            return Ok(read_exact_or_short(&self.f, &mut now, 0)?.is_none() || now != self.head[..n]);
        }
        Ok(false)
    }

    /// Read and process one chunk.
    fn poll(
        &mut self,
        buf: &mut [u8],
        follower: &mut Follower,
        deadline: Option<Instant>,
        on_event: &mut dyn FnMut(Seen<'_>) -> io::Result<()>,
    ) -> io::Result<Poll> {
        if self.was_reset()? {
            self.restart();
        }
        let n = self.f.read_at(buf, self.pos)?;
        if n == 0 {
            return Ok(Poll::Idle);
        }
        self.pos += n as u64;
        if !self.sync_head()? {
            // Truncated while we read: discard the chunk and start over.
            self.restart();
            return Ok(Poll::Read);
        }
        Ok(
            match follower.feed_with(&mut self.lines, &buf[..n], deadline, on_event)? {
                Step::More => Poll::Read,
                Step::Done => Poll::Done,
                Step::Late => Poll::Late,
            },
        )
    }

    /// Did the path now name a different file than the one we hold open?
    fn rotated(&self, path: &Path) -> io::Result<bool> {
        match std::fs::metadata(path) {
            Ok(m) => {
                let mine = self.f.metadata()?;
                Ok((m.dev(), m.ino()) != (mine.dev(), mine.ino()))
            }
            // Briefly absent mid-rotation: keep the old handle until it returns.
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }
}

/// Follow `path` until the locked run's final event or `timeout`. With
/// `from_end`, skip whatever the file already holds. Waits for the file to
/// appear; prints `watcher-s1 follow: watching FILE (offset N)` to stderr once
/// it is open and positioned. Survives truncation, in-place reuse (see
/// `Tail`'s head cache for the detection limit) and rotation: when the path is
/// replaced by a new file, the new file is read from its start while the old
/// descriptor keeps being polled, since its producer may still be writing the
/// run's remaining events (including the final one) there.
///
/// Errors: `InvalidInput` for a path that is not a regular file.
pub fn run(path: &Path, from_end: bool, timeout: Option<Duration>, out: &mut impl Write) -> io::Result<Outcome> {
    let deadline = timeout.map(|t| Instant::now() + t);
    let late = || deadline.is_some_and(|d| Instant::now() >= d);
    let f = loop {
        match open_regular(path) {
            Ok(f) => break f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                if late() {
                    return Ok(Outcome::TimedOut);
                }
                sleep(POLL)
            }
            Err(e) => return Err(e),
        }
    };
    let mut follower = Follower::default();
    let mut first = Tail::open(f, 0)?;
    if from_end {
        let len = first.f.metadata()?.len();
        first.pos = len;
        let mut landed_mid_line = false;
        if len > 0 {
            let mut last = [0u8; 1];
            match read_exact_or_short(&first.f, &mut last, len - 1)? {
                Some(()) => landed_mid_line = last[0] != b'\n',
                // Truncated while we looked: follow from the start.
                None => first.pos = 0,
            }
        }
        if first.sync_head()? {
            // Landed mid-line: the rest of that line is not ours.
            if landed_mid_line {
                first.lines.skip_partial_line();
            }
        } else {
            first.restart();
        }
    }
    eprintln!("watcher-s1 follow: watching {} (offset {})", path.display(), first.pos);
    let mut tails = vec![first];
    let mut buf = vec![0u8; CHUNK];
    let mut print = |s: Seen<'_>| {
        writeln!(out, "{}{}", "  ".repeat(s.depth), format_event(s.event))?;
        out.flush()
    };
    loop {
        if late() {
            return Ok(Outcome::TimedOut);
        }
        let mut progressed = false;
        for t in tails.iter_mut() {
            match t.poll(&mut buf, &mut follower, deadline, &mut print)? {
                Poll::Idle => {}
                Poll::Read => progressed = true,
                Poll::Done => return Ok(Outcome::Done),
                Poll::Late => return Ok(Outcome::TimedOut),
            }
        }
        if progressed {
            continue;
        }
        // Everything drained: only now look for a rotation.
        if tails.last().is_some_and(|t| t.rotated(path).unwrap_or(false)) {
            match open_regular(path) {
                Ok(f) => {
                    tails.push(Tail::open(f, 0)?);
                    if tails.len() > MAX_OLD_FILES + 1 {
                        tails.remove(0);
                    }
                    continue;
                }
                Err(e) if e.kind() == io::ErrorKind::InvalidInput => return Err(e),
                Err(_) => {}
            }
        }
        sleep(POLL);
    }
}

/// An event the caller has not seen yet.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    /// 1-based position among the locked run's events in the file.
    pub seq: u64,
    pub event: Value,
    /// `format_event` of it: the compact line `follow` prints.
    pub line: String,
    pub is_final: bool,
}

/// Library face of `follow` for one events file, without blocking: each
/// `poll` returns the locked run's events appended since the last one. It
/// shares `Follower`'s validation and run locking and `Tail`'s handling of
/// truncation and in-place reuse, but does not follow rotation (a producer
/// that owns a fresh file per run, such as the MCP server's, never rotates).
pub struct EventReader {
    path: PathBuf,
    tail: Option<Tail>,
    follower: Follower,
    seq: u64,
    done: bool,
    buf: Vec<u8>,
}

impl EventReader {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        EventReader {
            path: path.into(),
            tail: None,
            follower: Follower::default(),
            seq: 0,
            done: false,
            buf: vec![0u8; CHUNK],
        }
    }

    /// Events appended since the last call (none if the file does not exist
    /// yet). After the final event is returned, later calls return nothing.
    /// Errors: `InvalidInput` for a path that is not a regular file.
    pub fn poll(&mut self) -> io::Result<Vec<Event>> {
        let mut out = Vec::new();
        if self.done {
            return Ok(out);
        }
        if self.tail.is_none() {
            match open_regular(&self.path) {
                Ok(f) => self.tail = Some(Tail::open(f, 0)?),
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(out),
                Err(e) => return Err(e),
            }
        }
        let tail = self.tail.as_mut().expect("opened above");
        let seq = &mut self.seq;
        let mut collect = |s: Seen<'_>| {
            if s.ours {
                *seq += 1;
                out.push(Event {
                    seq: *seq,
                    event: s.event.clone(),
                    line: format_event(s.event),
                    is_final: s.is_final,
                });
            }
            Ok(())
        };
        loop {
            match tail.poll(&mut self.buf, &mut self.follower, None, &mut collect)? {
                Poll::Idle => break,
                Poll::Read => {}
                Poll::Done => {
                    self.done = true;
                    break;
                }
                Poll::Late => unreachable!("no deadline was given"),
            }
        }
        Ok(out)
    }

    /// The final event has been returned.
    pub fn is_done(&self) -> bool {
        self.done
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(run: &str, parent: Option<&str>, state: &str, exit: Option<i32>) -> String {
        serde_json::json!({
            "run_id": run, "caused_by": parent, "state": state, "reason": "exit", "severity": "info",
            "exit": exit.map(|c| serde_json::json!({"code": c, "signal": null})),
            "s1": {"fused": 0.5}, "evidence_tail": "a\nlast line\n\n",
        })
        .to_string()
            + "\n"
    }

    /// Feed one chunk through a fresh-buffer follower with no deadline.
    fn feed(f: &mut Follower, lb: &mut LineBuf, chunk: &[u8], out: &mut Vec<u8>) -> Step {
        f.feed(lb, chunk, None, out).unwrap()
    }

    fn fresh() -> (Follower, LineBuf, Vec<u8>) {
        Default::default()
    }

    fn lines(out: &[u8]) -> usize {
        out.iter().filter(|&&b| b == b'\n').count()
    }

    #[test]
    fn formats_one_compact_line() {
        let v: Value = serde_json::from_str(&ev("r", None, "done", Some(0))).unwrap();
        assert_eq!(format_event(&v), "done exit info exit=0 s1=0.50 last line");
    }

    #[test]
    fn formats_signal_exits_and_missing_fields() {
        let v = serde_json::json!({
            "run_id": "r", "state": "failing", "reason": "signal", "severity": "error",
            "exit": {"code": null, "signal": 9}, "s1": null, "evidence_tail": "",
        });
        assert_eq!(format_event(&v), "failing signal error signal=9 -");
        let v = serde_json::json!({"run_id": "r"});
        assert_eq!(format_event(&v), "- - - - -");
    }

    #[test]
    fn heartbeats_show_elapsed_and_last_line_not_the_tail() {
        let v = serde_json::json!({
            "run_id": "r", "state": "progressing", "reason": "heartbeat", "severity": "info",
            "exit": null, "s1": null, "evidence_tail": "old\n", "elapsed_ms": 12_345,
            "bytes_since_last": 3, "lines_since_last": 1, "last_line": "compiling foo",
        });
        assert_eq!(
            format_event(&v),
            "progressing heartbeat info - - elapsed=12.3s compiling foo"
        );
        let v = serde_json::json!({"reason": "heartbeat", "elapsed_ms": 1000, "last_line": null});
        assert_eq!(format_event(&v), "- heartbeat - - - elapsed=1.0s");
    }

    #[test]
    fn long_tails_are_clipped_with_an_ellipsis() {
        let v = serde_json::json!({"run_id": "r", "evidence_tail": "x".repeat(200)});
        let line = format_event(&v);
        assert!(line.ends_with(&format!("{}…", "x".repeat(TAIL_CHARS))), "{line}");
    }

    #[test]
    fn nested_final_does_not_stop_and_is_indented() {
        let (mut f, mut lb, mut out) = fresh();
        let text = [
            ev("outer", None, "progressing", None),
            ev("inner", Some("outer"), "done", Some(0)),
        ]
        .concat();
        assert_eq!(feed(&mut f, &mut lb, text.as_bytes(), &mut out), Step::More);
        let out = String::from_utf8(out).unwrap();
        assert!(out.lines().nth(1).unwrap().starts_with("  done"), "{out}");
        let again = ev("outer", None, "done", Some(0));
        assert_eq!(feed(&mut f, &mut lb, again.as_bytes(), &mut Vec::new()), Step::Done);
    }

    #[test]
    fn partial_line_is_buffered_and_garbage_skipped() {
        let (mut f, mut lb, mut out) = fresh();
        let line = ev("r", None, "done", Some(0));
        let (a, b) = line.split_at(20);
        assert_eq!(feed(&mut f, &mut lb, b"not json\n", &mut out), Step::More);
        assert_eq!(feed(&mut f, &mut lb, a.as_bytes(), &mut out), Step::More);
        assert!(out.is_empty());
        assert_eq!(feed(&mut f, &mut lb, b.as_bytes(), &mut out), Step::Done);
    }

    #[test]
    fn a_multibyte_character_split_across_chunks_survives() {
        let (mut f, mut lb, mut out) = fresh();
        let line = ev("r", None, "done", Some(0)).replace("last line", "r\u{e9}sum\u{e9} \u{1f600} done");
        let at = line.find('\u{e9}').unwrap() + 1; // between the two bytes of e-acute
        assert!(!line.is_char_boundary(at));
        let (a, b) = line.as_bytes().split_at(at);
        assert_eq!(feed(&mut f, &mut lb, a, &mut out), Step::More);
        assert_eq!(feed(&mut f, &mut lb, b, &mut out), Step::Done);
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("résumé \u{1f600} done"), "{out}");
    }

    #[test]
    fn reset_drops_the_partial_line() {
        let (mut f, mut lb, mut out) = fresh();
        feed(&mut f, &mut lb, b"{\"run_id\":\"old", &mut out);
        lb.reset();
        assert_eq!(
            feed(&mut f, &mut lb, ev("r", None, "done", Some(0)).as_bytes(), &mut out),
            Step::Done
        );
        assert_eq!(lines(&out), 1);
    }

    #[test]
    fn skip_partial_line_discards_through_the_newline() {
        let (mut f, mut lb, mut out) = fresh();
        lb.skip_partial_line();
        assert_eq!(feed(&mut f, &mut lb, b"tail of a line", &mut out), Step::More);
        let rest = format!("end\n{}", ev("r", None, "done", Some(0)));
        assert_eq!(feed(&mut f, &mut lb, rest.as_bytes(), &mut out), Step::Done);
        assert_eq!(lines(&out), 1);
    }

    #[test]
    fn malformed_objects_are_skipped_without_touching_the_lock() {
        let (mut f, mut lb, mut out) = fresh();
        let bad = [
            "{}",
            r#"{"run_id":"r","exit":false}"#,
            r#"{"run_id":"r","exit":"done"}"#,
            r#"{"run_id":"r","exit":[]}"#,
            r#"{"run_id":"r","exit":{"code":"0"}}"#,
            r#"{"run_id":"r","exit":{"signal":1.5}}"#,
            r#"{"run_id":""}"#,
            r#"{"run_id":7}"#,
            r#"{"run_id":null}"#,
            r#"{"run_id":"r","caused_by":3}"#,
            "[]",
            "\"str\"",
        ]
        .join("\n")
            + "\n";
        assert_eq!(feed(&mut f, &mut lb, bad.as_bytes(), &mut out), Step::More);
        assert!(out.is_empty(), "nothing printed: {}", String::from_utf8_lossy(&out));
        assert!(f.locked.is_none() && f.parents.is_empty());
        // A genuine run is now first, and its final event ends the follow.
        let text = ev("real", None, "progressing", None) + &ev("real", None, "done", Some(0));
        assert_eq!(feed(&mut f, &mut lb, text.as_bytes(), &mut out), Step::Done);
        assert_eq!(lines(&out), 2);
    }

    #[test]
    fn unknown_fields_and_sparse_events_are_accepted() {
        let (mut f, mut lb, mut out) = fresh();
        let text = "{\"run_id\":\"r\",\"future\":{\"x\":1},\"exit\":{\"code\":0}}\n";
        assert_eq!(feed(&mut f, &mut lb, text.as_bytes(), &mut out), Step::Done);
    }

    #[test]
    fn a_passed_deadline_stops_before_the_next_event() {
        let (mut f, mut lb, mut out) = fresh();
        let text = ev("r", None, "progressing", None).repeat(5) + &ev("r", None, "done", Some(0));
        let past = Some(Instant::now() - Duration::from_secs(1));
        assert_eq!(f.feed(&mut lb, text.as_bytes(), past, &mut out).unwrap(), Step::Late);
        assert!(out.is_empty());
    }

    fn file_with(bytes: &[u8]) -> (tempfile::NamedTempFile, File) {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(bytes).unwrap();
        let f = File::open(tmp.path()).unwrap();
        (tmp, f)
    }

    fn poll(t: &mut Tail, f: &mut Follower, out: &mut Vec<u8>) -> io::Result<Poll> {
        t.poll(&mut vec![0u8; CHUNK], f, None, &mut |s| {
            writeln!(out, "{}{}", "  ".repeat(s.depth), format_event(s.event))
        })
    }

    #[test]
    fn head_is_cached_when_starting_past_the_start() {
        for len in [1usize, 2, 3, 10, 63, 64, 65, 200] {
            let (mut tmp, f) = file_with(&vec![b'a'; len]);
            let t = Tail::open(f, len as u64).unwrap();
            assert_eq!(t.head.len(), len.min(HEAD_LEN), "len {len}");
            // Appending must neither panic nor look like a reset.
            tmp.write_all(b"{}\n").unwrap();
            let mut t = t;
            let mut out = Vec::new();
            assert!(!t.was_reset().unwrap());
            poll(&mut t, &mut Follower::default(), &mut out).unwrap();
            assert_eq!(t.head.len(), (len + 3).min(HEAD_LEN));
        }
    }

    #[test]
    fn truncate_and_regrow_is_noticed_from_a_cached_head() {
        let (tmp, f) = file_with(&[b'a'; 100]);
        let mut t = Tail::open(f, 100).unwrap();
        std::fs::write(tmp.path(), vec![b'b'; 300]).unwrap(); // same inode, longer, new head
        assert!(t.was_reset().unwrap());
        // A file that shrank below the head cache is a reset, not an I/O error.
        let (tmp, f) = file_with(&[b'a'; 100]);
        let mut t2 = Tail::open(f, 100).unwrap();
        std::fs::write(tmp.path(), b"a").unwrap();
        assert!(t2.was_reset().unwrap());
        t.restart();
        assert_eq!((t.pos, t.head.len()), (0, 0));
    }

    #[test]
    fn short_reads_from_a_concurrent_truncation_are_a_reset_not_an_error() {
        let (tmp, f) = file_with(&[b'a'; 100]);
        let mut t = Tail::open(f, 100).unwrap();
        // Truncated after the length check: the prefix read comes up short.
        t.head = vec![b'a'; 64];
        t.pos = 64;
        std::fs::write(tmp.path(), b"aaa").unwrap();
        assert!(t.was_reset().unwrap());
        // Extending the head past the new end reports "short", not an error.
        let (tmp, f) = file_with(&[b'a'; 100]);
        let mut t = Tail::open(f, 10).unwrap();
        t.pos = 50;
        std::fs::write(tmp.path(), b"aaaa").unwrap();
        assert!(!t.sync_head().unwrap());
    }

    #[test]
    fn the_head_check_does_not_see_a_rewrite_with_an_unchanged_head() {
        // The documented limit: same first 64 bytes, different later bytes.
        let prefix = vec![b'p'; 64];
        let mut old = prefix.clone();
        old.extend_from_slice(b"old-old-old\n");
        let (tmp, f) = file_with(&old);
        let mut t = Tail::open(f, old.len() as u64).unwrap();
        let mut new = prefix.clone();
        new.extend_from_slice(b"new-new-new-new-new\n");
        std::fs::write(tmp.path(), &new).unwrap();
        assert!(!t.was_reset().unwrap());
        // ...while a change anywhere inside the head is seen.
        let mut changed = new.clone();
        changed[10] = b'q';
        std::fs::write(tmp.path(), &changed).unwrap();
        assert!(t.was_reset().unwrap());
    }

    #[test]
    fn non_regular_files_are_refused_without_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("fifo");
        nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::from_bits_truncate(0o600)).unwrap();
        let e = open_regular(&fifo).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput, "{e}");
        let e = open_regular(dir.path()).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput, "{e}");
    }

    #[test]
    fn event_reader_returns_new_events_without_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("e.jsonl");
        let mut r = EventReader::new(&path);
        assert!(r.poll().unwrap().is_empty(), "a missing file is not an error");
        let mut f = std::fs::File::create(&path).unwrap();
        write!(f, "{}", ev("r", None, "progressing", None)).unwrap();
        let first = r.poll().unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!((first[0].seq, first[0].is_final), (1, false));
        assert_eq!(first[0].line, "progressing exit info - s1=0.50 last line");
        assert!(r.poll().unwrap().is_empty());
        // A half-written line waits for its newline; another run's events are not ours.
        let line = ev("r", None, "done", Some(0));
        let (a, b) = line.split_at(30);
        write!(f, "{}{a}", ev("other", None, "done", Some(1))).unwrap();
        assert!(r.poll().unwrap().is_empty());
        write!(f, "{b}").unwrap();
        let last = r.poll().unwrap();
        assert_eq!((last.len(), last[0].seq, last[0].is_final), (1, 2, true), "{last:?}");
        assert!(r.is_done() && r.poll().unwrap().is_empty());
    }

    #[test]
    fn event_reader_refuses_a_fifo() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("fifo");
        nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::from_bits_truncate(0o600)).unwrap();
        let e = EventReader::new(&fifo).poll().unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
    }
}
