//! A deliberately tiny HTTP/1.1 POST client with one wall-clock deadline
//! covering DNS, connect, TLS handshake, write and read. `http://` and
//! `https://` (rustls with the `ring` provider; trust roots are the bundled
//! Mozilla set plus any PEM bundle named by `SSL_CERT_FILE`). No async
//! runtime, so the deadline is ours to enforce on the socket.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::{Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq)]
pub struct Url {
    pub tls: bool,
    pub host: String,
    pub port: u16,
    pub path: String,
}

pub fn parse_url(url: &str) -> Result<Url, String> {
    let (tls, rest) = if let Some(r) = url.strip_prefix("https://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("http://") {
        (false, r)
    } else {
        return Err(format!("unsupported URL {url:?}: use http:// or https://"));
    };
    let default_port = if tls { 443 } else { 80 };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    if authority.is_empty() || authority.contains('@') {
        return Err(format!("unsupported URL authority in {url:?}"));
    }
    let (host, port) = if let Some(h) = authority.strip_prefix('[') {
        let end = h.find(']').ok_or_else(|| format!("bad IPv6 literal in {url:?}"))?;
        let port = match &h[end + 1..] {
            "" => default_port,
            p => p
                .strip_prefix(':')
                .and_then(|p| p.parse().ok())
                .ok_or_else(|| format!("bad port in {url:?}"))?,
        };
        (h[..end].to_string(), port)
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.parse().map_err(|_| format!("bad port in {url:?}"))?),
            None => (authority.to_string(), default_port),
        }
    };
    Ok(Url {
        tls,
        host,
        port,
        path: path.to_string(),
    })
}

fn remaining(deadline: Instant) -> Result<Duration, String> {
    let now = Instant::now();
    if now >= deadline {
        Err("timeout".into())
    } else {
        Ok(deadline - now)
    }
}

type DnsCache = Mutex<HashMap<(String, u16), Vec<SocketAddr>>>;
static DNS: OnceLock<DnsCache> = OnceLock::new();

/// Resolve once per process, under the deadline. The resolver runs on a
/// detached thread so a hung lookup costs the deadline, never more.
pub fn resolve(host: &str, port: u16, deadline: Instant) -> Result<Vec<SocketAddr>, String> {
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return Ok(vec![SocketAddr::new(ip, port)]);
    }
    let cache = DNS.get_or_init(Default::default);
    let key = (host.to_string(), port);
    if let Some(a) = cache.lock().unwrap().get(&key) {
        return Ok(a.clone());
    }
    let (tx, rx) = mpsc::channel();
    let h = host.to_string();
    std::thread::spawn(move || {
        let _ = tx.send((h.as_str(), port).to_socket_addrs().map(|a| a.collect::<Vec<_>>()));
    });
    let addrs = match rx.recv_timeout(remaining(deadline)?) {
        Ok(Ok(a)) if !a.is_empty() => a,
        Ok(Ok(_)) => return Err(format!("dns {host}: no addresses")),
        Ok(Err(e)) => return Err(format!("dns {host}: {e}")),
        Err(_) => return Err(format!("dns {host}: timeout")),
    };
    cache.lock().unwrap().insert(key, addrs.clone());
    Ok(addrs)
}

#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

/// POST `body` as JSON; everything must finish before `deadline`.
pub fn post_json(url: &str, body: &[u8], deadline: Instant) -> Result<Response, String> {
    let u = parse_url(url)?;
    let addrs = resolve(&u.host, u.port, deadline)?;
    let mut last = String::from("no address");
    let mut stream = None;
    for a in addrs {
        match TcpStream::connect_timeout(&a, remaining(deadline)?) {
            Ok(s) => {
                stream = Some(s);
                break;
            }
            Err(e) => last = format!("connect {a}: {e}"),
        }
    }
    let tcp = stream.ok_or(last)?;
    let _ = tcp.set_nodelay(true);
    // Every socket read and write (TLS handshake included) re-arms its
    // timeout from the one deadline, so a server that drips bytes cannot
    // stretch the call.
    let tcp = DeadlineStream { tcp, deadline };
    let mut s: Box<dyn ReadWrite> = if u.tls {
        let name = rustls::pki_types::ServerName::try_from(u.host.clone()).map_err(|e| format!("tls name: {e}"))?;
        let conn = rustls::ClientConnection::new(tls_config()?, name).map_err(|e| format!("tls: {e}"))?;
        Box::new(rustls::StreamOwned::new(conn, tcp))
    } else {
        Box::new(tcp)
    };
    let host_hdr = if u.host.contains(':') {
        format!("[{}]", u.host)
    } else {
        u.host.clone()
    };
    let head = format!(
        "POST {} HTTP/1.1\r\nHost: {}:{}\r\nContent-Type: application/json\r\nAccept: application/json\r\nUser-Agent: watcher-s1/{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        u.path,
        host_hdr,
        u.port,
        env!("CARGO_PKG_VERSION"),
        body.len()
    );
    s.write_all(head.as_bytes())
        .and_then(|_| s.write_all(body))
        .map_err(|e| io_err("write", e))?;

    let mut raw = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match s.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                raw.extend_from_slice(&buf[..n]);
                if let Some(done) = complete(&raw, false)? {
                    return Ok(done);
                }
                if raw.len() > 16 << 20 {
                    return Err("response too large".into());
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            // TLS peer closed without close_notify: for a `Connection: close`
            // response that is the end of the body; `complete` still checks
            // the framing below.
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(io_err("read", e)),
        }
    }
    complete(&raw, true)?.ok_or_else(|| "truncated response".into())
}

