//! The HTTP/1.1 side of a hop: an absolute-form request to the intercept
//! proxy on a worker thread, and the response read back (head, then a
//! chunked or length-delimited body) as [`Msg`]s for the main queue.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};

use super::{post, Hop, Msg, READ_TIMEOUT};

/// Worker thread: one hop through the proxy, streamed back.
pub(super) fn exchange(
    id: u64,
    generation: u64,
    port: u16,
    hop: &Hop,
    cookie: Option<&str>,
    stopped: &AtomicBool,
) {
    if let Err(e) = exchange_inner(id, generation, port, hop, cookie, stopped) {
        if !stopped.load(Ordering::Relaxed) {
            post(id, generation, Msg::Fail(e.to_string()));
        }
    }
}

fn exchange_inner(
    id: u64,
    generation: u64,
    port: u16,
    hop: &Hop,
    cookie: Option<&str>,
    stopped: &AtomicBool,
) -> std::io::Result<()> {
    let mut sock = TcpStream::connect(("127.0.0.1", port))?;
    sock.set_nodelay(true)?;
    sock.set_read_timeout(Some(READ_TIMEOUT))?;
    sock.write_all(request_head(hop, cookie).as_bytes())?;
    sock.write_all(&hop.body)?;
    sock.flush()?;

    let mut r = BufReader::new(sock);
    let (status, headers) = loop {
        let (status, headers) = read_head(&mut r)?;
        // Interim responses are not the answer.
        if (100..200).contains(&status) && status != 101 {
            continue;
        }
        break (status, headers);
    };
    let header = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.trim().to_string())
    };
    let chunked =
        header("transfer-encoding").is_some_and(|v| v.to_ascii_lowercase().contains("chunked"));
    let length = header("content-length").and_then(|v| v.parse::<u64>().ok());
    let bodyless = hop.method.eq_ignore_ascii_case("HEAD") || status == 204 || status == 304;
    post(id, generation, Msg::Head { status, headers });
    if !bodyless {
        let send = |chunk: &[u8]| {
            if !chunk.is_empty() {
                post(id, generation, Msg::Data(chunk.to_vec()));
            }
        };
        if chunked {
            read_chunked(&mut r, stopped, send)?;
        } else {
            read_plain(&mut r, length, stopped, send)?;
        }
    }
    if !stopped.load(Ordering::Relaxed) {
        post(id, generation, Msg::Done);
    }
    Ok(())
}

/// The absolute-form request line and headers for `hop`.
fn request_head(hop: &Hop, cookie: Option<&str>) -> String {
    use std::fmt::Write as _;
    let authority = hop
        .url
        .split_once("://")
        .map_or("", |(_, r)| r.split(['/', '?', '#']).next().unwrap_or(""));
    let mut head = format!(
        "{} {} HTTP/1.1\r\nHost: {authority}\r\n",
        hop.method, hop.url
    );
    for (k, v) in &hop.headers {
        let lk = k.to_ascii_lowercase();
        if matches!(
            lk.as_str(),
            "host" | "content-length" | "connection" | "transfer-encoding"
        ) {
            continue;
        }
        let _ = write!(head, "{k}: {v}\r\n");
    }
    if let Some(c) = cookie {
        let _ = write!(head, "Cookie: {c}\r\n");
    }
    if !hop.body.is_empty() || !matches!(hop.method.as_str(), "GET" | "HEAD" | "OPTIONS") {
        let _ = write!(head, "Content-Length: {}\r\n", hop.body.len());
    }
    // Tells the proxy to abort with a plain close (see its LOOPBACK_MARK).
    head.push_str("X-Vs-Loopback: 1\r\nConnection: close\r\n\r\n");
    head
}

/// A chunked body, decoded, to `send`.
fn read_chunked(
    r: &mut impl BufRead,
    stopped: &AtomicBool,
    send: impl Fn(&[u8]),
) -> std::io::Result<()> {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let mut line = String::new();
        if r.read_line(&mut line)? == 0 {
            return Ok(());
        }
        let size = usize::from_str_radix(line.trim().split(';').next().unwrap_or("0"), 16)
            .map_err(|_| std::io::Error::other("bad chunk size"))?;
        if size == 0 {
            // Trailers, up to the empty line.
            loop {
                let mut t = String::new();
                if r.read_line(&mut t)? == 0 || t.trim().is_empty() {
                    return Ok(());
                }
            }
        }
        let mut left = size;
        while left > 0 {
            if stopped.load(Ordering::Relaxed) {
                return Ok(());
            }
            let cap = left.min(buf.len());
            let n = r.read(&mut buf[..cap])?;
            if n == 0 {
                return Err(std::io::Error::other("connection closed inside a chunk"));
            }
            send(&buf[..n]);
            left -= n;
        }
        let mut crlf = String::new();
        r.read_line(&mut crlf)?;
    }
}

/// A body of `length` bytes, or up to the close without one, to `send`.
fn read_plain(
    r: &mut impl Read,
    length: Option<u64>,
    stopped: &AtomicBool,
    send: impl Fn(&[u8]),
) -> std::io::Result<()> {
    let mut buf = vec![0u8; 64 * 1024];
    let mut left = length;
    loop {
        if stopped.load(Ordering::Relaxed) {
            return Ok(());
        }
        let want = left.map_or(buf.len(), |l| {
            usize::try_from(l).unwrap_or(usize::MAX).min(buf.len())
        });
        if want == 0 {
            return Ok(());
        }
        let n = r.read(&mut buf[..want])?;
        if n == 0 {
            if left.is_some_and(|l| l > 0) {
                return Err(std::io::Error::other(
                    "connection closed before the body ended",
                ));
            }
            return Ok(());
        }
        send(&buf[..n]);
        if let Some(l) = left.as_mut() {
            *l -= n as u64;
        }
    }
}

fn read_head(r: &mut impl BufRead) -> std::io::Result<(u16, Vec<(String, String)>)> {
    let mut line = String::new();
    if r.read_line(&mut line)? == 0 {
        return Err(std::io::Error::other(
            "the proxy closed the connection without a response",
        ));
    }
    let status = line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| std::io::Error::other(format!("bad status line: {}", line.trim())))?;
    let mut headers = Vec::new();
    loop {
        let mut h = String::new();
        if r.read_line(&mut h)? == 0 {
            break;
        }
        let h = h.trim_end_matches(['\r', '\n']);
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    Ok((status, headers))
}
