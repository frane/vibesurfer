//! The proxy itself: blocking sockets, one thread per browser
//! connection.
//!
//! Browsers reach it two ways. Chromium and libsoup send plain-http
//! requests in absolute form (`GET http://host/path`) and use
//! `CONNECT` for https. WebKit on macOS uses `CONNECT host:port` for
//! everything, so after a `CONNECT` the first byte decides: a TLS
//! handshake (0x16) is answered with a leaf certificate for the host
//! from the daemon's CA and the requests inside are read as
//! `https://` ones; anything else is read as plain HTTP requests for
//! that authority.
//!
//! Each request gets its own connection on both sides, with
//! `Connection: close`: a browser retries a request whose reused
//! connection closed without a response, which would undo an abort.
//! Bodies are re-framed (Content-Length when known, chunked otherwise).

use std::io::{self, BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

use super::classify::classify;
use super::http::{
    self, copy_body, get, parse_http_url, parse_status_line, read_body, read_head, request_framing,
    response_framing, strip_hop_by_hop, write_chunk, write_head, write_last_chunk, Framing, Head,
    Headers,
};
use super::tls::{Mitm, Stream};
use super::{Decision, Overrides, ProxyRequest, SessionNet};

/// An idle browser connection is closed after this long.
const BROWSER_IDLE: Duration = Duration::from_secs(120);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Upstream silence longer than this fails the request.
const UPSTREAM_READ: Duration = Duration::from_secs(300);

pub(super) fn serve(listener: &TcpListener, net: &Arc<SessionNet>) {
    for stream in listener.incoming() {
        if net.is_closed() {
            break;
        }
        let Ok(stream) = stream else { continue };
        let net = net.clone();
        let _ = std::thread::Builder::new()
            .name("vs-net-conn".into())
            .spawn(move || {
                if let Err(e) = handle_conn(&net, stream) {
                    tracing::debug!(error = %e, "proxy connection ended");
                }
            });
    }
}

/// Inside a `CONNECT`: the scheme and authority requests carry only
/// a path for.
struct Tunnel {
    https: bool,
    authority: String,
}

fn handle_conn(net: &SessionNet, stream: TcpStream) -> io::Result<()> {
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(BROWSER_IDLE))?;
    let mut conn = BufReader::new(Stream::Plain(stream));
    let mut tunnel: Option<Tunnel> = None;
    loop {
        let Some(head) = read_head(&mut conn)? else {
            conn.get_mut().finish();
            return Ok(());
        };
        let mut parts = head.line.split_whitespace();
        let method = parts.next().unwrap_or("").to_string();
        let target = parts.next().unwrap_or("").to_string();

        if method.eq_ignore_ascii_case("CONNECT") {
            if tunnel.is_some() {
                return Err(http::bad("CONNECT inside a tunnel"));
            }
            conn.get_mut()
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")?;
            let first = conn.fill_buf()?.first().copied();
            match first {
                None => return Ok(()),
                Some(0x16) => {
                    let early = conn.buffer().to_vec();
                    let Stream::Plain(sock) = conn.into_inner() else {
                        return Err(http::bad("TLS inside TLS"));
                    };
                    let host = split_authority(&target, 443).0;
                    conn = BufReader::new(net.mitm.accept(sock, &early, &host)?);
                    // Report `https://host/…` without the default port,
                    // as the browser would.
                    let a = target.strip_suffix(":443").unwrap_or(&target);
                    tunnel = Some(Tunnel {
                        https: true,
                        authority: a.to_string(),
                    });
                    continue;
                }
                Some(_) => {
                    // WebKit tunnels plain http as `CONNECT host:80`;
                    // report `http://host/…` as the browser would.
                    let a = target.strip_suffix(":80").unwrap_or(&target);
                    tunnel = Some(Tunnel {
                        https: false,
                        authority: a.to_string(),
                    });
                    continue;
                }
            }
        }

        let url = match (absolute_http(&target), &tunnel) {
            (Some(url), _) => url,
            (None, Some(t)) => format!(
                "{}://{}{target}",
                if t.https { "https" } else { "http" },
                t.authority
            ),
            (None, None) => match head.get("host") {
                Some(h) if target.starts_with('/') => format!("http://{h}{target}"),
                _ => return Err(http::bad("request target is not an http URL")),
            },
        };
        let body = read_body(&mut conn, request_framing(&head.headers))?;
        let plain_abort = head.get(LOOPBACK_MARK).is_some();
        let req = ProxyRequest {
            method,
            url,
            headers: head
                .headers
                .iter()
                .filter(|(k, _)| {
                    let k = k.to_ascii_lowercase();
                    !k.starts_with("proxy-") && k != LOOPBACK_MARK
                })
                .cloned()
                .collect(),
            body,
        };
        // One request per browser connection. A browser that sees a
        // reused connection close without a response retries the
        // request, which would turn an abort into a second attempt
        // (see `abort` for the retry CFNetwork makes even so).
        return exchange(net, &req, conn, plain_abort);
    }
}

