//! The proxy itself: blocking sockets, one thread per browser
//! connection.
//!
//! Browsers reach it two ways. Chromium and libsoup send plain-http
//! requests in absolute form (`GET http://host/path`) and use
//! `CONNECT` for https. WebKit on macOS uses `CONNECT host:port` for
//! everything, so after a `CONNECT` the first byte decides: a TLS
//! handshake (0x16) is tunnelled blind, anything else is read as HTTP
//! requests for that authority.
//!
//! Each request gets its own connection on both sides, with
//! `Connection: close`: a browser retries a request whose reused
//! connection closed without a response, which would undo an abort.
//! Bodies are re-framed (Content-Length when known, chunked otherwise).

use std::io::{self, BufRead, BufReader, Write};
use std::net::{Shutdown, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

use super::classify::classify;
use super::http::{
    self, copy_body, get, parse_http_url, parse_status_line, read_body, read_head, request_framing,
    response_framing, strip_hop_by_hop, write_chunk, write_head, write_last_chunk, Framing, Head,
    Headers,
};
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

fn handle_conn(net: &SessionNet, stream: TcpStream) -> io::Result<()> {
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(BROWSER_IDLE))?;
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    // Set inside a CONNECT tunnel: requests then carry only a path.
    let mut authority: Option<String> = None;
    loop {
        let Some(head) = read_head(&mut reader)? else {
            return Ok(());
        };
        let mut parts = head.line.split_whitespace();
        let method = parts.next().unwrap_or("").to_string();
        let target = parts.next().unwrap_or("").to_string();
        let version = parts.next().unwrap_or("HTTP/1.1").to_string();

        if method.eq_ignore_ascii_case("CONNECT") {
            if authority.is_some() {
                return Err(http::bad("CONNECT inside a tunnel"));
            }
            writer.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")?;
            let first = reader.fill_buf()?.first().copied();
            match first {
                None => return Ok(()),
                Some(0x16) => {
                    net.ev_tunnel(&target);
                    return blind_tunnel(reader, writer, &target);
                }
                Some(_) => {
                    // WebKit tunnels plain http as `CONNECT host:80`;
                    // report `http://host/…` as the browser would.
                    let a = target.strip_suffix(":80").unwrap_or(&target);
                    authority = Some(a.to_string());
                    continue;
                }
            }
        }

        let url = match &authority {
            _ if target.starts_with("http://") || target.starts_with("HTTP://") => target.clone(),
            Some(a) => format!("http://{a}{target}"),
            None => match head.get("host") {
                Some(h) if target.starts_with('/') => format!("http://{h}{target}"),
                _ => return Err(http::bad("request target is not an http URL")),
            },
        };
        let body = read_body(&mut reader, request_framing(&head.headers))?;
        // One request per browser connection. A browser that sees a
        // reused connection close without a response retries the
        // request, which would turn an abort into a second attempt;
        // on a fresh connection the abort stands.
        let _ = wants_keep_alive(&version, &head);
        let keep_alive = false;
        let req = ProxyRequest {
            method,
            url,
            headers: head
                .headers
                .iter()
                .filter(|(k, _)| !k.to_ascii_lowercase().starts_with("proxy-"))
                .cloned()
                .collect(),
            body,
        };
        if !exchange(net, &req, &mut reader, &mut writer, keep_alive)? {
            return Ok(());
        }
    }
}

fn wants_keep_alive(version: &str, head: &Head) -> bool {
    let says = |name: &str, what: &str| {
        head.get(name)
            .is_some_and(|v| v.to_ascii_lowercase().contains(what))
    };
    if says("connection", "close") || says("proxy-connection", "close") {
        return false;
    }
    version.eq_ignore_ascii_case("HTTP/1.1")
        || says("connection", "keep-alive")
        || says("proxy-connection", "keep-alive")
}

/// Record, maybe hold, then answer one request. Returns whether the
/// browser connection can carry another.
fn exchange(
    net: &SessionNet,
    req: &ProxyRequest,
    reader: &mut BufReader<TcpStream>,
    writer: &mut TcpStream,
    keep_alive: bool,
) -> io::Result<bool> {
    let id = net.next_id();
    net.ev_request(id, req, classify(&req.method, &req.url, &req.headers));
    let decision = if net.routing() {
        net.hold(id, req)
    } else {
        Decision::Continue(Overrides::default())
    };
    match decision {
        Decision::Abort(error) => {
            net.ev_failed(id, &error);
            let _ = writer.shutdown(Shutdown::Both);
            Ok(false)
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
            hs.push(connection_field(keep_alive));
            write_head(writer, &format!("HTTP/1.1 {status} {reason}"), &hs)?;
            if !req.method.eq_ignore_ascii_case("HEAD") {
                writer.write_all(&body)?;
            }
            writer.flush()?;
            net.ev_finished(id, body.len() as u64);
            net.keep_body(id, body);
            Ok(keep_alive)
        }
        Decision::Continue(o) => {
            let (out, url_changed) = o.apply(req);
            forward(net, id, req, &out, url_changed, reader, writer, keep_alive)
        }
    }
}

fn connection_field(keep_alive: bool) -> (String, String) {
    (
        "Connection".into(),
        if keep_alive { "keep-alive" } else { "close" }.into(),
    )
}

