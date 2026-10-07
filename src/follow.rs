//! `watcher-s1 follow FILE`: tail an events file, one compact line per
//! event, and return when the run it locked onto finishes.
//!
//! The file may be shared with nested watchers and reused across runs, so the
//! first `run_id` seen is the run; only its final event (non-null `exit`)
//! ends the follow.

use serde_json::Value;
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::thread::sleep;
use std::time::Duration;

const POLL: Duration = Duration::from_millis(200);
/// Characters of the evidence tail shown on a line.
const TAIL_CHARS: usize = 80;

/// Incremental event-line reader: feed it file bytes, get lines out.
#[derive(Default)]
pub struct Follower {
    /// Bytes after the last newline, held until the line completes.
    partial: Vec<u8>,
    /// The run being followed: the first `run_id` seen.
    locked: Option<String>,
    /// run_id -> caused_by, for indenting nested runs.
    parents: HashMap<String, Option<String>>,
}

impl Follower {
    /// Consume a chunk, writing one line per complete event to `out`.
    /// Returns true once the locked run's final event has been printed.
    pub fn feed(&mut self, chunk: &[u8], out: &mut impl Write) -> io::Result<bool> {
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

/// Follow `path` until the locked run's final event. With `from_end`, skip
/// whatever the file already holds. Waits for the file to appear.
pub fn run(path: &Path, from_end: bool, out: &mut impl Write) -> io::Result<()> {
    let mut f = loop {
        match File::open(path) {
            Ok(f) => break f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => sleep(POLL),
            Err(e) => return Err(e),
        }
    };
    let mut pos = if from_end { f.metadata()?.len() } else { 0 };
    let mut follower = Follower::default();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        // A file truncated for reuse starts over.
        if f.metadata()?.len() < pos {
            pos = 0;
        }
        f.seek(SeekFrom::Start(pos))?;
        let n = f.read(&mut buf)?;
        if n == 0 {
            sleep(POLL);
            continue;
        }
        pos += n as u64;
        if follower.feed(&buf[..n], out)? {
            return Ok(());
        }
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
}