/// `host` and port of a `host:port` authority (IPv6 unbracketed).
fn split_authority(authority: &str, default: u16) -> (String, u16) {
    if let Some(v6) = authority.strip_prefix('[') {
        let (h, after) = v6.split_once(']').unwrap_or((v6, ""));
        let port = after
            .strip_prefix(':')
            .and_then(|p| p.parse().ok())
            .unwrap_or(default);
        return (h.to_string(), port);
    }
    match authority.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().unwrap_or(default)),
        None => (authority.to_string(), default),
    }
}

/// Whether the browser closed (or wrote to) a connection that should be
/// silently waiting for its one response: either way it gave up on the
/// request.
fn browser_gone(sock: &TcpStream) -> bool {
    if sock.set_nonblocking(true).is_err() {
        return false;
    }
    let mut b = [0u8; 1];
    let gone = !matches!(sock.peek(&mut b), Err(ref e) if e.kind() == io::ErrorKind::WouldBlock);
    let _ = sock.set_nonblocking(false);
    gone
}

/// Record, maybe hold, then answer one request. The browser
/// connection is consumed: it carries one request.
/// Prepended to service worker scripts (see `forward`): it reports the
/// responses a worker builds itself or serves from its cache, which no
/// network request carries, to [`SW_REPORT_PATH`] on its own origin.
const SW_PRELUDE: &str = include_str!("sw_prelude.js");
/// Where the prelude posts its reports. Answered here, never forwarded
/// and never reported as a request of its own.
const SW_REPORT_PATH: &str = "/__vs_sw_report__";

fn is_sw_report(req: &ProxyRequest) -> bool {
    req.method.eq_ignore_ascii_case("POST")
        && get(&req.headers, "x-vs-sw-report") == Some("1")
        && parse_http_url(&req.url).is_ok_and(|u| u.path == SW_REPORT_PATH)
}

fn is_sw_script(req: &ProxyRequest) -> bool {
    get(&req.headers, "service-worker").is_some_and(|v| v.eq_ignore_ascii_case("script"))
}

/// Prepended to dedicated and shared worker scripts: it runs code for
/// the test on request (Playwright's worker.evaluate).
const WORKER_PRELUDE: &str = include_str!("worker_prelude.js");

fn is_worker_script(req: &ProxyRequest) -> bool {
    get(&req.headers, "sec-fetch-dest")
        .is_some_and(|v| v.eq_ignore_ascii_case("worker") || v.eq_ignore_ascii_case("sharedworker"))
}

/// `out` as sent upstream: a service worker script is asked for
/// uncompressed, so the prelude can go in front of it.
fn without_compression(out: &ProxyRequest, sw: bool) -> std::borrow::Cow<'_, ProxyRequest> {
    if !sw {
        return std::borrow::Cow::Borrowed(out);
    }
    let mut o = out.clone();
    o.headers
        .retain(|(k, _)| !k.eq_ignore_ascii_case("accept-encoding"));
    std::borrow::Cow::Owned(o)
}

