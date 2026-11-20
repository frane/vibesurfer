//! TLS for the intercept proxy: the daemon's CA, per-host leaf
//! certificates for the browser side, verified TLS to upstream, and a
//! blocking stream type that is either plain TCP or TLS over it.
//!
//! The CA (`intercept-ca.pem` and `intercept-ca.key`, the key 0600)
//! is made on first use and kept in the daemon home. Every leaf shares
//! one key pair made at startup, so a leaf's public key is the same
//! for every host; leaves are made on demand and cached in memory.
//! Engines trust the CA for intercept sessions only.
//!
//! Upstream certificates are verified against the Mozilla roots
//! (`webpki-roots`), plus any PEM certificates in the file named by
//! `VS_INTERCEPT_EXTRA_ROOTS` (tests use it for a local TLS server).

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    Issuer, KeyPair, KeyUsagePurpose,
};
use rustls::crypto::ring as provider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use rustls::server::{ClientHello, ResolvesServerCert, StoresServerSessions};
use rustls::sign::CertifiedKey;
use rustls::{ClientConfig, ClientConnection, RootCertStore, ServerConfig, ServerConnection};

const CA_CERT_FILE: &str = "intercept-ca.pem";
const CA_KEY_FILE: &str = "intercept-ca.key";
const CA_NAME: &str = "vibesurfer intercept CA";
/// Leaves stay under the 398-day limit some verifiers apply.
const LEAF_DAYS: i64 = 365;
const CA_YEARS: i64 = 10;

fn tls_err(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(format!("tls: {e}"))
}

/// The CA, leaf issuing, and the upstream client config.
pub struct Mitm {
    ca_der: CertificateDer<'static>,
    issuer: Issuer<'static, KeyPair>,
    leaf_key: KeyPair,
    leaf_signer: Arc<dyn rustls::sign::SigningKey>,
    leaves: Mutex<HashMap<String, Arc<CertifiedKey>>>,
    sessions: Arc<dyn StoresServerSessions>,
    upstream: Arc<ClientConfig>,
}

impl std::fmt::Debug for Mitm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mitm").finish_non_exhaustive()
    }
}

/// The CA's parameters. Fixed, so an issuer rebuilt from the stored
/// key names the same subject as the stored certificate.
fn ca_params() -> CertificateParams {
    let mut p = CertificateParams::default();
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, CA_NAME);
    dn.push(DnType::OrganizationName, "vibesurfer");
    p.distinguished_name = dn;
    p.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    p.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    p
}

fn now() -> time::OffsetDateTime {
    time::OffsetDateTime::now_utc()
}

