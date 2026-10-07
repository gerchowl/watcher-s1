//! The output ring buffer and the text views the detectors and System One
//! read from it (ANSI stripped, carriage-return overwrites applied).

pub const DEFAULT_CAPACITY: usize = 16 * 1024;

pub struct Ring {
    buf: Vec<u8>,
    cap: usize,
    total: u64,
}

impl Ring {
    pub fn new(cap: usize) -> Self {
        Self {
            buf: Vec::with_capacity(cap * 2),
            cap,
            total: 0,
        }
    }

    pub fn push(&mut self, data: &[u8]) {
        self.total += data.len() as u64;
        if data.len() >= self.cap {
            self.buf.clear();
            self.buf.extend_from_slice(&data[data.len() - self.cap..]);
            return;
        }
        self.buf.extend_from_slice(data);
        if self.buf.len() > self.cap * 2 {
            let cut = self.buf.len() - self.cap;
            self.buf.drain(..cut);
        }
    }

    /// The last `cap` raw bytes.
    pub fn raw(&self) -> &[u8] {
        let n = self.buf.len().min(self.cap);
        &self.buf[self.buf.len() - n..]
    }

    pub fn total_bytes(&self) -> u64 {
        self.total
    }

    /// The cleaned text of the buffer.
    pub fn text(&self) -> String {
        clean(self.raw())
    }
}

/// Strip ANSI escapes and apply `\r` overwrites (progress bars) per line.
pub fn clean(raw: &[u8]) -> String {
    let s = String::from_utf8_lossy(raw);
    let mut out = String::with_capacity(s.len());
    let mut line = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => skip_escape(&mut chars),
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    continue;
                }
                line.clear();
            }
            '\n' => {
                out.push_str(line.trim_end());
                out.push('\n');
                line.clear();
            }
            '\x08' => {
                line.pop();
            }
            c if c.is_control() && c != '\t' => {}
            c => line.push(c),
        }
    }
    out.push_str(&line);
    out
}

fn skip_escape(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    match chars.next() {
        // CSI: parameters/intermediates then one final byte in @..~
        Some('[') => {
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        }
        // OSC / DCS / PM / APC: until BEL or ST (ESC \)
        Some(']' | 'P' | '^' | '_' | 'X') => {
            while let Some(c) = chars.next() {
                if c == '\x07' {
                    break;
                }
                if c == '\x1b' {
                    if chars.peek() == Some(&'\\') {
                        chars.next();
                    }
                    break;
                }
            }
        }
        // Charset designation takes one more char, e.g. ESC ( B
        Some('(' | ')' | '*' | '+') => {
            chars.next();
        }
        _ => {}
    }
}

/// Where the incremental ANSI parser is between chunks.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Esc {
    /// Saw ESC; the next char picks the sequence kind.
    Start,
    Csi,
    /// OSC / DCS / PM / APC / SOS: until BEL or ST.
    Osc,
    /// Inside a string sequence, saw ESC (ST starts with it).
    OscSt,
    /// A charset designation takes one more char.
    Charset,
}

/// The last non-empty output line, kept incrementally and independently of
/// the evidence [`Ring`]: the ring evicts, this does not forget. Bounded:
/// the first [`LineTracker::MAX`] cleaned chars of the current line, the same
/// of the last completed non-empty line, plus parser state that survives
/// chunk boundaries (a split escape sequence or UTF-8 character). It follows
/// the same cleaning rules as [`clean`]: ANSI stripped, `\r` restarts the
/// line, backspace erases.
#[derive(Default)]
pub struct LineTracker {
    esc: Option<Esc>,
    /// An incomplete trailing UTF-8 sequence (at most 3 bytes).
    utf8: Vec<u8>,
    /// First `MAX` chars of the current line.
    cur: String,
    /// Chars in the current line, counting those past `MAX` (not stored).
    cur_len: usize,
    /// A `\r` seen and not yet resolved: `\r\n` keeps the line, anything
    /// else restarts it.
    cr: bool,
    done: Option<String>,
}

impl LineTracker {
    pub const MAX: usize = 200;