fn exchange(
    net: &SessionNet,
    req: &ProxyRequest,
    mut conn: BufReader<Stream>,
    plain_abort: bool,
) -> io::Result<()> {
    if is_sw_report(req) {
        net.ev_service_worker(&req.body);
        let w = conn.get_mut();
        write_head(w, "HTTP/1.1 204 No Content", &vec![close_field()])?;
        w.flush()?;
        w.finish();
        return Ok(());
    }
    let id = net.next_id();
    // Held before it is announced, so an immediate decision finds it.
    let parked = net.routing().then(|| net.park(id, req));
    net.ev_request(id, req, classify(&req.method, &req.url, &req.headers));
    let decision = match parked {
        Some(rx) => {
            let gone = || browser_gone(conn.get_ref().sock());
            net.hold(id, &rx, &gone)
        }
        None => Decision::Continue(Overrides::default()),
    };
    match decision {
        Decision::Abort(error) => {
            net.ev_failed(id, &error);
            abort(conn.get_mut(), plain_abort);
            Ok(())
        }
        Decision::Fulfill {
            status,
            headers,
            body,
        } => {
            let reason = http::reason(status);
            net.ev_response(id, status, reason, &req.url, &headers);
            let mut hs = strip_hop_by_hop(&headers);
            hs.push(("Content-Length".into(), body.len().to_string()));
            hs.push(close_field());
            let w = conn.get_mut();
            write_head(w, &format!("HTTP/1.1 {status} {reason}"), &hs)?;
            if !req.method.eq_ignore_ascii_case("HEAD") {
                w.write_all(&body)?;
            }
            w.flush()?;
            w.finish();
            net.ev_finished(id, body.len() as u64);
            net.keep_body(id, body);
            Ok(())
        }
        Decision::Continue(o) => {
            let (out, url_changed) = o.apply(req);
            forward(net, id, req, &out, url_changed, conn, plain_abort)
        }
    }
}

/// The header the macOS loopback scheme handler (`vs-engine-webkit`'s
/// `loopback.rs`) marks its requests with. It reports a closed
/// connection as a network error itself, so an abort is a plain close
/// for it, without the head CFNetwork needs. Taken off before anything
/// else sees the request.
const LOOPBACK_MARK: &str = "x-vs-loopback";

/// Fail the browser's request without a retry. On macOS, CFNetwork
/// silently retries a request whose connection closed before any
/// response bytes, and treats a malformed header line as a response
/// (fetch resolves with it). A response head that promises a body,
/// followed by a close, is a hard network error there: fetch rejects
/// and a navigation fails, with no retry. Chromium and libsoup do not
/// retry a fresh connection, so elsewhere the connection is just
/// dropped.
fn abort(conn: &mut Stream, plain: bool) {
    if cfg!(target_os = "macos") && !plain {
        let _ = conn.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1048576\r\n\r\n");
        let _ = conn.flush();
    }
    conn.kill();
}

fn close_field() -> (String, String) {
    ("Connection".into(), "close".into())
}

/// An absolute-form request target as the http(s) URL to fetch. libsoup
/// (WebKitGTK) asks a proxy for a WebSocket as `GET ws://host/path`;
/// the handshake is plain HTTP, so `ws`/`wss` map to `http`/`https`.
pub(super) fn absolute_http(target: &str) -> Option<String> {
    let (scheme, rest) = target.split_once("://")?;
    let scheme = match scheme.to_ascii_lowercase().as_str() {
        "http" | "ws" => "http",
        "https" | "wss" => "https",
        _ => return None,
    };
    Some(format!("{scheme}://{rest}"))
}

/// Upgrade request (WebSocket): keep `Connection`/`Upgrade` upstream.
fn upgrade_of(headers: &Headers) -> Option<String> {
    let conn_upgrade =
        get(headers, "connection").is_some_and(|v| v.to_ascii_lowercase().contains("upgrade"));
    get(headers, "upgrade")
        .filter(|_| conn_upgrade)
        .map(str::to_string)
}

