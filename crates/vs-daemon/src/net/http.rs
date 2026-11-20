//! Minimal blocking HTTP/1.1 framing for the intercept proxy: read a
//! message head, read or stream a body by its framing, write heads and
//! chunked bodies. Nothing here knows about sessions or events.

use std::io::{self, BufRead, Write};

/// Largest message head the proxy accepts.
const MAX_HEAD: usize = 64 * 1024;

pub(crate) type Headers = Vec<(String, String)>;

/// A request or status line plus its header fields, as received.
#[derive(Debug, Clone)]
pub(crate) struct Head {
    pub(crate) line: String,
    pub(crate) headers: Headers,
}

impl Head {
    pub(crate) fn get(&self, name: &str) -> Option<&str> {
        get(&self.headers, name)
    }
}

pub(crate) fn get<'a>(headers: &'a Headers, name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// Read one message head. `Ok(None)` on a clean EOF before any byte,
/// which is how an idle keep-alive connection ends.
pub(crate) fn read_head(r: &mut impl BufRead) -> io::Result<Option<Head>> {
    let mut total = 0usize;
    let mut line = String::new();
    // Tolerate stray CRLFs between messages (RFC 9112 2.2).
    loop {
        line.clear();
        let n = r.read_line(&mut line)?;
        if n == 0 {
            return Ok(None);
        }
        total += n;
        if !line.trim().is_empty() {
            break;
        }
        if total > MAX_HEAD {
            return Err(bad("head too large"));
        }
    }
    let start = line.trim_end().to_string();
    let mut headers = Vec::new();
    loop {
        line.clear();
        let n = r.read_line(&mut line)?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "eof in head"));
        }
        total += n;
        if total > MAX_HEAD {
            return Err(bad("head too large"));
        }
        let l = line.trim_end_matches(['\r', '\n']);
        if l.is_empty() {
            break;
        }
        if let Some((k, v)) = l.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    Ok(Some(Head {
        line: start,
        headers,
    }))
}

/// How a message body is delimited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Framing {
    Empty,
    Length(u64),
    Chunked,
    /// Until the peer closes (responses only).
    Close,
}

fn is_chunked(headers: &Headers) -> bool {
    get(headers, "transfer-encoding").is_some_and(|v| v.to_ascii_lowercase().contains("chunked"))
}

fn content_length(headers: &Headers) -> Option<u64> {
    get(headers, "content-length").and_then(|v| v.trim().parse().ok())
}

pub(crate) fn request_framing(headers: &Headers) -> Framing {
    if is_chunked(headers) {
        Framing::Chunked
    } else {
        match content_length(headers) {
            Some(0) | None => Framing::Empty,
            Some(n) => Framing::Length(n),
        }
    }
}

pub(crate) fn response_framing(method: &str, status: u16, headers: &Headers) -> Framing {
    if method.eq_ignore_ascii_case("HEAD")
        || (100..200).contains(&status)
        || status == 204
        || status == 304
    {
        Framing::Empty
    } else if is_chunked(headers) {
        Framing::Chunked
    } else {
        match content_length(headers) {
            Some(n) => Framing::Length(n),
            None => Framing::Close,
        }
    }
}

/// Read a body by `framing`, handing each decoded piece to `sink`.
/// Returns the decoded size.
pub(crate) fn copy_body(
    r: &mut impl BufRead,
    framing: Framing,
    sink: &mut dyn FnMut(&[u8]) -> io::Result<()>,
) -> io::Result<u64> {
    let mut buf = vec![0u8; 32 * 1024];
    match framing {
        Framing::Empty => Ok(0),
        Framing::Length(n) => {
            let mut left = n;
            while left > 0 {
                let want = usize::try_from(left.min(buf.len() as u64)).unwrap_or(buf.len());
                let got = r.read(&mut buf[..want])?;
                if got == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "body cut short",
                    ));
                }
                sink(&buf[..got])?;
                left -= got as u64;
            }
            Ok(n)
        }
        Framing::Close => {
            let mut total = 0u64;
            loop {
                let got = r.read(&mut buf)?;
                if got == 0 {
                    return Ok(total);
                }
                sink(&buf[..got])?;
                total += got as u64;
            }
        }
        Framing::Chunked => {
            let mut total = 0u64;
            let mut line = String::new();
            loop {
                line.clear();
                if r.read_line(&mut line)? == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "eof in chunk size",
                    ));
                }
                let size_str = line.trim().split(';').next().unwrap_or("").trim();
                let size = u64::from_str_radix(size_str, 16).map_err(|_| bad("bad chunk size"))?;
                if size == 0 {
                    // Trailers, then the final empty line.
                    loop {
                        line.clear();
                        if r.read_line(&mut line)? == 0 || line.trim().is_empty() {
                            return Ok(total);
                        }
                    }
                }
                let mut left = size;
                while left > 0 {
                    let want = usize::try_from(left.min(buf.len() as u64)).unwrap_or(buf.len());
                    let got = r.read(&mut buf[..want])?;
                    if got == 0 {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "chunk cut short",
                        ));
                    }
                    sink(&buf[..got])?;
                    left -= got as u64;
                }
                total += size;
                line.clear();
                r.read_line(&mut line)?;
            }
        }
    }
}

