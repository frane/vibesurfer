//! Loopback web surface for sensitive-data entry.
//!
//! `vs prompt-input` / `vs prompt-form` park until a human provides
//! values. The tty path (`vs pending fulfill`) works but is awkward:
//! no password manager, one field at a time. This module serves a
//! browser form instead: the daemon listens on `127.0.0.1:<random>`,
//! mints single-use capability URLs (`/entry/<nonce>`), and a GET
//! renders every currently-pending entry as one HTML form. Submit
//! fulfills them all through the same [`PendingQueue`] path the tty
//! uses.
//!
//! Security model: the URL is the auth. Nonces are 256-bit random,
//! base64url, expire after [`NONCE_TTL`], and are consumed by the
//! POST that submits values. The listener binds loopback only and is
//! started lazily on the first URL mint; with no live nonce the
//! surface answers 404 to everything, so an idle listener is inert.
//! Values never appear in responses or logs. This is the same trust
//! boundary as the daemon's Unix socket — the local user — with the
//! nonce guarding against other local processes probing the port.
//!
//! The server is deliberately minimal: HTTP/1.1, `Connection: close`,
//! one thread per connection, GET and POST on `/entry/<nonce>` only.
//! Loopback + human-scale traffic; nothing here needs an HTTP stack.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ring::rand::{SecureRandom, SystemRandom};

use super::pending::PendingQueue;

/// How long a minted entry URL stays valid.
const NONCE_TTL: Duration = Duration::from_secs(10 * 60);
/// Cap on request head + body. A submitted form is a handful of
/// values; anything bigger is not our client.
const MAX_HEAD: usize = 16 * 1024;
const MAX_BODY: usize = 256 * 1024;

/// The web-entry surface. One per daemon, started lazily on the
/// first URL mint (see [`WebEntry::start`]'s caller in
/// `daemon::mod`); the accept thread lives for the daemon process.
pub struct WebEntry {
    queue: Arc<PendingQueue>,
    port: u16,
    nonces: Mutex<HashMap<String, Instant>>,
}

