//! The supervisor's control socket (`--control PATH`, hidden): how a caller
//! that is not the watcher's parent (the MCP server, possibly a later one)
//! asks a live watcher for its status or tells it to stop, without PID
//! signalling and without trusting any file.
//!
//! Protocol: one Unix stream connection per request, one line of JSON each
//! way.
//!
//! ```text
//! {"op":"status"}               -> {"ok":true,"pid":123,"pgid":123,"state":"progressing","elapsed_ms":4200}
//! {"op":"stop","grace_ms":5000} -> {"ok":true}
//! anything else                 -> {"ok":false,"error":"..."}
//! ```
//!
//! `stop` makes the supervisor run its own `--timeout` escalation (TERM to
//! the job's process group, KILL after the grace, KILL again before it reaps
//! the leader) and end with `reason: stopped`. `pid`/`pgid` are `null` in
//! `--log` mode, which has no child.
//!
//! The server half ([`ControlServer`]) lives inside the supervisor's poll
//! loops and never blocks: accept and read are nonblocking, a request is at
//! most [`MAX_REQUEST`] bytes and a connection lives at most [`CONN_TTL`].
//! The client half ([`request`]) is blocking with hard timeouts; call it off
//! any async runtime thread.

use serde::Deserialize;
use serde_json::{Value, json};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// A request line is at most this many bytes, newline included.
pub const MAX_REQUEST: usize = 4096;
/// A connection that has not delivered a full request by then is dropped.
pub const CONN_TTL: Duration = Duration::from_secs(2);
/// Open connections the supervisor holds at once; more are refused.
const MAX_CONNS: usize = 16;
/// `sockaddr_un.sun_path` holds 108 bytes on Linux (104 on the BSDs and macOS),
/// the terminating NUL included.
pub const MAX_SOCKET_PATH: usize = 100;

/// What a client asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
    Status,
    Stop { grace: Duration },
}

#[derive(Deserialize)]
struct Wire {
    op: String,
    #[serde(default)]
    grace_ms: Option<u64>,
}

/// Longest grace a stop may ask for; the caller waits for it, so cap it.
const MAX_GRACE: Duration = Duration::from_secs(24 * 3600);

fn parse(line: &[u8]) -> Result<Request, String> {
    let w: Wire = serde_json::from_slice(line).map_err(|e| format!("bad request: {e}"))?;
    match w.op.as_str() {
        "status" => Ok(Request::Status),
        "stop" => Ok(Request::Stop {
            grace: Duration::from_millis(w.grace_ms.unwrap_or(10_000)).min(MAX_GRACE),
        }),
        other => Err(format!("unknown op {other:?}")),
    }
}

/// The answer to `status`.
#[derive(Debug, Clone, PartialEq)]
pub struct StatusReply {
    pub pid: Option<i32>,
    pub pgid: Option<i32>,
    pub state: &'static str,
    pub elapsed_ms: u64,
}

impl StatusReply {
    fn to_json(&self) -> Value {
        json!({"ok": true, "pid": self.pid, "pgid": self.pgid, "state": self.state, "elapsed_ms": self.elapsed_ms})
    }
}

struct Conn {
    stream: UnixStream,
    buf: Vec<u8>,
    deadline: Instant,
}

/// The listening side, owned by the supervisor for the life of the run.
pub struct ControlServer {
    listener: UnixListener,
    path: PathBuf,
    /// (dev, ino) of the socket we created, so Drop removes only our own.
    ident: (u64, u64),
    conns: Vec<Conn>,
}