    pub fn feed(&mut self, data: &[u8]) {
        let mut buf = std::mem::take(&mut self.utf8);
        buf.extend_from_slice(data);
        let mut rest = &buf[..];
        loop {
            match std::str::from_utf8(rest) {
                Ok(s) => {
                    s.chars().for_each(|c| self.push(c));
                    break;
                }
                Err(e) => {
                    let (ok, bad) = rest.split_at(e.valid_up_to());
                    std::str::from_utf8(ok)
                        .unwrap_or_default()
                        .chars()
                        .for_each(|c| self.push(c));
                    match e.error_len() {
                        Some(n) => {
                            self.push('\u{FFFD}');
                            rest = &bad[n..];
                        }
                        None => {
                            self.utf8 = bad.to_vec();
                            break;
                        }
                    }
                }
            }
        }
    }

    fn push(&mut self, c: char) {
        match self.esc {
            None => {}
            Some(Esc::Start) => {
                self.esc = match c {
                    '[' => Some(Esc::Csi),
                    ']' | 'P' | '^' | '_' | 'X' => Some(Esc::Osc),
                    '(' | ')' | '*' | '+' => Some(Esc::Charset),
                    _ => None,
                };
                return;
            }
            Some(Esc::Csi) => {
                if ('@'..='~').contains(&c) {
                    self.esc = None;
                }
                return;
            }
            Some(Esc::Osc) => {
                match c {
                    '\x07' => self.esc = None,
                    '\x1b' => self.esc = Some(Esc::OscSt),
                    _ => {}
                }
                return;
            }
            Some(Esc::OscSt) => {
                self.esc = None;
                if c == '\\' {
                    return;
                }
                // Not an ST: the char is ordinary text.
            }
            Some(Esc::Charset) => {
                self.esc = None;
                return;
            }
        }
        if c == '\n' {
            // `\r\n` is one line break; a lone `\r` before it already restarted.
            self.cr = false;
            self.finish_line();
            return;
        }
        if std::mem::take(&mut self.cr) {
            self.restart();
        }
        match c {
            '\x1b' => self.esc = Some(Esc::Start),
            '\r' => self.cr = true,
            '\x08' => {
                // Past MAX the erased char was never stored.
                if self.cur_len > Self::MAX || self.cur.pop().is_some() {
                    self.cur_len -= 1;
                }
            }
            c if c.is_control() && c != '\t' => {}
            c => {
                if self.cur_len < Self::MAX {
                    self.cur.push(c);
                }
                self.cur_len += 1;
            }
        }
    }

    fn restart(&mut self) {
        self.cur.clear();
        self.cur_len = 0;
    }

    /// The current line, trimmed, when it has any content.
    fn current(&self) -> Option<&str> {
        let s = if self.cur_len > Self::MAX {
            &self.cur[..]
        } else {
            self.cur.trim_end()
        };
        (!self.cr && !s.trim().is_empty()).then_some(s)
    }

    fn finish_line(&mut self) {
        if let Some(s) = self.current().map(str::to_owned) {
            self.done = Some(s);
        }
        self.restart();
    }

    /// The last non-empty line so far (the unterminated one counts), cleaned
    /// and cut to [`Self::MAX`] chars; `None` before any.
    pub fn last_line(&self) -> Option<String> {
        self.current().map(str::to_owned).or_else(|| self.done.clone())
    }
}

/// The last `n` bytes of `s`, starting on a char boundary (prefer a line start).
pub fn tail(s: &str, n: usize) -> &str {
    if s.len() <= n {
        return s;
    }
    let mut i = s.len() - n;
    while !s.is_char_boundary(i) {
        i += 1;
    }
    let t = &s[i..];
    match t.find('\n') {
        Some(nl) if nl < 200 && nl + 1 < t.len() => &t[nl + 1..],
        _ => t,
    }
}