/// Read a whole body into memory.
pub(crate) fn read_body(r: &mut impl BufRead, framing: Framing) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    copy_body(r, framing, &mut |b| {
        out.extend_from_slice(b);
        Ok(())
    })?;
    Ok(out)
}

/// Write a head: start line, fields, blank line.
pub(crate) fn write_head(w: &mut impl Write, line: &str, headers: &Headers) -> io::Result<()> {
    let mut out = String::with_capacity(256);
    out.push_str(line);
    out.push_str("\r\n");
    for (k, v) in headers {
        out.push_str(k);
        out.push_str(": ");
        out.push_str(v);
        out.push_str("\r\n");
    }
    out.push_str("\r\n");
    w.write_all(out.as_bytes())
}

pub(crate) fn write_chunk(w: &mut impl Write, data: &[u8]) -> io::Result<()> {
    if data.is_empty() {
        return Ok(());
    }
    write!(w, "{:x}\r\n", data.len())?;
    w.write_all(data)?;
    w.write_all(b"\r\n")
}

pub(crate) fn write_last_chunk(w: &mut impl Write) -> io::Result<()> {
    w.write_all(b"0\r\n\r\n")
}

/// Fields that describe one connection hop, never forwarded as-is.
/// Names listed in a `Connection` field are hop-by-hop too.
pub(crate) fn strip_hop_by_hop(headers: &Headers) -> Headers {
    let listed: Vec<String> = get(headers, "connection")
        .map(|v| {
            v.split(',')
                .map(|s| s.trim().to_ascii_lowercase())
                .collect()
        })
        .unwrap_or_default();
    headers
        .iter()
        .filter(|(k, _)| {
            let k = k.to_ascii_lowercase();
            !matches!(
                k.as_str(),
                "connection"
                    | "proxy-connection"
                    | "keep-alive"
                    | "proxy-authorization"
                    | "proxy-authenticate"
                    | "te"
                    | "trailer"
                    | "transfer-encoding"
                    | "content-length"
                    | "upgrade"
            ) && !listed.contains(&k)
        })
        .cloned()
        .collect()
}

/// The standard reason phrase for `status`, or `""`.
pub(crate) fn reason(status: u16) -> &'static str {
    match status {
        100 => "Continue",
        101 => "Switching Protocols",
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        206 => "Partial Content",
        301 => "Moved Permanently",
        302 => "Found",
        303 => "See Other",
        304 => "Not Modified",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        410 => "Gone",
        413 => "Content Too Large",
        415 => "Unsupported Media Type",
        422 => "Unprocessable Content",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "",
    }
}

/// Split `HTTP/1.1 200 OK` into status and reason text.
pub(crate) fn parse_status_line(line: &str) -> io::Result<(u16, String)> {
    let mut it = line.splitn(3, ' ');
    let proto = it.next().unwrap_or("");
    if !proto.starts_with("HTTP/") {
        return Err(bad("not an HTTP response"));
    }
    let status = it
        .next()
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| bad("bad status"))?;
    Ok((status, it.next().unwrap_or("").trim().to_string()))
}

/// A parsed `http://` or `https://` URL, enough to open a connection
/// and write the request target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Target {
    /// `https`.
    pub(crate) tls: bool,
    /// Host as written, without brackets for IPv6.
    pub(crate) host: String,
    pub(crate) port: u16,
    /// `host[:port]` as it belongs in a `Host` field.
    pub(crate) authority: String,
    /// Path and query, at least `/`.
    pub(crate) path: String,
}

