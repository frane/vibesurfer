//! Proxy behaviour over real sockets, without a browser: absolute-form
//! requests (Chromium, libsoup), `CONNECT` tunnels (WebKit on macOS),
//! holding, and re-framing of upstream bodies.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use super::http::{read_body, read_head, response_framing};
use super::{Decision, Overrides, SessionNet};

/// Upstream that answers every request on its own connection: path
/// `/chunked` with a chunked body, `/close` with a close-delimited one,
/// anything else with `Content-Length`. The body echoes the method,
/// path and the `x-test` header.
fn upstream() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for s in l.incoming() {
            let Ok(s) = s else { continue };
            std::thread::spawn(move || {
                let mut w = s.try_clone().unwrap();
                let mut r = BufReader::new(s);
                let head = read_head(&mut r).unwrap().unwrap();
                let req_body =
                    read_body(&mut r, super::http::request_framing(&head.headers)).unwrap();
                let mut parts = head.line.split(' ');
                let method = parts.next().unwrap().to_string();
                let path = parts.next().unwrap().to_string();
                let body = format!(
                    "{method} {path} x-test={} body={}",
                    head.get("x-test").unwrap_or("-"),
                    String::from_utf8_lossy(&req_body)
                );
                let out = match path.as_str() {
                    "/chunked" => format!(
                        "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{body}\r\n0\r\n\r\n",
                        body.len()
                    ),
                    "/close" => format!("HTTP/1.1 200 OK\r\n\r\n{body}"),
                    _ => format!(
                        "HTTP/1.1 201 Created\r\nContent-Length: {}\r\nX-Up: 1\r\n\r\n{body}",
                        body.len()
                    ),
                };
                let _ = w.write_all(out.as_bytes());
            });
        }
    });
    port
}

/// Send `raw` on a fresh connection to the proxy and read one response.
fn roundtrip(conn: &mut BufReader<TcpStream>, raw: &str, method: &str) -> (u16, String) {
    conn.get_mut().write_all(raw.as_bytes()).unwrap();
    let head = read_head(conn).unwrap().expect("response head");
    let (status, _) = super::http::parse_status_line(&head.line).unwrap();
    let body = read_body(conn, response_framing(method, status, &head.headers)).unwrap();
    (status, String::from_utf8(body).unwrap())
}

fn connect(net: &SessionNet) -> BufReader<TcpStream> {
    let s = TcpStream::connect(("127.0.0.1", net.port())).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    BufReader::new(s)
}

fn wait_for_req(net: &SessionNet, needle: &str) -> u64 {
    for _ in 0..50 {
        let ev = net.poll(Duration::from_millis(200));
        if let Some(line) = ev
            .lines()
            .find(|l| l.starts_with("req ") && l.contains(needle))
        {
            return line.split(' ').nth(1).unwrap().parse().unwrap();
        }
    }
    panic!("no req for {needle}");
}

#[test]
fn absolute_form_requests_are_forwarded_and_recorded() {
    let up = upstream();
    let net = SessionNet::start().unwrap();
    for path in ["/plain", "/chunked", "/close"] {
        // One request per connection: the proxy closes after each answer.
        let mut c = connect(&net);
        let (status, body) = roundtrip(
            &mut c,
            &format!(
                "GET http://vs.localhost:{up}{path} HTTP/1.1\r\nHost: vs.localhost:{up}\r\nX-Test: t\r\nProxy-Connection: keep-alive\r\n\r\n"
            ),
            "GET",
        );
        assert_eq!(body, format!("GET {path} x-test=t body="));
        assert_eq!(status, if path == "/plain" { 201 } else { 200 });
    }
    let ev = net.poll(Duration::ZERO);
    assert!(
        ev.contains(&format!(
            "req 1 GET http://vs.localhost:{up}/plain type=other nav=0\n"
        )),
        "{ev}"
    );
    assert!(ev.contains("h X-Test: t\n"), "{ev}");
    assert!(!ev.contains("Proxy-Connection"), "{ev}");
    assert!(
        ev.contains(&format!(
            "res 1 201 http://vs.localhost:{up}/plain Created\n"
        )),
        "{ev}"
    );
    assert!(ev.contains("fin 3 size="), "{ev}");
    assert_eq!(
        String::from_utf8(net.body(2).unwrap()).unwrap(),
        "GET /chunked x-test=t body="
    );
    net.shutdown();
}

#[test]
fn connect_tunnel_carries_plain_http() {
    let up = upstream();
    let net = SessionNet::start().unwrap();
    let mut c = connect(&net);
    c.get_mut()
        .write_all(
            format!("CONNECT vs.localhost:{up} HTTP/1.1\r\nHost: vs.localhost:{up}\r\n\r\n")
                .as_bytes(),
        )
        .unwrap();
    let mut line = String::new();
    c.read_line(&mut line).unwrap();
    assert!(line.starts_with("HTTP/1.1 200"), "{line}");
    c.read_line(&mut line).unwrap();
    let (status, body) = roundtrip(
        &mut c,
        "POST /plain HTTP/1.1\r\nHost: vs.localhost\r\nContent-Length: 2\r\n\r\nhi",
        "POST",
    );
    assert_eq!(
        (status, body.as_str()),
        (201, "POST /plain x-test=- body=hi")
    );
    let ev = net.poll(Duration::ZERO);
    assert!(
        ev.contains(&format!("req 1 POST http://vs.localhost:{up}/plain")),
        "{ev}"
    );
    assert!(ev.contains("body aGk=\n"), "{ev}");
    net.shutdown();
}