/// A TCP stream whose every read and write is bounded by `deadline`.
struct DeadlineStream {
    tcp: TcpStream,
    deadline: Instant,
}

impl DeadlineStream {
    fn arm(&self) -> std::io::Result<()> {
        let left = self.deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "deadline"));
        }
        // Darwin answers EINVAL to setsockopt once the peer has shut the
        // connection down. That means "closed": skip the bound and let the
        // read or write that follows report it (EOF or an error).
        for r in [
            self.tcp.set_read_timeout(Some(left)),
            self.tcp.set_write_timeout(Some(left)),
        ] {
            match r {
                Err(e) if e.kind() == std::io::ErrorKind::InvalidInput => {}
                other => other?,
            }
        }
        Ok(())
    }
}

impl Read for DeadlineStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.arm()?;
        self.tcp.read(buf)
    }
}

impl Write for DeadlineStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.arm()?;
        self.tcp.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.tcp.flush()
    }
}

trait ReadWrite: Read + Write {}
impl<T: Read + Write> ReadWrite for T {}

type TlsConfig = std::sync::Arc<rustls::ClientConfig>;

/// One TLS client config per process: bundled roots + SSL_CERT_FILE.
fn tls_config() -> Result<TlsConfig, String> {
    static CFG: OnceLock<Result<TlsConfig, String>> = OnceLock::new();
    CFG.get_or_init(|| {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        if let Some(path) = std::env::var_os("SSL_CERT_FILE") {
            use rustls::pki_types::{CertificateDer, pem::PemObject};
            let certs = CertificateDer::pem_file_iter(&path).map_err(|e| format!("SSL_CERT_FILE: {e}"))?;
            for c in certs {
                let c = c.map_err(|e| format!("SSL_CERT_FILE: {e}"))?;
                roots.add(c).map_err(|e| format!("SSL_CERT_FILE: {e}"))?;
            }
        }
        let provider = std::sync::Arc::new(rustls::crypto::ring::default_provider());
        let cfg = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| format!("tls: {e}"))?
            .with_root_certificates(roots)
            .with_no_client_auth();
        Ok(std::sync::Arc::new(cfg))
    })
    .clone()
}

fn io_err(what: &str, e: std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => "timeout".into(),
        _ => format!("{what}: {e}"),
    }
}

/// Parse `raw` if it holds a complete response (Content-Length or chunked).
fn complete(raw: &[u8], eof: bool) -> Result<Option<Response>, String> {
    let Some(hend) = find(raw, b"\r\n\r\n") else {
        return Ok(None);
    };
    let head = std::str::from_utf8(&raw[..hend]).map_err(|_| "non-UTF-8 response head")?;
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("bad status line {status_line:?}"))?;
    let mut len = None;
    let mut chunked = false;
    for l in lines {
        if let Some((k, v)) = l.split_once(':') {
            let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
            if k == "content-length" {
                len = Some(v.parse::<usize>().map_err(|_| "bad content-length")?);
            } else if k == "transfer-encoding" && v.to_ascii_lowercase().contains("chunked") {
                chunked = true;
            }
        }
    }
    let body = &raw[hend + 4..];
    if chunked {
        return Ok(dechunk(body).map(|b| Response { status, body: b }));
    }
    Ok(match len {
        Some(n) if body.len() >= n => Some(Response {
            status,
            body: body[..n].to_vec(),
        }),
        Some(_) => None,
        // No framing: the body runs to EOF.
        None if eof => Some(Response {
            status,
            body: body.to_vec(),
        }),
        None => None,
    })
}

fn dechunk(mut b: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let le = find(b, b"\r\n")?;
        let size_s = std::str::from_utf8(&b[..le]).ok()?;
        let size = usize::from_str_radix(size_s.split(';').next()?.trim(), 16).ok()?;
        b = &b[le + 2..];
        if size == 0 {
            return Some(out);
        }
        if b.len() < size + 2 {
            return None;
        }
        out.extend_from_slice(&b[..size]);
        b = &b[size + 2..];
    }
}

fn find(h: &[u8], n: &[u8]) -> Option<usize> {
    h.windows(n.len()).position(|w| w == n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_urls() {
        assert_eq!(
            parse_url("http://sage.example:8023/v1/systemone").unwrap(),
            Url {
                tls: false,
                host: "sage.example".into(),
                port: 8023,
                path: "/v1/systemone".into()
            }
        );
        assert_eq!(parse_url("http://h").unwrap().port, 80);
        assert_eq!(parse_url("http://[::1]:9/x").unwrap().host, "::1");
        let u = parse_url("https://h/x").unwrap();
        assert!(u.tls && u.port == 443);
        assert!(parse_url("ftp://h/x").unwrap_err().contains("http:// or https://"));
        assert!(parse_url("http://h:notaport/x").is_err());
    }

    #[test]
    fn parses_content_length_and_chunked() {
        let r = complete(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nhi", false)
            .unwrap()
            .unwrap();
        assert_eq!((r.status, r.body.as_slice()), (200, &b"hi"[..]));
        assert!(
            complete(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhi", true)
                .unwrap()
                .is_none()
        );
        let r = complete(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nhi\r\n1\r\n!\r\n0\r\n\r\n",
            false,
        )
        .unwrap()
        .unwrap();
        assert_eq!(r.body, b"hi!");
        let r = complete(b"HTTP/1.0 200 OK\r\n\r\n{}", true).unwrap().unwrap();
        assert_eq!(r.body, b"{}");
    }
}