/// The unterminated last line (what a prompt looks like), or "" when the
/// output ends with a newline.
pub fn last_line(s: &str) -> &str {
    match s.rfind('\n') {
        Some(i) => &s[i + 1..],
        None => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_keeps_the_last_cap_bytes() {
        let mut r = Ring::new(8);
        r.push(b"0123456789");
        assert_eq!(r.raw(), b"23456789");
        r.push(b"ab");
        assert_eq!(r.raw(), b"456789ab");
        for _ in 0..100 {
            r.push(b"xyz");
        }
        assert_eq!(r.raw().len(), 8);
        assert_eq!(r.total_bytes(), 312);
    }

    #[test]
    fn clean_strips_ansi_and_applies_cr() {
        assert_eq!(clean(b"\x1b[31merror\x1b[0m: x\r\n"), "error: x\n");
        assert_eq!(clean(b"10%\r50%\r100%\ndone"), "100%\ndone");
        assert_eq!(clean(b"\x1b]0;title\x07Password: "), "Password: ");
        assert_eq!(clean(b"ab\x08c"), "ac");
    }

    #[test]
    fn tail_respects_char_and_line_boundaries() {
        let s = "line one\nline two\nline three\n";
        assert_eq!(tail(s, 15), "line three\n");
        assert_eq!(tail("ééé", 3), "é");
        assert_eq!(tail("short", 100), "short");
    }

    fn tracked(chunks: &[&[u8]]) -> Option<String> {
        let mut t = LineTracker::default();
        chunks.iter().for_each(|c| t.feed(c));
        t.last_line()
    }

    fn cleaned_last(raw: &[u8]) -> Option<String> {
        clean(raw)
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .map(|l| l.trim_end().to_owned())
    }

    #[test]
    fn tracker_remembers_past_the_ring() {
        // Review repro 1: the last non-empty line is far behind the ring.
        let mut r = Ring::new(DEFAULT_CAPACITY);
        let mut t = LineTracker::default();
        let mut out = b"important\n".to_vec();
        out.extend(std::iter::repeat_n(b'\n', 20000));
        for c in out.chunks(4096) {
            r.push(c);
            t.feed(c);
        }
        assert_eq!(r.text().trim(), "");
        assert_eq!(t.last_line().as_deref(), Some("important"));
        // Review repro 2: a 20 KB line keeps its start, cut to 200.
        let mut t = LineTracker::default();
        let mut out = b"BEGIN".to_vec();
        out.extend(std::iter::repeat_n(b'x', 20000));
        out.push(b'\n');
        out.chunks(7000).for_each(|c| t.feed(c));
        assert_eq!(t.last_line(), Some(format!("BEGIN{}", "x".repeat(195))));
    }

    #[test]
    fn tracker_matches_clean_on_every_split() {
        let raw =
            "one\n\x1b[31mr\u{e9}\u{1F600}d\x1b[0m\n\x1b]0;ti\ntle\x07 \n10%\r50%\r\nab\x08c\r\n  \nlast \u{2713}";
        let want = cleaned_last(raw.as_bytes());
        assert_eq!(want.as_deref(), Some("last \u{2713}"));
        for cut in 0..=raw.len() {
            let (a, b) = raw.as_bytes().split_at(cut);
            assert_eq!(tracked(&[a, b]), want, "split at {cut}");
        }
        // Byte at a time, at every prefix.
        for end in (1..=raw.len()).filter(|e| raw.is_char_boundary(*e)) {
            let mut t = LineTracker::default();
            raw.as_bytes()[..end].iter().for_each(|b| t.feed(&[*b]));
            assert_eq!(t.last_line(), cleaned_last(&raw.as_bytes()[..end]), "prefix {end}");
        }
    }

    #[test]
    fn tracker_carriage_return_and_split_escapes() {
        assert_eq!(tracked(&[b"10%\r50%\r100%"]).as_deref(), Some("100%"));
        // A bare \r restarts the line: back to the last completed one.
        assert_eq!(tracked(&[b"done\nprogress\r"]).as_deref(), Some("done"));
        assert_eq!(tracked(&[b"ok\r", b"\n"]).as_deref(), Some("ok"));
        // Escapes split across chunks (CSI, OSC with ST) leave no residue.
        assert_eq!(
            tracked(&[b"a\x1b", b"[3", b"1mb\x1b]0;t", b"itle\x1b", b"\\c"]).as_deref(),
            Some("abc")
        );
        assert_eq!(tracked(&[]), None);
        assert_eq!(tracked(&[b"\n\n  \n"]), None);
    }

    #[test]
    fn last_line_is_the_unterminated_tail() {
        assert_eq!(last_line("a\nPassword: "), "Password: ");
        assert_eq!(last_line("a\n"), "");
    }
}