/// Open an upstream connection (TLS for https, verified), send `out`,
/// read up to the final response head.
fn send_upstream(
    mitm: &Mitm,
    out: &ProxyRequest,
    url_changed: bool,
    insecure: bool,
) -> io::Result<(BufReader<Stream>, u16, String, Head)> {
    let target = parse_http_url(&out.url)?;
    let sock = connect(&target.host, target.port)?;
    sock.set_nodelay(true)?;
    sock.set_read_timeout(Some(UPSTREAM_READ))?;
    let mut stream = if target.tls {
        sock.set_read_timeout(Some(CONNECT_TIMEOUT))?;
        let s = mitm.connect(sock, &target.host, insecure)?;
        s.set_read_timeout(Some(UPSTREAM_READ))?;
        s
    } else {
        Stream::Plain(sock)
    };
    let upgrade = upgrade_of(&out.headers);
    let mut hs = strip_hop_by_hop(&out.headers);
    let has_host = hs.iter().any(|(k, _)| k.eq_ignore_ascii_case("host"));
    if url_changed || !has_host {
        hs.retain(|(k, _)| !k.eq_ignore_ascii_case("host"));
        hs.insert(0, ("Host".into(), target.authority.clone()));
    }
    let method = out.method.to_ascii_uppercase();
    if !out.body.is_empty() || matches!(method.as_str(), "POST" | "PUT" | "PATCH") {
        hs.push(("Content-Length".into(), out.body.len().to_string()));
    }
    match &upgrade {
        Some(u) => {
            hs.push(("Connection".into(), "Upgrade".into()));
            hs.push(("Upgrade".into(), u.clone()));
        }
        None => hs.push(close_field()),
    }
    write_head(
        &mut stream,
        &format!("{} {} HTTP/1.1", out.method, target.path),
        &hs,
    )?;
    stream.write_all(&out.body)?;
    stream.flush()?;
    let mut r = BufReader::new(stream);
    loop {
        let head = read_head(&mut r)?
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "empty response"))?;
        let (status, reason) = parse_status_line(&head.line)?;
        // Interim responses are consumed here; 101 is final.
        if (100..200).contains(&status) && status != 101 {
            continue;
        }
        return Ok((r, status, reason, head));
    }
}