#[test]
fn held_requests_follow_the_decision() {
    let up = upstream();
    let net = SessionNet::start().unwrap();
    net.set_route(true);
    let send = |net: &Arc<SessionNet>, path: &'static str| {
        let mut c = connect(net);
        std::thread::spawn(move || {
            c.get_mut()
                .write_all(
                    format!("GET http://127.0.0.1:{up}{path} HTTP/1.1\r\nHost: x\r\n\r\n")
                        .as_bytes(),
                )
                .unwrap();
            match read_head(&mut c) {
                Ok(Some(head)) => {
                    let (status, _) = super::http::parse_status_line(&head.line).unwrap();
                    let body =
                        read_body(&mut c, response_framing("GET", status, &head.headers)).unwrap();
                    Some((status, String::from_utf8(body).unwrap()))
                }
                _ => None,
            }
        })
    };

    // Fulfill.
    let t = send(&net, "/a");
    let id = wait_for_req(&net, "/a ");
    assert!(net.decide(
        id,
        Decision::Fulfill {
            status: 404,
            headers: vec![("X-From".into(), "client".into())],
            body: b"nope".to_vec(),
        }
    ));
    assert_eq!(t.join().unwrap(), Some((404, "nope".into())));

    // Fetch, then continue with a changed header and URL.
    let t = send(&net, "/b");
    let id = wait_for_req(&net, "/b ");
    let f = net.fetch(id, &Overrides::default()).unwrap();
    assert_eq!(
        (f.status, f.body.as_slice()),
        (201, b"GET /b x-test=- body=".as_slice())
    );
    let o = Overrides {
        url: Some(format!("http://127.0.0.1:{up}/c")),
        headers: Some(vec![("X-Test".into(), "changed".into())]),
        ..Overrides::default()
    };
    assert!(net.decide(id, Decision::Continue(o)));
    assert_eq!(
        t.join().unwrap(),
        Some((201, "GET /c x-test=changed body=".into()))
    );

    // Abort closes the connection without a response.
    let t = send(&net, "/d");
    let id = wait_for_req(&net, "/d ");
    assert!(net.decide(id, Decision::Abort("blockedbyclient".into())));
    assert_eq!(t.join().unwrap(), None);
    assert!(net
        .poll(Duration::ZERO)
        .contains(&format!("fail {id} blockedbyclient\n")));
    assert!(!net.decide(id, Decision::Abort("again".into())));

    // Routing off releases what is held.
    let t = send(&net, "/e");
    wait_for_req(&net, "/e ");
    net.set_route(false);
    assert_eq!(
        t.join().unwrap(),
        Some((201, "GET /e x-test=- body=".into()))
    );
    net.shutdown();
}

#[test]
fn unreachable_upstream_fails_the_request() {
    let net = SessionNet::start().unwrap();
    // A port nothing listens on: bind, read the port, drop.
    let dead = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut c = connect(&net);
    c.get_mut()
        .write_all(format!("GET http://127.0.0.1:{dead}/ HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
        .unwrap();
    let mut rest = Vec::new();
    let _ = c.read_to_end(&mut rest);
    assert!(rest.is_empty());
    assert!(net.poll(Duration::ZERO).contains("fail 1 "));
    net.shutdown();
}

#[test]
fn tls_after_connect_is_tunnelled_blind() {
    // An echo server stands in for a TLS endpoint.
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let (mut s, _) = l.accept().unwrap();
        let mut buf = [0u8; 5];
        s.read_exact(&mut buf).unwrap();
        s.write_all(&buf).unwrap();
    });
    let net = SessionNet::start().unwrap();
    let mut c = connect(&net);
    c.get_mut()
        .write_all(format!("CONNECT 127.0.0.1:{port} HTTP/1.1\r\n\r\n").as_bytes())
        .unwrap();
    let mut line = String::new();
    c.read_line(&mut line).unwrap();
    c.read_line(&mut line).unwrap();
    c.get_mut().write_all(&[0x16, 3, 1, 0, 0]).unwrap();
    let mut echo = [0u8; 5];
    c.read_exact(&mut echo).unwrap();
    assert_eq!(echo, [0x16, 3, 1, 0, 0]);
    assert_eq!(net.poll(Duration::ZERO), format!("tun 127.0.0.1:{port}\n"));
    net.shutdown();
}

#[test]
fn connect_to_port_80_reports_the_url_without_it() {
    let net = SessionNet::start().unwrap();
    net.set_route(true);
    let mut c = connect(&net);
    c.get_mut()
        .write_all(b"CONNECT example.test:80 HTTP/1.1\r\n\r\nGET /p HTTP/1.1\r\nHost: example.test\r\n\r\n")
        .unwrap();
    let id = wait_for_req(&net, "http://example.test/p ");
    assert!(net.decide(
        id,
        Decision::Fulfill {
            status: 204,
            headers: vec![],
            body: vec![]
        }
    ));
    net.shutdown();
}