impl Mitm {
    /// Load the CA from `dir`, or make and store one. `None` makes a
    /// CA that lives only in memory. Upstream trust is the Mozilla
    /// roots plus `extra_roots`.
    pub fn new(dir: Option<&Path>, extra_roots: Vec<CertificateDer<'static>>) -> io::Result<Self> {
        let (ca_der, ca_key) = match dir {
            Some(d) => load_or_create_ca(d)?,
            None => make_ca()?,
        };
        let issuer = Issuer::new(ca_params(), ca_key);
        let leaf_key = KeyPair::generate().map_err(tls_err)?;
        let leaf_signer = provider::default_provider()
            .key_provider
            .load_private_key(PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
                leaf_key.serialize_der(),
            )))
            .map_err(tls_err)?;
        let mut roots = RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        for c in extra_roots {
            roots.add(c).map_err(tls_err)?;
        }
        let mut upstream =
            ClientConfig::builder_with_provider(Arc::new(provider::default_provider()))
                .with_safe_default_protocol_versions()
                .map_err(tls_err)?
                .with_root_certificates(roots)
                .with_no_client_auth();
        upstream.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(Self {
            ca_der,
            issuer,
            leaf_key,
            leaf_signer,
            leaves: Mutex::new(HashMap::new()),
            sessions: rustls::server::ServerSessionMemoryCache::new(1024),
            upstream: Arc::new(upstream),
        })
    }

    /// The daemon's CA as `new` would set it up, with extra upstream
    /// roots from `VS_INTERCEPT_EXTRA_ROOTS`.
    pub fn from_env(dir: Option<&Path>) -> io::Result<Self> {
        let mut extra = Vec::new();
        if let Some(p) = std::env::var_os("VS_INTERCEPT_EXTRA_ROOTS") {
            for c in CertificateDer::pem_file_iter(&p).map_err(tls_err)? {
                extra.push(c.map_err(tls_err)?);
            }
        }
        Self::new(dir, extra)
    }

    /// The CA certificate, DER.
    #[must_use]
    pub fn ca_der(&self) -> &[u8] {
        &self.ca_der
    }

    /// Base64 SHA-256 of the leaf key's and the CA key's
    /// `SubjectPublicKeyInfo`, comma-separated: the form Chromium's
    /// `--ignore-certificate-errors-spki-list` takes. Every leaf uses
    /// the same key, so the list covers every host.
    #[must_use]
    pub fn spki_pins(&self) -> String {
        use base64::Engine as _;
        use rcgen::PublicKeyData as _;
        [
            self.leaf_key.subject_public_key_info(),
            self.issuer.key().subject_public_key_info(),
        ]
        .iter()
        .map(|spki| {
            let d = ring::digest::digest(&ring::digest::SHA256, spki);
            base64::engine::general_purpose::STANDARD.encode(d.as_ref())
        })
        .collect::<Vec<_>>()
        .join(",")
    }

    fn leaf(&self, host: &str) -> Option<Arc<CertifiedKey>> {
        let host = host.trim_start_matches('[').trim_end_matches(']');
        let host = host.to_ascii_lowercase();
        if let Some(k) = self.leaves.lock().expect("poisoned").get(&host) {
            return Some(k.clone());
        }
        let mut p = CertificateParams::new(vec![host.clone()]).ok()?;
        p.distinguished_name = DistinguishedName::new();
        p.distinguished_name.push(DnType::CommonName, host.as_str());
        p.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        p.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        p.use_authority_key_identifier_extension = true;
        let t = now();
        p.not_before = t - time::Duration::days(1);
        p.not_after = t + time::Duration::days(LEAF_DAYS);
        let cert = p.signed_by(&self.leaf_key, &self.issuer).ok()?;
        let key = Arc::new(CertifiedKey::new(
            vec![cert.der().clone(), self.ca_der.clone()],
            self.leaf_signer.clone(),
        ));
        self.leaves
            .lock()
            .expect("poisoned")
            .insert(host, key.clone());
        Some(key)
    }

    /// Finish the browser's TLS handshake on `sock`, as `host` (or the
    /// name the browser sent in SNI). `early` is what was already read
    /// off the socket.
    pub(crate) fn accept(
        self: &Arc<Self>,
        sock: TcpStream,
        early: &[u8],
        host: &str,
    ) -> io::Result<Stream> {
        let mut config =
            ServerConfig::builder_with_provider(Arc::new(provider::default_provider()))
                .with_safe_default_protocol_versions()
                .map_err(tls_err)?
                .with_no_client_auth()
                .with_cert_resolver(Arc::new(Resolver {
                    mitm: self.clone(),
                    fallback: host.to_string(),
                }));
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        config.session_storage = self.sessions.clone();
        let mut conn = ServerConnection::new(Arc::new(config)).map_err(tls_err)?;
        let mut early = early;
        while !early.is_empty() {
            conn.read_tls(&mut early)?;
            conn.process_new_packets().map_err(tls_err)?;
        }
        let mut s = sock;
        while conn.is_handshaking() {
            conn.complete_io(&mut s)?;
        }
        Ok(Stream::Tls(TlsStream::new(s, conn.into())))
    }

    /// TLS client handshake to `host` over `sock`, verifying the
    /// server's certificate.
    pub(crate) fn connect(&self, sock: TcpStream, host: &str) -> io::Result<Stream> {
        let name = host.trim_start_matches('[').trim_end_matches(']');
        let name = ServerName::try_from(name.to_string()).map_err(tls_err)?;
        let mut conn = ClientConnection::new(self.upstream.clone(), name).map_err(tls_err)?;
        let mut s = sock;
        while conn.is_handshaking() {
            conn.complete_io(&mut s).map_err(tls_err)?;
        }
        Ok(Stream::Tls(TlsStream::new(s, conn.into())))
    }
}