/// Resolve and connect. `localhost` and `*.localhost` go to loopback
/// without asking the resolver: WebKit on macOS only proxies loopback
/// servers under a `*.localhost` name, which system resolvers do not
/// always map.
fn connect(host: &str, port: u16) -> io::Result<TcpStream> {
    let lower = host.to_ascii_lowercase();
    let host = if lower == "localhost" || lower.ends_with(".localhost") {
        "127.0.0.1"
    } else {
        host
    };
    let mut last = io::Error::new(io::ErrorKind::NotFound, format!("cannot resolve {host}"));
    for addr in (host, port).to_socket_addrs()? {
        match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
            Ok(s) => return Ok(s),
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// A `101 Switching Protocols` (WebSocket): hand the head on, then
/// pipe both ways.
fn upgrade(
    net: &SessionNet,
    id: u64,
    conn: BufReader<Stream>,
    up_r: BufReader<Stream>,
    head: &Head,
) -> io::Result<()> {
    let leftover_browser = conn.buffer().to_vec();
    let leftover_up = up_r.buffer().to_vec();
    let mut browser = conn.into_inner();
    let mut upstream = up_r.into_inner();
    write_head(&mut browser, &head.line, &head.headers)?;
    browser.write_all(&leftover_up)?;
    browser.flush()?;
    net.ev_finished(id, 0);
    upstream.write_all(&leftover_browser)?;
    upstream.flush()?;
    pipe(browser, upstream);
    Ok(())
}

fn forward(
    net: &SessionNet,
    id: u64,
    req: &ProxyRequest,
    out: &ProxyRequest,
    url_changed: bool,
    mut conn: BufReader<Stream>,
    plain_abort: bool,
) -> io::Result<()> {
    let sw = is_sw_script(req);
    let worker = is_worker_script(req);
    let out = &without_compression(out, sw || worker);
    let (mut up_r, status, reason, head) =
        match send_upstream(&net.mitm, out, url_changed, net.insecure()) {
            Ok(v) => v,
            Err(e) => {
                net.ev_failed(id, &e.to_string());
                abort(conn.get_mut(), plain_abort);
                return Ok(());
            }
        };
    net.ev_response(id, status, &reason, &req.url, &head.headers);

    if status == 101 {
        return upgrade(net, id, conn, up_r, &head);
    }

    let framing = response_framing(&out.method, status, &head.headers);
    let encoded = get(&head.headers, "content-encoding")
        .is_some_and(|e| !e.trim().eq_ignore_ascii_case("identity"));
    let prelude: &[u8] = match (sw, worker) {
        _ if status != 200 || encoded => b"",
        (true, _) => SW_PRELUDE.as_bytes(),
        (_, true) => WORKER_PRELUDE.as_bytes(),
        _ => b"",
    };
    let mut hs = strip_hop_by_hop(&head.headers);
    let chunked = match framing {
        Framing::Empty => {
            // HEAD and 304 may describe a body they do not carry.
            if let Some(cl) = get(&head.headers, "content-length") {
                hs.push(("Content-Length".into(), cl.to_string()));
            }
            false
        }
        Framing::Length(n) => {
            hs.push((
                "Content-Length".into(),
                (n + prelude.len() as u64).to_string(),
            ));
            false
        }
        Framing::Chunked | Framing::Close => {
            hs.push(("Transfer-Encoding".into(), "chunked".into()));
            true
        }
    };
    hs.push(close_field());
    let status_line = if reason.is_empty() {
        format!("HTTP/1.1 {status} {}", http::reason(status))
    } else {
        format!("HTTP/1.1 {status} {reason}")
    };
    let writer = conn.get_mut();
    write_head(writer, &status_line, &hs)?;
    if !prelude.is_empty() {
        if chunked {
            write_chunk(writer, prelude)?;
        } else {
            writer.write_all(prelude)?;
        }
    }

    let mut kept: Option<Vec<u8>> = Some(Vec::new());
    let copied = copy_body(&mut up_r, framing, &mut |b| {
        if let Some(k) = kept.as_mut() {
            if k.len() + b.len() > super::BODY_CACHE_BYTES {
                kept = None;
            } else {
                k.extend_from_slice(b);
            }
        }
        if chunked {
            write_chunk(writer, b)
        } else {
            writer.write_all(b)
        }
    });
    drop(up_r);
    match copied {
        Ok(n) => {
            if chunked {
                write_last_chunk(writer)?;
            }
            writer.flush()?;
            writer.finish();
            net.ev_finished(id, n);
            if let Some(k) = kept {
                net.keep_body(id, k);
            }
            Ok(())
        }
        Err(e) => {
            net.ev_failed(id, &e.to_string());
            writer.kill();
            Ok(())
        }
    }
}

/// `vs_net_fetch`: the whole exchange, into memory.
pub(super) fn fetch_full(
    mitm: &Mitm,
    out: &ProxyRequest,
    url_changed: bool,
    insecure: bool,
) -> io::Result<(u16, String, Headers, Vec<u8>)> {
    let (mut r, status, reason, head) = send_upstream(mitm, out, url_changed, insecure)?;
    let body = read_body(&mut r, response_framing(&out.method, status, &head.headers))?;
    let reason = if reason.is_empty() {
        http::reason(status).to_string()
    } else {
        reason
    };
    Ok((status, reason, head.headers, body))
}

/// Copy both directions until either side closes.
fn pipe(a: Stream, b: Stream) {
    let _ = a.set_read_timeout(None);
    let _ = b.set_read_timeout(None);
    let (Ok(mut a_r), Ok(mut b_w)) = (a.try_clone(), b.try_clone()) else {
        return;
    };
    let t = std::thread::spawn(move || {
        let _ = io::copy(&mut a_r, &mut b_w);
        b_w.finish();
    });
    let (mut b_r, mut a_w) = (b, a);
    let _ = io::copy(&mut b_r, &mut a_w);
    a_w.finish();
    let _ = t.join();
}