pub(crate) fn parse_http_url(url: &str) -> io::Result<Target> {
    let (tls, rest) = match url.split_once("://") {
        Some((s, rest)) if s.eq_ignore_ascii_case("http") => (false, rest),
        Some((s, rest)) if s.eq_ignore_ascii_case("https") => (true, rest),
        _ => return Err(bad("only http:// and https:// URLs can be forwarded")),
    };
    let rest = rest.split('#').next().unwrap_or(rest);
    let split = rest.find(['/', '?']).unwrap_or(rest.len());
    let (authority, path) = rest.split_at(split);
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    if authority.is_empty() {
        return Err(bad("URL has no host"));
    }
    let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
        let (h, after) = v6.split_once(']').ok_or_else(|| bad("bad IPv6 host"))?;
        let port = after.strip_prefix(':').map(str::parse::<u16>).transpose();
        (h.to_string(), port.map_err(|_| bad("bad port"))?)
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => (
                h.to_string(),
                Some(p.parse::<u16>().map_err(|_| bad("bad port"))?),
            ),
            None => (authority.to_string(), None),
        }
    };
    let path = if path.is_empty() {
        "/".to_string()
    } else if path.starts_with('?') {
        format!("/{path}")
    } else {
        path.to_string()
    };
    Ok(Target {
        tls,
        host,
        port: port.unwrap_or(if tls { 443 } else { 80 }),
        authority: authority.to_string(),
        path,
    })
}

pub(crate) fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read as _;

    #[test]
    fn parses_urls() {
        let t = parse_http_url("http://127.0.0.1:8080/a/b?c=1#frag").unwrap();
        assert_eq!(
            (
                t.host.as_str(),
                t.port,
                t.authority.as_str(),
                t.path.as_str()
            ),
            ("127.0.0.1", 8080, "127.0.0.1:8080", "/a/b?c=1")
        );
        let t = parse_http_url("http://example.com").unwrap();
        assert_eq!((t.port, t.path.as_str()), (80, "/"));
        let t = parse_http_url("http://[::1]:9/x").unwrap();
        assert_eq!((t.host.as_str(), t.port), ("::1", 9));
        let t = parse_http_url("http://h?q").unwrap();
        assert_eq!(t.path, "/?q");
        let t = parse_http_url("HTTPS://h/").unwrap();
        assert!(t.tls);
        assert_eq!(t.port, 443);
        assert!(parse_http_url("ftp://h/").is_err());
    }

    #[test]
    fn reads_chunked_and_length_bodies() {
        let mut r =
            io::Cursor::new(b"4\r\nWiki\r\n5;x=y\r\npedia\r\n0\r\nT: v\r\n\r\nrest".to_vec());
        assert_eq!(read_body(&mut r, Framing::Chunked).unwrap(), b"Wikipedia");
        let mut rest = String::new();
        r.read_to_string(&mut rest).unwrap();
        assert_eq!(rest, "rest");
        let mut r = io::Cursor::new(b"abcdef".to_vec());
        assert_eq!(read_body(&mut r, Framing::Length(3)).unwrap(), b"abc");
    }

    #[test]
    fn reads_heads_and_framing() {
        let mut r = io::Cursor::new(
            b"\r\nGET /x HTTP/1.1\r\nHost: a\r\nContent-Length: 2\r\n\r\nhi".to_vec(),
        );
        let h = read_head(&mut r).unwrap().unwrap();
        assert_eq!(h.line, "GET /x HTTP/1.1");
        assert_eq!(h.get("host"), Some("a"));
        assert_eq!(request_framing(&h.headers), Framing::Length(2));
        assert_eq!(response_framing("HEAD", 200, &h.headers), Framing::Empty);
        assert_eq!(response_framing("GET", 200, &vec![]), Framing::Close);
        assert!(read_head(&mut io::Cursor::new(Vec::new()))
            .unwrap()
            .is_none());
    }

    #[test]
    fn strips_hop_by_hop_fields() {
        let h: Headers = vec![
            ("Connection".into(), "keep-alive, X-Foo".into()),
            ("X-Foo".into(), "1".into()),
            ("Proxy-Connection".into(), "keep-alive".into()),
            ("Accept".into(), "*/*".into()),
        ];
        assert_eq!(strip_hop_by_hop(&h), vec![("Accept".into(), "*/*".into())]);
    }
}