#[derive(Debug)]
struct Resolver {
    mitm: Arc<Mitm>,
    fallback: String,
}

impl ResolvesServerCert for Resolver {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        self.mitm
            .leaf(hello.server_name().unwrap_or(&self.fallback))
    }
}

fn make_ca() -> io::Result<(CertificateDer<'static>, KeyPair)> {
    let key = KeyPair::generate().map_err(tls_err)?;
    let mut p = ca_params();
    let t = now();
    p.not_before = t - time::Duration::days(1);
    p.not_after = t + time::Duration::days(365 * CA_YEARS);
    let cert = p.self_signed(&key).map_err(tls_err)?;
    Ok((cert.der().clone(), key))
}

fn load_or_create_ca(dir: &Path) -> io::Result<(CertificateDer<'static>, KeyPair)> {
    let cert_path = dir.join(CA_CERT_FILE);
    let key_path = dir.join(CA_KEY_FILE);
    if let (Ok(cert), Ok(key)) = (
        std::fs::read_to_string(&cert_path),
        std::fs::read_to_string(&key_path),
    ) {
        if let (Ok(c), Ok(k)) = (
            CertificateDer::from_pem_slice(cert.as_bytes()),
            KeyPair::from_pem(&key),
        ) {
            return Ok((c, k));
        }
        tracing::warn!(path = %cert_path.display(), "intercept CA unreadable; making a new one");
    }
    std::fs::create_dir_all(dir)?;
    let (der, key) = make_ca()?;
    let cert_pem = pem_cert(&der);
    write_private(&key_path, key.serialize_pem().as_bytes())?;
    let tmp = dir.join(format!("{CA_CERT_FILE}.{}", std::process::id()));
    std::fs::write(&tmp, cert_pem)?;
    std::fs::rename(&tmp, &cert_path)?;
    Ok((der, key))
}

fn pem_cert(der: &[u8]) -> String {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD.encode(der);
    let mut s = String::from("-----BEGIN CERTIFICATE-----\n");
    for chunk in b64.as_bytes().chunks(64) {
        s.push_str(std::str::from_utf8(chunk).unwrap_or_default());
        s.push('\n');
    }
    s.push_str("-----END CERTIFICATE-----\n");
    s
}

fn write_private(path: &Path, data: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension(format!("key.{}", std::process::id()));
    {
        let mut o = std::fs::OpenOptions::new();
        o.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            o.mode(0o600);
        }
        let mut f = o.open(&tmp)?;
        f.write_all(data)?;
    }
    std::fs::rename(&tmp, path)
}

// -------------------------------------------------------------------
// Streams.
// -------------------------------------------------------------------

/// A browser or upstream connection: plain TCP, or TLS over it.
///
/// Clones share the TLS state behind a mutex and own a clone of the
/// socket, so one clone can read while another writes (upgraded
/// connections are piped that way). The socket is read without the
/// lock held.
#[derive(Debug)]
pub(crate) enum Stream {
    Plain(TcpStream),
    Tls(TlsStream),
}

#[derive(Debug)]
pub(crate) struct TlsStream {
    sock: TcpStream,
    conn: Arc<Mutex<rustls::Connection>>,
    raw: Vec<u8>,
}