/// Upgrade request (WebSocket): keep `Connection`/`Upgrade` upstream.
fn upgrade_of(headers: &Headers) -> Option<String> {
    let conn_upgrade =
        get(headers, "connection").is_some_and(|v| v.to_ascii_lowercase().contains("upgrade"));
    get(headers, "upgrade")
        .filter(|_| conn_upgrade)
        .map(str::to_string)
}

/// Open an upstream connection, send `out`, read up to the final
/// response head.
fn send_upstream(
    out: &ProxyRequest,
    url_changed: bool,
) -> io::Result<(TcpStream, BufReader<TcpStream>, u16, String, Head)> {
    let target = parse_http_url(&out.url)?;
    let stream = connect(&target.host, target.port)?;
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(UPSTREAM_READ))?;
    let mut w = stream.try_clone()?;
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
        None => hs.push(("Connection".into(), "close".into())),
    }
    write_head(
        &mut w,
        &format!("{} {} HTTP/1.1", out.method, target.path),
        &hs,
    )?;
    w.write_all(&out.body)?;
    w.flush()?;
    let mut r = BufReader::new(stream.try_clone()?);
    loop {
        let head = read_head(&mut r)?
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "empty response"))?;
        let (status, reason) = parse_status_line(&head.line)?;
        // Interim responses are consumed here; 101 is final.
        if (100..200).contains(&status) && status != 101 {
            continue;
        }
        return Ok((stream, r, status, reason, head));
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

#[allow(clippy::too_many_arguments)]
fn forward(
    net: &SessionNet,
    id: u64,
    req: &ProxyRequest,
    out: &ProxyRequest,
    url_changed: bool,
    reader: &mut BufReader<TcpStream>,
    writer: &mut TcpStream,
    keep_alive: bool,
) -> io::Result<bool> {
    let (upstream, mut up_r, status, reason, head) = match send_upstream(out, url_changed) {
        Ok(v) => v,
        Err(e) => {
            net.ev_failed(id, &e.to_string());
            let _ = writer.shutdown(Shutdown::Both);
            return Ok(false);
        }
    };
    net.ev_response(id, status, &reason, &req.url, &head.headers);

    if status == 101 {
        write_head(writer, &head.line, &head.headers)?;
        writer.write_all(up_r.buffer())?;
        writer.flush()?;
        net.ev_finished(id, 0);
        let browser = reader.get_ref().try_clone()?;
        let mut upstream_w = upstream.try_clone()?;
        upstream_w.write_all(reader.buffer())?;
        pipe(browser, upstream);
        return Ok(false);
    }

    let framing = response_framing(&out.method, status, &head.headers);
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
            hs.push(("Content-Length".into(), n.to_string()));
            false
        }
        Framing::Chunked | Framing::Close => {
            hs.push(("Transfer-Encoding".into(), "chunked".into()));
            true
        }
    };
    hs.push(connection_field(keep_alive));
    let status_line = if reason.is_empty() {
        format!("HTTP/1.1 {status} {}", http::reason(status))
    } else {
        format!("HTTP/1.1 {status} {reason}")
    };
    write_head(writer, &status_line, &hs)?;

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
    drop(upstream);
    match copied {
        Ok(n) => {
            if chunked {
                write_last_chunk(writer)?;
            }
            writer.flush()?;
            net.ev_finished(id, n);
            if let Some(k) = kept {
                net.keep_body(id, k);
            }
            Ok(keep_alive)
        }
        Err(e) => {
            net.ev_failed(id, &e.to_string());
            let _ = writer.shutdown(Shutdown::Both);
            Ok(false)
        }
    }
}

/// `vs_net_fetch`: the whole exchange, into memory.
pub(super) fn fetch_full(
    out: &ProxyRequest,
    url_changed: bool,
) -> io::Result<(u16, String, Headers, Vec<u8>)> {
    let (_stream, mut r, status, reason, head) = send_upstream(out, url_changed)?;
    let body = read_body(&mut r, response_framing(&out.method, status, &head.headers))?;
    let reason = if reason.is_empty() {
        http::reason(status).to_string()
    } else {
        reason
    };
    Ok((status, reason, head.headers, body))
}

/// TLS inside CONNECT: pass bytes both ways untouched.
fn blind_tunnel(
    reader: BufReader<TcpStream>,
    writer: TcpStream,
    authority: &str,
) -> io::Result<()> {
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (
            h.trim_start_matches('[').trim_end_matches(']'),
            p.parse().unwrap_or(443),
        ),
        None => (authority, 443),
    };
    let mut upstream = connect(host, port)?;
    upstream.write_all(reader.buffer())?;
    drop(writer);
    pipe(reader.into_inner(), upstream);
    Ok(())
}

/// Copy both directions until either side closes.
fn pipe(a: TcpStream, b: TcpStream) {
    let _ = a.set_read_timeout(None);
    let _ = b.set_read_timeout(None);
    let (Ok(mut a_r), Ok(mut b_w)) = (a.try_clone(), b.try_clone()) else {
        return;
    };
    let t = std::thread::spawn(move || {
        let _ = io::copy(&mut a_r, &mut b_w);
        let _ = b_w.shutdown(Shutdown::Write);
    });
    let (mut b_r, mut a_w) = (b, a);
    let _ = io::copy(&mut b_r, &mut a_w);
    let _ = a_w.shutdown(Shutdown::Write);
    let _ = t.join();
}