impl WebEntry {
    /// Bind the loopback listener and spawn the accept thread.
    pub fn start(queue: Arc<PendingQueue>) -> std::io::Result<Arc<Self>> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let port = listener.local_addr()?.port();
        let surface = Arc::new(Self {
            queue,
            port,
            nonces: Mutex::new(HashMap::new()),
        });
        let accept = surface.clone();
        std::thread::Builder::new()
            .name("vs-webentry".into())
            .spawn(move || accept.accept_loop(&listener))?;
        Ok(surface)
    }

    /// Mint a fresh capability URL for this surface.
    pub fn mint(&self) -> String {
        let nonce = fresh_nonce();
        {
            let mut guard = self.nonces.lock().unwrap();
            guard.retain(|_, exp| *exp > Instant::now());
            guard.insert(nonce.clone(), Instant::now() + NONCE_TTL);
        }
        format!("http://127.0.0.1:{}/entry/{nonce}", self.port)
    }

    fn accept_loop(self: &Arc<Self>, listener: &TcpListener) {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let this = self.clone();
            let _ = std::thread::Builder::new()
                .name("vs-webentry-conn".into())
                .spawn(move || {
                    let _ = this.handle(stream);
                });
        }
    }

    /// Check `nonce` is live; `consume` removes it (POST path).
    fn nonce_ok(&self, nonce: &str, consume: bool) -> bool {
        let mut guard = self.nonces.lock().unwrap();
        match guard.get(nonce) {
            Some(exp) if *exp > Instant::now() => {
                if consume {
                    guard.remove(nonce);
                }
                true
            }
            Some(_) => {
                guard.remove(nonce);
                false
            }
            None => false,
        }
    }

    fn handle(&self, mut stream: TcpStream) -> std::io::Result<()> {
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        let mut reader = BufReader::new(stream.try_clone()?);

        let mut request_line = String::new();
        reader.read_line(&mut request_line)?;
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or("").to_string();
        let path = parts.next().unwrap_or("").to_string();

        // Drain headers; the only one we act on is Content-Length.
        let mut content_length = 0usize;
        let mut head_bytes = request_line.len();
        loop {
            let mut line = String::new();
            let n = reader.read_line(&mut line)?;
            head_bytes += n;
            if n == 0 || line == "\r\n" || line == "\n" || head_bytes > MAX_HEAD {
                break;
            }
            if let Some(v) = line
                .to_ascii_lowercase()
                .strip_prefix("content-length:")
                .map(str::trim)
                .and_then(|v| v.parse::<usize>().ok())
            {
                content_length = v;
            }
        }

        let response = match (method.as_str(), path.strip_prefix("/entry/")) {
            ("GET", Some(nonce)) if self.nonce_ok(nonce, false) => self.render_form(nonce),
            ("POST", Some(nonce)) => {
                if content_length > MAX_BODY {
                    http_page(413, "Too large", "<p>Form body too large.</p>")
                } else {
                    let mut body = vec![0u8; content_length];
                    reader.read_exact(&mut body)?;
                    if self.nonce_ok(nonce, true) {
                        self.submit(&body)
                    } else {
                        expired_page()
                    }
                }
            }
            ("GET", Some(_)) => expired_page(),
            _ => http_page(404, "Not found", "<p>Nothing here.</p>"),
        };
        stream.write_all(response.as_bytes())?;
        stream.flush()
    }

    /// GET: every pending entry, one form. Non-secret fields get
    /// plain text inputs; secret fields get password inputs with
    /// autocomplete hints so password managers offer to fill them.
    fn render_form(&self, nonce: &str) -> String {
        let entries = self.queue.list();
        if entries.is_empty() {
            return http_page(
                200,
                "vibesurfer — nothing pending",
                "<p>No input is currently requested. If the agent just asked, reload.</p>",
            );
        }
        let mut rows = String::new();
        for e in &entries {
            use std::fmt::Write as _;
            let label = escape_html(&e.message);
            let id = escape_html(&e.id);
            let (ty, ac) = if e.secret {
                ("password", "current-password")
            } else {
                ("text", "on")
            };
            let _ = write!(
                rows,
                "<label for=\"{id}\">{label}</label>\n\
                 <input id=\"{id}\" name=\"{id}\" type=\"{ty}\" autocomplete=\"{ac}\" required>\n"
            );
        }
        let body = format!(
            "<p>An agent is waiting on the value{} below. Values go straight to the local \
             vibesurfer daemon and are filled into the page there — the agent never sees \
             what you type.</p>\n\
             <form method=\"post\" action=\"/entry/{nonce}\" autocomplete=\"on\">\n\
             {rows}<button type=\"submit\">Submit</button>\n</form>",
            if entries.len() == 1 { "" } else { "s" },
        );
        http_page(200, "vibesurfer — input requested", &body)
    }

    /// POST: fulfill each `id=value` pair that names a live entry.
    fn submit(&self, body: &[u8]) -> String {
        let mut fulfilled = 0usize;
        for (key, value) in parse_form_urlencoded(body) {
            if self.queue.fulfill(&key, value) {
                fulfilled += 1;
            }
        }
        if fulfilled == 0 {
            http_page(
                200,
                "vibesurfer — nothing submitted",
                "<p>Those entries were no longer pending (already fulfilled, cancelled, \
                 or timed out).</p>",
            )
        } else {
            http_page(
                200,
                "vibesurfer — done",
                &format!(
                    "<p>{fulfilled} value{} delivered. The agent is resuming — you can \
                     close this tab.</p>",
                    if fulfilled == 1 { "" } else { "s" }
                ),
            )
        }
    }
}

fn fresh_nonce() -> String {
    let mut buf = [0u8; 32];
    SystemRandom::new().fill(&mut buf).expect("system rng");
    base64url(&buf)
}

fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let mut acc = 0u32;
        for (i, &b) in chunk.iter().enumerate() {
            acc |= u32::from(b) << (16 - 8 * i);
        }
        for i in 0..=chunk.len() {
            let idx = usize::try_from((acc >> (18 - 6 * i)) & 0x3f).expect("6-bit index");
            out.push(char::from(ALPHABET[idx]));
        }
    }
    out
}

/// Decode `application/x-www-form-urlencoded`: `+` is space, `%XX`
/// is a byte, pairs split on `&` and `=`. Invalid UTF-8 after
/// decoding drops the pair (field values come from a browser form;
/// they are UTF-8 or garbage).
fn parse_form_urlencoded(body: &[u8]) -> Vec<(String, String)> {
    fn decode(s: &[u8]) -> Option<String> {
        let mut out = Vec::with_capacity(s.len());
        let mut i = 0;
        while i < s.len() {
            match s[i] {
                b'+' => out.push(b' '),
                b'%' if i + 3 <= s.len() => {
                    let hex = std::str::from_utf8(&s[i + 1..i + 3]).ok()?;
                    out.push(u8::from_str_radix(hex, 16).ok()?);
                    i += 2;
                }
                b => out.push(b),
            }
            i += 1;
        }
        String::from_utf8(out).ok()
    }
    body.split(|&b| b == b'&')
        .filter_map(|pair| {
            let eq = pair.iter().position(|&b| b == b'=')?;
            Some((decode(&pair[..eq])?, decode(&pair[eq + 1..])?))
        })
        .collect()
}

fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

fn expired_page() -> String {
    http_page(
        410,
        "vibesurfer — link expired",
        "<p>This entry link is no longer valid (used or expired). Ask the agent for a \
         fresh one — or run <code>vs pending url</code>.</p>",
    )
}

fn http_page(status: u16, title: &str, body_html: &str) -> String {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        410 => "Gone",
        413 => "Payload Too Large",
        _ => "Error",
    };
    let page = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>{title}</title><style>\
         body{{font:16px/1.5 system-ui,sans-serif;max-width:28rem;margin:3rem auto;padding:0 1rem;color:#1a1a1a;background:#fff}}\
         @media(prefers-color-scheme:dark){{body{{color:#e6e6e6;background:#111}}input{{background:#1a1a1a;color:#e6e6e6;border-color:#444}}}}\
         label{{display:block;margin:1rem 0 .25rem;font-weight:600}}\
         input{{width:100%;padding:.5rem;font-size:1rem;border:1px solid #bbb;border-radius:4px;box-sizing:border-box}}\
         button{{margin-top:1.25rem;padding:.5rem 1.5rem;font-size:1rem}}\
         </style></head><body><h1 style=\"font-size:1.2rem\">{title}</h1>{body_html}</body></html>",
        title = escape_html(title),
    );
    format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-store\r\n\
         Referrer-Policy: no-referrer\r\n\
         X-Content-Type-Options: nosniff\r\n\
         Connection: close\r\n\r\n{page}",
        page.len(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urlencoded_decoding() {
        let pairs = parse_form_urlencoded(b"p_1=hunter2&p_2=a+b%20c&p_3=%C3%A9");
        assert_eq!(
            pairs,
            vec![
                ("p_1".into(), "hunter2".into()),
                ("p_2".into(), "a b c".into()),
                ("p_3".into(), "\u{e9}".into()),
            ]
        );
    }

    #[test]
    fn urlencoded_bad_pairs_dropped() {
        let pairs = parse_form_urlencoded(b"novalue&p_1=%zz&p_2=ok");
        assert_eq!(pairs, vec![("p_2".into(), "ok".into())]);
    }

    #[test]
    fn html_escaping() {
        assert_eq!(
            escape_html("<b>&\"'x"),
            "&lt;b&gt;&amp;&quot;&#39;x"
        );
    }

    #[test]
    fn base64url_shape() {
        // 32 bytes -> 43 chars, no padding, url-safe alphabet.
        let s = base64url(&[0xffu8; 32]);
        assert_eq!(s.len(), 43);
        assert!(s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'));
    }
}
