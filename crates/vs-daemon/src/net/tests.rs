//! Proxy behaviour over real sockets, without a browser: absolute-form
//! requests (Chromium, libsoup), `CONNECT` tunnels (WebKit on macOS),
//! holding, and re-framing of upstream bodies.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use super::http::{read_body, read_head, response_framing};
use super::{Decision, Mitm, Overrides, SessionNet};

/// What an aborted request gets on the wire (see `proxy::abort`).
const ABORTED: &[u8] = if cfg!(target_os = "macos") {
    b"HTTP/1.1 200 OK\r\nContent-Length: 1048576\r\n\r\n"
} else {
    b""
};

/// An aborted request as the test clients read it: nothing, or the
/// macOS abort response (status 502, empty close-delimited body).
fn was_aborted(got: Option<(u16, String)>) -> bool {
    match got {
        None => true,
        Some((502, body)) => cfg!(target_os = "macos") && body.is_empty(),
        Some(_) => false,
    }
}

fn start() -> Arc<SessionNet> {
    SessionNet::start(Arc::new(Mitm::new(None, vec![]).unwrap())).unwrap()
}

/// Upstream that answers every request on its own connection: path
/// `/chunked` with a chunked body, `/close` with a close-delimited one,
/// anything else with `Content-Length`. The body echoes the method,
/// path and the `x-test` header.
fn upstream() -> u16 {
    serve_upstream(None)
}

/// [`upstream`], over TLS with a leaf for `vs.localhost` from `ca`
/// when given.
fn serve_upstream(ca: Option<Arc<TestCa>>) -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for s in l.incoming() {
            let Ok(s) = s else { continue };
            let ca = ca.clone();
            std::thread::spawn(move || match ca {
                None => answer(BufReader::new(s)),
                Some(ca) => {
                    let conn = rustls::ServerConnection::new(ca.server_config()).unwrap();
                    answer(BufReader::new(rustls::StreamOwned::new(conn, s)));
                }
            });
        }
    });
    port
}

fn answer<S: Read + Write>(mut r: BufReader<S>) {
    let Ok(Some(head)) = read_head(&mut r) else {
        return;
    };
    let req_body = read_body(&mut r, super::http::request_framing(&head.headers)).unwrap();
    let mut parts = head.line.split(' ');
    let method = parts.next().unwrap().to_string();
    let path = parts.next().unwrap().to_string();
    let body = format!(
        "{method} {path} x-test={} body={}",
        head.get("x-test").unwrap_or("-"),
        String::from_utf8_lossy(&req_body)
    );
    if path == "/ws" {
        let _ = r.get_mut().write_all(
            b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: test\r\n\r\n",
        );
        let _ = r.get_mut().flush();
        let mut buf = [0u8; 5];
        if r.read_exact(&mut buf).is_ok() {
            let _ = r.get_mut().write_all(&buf);
            let _ = r.get_mut().flush();
        }
        return;
    }
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
    let _ = r.get_mut().write_all(out.as_bytes());
    let _ = r.get_mut().flush();
}

/// Send `raw` on a fresh connection to the proxy and read one response.
fn roundtrip<S: Read + Write>(conn: &mut BufReader<S>, raw: &str, method: &str) -> (u16, String) {
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
    let net = start();
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
    let net = start();
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
    let net = start();
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
                    let (status, _) = super::http::parse_status_line(&head.line).ok()?;
                    let body =
                        read_body(&mut c, response_framing("GET", status, &head.headers)).ok()?;
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
    assert!(was_aborted(t.join().unwrap()));
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
    let net = start();
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
    assert_eq!(rest, ABORTED);
    assert!(net.poll(Duration::ZERO).contains("fail 1 "));
    net.shutdown();
}