impl TlsStream {
    fn new(sock: TcpStream, conn: rustls::Connection) -> Self {
        Self {
            sock,
            conn: Arc::new(Mutex::new(conn)),
            raw: vec![0; 16 * 1024 + 256],
        }
    }

    fn flush_tls(conn: &mut rustls::Connection, sock: &mut TcpStream) -> io::Result<()> {
        while conn.wants_write() {
            conn.write_tls(sock)?;
        }
        Ok(())
    }
}

impl Read for TlsStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            {
                let mut c = self.conn.lock().expect("poisoned");
                match c.reader().read(buf) {
                    Ok(n) => return Ok(n),
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    // Peer closed without close_notify: an end of
                    // stream all the same. Framing catches truncation.
                    Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(0),
                    Err(e) => return Err(e),
                }
            }
            let n = self.sock.read(&mut self.raw)?;
            let mut c = self.conn.lock().expect("poisoned");
            let mut rd = &self.raw[..n];
            if n == 0 {
                c.read_tls(&mut rd)?;
            }
            while !rd.is_empty() {
                c.read_tls(&mut rd)?;
                if let Err(e) = c.process_new_packets() {
                    let _ = Self::flush_tls(&mut c, &mut self.sock);
                    return Err(tls_err(e));
                }
            }
            Self::flush_tls(&mut c, &mut self.sock)?;
        }
    }
}

impl Write for TlsStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut c = self.conn.lock().expect("poisoned");
        let n = c.writer().write(buf)?;
        Self::flush_tls(&mut c, &mut self.sock)?;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut c = self.conn.lock().expect("poisoned");
        c.writer().flush()?;
        Self::flush_tls(&mut c, &mut self.sock)?;
        self.sock.flush()
    }
}

impl Stream {
    pub(crate) fn sock(&self) -> &TcpStream {
        match self {
            Self::Plain(s) => s,
            Self::Tls(t) => &t.sock,
        }
    }

    pub(crate) fn try_clone(&self) -> io::Result<Self> {
        Ok(match self {
            Self::Plain(s) => Self::Plain(s.try_clone()?),
            Self::Tls(t) => Self::Tls(TlsStream {
                sock: t.sock.try_clone()?,
                conn: t.conn.clone(),
                raw: vec![0; t.raw.len()],
            }),
        })
    }

    pub(crate) fn set_read_timeout(&self, d: Option<Duration>) -> io::Result<()> {
        self.sock().set_read_timeout(d)
    }

    /// Drop the connection without a TLS goodbye: the peer sees the
    /// exchange fail.
    pub(crate) fn kill(&self) {
        let _ = self.sock().shutdown(Shutdown::Both);
    }

    /// End our direction: close_notify for TLS, then a TCP half-close.
    pub(crate) fn finish(&mut self) {
        if let Self::Tls(t) = self {
            let mut c = t.conn.lock().expect("poisoned");
            c.send_close_notify();
            let _ = TlsStream::flush_tls(&mut c, &mut t.sock);
        }
        let _ = self.sock().shutdown(Shutdown::Write);
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Plain(s) => s.read(buf),
            Self::Tls(t) => t.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::Plain(s) => s.write(buf),
            Self::Tls(t) => t.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Plain(s) => s.flush(),
            Self::Tls(t) => t.flush(),
        }
    }
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn ca_is_stored_once_and_reused() {
        let dir = tempfile::tempdir().unwrap();
        let a = Mitm::new(Some(dir.path()), vec![]).unwrap();
        let b = Mitm::new(Some(dir.path()), vec![]).unwrap();
        assert_eq!(a.ca_der(), b.ca_der());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(dir.path().join(CA_KEY_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // A leaf from a reloaded CA chains to the stored certificate.
        let leaf = b.leaf("example.test").unwrap();
        assert_eq!(leaf.cert[1].as_ref(), a.ca_der());
    }
}
