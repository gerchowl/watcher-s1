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

    #[test]
    fn last_line_is_the_unterminated_tail() {
        assert_eq!(last_line("a\nPassword: "), "Password: ");
        assert_eq!(last_line("a\n"), "");
    }
}