#[test]
fn connect_to_port_80_reports_the_url_without_it() {
    let net = start();
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

// ---------------------------------------------------------------
// HTTPS.
// ---------------------------------------------------------------

/// A CA for the test upstream, separate from the proxy's.
struct TestCa {
    ca_der: rustls::pki_types::CertificateDer<'static>,
    leaf_der: rustls::pki_types::CertificateDer<'static>,
    leaf_key: Vec<u8>,
}

impl TestCa {
    fn new() -> Arc<Self> {
        let ca_key = rcgen::KeyPair::generate().unwrap();
        let mut p = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        p.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        p.distinguished_name
            .push(rcgen::DnType::CommonName, "test upstream CA");
        let ca = rcgen::CertifiedIssuer::self_signed(p, ca_key).unwrap();
        let leaf_key = rcgen::KeyPair::generate().unwrap();
        let leaf = rcgen::CertificateParams::new(vec!["vs.localhost".to_string()])
            .unwrap()
            .signed_by(&leaf_key, &ca)
            .unwrap();
        Arc::new(Self {
            ca_der: ca.der().clone(),
            leaf_der: leaf.der().clone(),
            leaf_key: leaf_key.serialize_der(),
        })
    }

    fn server_config(&self) -> Arc<rustls::ServerConfig> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        Arc::new(
            rustls::ServerConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_no_client_auth()
                .with_single_cert(
                    vec![self.leaf_der.clone()],
                    rustls::pki_types::PrivateKeyDer::Pkcs8(self.leaf_key.clone().into()),
                )
                .unwrap(),
        )
    }
}

/// A proxy whose upstream trust includes `ca`.
fn start_trusting(ca: &TestCa) -> (Arc<SessionNet>, Arc<Mitm>) {
    let mitm = Arc::new(Mitm::new(None, vec![ca.ca_der.clone()]).unwrap());
    (SessionNet::start(mitm.clone()).unwrap(), mitm)
}

type TlsConn = BufReader<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>;

/// `CONNECT host:port`, then TLS to the proxy as `host`, trusting only
/// the proxy's CA.
fn connect_tls(net: &SessionNet, mitm: &Mitm, host: &str, port: u16) -> TlsConn {
    let mut c = connect(net);
    c.get_mut()
        .write_all(
            format!("CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\n\r\n").as_bytes(),
        )
        .unwrap();
    let mut line = String::new();
    c.read_line(&mut line).unwrap();
    assert!(line.starts_with("HTTP/1.1 200"), "{line}");
    c.read_line(&mut line).unwrap();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(mitm.ca_der().to_vec().into()).unwrap();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let name = rustls::pki_types::ServerName::try_from(host.to_string()).unwrap();
    let conn = rustls::ClientConnection::new(Arc::new(config), name).unwrap();
    BufReader::new(rustls::StreamOwned::new(conn, c.into_inner()))
}

/// One https GET through the proxy on its own thread; `None` when the
/// connection closes without a response.
fn https_get(
    net: &Arc<SessionNet>,
    mitm: &Arc<Mitm>,
    host: &'static str,
    port: u16,
    path: &'static str,
) -> std::thread::JoinHandle<Option<(u16, String)>> {
    let mut c = connect_tls(net, mitm, host, port);
    std::thread::spawn(move || {
        let raw = format!("GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nX-Test: s\r\n\r\n");
        c.get_mut().write_all(raw.as_bytes()).ok()?;
        let head = read_head(&mut c).ok()??;
        let (status, _) = super::http::parse_status_line(&head.line).ok()?;
        let body = read_body(&mut c, response_framing("GET", status, &head.headers)).ok()?;
        Some((status, String::from_utf8(body).unwrap()))
    })
}

#[test]
fn https_requests_are_forwarded_and_recorded() {
    let ca = TestCa::new();
    let up = serve_upstream(Some(ca.clone()));
    let (net, mitm) = start_trusting(&ca);
    for path in ["/plain", "/chunked", "/close"] {
        let got = https_get(&net, &mitm, "vs.localhost", up, path)
            .join()
            .unwrap();
        assert_eq!(got.unwrap().1, format!("GET {path} x-test=s body="));
    }
    let ev = net.poll(Duration::ZERO);
    assert!(
        ev.contains(&format!(
            "req 1 GET https://vs.localhost:{up}/plain type=other nav=0\n"
        )),
        "{ev}"
    );
    assert!(
        ev.contains(&format!(
            "res 1 201 https://vs.localhost:{up}/plain Created\n"
        )),
        "{ev}"
    );
    assert!(ev.contains("fin 3 size="), "{ev}");
    assert_eq!(
        String::from_utf8(net.body(2).unwrap()).unwrap(),
        "GET /chunked x-test=s body="
    );
    net.shutdown();
}

