//! A tiny server that answers every request with its hit count, over
//! plain HTTP or TLS. The TLS variant uses a leaf for `vs.localhost`
//! signed by its own CA, written as PEM to [`HitServer::ca_pem_path`]
//! for the daemon's `VS_INTERCEPT_EXTRA_ROOTS`.
//!
//! Response: `200`, `Content-Type: text/plain`,
//! `Access-Control-Allow-Origin: *`, body = the hit number (1, 2, ...).
//! One request per connection.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

pub struct HitServer {
    pub port: u16,
    hits: Arc<AtomicU64>,
    dir: Option<tempfile::TempDir>,
    ca_pem: Option<PathBuf>,
}

impl HitServer {
    pub fn plain() -> Self {
        Self::start(None)
    }

    pub fn tls() -> Self {
        let ca_key = rcgen::KeyPair::generate().unwrap();
        let mut p = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        p.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        p.distinguished_name
            .push(rcgen::DnType::CommonName, "vs test upstream CA");
        let ca = rcgen::CertifiedIssuer::self_signed(p, ca_key).unwrap();
        let leaf_key = rcgen::KeyPair::generate().unwrap();
        let leaf = rcgen::CertificateParams::new(vec!["vs.localhost".to_string()])
            .unwrap()
            .signed_by(&leaf_key, &ca)
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let pem = dir.path().join("upstream-ca.pem");
        std::fs::write(&pem, ca.pem()).unwrap();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![leaf.der().clone()],
                rustls::pki_types::PrivateKeyDer::Pkcs8(leaf_key.serialize_der().into()),
            )
            .unwrap();
        let mut s = Self::start(Some(Arc::new(config)));
        s.dir = Some(dir);
        s.ca_pem = Some(pem);
        s
    }

    fn start(tls: Option<Arc<rustls::ServerConfig>>) -> Self {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let hits = Arc::new(AtomicU64::new(0));
        let h = hits.clone();
        std::thread::spawn(move || {
            for s in l.incoming() {
                let Ok(s) = s else { continue };
                let (h, tls) = (h.clone(), tls.clone());
                std::thread::spawn(move || match tls {
                    None => answer(s, &h),
                    Some(cfg) => {
                        let conn = rustls::ServerConnection::new(cfg).unwrap();
                        answer(rustls::StreamOwned::new(conn, s), &h);
                    }
                });
            }
        });
        Self {
            port,
            hits,
            dir: None,
            ca_pem: None,
        }
    }

    pub fn hits(&self) -> u64 {
        self.hits.load(Ordering::SeqCst)
    }

    /// The upstream CA as PEM (TLS variant only).
    pub fn ca_pem_path(&self) -> &std::path::Path {
        self.ca_pem.as_deref().expect("TLS hit server")
    }

    /// `http(s)://vs.localhost:<port><path>`.
    pub fn url(&self, path: &str) -> String {
        let scheme = if self.ca_pem.is_some() {
            "https"
        } else {
            "http"
        };
        format!("{scheme}://vs.localhost:{}{path}", self.port)
    }
}

fn answer<S: Read + Write>(s: S, hits: &AtomicU64) {
    let mut r = BufReader::new(s);
    let mut line = String::new();
    // Read the head; requests here carry no body.
    loop {
        line.clear();
        match r.read_line(&mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) if line == "\r\n" || line == "\n" => break,
            Ok(_) => {}
        }
    }
    let n = hits.fetch_add(1, Ordering::SeqCst) + 1;
    let body = n.to_string();
    let out = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let w = r.get_mut();
    let _ = w.write_all(out.as_bytes());
    let _ = w.flush();
}
