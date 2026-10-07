//! `watcher-s1 follow FILE`: tail an events file, one compact line per
//! event, and return when the run it locked onto finishes.
//!
//! The file may be shared with nested watchers and reused across runs, so the
//! first `run_id` seen is the run; only its final event (non-null `exit`)
//! ends the follow.

use serde_json::Value;
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Write};
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::Path;
use std::thread::sleep;
use std::time::{Duration, Instant};

const POLL: Duration = Duration::from_millis(200);
/// Characters of the evidence tail shown on a line.
const TAIL_CHARS: usize = 80;

/// Incremental event-line reader: feed it file bytes, get lines out.
#[derive(Default)]
pub struct Follower {
    /// Bytes after the last newline, held until the line completes.
    partial: Vec<u8>,
    /// Drop bytes up to and including the next newline (a line we joined
    /// mid-way).
    skipping: bool,
    /// The run being followed: the first `run_id` seen.
    locked: Option<String>,
    /// run_id -> caused_by, for indenting nested runs.
    parents: HashMap<String, Option<String>>,
}

impl Follower {
    /// Consume a chunk, writing one line per complete event to `out`.
    /// Returns true once the locked run's final event has been printed.
    pub fn feed(&mut self, mut chunk: &[u8], out: &mut impl Write) -> io::Result<bool> {
        if self.skipping {
            match chunk.iter().position(|&b| b == b'\n') {
                Some(i) => {
                    chunk = &chunk[i + 1..];
                    self.skipping = false;
                }
                None => return Ok(false),
            }
        }
        self.partial.extend_from_slice(chunk);
        while let Some(nl) = self.partial.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.partial.drain(..=nl).collect();
            let line = String::from_utf8_lossy(&line[..nl]);
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Value>(&line) {
                Ok(ev) if ev.is_object() => {
                    if self.event(&ev, out)? {
                        return Ok(true);
                    }
                }
                _ => eprintln!(
                    "watcher-s1 follow: skipping a malformed line: {}",
                    clip(&line, TAIL_CHARS)
                ),
            }
        }
        Ok(false)
    }

    /// The file restarted: a half-read line from the old content is void.
    pub fn reset_partial(&mut self) {
        self.partial.clear();
        self.skipping = false;
    }

    /// Discard through the next newline before parsing anything.
    pub fn skip_partial_line(&mut self) {
        self.partial.clear();
        self.skipping = true;
    }

    fn event(&mut self, ev: &Value, out: &mut impl Write) -> io::Result<bool> {
        let run_id = str_of(ev, "run_id");
        let caused_by = ev["caused_by"].as_str().map(str::to_owned);
        self.parents.entry(run_id.to_owned()).or_insert(caused_by);
        let locked = self.locked.get_or_insert_with(|| run_id.to_owned());
        let ours = locked == run_id;
        writeln!(out, "{}{}", "  ".repeat(self.depth(run_id)), format_event(ev))?;
        out.flush()?;
        Ok(ours && !ev["exit"].is_null())
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

/// `state reason severity exit s1 tail`
pub fn format_event(ev: &Value) -> String {
    let exit = match (ev["exit"]["code"].as_i64(), ev["exit"]["signal"].as_i64()) {
        (Some(c), _) => format!("exit={c}"),
        (None, Some(s)) => format!("signal={s}"),
        _ => "-".into(),
    };
    let s1 = ev["s1"]["fused"].as_f64().map_or("-".into(), |f| format!("s1={f:.2}"));
    let tail = ev["evidence_tail"]
        .as_str()
        .and_then(|t| t.lines().rev().find(|l| !l.trim().is_empty()))
        .map_or(String::new(), |l| clip(l.trim(), TAIL_CHARS));
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

/// The open file plus what is needed to notice it was replaced or reset.
struct Tail {
    f: File,
    pos: u64,
    /// The first bytes of the file as first read. A file truncated and
    /// regrown past `pos` between two polls keeps its inode and is longer
    /// than `pos`, but its head differs, which this catches. Residual
    /// limitation: a regrown file whose first `HEAD_LEN` bytes are identical
    /// to the old ones (not the case for event lines, which carry a run_id)
    /// goes unnoticed.
    head: Vec<u8>,
}

impl Tail {
    fn new(f: File, pos: u64) -> Self {
        Tail {
            f,
            pos,
            head: Vec::new(),
        }
    }

    /// True if the file shrank or its head changed: it was reused in place.
    fn was_reset(&mut self) -> io::Result<bool> {
        if self.f.metadata()?.len() < self.pos {
            return Ok(true);
        }
        let n = self.head.len().min(self.pos as usize);
        if n > 0 {
            let mut now = vec![0u8; n];
            self.f.read_exact_at(&mut now, 0)?;
            if now != self.head[..n] {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Read the next chunk at `pos`, remembering the file head.
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.f.read_at(buf, self.pos)?;
        if self.pos < HEAD_LEN as u64 && n > 0 {
            let have = self.head.len();
            let end = (self.pos as usize + n).min(HEAD_LEN);
            if end > have {
                self.head
                    .extend_from_slice(&buf[have - self.pos as usize..end - self.pos as usize]);
            }
        }
        self.pos += n as u64;
        Ok(n)
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
/// appear. Survives truncation, in-place reuse and rotation (the path
/// replaced by a new file): the new file is read from its start.
pub fn run(path: &Path, from_end: bool, timeout: Option<Duration>, out: &mut impl Write) -> io::Result<Outcome> {
    let deadline = timeout.map(|t| Instant::now() + t);
    let late = || deadline.is_some_and(|d| Instant::now() >= d);
    let f = loop {
        match File::open(path) {
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
    let mut tail = Tail::new(f, 0);
    if from_end {
        let len = tail.f.metadata()?.len();
        tail.pos = len;
        if len > 0 {
            let mut last = [0u8; 1];
            tail.f.read_exact_at(&mut last, len - 1)?;
            if last[0] != b'\n' {
                // Landed mid-line: the rest of that line is not ours.
                follower.skip_partial_line();
            }
        }
    }
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        if tail.was_reset()? {
            tail.pos = 0;
            tail.head.clear();
            follower.reset_partial();
        }
        let n = tail.read(&mut buf)?;
        if n > 0 {
            if follower.feed(&buf[..n], out)? {
                return Ok(Outcome::Done);
            }
            continue;
        }
        // Drained to EOF: only now is it safe to leave a rotated-away file.
        if tail.rotated(path)?
            && let Ok(f) = File::open(path)
        {
            tail = Tail::new(f, 0);
            follower.reset_partial();
            continue;
        }
        if late() {
            return Ok(Outcome::TimedOut);
        }
        sleep(POLL);
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

    #[test]
    fn formats_one_compact_line() {
        let v: Value = serde_json::from_str(&ev("r", None, "done", Some(0))).unwrap();
        assert_eq!(format_event(&v), "done exit info exit=0 s1=0.50 last line");
    }

    #[test]
    fn nested_final_does_not_stop_and_is_indented() {
        let mut f = Follower::default();
        let mut out = Vec::new();
        let text = [
            ev("outer", None, "progressing", None),
            ev("inner", Some("outer"), "done", Some(0)),
        ]
        .concat();
        assert!(!f.feed(text.as_bytes(), &mut out).unwrap());
        let out = String::from_utf8(out).unwrap();
        assert!(out.lines().nth(1).unwrap().starts_with("  done"), "{out}");
        assert!(
            f.feed(ev("outer", None, "done", Some(0)).as_bytes(), &mut Vec::new())
                .unwrap()
        );
    }

    #[test]
    fn partial_line_is_buffered_and_garbage_skipped() {
        let mut f = Follower::default();
        let mut out = Vec::new();
        let line = ev("r", None, "done", Some(0));
        let (a, b) = line.split_at(20);
        assert!(!f.feed(b"not json\n", &mut out).unwrap());
        assert!(!f.feed(a.as_bytes(), &mut out).unwrap() && out.is_empty());
        assert!(f.feed(b.as_bytes(), &mut out).unwrap());
    }

    #[test]
    fn reset_drops_the_partial_line() {
        let mut f = Follower::default();
        let mut out = Vec::new();
        f.feed(b"{\"run_id\":\"old", &mut out).unwrap();
        f.reset_partial();
        assert!(f.feed(ev("r", None, "done", Some(0)).as_bytes(), &mut out).unwrap());
        assert_eq!(out.iter().filter(|&&b| b == b'\n').count(), 1);
    }

    #[test]
    fn skip_partial_line_discards_through_the_newline() {
        let mut f = Follower::default();
        let mut out = Vec::new();
        f.skip_partial_line();
        assert!(!f.feed(b"tail of a line", &mut out).unwrap());
        let rest = format!("end\n{}", ev("r", None, "done", Some(0)));
        assert!(f.feed(rest.as_bytes(), &mut out).unwrap());
        assert_eq!(out.iter().filter(|&&b| b == b'\n').count(), 1);
    }
}