impl ControlServer {
    /// Create the socket at `path` (mode 0600). Refuses a path that exists
    /// and is not a stale socket (nothing accepting on it).
    pub fn bind(path: &Path) -> io::Result<Self> {
        if path.as_os_str().len() > MAX_SOCKET_PATH {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("control path is longer than {MAX_SOCKET_PATH} bytes"),
            ));
        }
        match std::fs::symlink_metadata(path) {
            Ok(m) if m.file_type().is_socket() => match UnixStream::connect(path) {
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::AddrInUse,
                        format!("{} is a live control socket", path.display()),
                    ));
                }
                Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => std::fs::remove_file(path)?,
                Err(e) => return Err(e),
            },
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("{} exists and is not a stale socket", path.display()),
                ));
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        // Closed to everyone but us right after creation. (The MCP server
        // puts it in a 0700 directory, so nobody can connect in between; no
        // umask juggling, which would race with other threads.)
        let listener = UnixListener::bind(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        let m = std::fs::symlink_metadata(path)?;
        Ok(ControlServer {
            listener,
            path: path.to_path_buf(),
            ident: (m.dev(), m.ino()),
            conns: Vec::new(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Add the listener and open connections to a `poll(2)` set, so the loop
    /// wakes for a request instead of waiting out its tick.
    pub fn push_pollfds(&self, fds: &mut Vec<libc::pollfd>) {
        let mut add = |fd: RawFd| {
            fds.push(libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            })
        };
        add(self.listener.as_raw_fd());
        for c in &self.conns {
            add(c.stream.as_raw_fd());
        }
    }

    /// Accept and read whatever is ready, answer complete requests, and
    /// return the grace of a `stop` if one arrived. Never blocks.
    pub fn service(&mut self, status: &dyn Fn() -> StatusReply) -> Option<Duration> {
        while let Ok((stream, _)) = self.listener.accept() {
            if self.conns.len() >= MAX_CONNS || stream.set_nonblocking(true).is_err() {
                continue; // dropped: the client sees EOF
            }
            self.conns.push(Conn {
                stream,
                buf: Vec::new(),
                deadline: Instant::now() + CONN_TTL,
            });
        }
        let now = Instant::now();
        let mut stop = None;
        let mut keep = Vec::with_capacity(self.conns.len());
        for mut c in std::mem::take(&mut self.conns) {
            match read_request(&mut c) {
                Got::Pending if now < c.deadline => keep.push(c),
                Got::Pending | Got::Closed => {}
                Got::Bad(why) => {
                    reply(&mut c.stream, &json!({"ok": false, "error": why}));
                    // Take what the client already sent (bounded): closing
                    // with unread data would reset the connection and could
                    // discard the reply.
                    let mut sink = [0u8; 4096];
                    for _ in 0..16 {
                        if !matches!(c.stream.read(&mut sink), Ok(n) if n > 0) {
                            break;
                        }
                    }
                }
                Got::Line(line) => match parse(&line) {
                    Ok(Request::Status) => reply(&mut c.stream, &status().to_json()),
                    Ok(Request::Stop { grace }) => {
                        reply(&mut c.stream, &json!({"ok": true}));
                        stop = Some(stop.map_or(grace, |g: Duration| g.min(grace)));
                    }
                    Err(why) => reply(&mut c.stream, &json!({"ok": false, "error": why})),
                },
            }
        }
        self.conns = keep;
        stop
    }
}

impl Drop for ControlServer {
    fn drop(&mut self) {
        // Only our own socket: a successor may have replaced the path.
        if let Ok(m) = std::fs::symlink_metadata(&self.path)
            && (m.dev(), m.ino()) == self.ident
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

enum Got {
    Pending,
    Closed,
    Bad(String),
    Line(Vec<u8>),
}

fn read_request(c: &mut Conn) -> Got {
    let mut chunk = [0u8; 512];
    loop {
        match c.stream.read(&mut chunk) {
            Ok(0) => return Got::Closed,
            Ok(n) => {
                c.buf.extend_from_slice(&chunk[..n]);
                if let Some(i) = c.buf.iter().position(|&b| b == b'\n') {
                    c.buf.truncate(i);
                    return Got::Line(std::mem::take(&mut c.buf));
                }
                if c.buf.len() >= MAX_REQUEST {
                    return Got::Bad(format!("request longer than {MAX_REQUEST} bytes"));
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Got::Pending,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return Got::Closed,
        }
    }
}

/// One nonblocking write of a short reply; a client that cannot take it is dropped.
fn reply(stream: &mut UnixStream, v: &Value) {
    let _ = stream.write(format!("{v}\n").as_bytes());
}

/// Why a client request failed.
#[derive(Debug)]
pub enum ClientError {
    /// Nothing is listening: the watcher is gone (or never got that far).
    Unreachable(io::Error),
    /// Connected but no valid answer in time, or a malformed one.
    Failed(String),
    /// The watcher answered `ok: false`.
    Refused(String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Unreachable(e) => write!(f, "control socket unreachable: {e}"),
            ClientError::Failed(s) | ClientError::Refused(s) => f.write_str(s),
        }
    }
}

/// Send one request and read the one-line reply, within `timeout` overall
/// for the read and write halves (connect to a local socket is immediate
/// unless the supervisor's backlog is full, which the caller bounds by
/// running this off its runtime thread). The reply is size-limited.
pub fn request(path: &Path, req: &Value, timeout: Duration) -> Result<Value, ClientError> {
    let mut s = UnixStream::connect(path).map_err(ClientError::Unreachable)?;
    let io_err = |e: io::Error| ClientError::Failed(format!("control socket: {e}"));
    s.set_read_timeout(Some(timeout)).map_err(io_err)?;
    s.set_write_timeout(Some(timeout)).map_err(io_err)?;
    writeln!(s, "{req}").map_err(io_err)?;
    let mut out = Vec::new();
    let deadline = Instant::now() + timeout;
    let mut chunk = [0u8; 512];
    loop {
        if Instant::now() >= deadline {
            return Err(ClientError::Failed("control socket: timed out".into()));
        }
        match s.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                out.extend_from_slice(&chunk[..n]);
                if out.len() > MAX_REQUEST * 4 {
                    return Err(ClientError::Failed("control socket: reply too long".into()));
                }
                if out.contains(&b'\n') {
                    break;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(io_err(e)),
        }
    }
    let v: Value =
        serde_json::from_slice(&out).map_err(|e| ClientError::Failed(format!("control socket: bad reply ({e})")))?;
    if v["ok"] == true {
        Ok(v)
    } else {
        Err(ClientError::Refused(
            v["error"].as_str().unwrap_or("refused").to_owned(),
        ))
    }
}

/// `status` of the watcher at `path`.
pub fn status(path: &Path, timeout: Duration) -> Result<Value, ClientError> {
    request(path, &json!({"op": "status"}), timeout)
}

/// Ask the watcher at `path` to stop with `grace`.
pub fn stop(path: &Path, grace: Duration, timeout: Duration) -> Result<(), ClientError> {
    request(
        path,
        &json!({"op": "stop", "grace_ms": grace.as_millis() as u64}),
        timeout,
    )
    .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream as Us;

    fn reply_with(state: &'static str) -> StatusReply {
        StatusReply {
            pid: Some(7),
            pgid: Some(7),
            state,
            elapsed_ms: 5,
        }
    }

    /// Serve until `stop` or `n` rounds.
    fn pump(s: &mut ControlServer) -> Option<Duration> {
        let mut got = None;
        for _ in 0..200 {
            got = got.or(s.service(&|| reply_with("progressing")));
            if got.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        got
    }

    #[test]
    fn status_and_stop_round_trip_and_the_socket_is_private_and_removed() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("c.sock");
        let mut s = ControlServer::bind(&p).unwrap();
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        let pp = p.clone();
        let t = std::thread::spawn(move || {
            let st = status(&pp, Duration::from_secs(5)).unwrap();
            stop(&pp, Duration::from_millis(1500), Duration::from_secs(5)).unwrap();
            st
        });
        assert_eq!(pump(&mut s), Some(Duration::from_millis(1500)));
        let st = t.join().unwrap();
        assert_eq!(
            (st["pid"].as_i64(), st["state"].as_str(), st["elapsed_ms"].as_u64()),
            (Some(7), Some("progressing"), Some(5))
        );
        drop(s);
        assert!(!p.exists(), "the socket is removed on exit");
    }

    #[test]
    fn junk_oversize_and_idle_connections_never_block_the_server() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("c.sock");
        let mut s = ControlServer::bind(&p).unwrap();
        let mut idle = Us::connect(&p).unwrap(); // says nothing
        let mut junk = Us::connect(&p).unwrap();
        junk.write_all(b"not json\n").unwrap();
        let mut big = Us::connect(&p).unwrap();
        big.write_all(&vec![b'x'; MAX_REQUEST + 10]).unwrap();
        let mut unknown = Us::connect(&p).unwrap();
        unknown.write_all(b"{\"op\":\"reboot\"}\n").unwrap();
        for _ in 0..20 {
            assert!(s.service(&|| reply_with("x")).is_none());
            std::thread::sleep(Duration::from_millis(5));
        }
        for c in [&mut junk, &mut big, &mut unknown] {
            c.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let mut text = String::new();
            c.read_to_string(&mut text).unwrap();
            assert!(text.contains("\"ok\":false"), "{text}");
        }
        // The idle one is still held (no reply, not closed) until its deadline.
        idle.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
        assert!(idle.read(&mut [0u8; 8]).is_err());
        // And a real request is still answered.
        let pp = p.clone();
        let t = std::thread::spawn(move || status(&pp, Duration::from_secs(5)).unwrap());
        for _ in 0..100 {
            s.service(&|| reply_with("x"));
            if t.is_finished() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(t.join().unwrap()["state"], "x");
    }

    #[test]
    fn bind_refuses_live_sockets_and_regular_files_but_replaces_stale_ones() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("c.sock");
        let live = ControlServer::bind(&p).unwrap();
        assert_eq!(ControlServer::bind(&p).err().unwrap().kind(), io::ErrorKind::AddrInUse);
        // A crashed supervisor leaves the file but nothing accepting.
        let stale = d.path().join("stale.sock");
        drop(UnixListener::bind(&stale).unwrap());
        assert!(stale.exists());
        drop(ControlServer::bind(&stale).unwrap());
        let file = d.path().join("file");
        std::fs::write(&file, "keep").unwrap();
        assert_eq!(
            ControlServer::bind(&file).err().unwrap().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "keep");
        let link = d.path().join("link");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        assert!(ControlServer::bind(&link).is_err());
        let long = d.path().join("x".repeat(MAX_SOCKET_PATH));
        assert_eq!(
            ControlServer::bind(&long).err().unwrap().kind(),
            io::ErrorKind::InvalidInput
        );
        drop(live);
    }

    #[test]
    fn a_dropped_server_does_not_remove_a_successors_socket() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("c.sock");
        let old = ControlServer::bind(&p).unwrap();
        // Simulate a crash-and-restart: the path now names another socket.
        std::fs::remove_file(&p).unwrap();
        let new = ControlServer::bind(&p).unwrap();
        drop(old);
        assert!(p.exists());
        drop(new);
        assert!(!p.exists());
    }

    #[test]
    fn requests_parse_strictly() {
        assert_eq!(parse(br#"{"op":"status"}"#), Ok(Request::Status));
        assert_eq!(
            parse(br#"{"op":"stop","grace_ms":250}"#),
            Ok(Request::Stop {
                grace: Duration::from_millis(250)
            })
        );
        assert!(parse(br#"{"op":"stop","grace_ms":-1}"#).is_err());
        assert!(parse(br#"{"op":"kill"}"#).is_err());
        assert!(parse(b"{}").is_err());
        assert!(matches!(
            parse(br#"{"op":"stop","grace_ms":18446744073709551615}"#),
            Ok(Request::Stop { grace }) if grace == MAX_GRACE
        ));
    }

    #[test]
    fn a_missing_socket_is_unreachable() {
        let d = tempfile::tempdir().unwrap();
        let e = status(&d.path().join("none"), Duration::from_secs(1)).unwrap_err();
        assert!(matches!(e, ClientError::Unreachable(_)), "{e}");
    }
}