#[test]
fn https_requests_can_be_held_fulfilled_aborted_and_fetched() {
    let ca = TestCa::new();
    let up = serve_upstream(Some(ca.clone()));
    let (net, mitm) = start_trusting(&ca);
    net.set_route(true);

    // Fulfill a host that does not exist: upstream is never touched,
    // and the default port is left out of the URL.
    let t = https_get(&net, &mitm, "example.invalid", 443, "/cars");
    let id = wait_for_req(&net, "https://example.invalid/cars ");
    assert!(net.decide(
        id,
        Decision::Fulfill {
            status: 200,
            headers: vec![("Content-Type".into(), "application/json".into())],
            body: br#"[{"name":"car"}]"#.to_vec(),
        }
    ));
    assert_eq!(t.join().unwrap(), Some((200, r#"[{"name":"car"}]"#.into())));

    // Fetch reads the real response over verified TLS; continue then
    // delivers it.
    let t = https_get(&net, &mitm, "vs.localhost", up, "/b");
    let id = wait_for_req(&net, "/b ");
    let f = net.fetch(id, &Overrides::default()).unwrap();
    assert_eq!(
        (f.status, f.body.as_slice()),
        (201, b"GET /b x-test=s body=".as_slice())
    );
    assert!(net.decide(id, Decision::Continue(Overrides::default())));
    assert_eq!(
        t.join().unwrap(),
        Some((201, "GET /b x-test=s body=".into()))
    );

    // Abort closes the connection without a response.
    let t = https_get(&net, &mitm, "vs.localhost", up, "/d");
    let id = wait_for_req(&net, "/d ");
    assert!(net.decide(id, Decision::Abort("blockedbyclient".into())));
    assert!(was_aborted(t.join().unwrap()));
    assert!(net
        .poll(Duration::ZERO)
        .contains(&format!("fail {id} blockedbyclient\n")));
    net.shutdown();
}

#[test]
fn untrusted_upstream_certificate_fails_the_request() {
    let ca = TestCa::new();
    let up = serve_upstream(Some(ca));
    // The proxy trusts only the public roots: the test CA is unknown.
    let net = start();
    let t = https_get(&net, &net.mitm, "vs.localhost", up, "/x");
    assert!(was_aborted(t.join().unwrap()));
    let ev = net.poll(Duration::ZERO);
    assert!(ev.contains("fail 1 "), "{ev}");
    assert!(ev.contains("tls"), "{ev}");
    net.shutdown();
}

#[test]
fn https_upgrade_is_piped_both_ways() {
    let ca = TestCa::new();
    let up = serve_upstream(Some(ca.clone()));
    let (net, mitm) = start_trusting(&ca);
    let mut c = connect_tls(&net, &mitm, "vs.localhost", up);
    c.get_mut()
        .write_all(
            format!(
                "GET /ws HTTP/1.1\r\nHost: vs.localhost:{up}\r\nConnection: Upgrade\r\nUpgrade: test\r\n\r\n"
            )
            .as_bytes(),
        )
        .unwrap();
    let head = read_head(&mut c).unwrap().unwrap();
    assert!(head.line.starts_with("HTTP/1.1 101"), "{}", head.line);
    c.get_mut().write_all(b"hello").unwrap();
    c.get_mut().flush().unwrap();
    let mut echo = [0u8; 5];
    c.read_exact(&mut echo).unwrap();
    assert_eq!(&echo, b"hello");
    net.shutdown();
}

#[test]
fn a_held_request_the_browser_drops_fails_as_cancelled() {
    let net = start();
    net.set_route(true);
    let up = upstream();
    let mut c = connect(&net);
    c.get_mut()
        .write_all(format!("GET http://127.0.0.1:{up}/x HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
        .unwrap();
    let id = wait_for_req(&net, "/x");
    // The page gives up (AbortController, a removed frame): the connection closes.
    drop(c);
    let mut seen = String::new();
    for _ in 0..30 {
        seen.push_str(&net.poll(Duration::from_millis(100)));
        if seen.contains(&format!("fail {id} cancelled")) {
            break;
        }
    }
    assert!(seen.contains(&format!("fail {id} cancelled")), "{seen}");
    // A decision after that finds nothing to decide.
    assert!(!net.decide(id, Decision::Continue(Overrides::default())));
    net.shutdown();
}
