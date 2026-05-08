//! Local HTTP server that serves the M6 fixture pages.
//!
//! Binds 127.0.0.1 on a random port, runs in a worker thread, and
//! serves files from `<workspace>/fixtures/` plus a small set of
//! synthetic endpoints used by the fixtures' inline scripts:
//!
//! - `POST /login` — sets `session_id` cookie, redirects to `/dashboard`.
//! - `GET  /dashboard` — serves dashboard.html only when the session
//!   cookie is present (otherwise 401).
//! - `GET  /api/ok` — 200 JSON `{"ok":true}`.
//! - `GET  /api/missing` — 404.
//! - `POST /api/explode` — 500 with a short body.
//! - `GET  /api/slow` — 200 after a 500ms delay.
//! - `POST /echo` — 200 echoes the request body.
//! - `GET  /img/pixel.gif` — a tiny 1×1 transparent GIF.
//! - `GET  /static/script-a.js`, `/static/script-b.js` — small JS
//!   files, used by `scripts.html`.
//!
//! Every response gets `Cache-Control: no-store` to keep WebKit's
//! caches out of the test loop.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

/// Owns the fixture server's listening socket and worker thread.
pub struct FixtureServer {
    addr: SocketAddr,
    server: Arc<Server>,
    handle: Option<JoinHandle<()>>,
}

impl FixtureServer {
    pub fn start() -> Self {
        let server = Server::http("127.0.0.1:0").expect("bind fixture server");
        let addr = server.server_addr().to_ip().expect("ipv4 addr");
        let server = Arc::new(server);
        let server_for_worker = server.clone();
        let handle = std::thread::Builder::new()
            .name("vs-fixture-server".into())
            .spawn(move || run(&server_for_worker))
            .expect("spawn fixture worker");
        Self {
            addr,
            server,
            handle: Some(handle),
        }
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn url(&self, path: &str) -> String {
        let p = if path.starts_with('/') {
            path.to_string()
        } else {
            format!("/{path}")
        };
        format!("http://{}{}", self.addr, p)
    }
}

impl Drop for FixtureServer {
    fn drop(&mut self) {
        self.server.unblock();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn fixtures_root() -> PathBuf {
    let cargo_manifest = env!("CARGO_MANIFEST_DIR");
    PathBuf::from(cargo_manifest).join("../../fixtures")
}

fn run(server: &Server) {
    for req in server.incoming_requests() {
        if let Err(e) = handle(req) {
            eprintln!("fixture server: {e}");
        }
    }
}

fn handle(mut req: Request) -> std::io::Result<()> {
    let url = req.url().to_string();
    let method = req.method().clone();
    let path = url.split('?').next().unwrap_or(&url).to_string();

    if path == "/login" && matches!(method, Method::Post) {
        return handle_login(req);
    }
    if path == "/dashboard" {
        return handle_dashboard(req);
    }
    if path == "/api/ok" {
        return respond_text(req, 200, "application/json", r#"{"ok":true}"#);
    }
    if path == "/api/missing" {
        return respond_text(req, 404, "text/plain", "missing");
    }
    if path == "/api/explode" {
        return respond_text(req, 500, "text/plain", "explode");
    }
    if path == "/api/slow" {
        std::thread::sleep(Duration::from_millis(500));
        return respond_text(req, 200, "text/plain", "slow ok");
    }
    if path == "/echo" {
        let mut body = String::new();
        req.as_reader().read_to_string(&mut body)?;
        return respond_text(req, 200, "text/plain", &body);
    }
    if path == "/img/pixel.gif" {
        return respond_pixel(req);
    }
    if path.starts_with("/static/") {
        return handle_static(req, &path);
    }

    let rel = path.trim_start_matches('/');
    let root = fixtures_root();
    let candidate = root.join(rel);
    if candidate.exists() && candidate.is_file() {
        return serve_file(req, &candidate);
    }
    respond_text(req, 404, "text/plain", &format!("not found: {path}"))
}

fn handle_login(mut req: Request) -> std::io::Result<()> {
    let mut body = String::new();
    req.as_reader().read_to_string(&mut body)?;
    let resp = Response::empty(StatusCode(303))
        .with_header(header(
            "Set-Cookie",
            "session_id=ssid-fixture; Path=/; SameSite=Lax",
        ))
        .with_header(header("Location", "/dashboard"))
        .with_header(no_store());
    req.respond(resp)
}

fn handle_dashboard(req: Request) -> std::io::Result<()> {
    let cookie_header = req
        .headers()
        .iter()
        .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case("Cookie"))
        .map(|h| h.value.as_str().to_string())
        .unwrap_or_default();
    if !cookie_header.contains("session_id=") {
        return respond_text(req, 401, "text/plain", "unauthenticated");
    }
    let path = fixtures_root().join("dashboard.html");
    serve_file(req, &path)
}

fn handle_static(req: Request, path: &str) -> std::io::Result<()> {
    let body = match path {
        "/static/script-a.js" => "window.__scriptA = true; console.log('script-a loaded');",
        "/static/script-b.js" => "window.__scriptB = true; console.log('script-b loaded');",
        _ => return respond_text(req, 404, "text/plain", "not found"),
    };
    respond_text(req, 200, "application/javascript", body)
}

fn respond_pixel(req: Request) -> std::io::Result<()> {
    static PIXEL: &[u8] = &[
        0x47, 0x49, 0x46, 0x38, 0x39, 0x61, 0x01, 0x00, 0x01, 0x00, 0x80, 0x00, 0x00, 0xff, 0xff,
        0xff, 0x00, 0x00, 0x00, 0x21, 0xf9, 0x04, 0x01, 0x00, 0x00, 0x00, 0x00, 0x2c, 0x00, 0x00,
        0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x02, 0x02, 0x44, 0x01, 0x00, 0x3b,
    ];
    let resp = Response::from_data(PIXEL.to_vec())
        .with_header(header("Content-Type", "image/gif"))
        .with_header(no_store());
    req.respond(resp)
}

fn serve_file(req: Request, path: &Path) -> std::io::Result<()> {
    let bytes = std::fs::read(path)?;
    let ct = match path.extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "application/javascript",
        Some("css") => "text/css",
        Some("json") => "application/json",
        Some("gif") => "image/gif",
        _ => "application/octet-stream",
    };
    let resp = Response::from_data(bytes)
        .with_header(header("Content-Type", ct))
        .with_header(no_store());
    req.respond(resp)
}

fn respond_text(req: Request, status: u16, content_type: &str, body: &str) -> std::io::Result<()> {
    let resp = Response::from_string(body.to_string())
        .with_status_code(status)
        .with_header(header("Content-Type", content_type))
        .with_header(no_store());
    req.respond(resp)
}

fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("valid header")
}

fn no_store() -> Header {
    header("Cache-Control", "no-store")
}
