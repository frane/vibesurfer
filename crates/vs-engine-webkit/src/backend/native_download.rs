//! Downloads the engine itself received.
//!
//! The page-side shim (`download_shim.js`) sees saves the page makes in
//! script: a `download` anchor, a `blob:` link, `window.open`. A server
//! response marked `Content-Disposition: attachment`, or one whose type
//! the engine cannot render, never reaches page script. It arrives on a
//! navigation (a link, a form POST, a hidden iframe) and the engine
//! turns it into a download. Each backend hands those to its download
//! API, writes the file to a temp path, and records the bytes here.
//! `vs_download` drains this buffer and the shim's buffer as one.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::engine::{Download, DownloadEntry};

/// Native ids start here so they never collide with the shim's ids,
/// which count up from 1 per page.
pub(crate) const NATIVE_ID_BASE: u64 = 1 << 40;

/// Same caps as the shim: a few entries, 64 MiB each.
const MAX_ENTRIES: usize = 8;
pub(crate) const MAX_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug)]
struct Item {
    entry: DownloadEntry,
    bytes: Vec<u8>,
    /// Epoch ms when the response arrived. Same clock as the page's
    /// `Date.now()`, so it orders against shim entries.
    started_ms: u64,
}

#[derive(Debug, Default)]
pub(crate) struct NativeDownloads {
    seq: u64,
    items: VecDeque<Item>,
}

pub(crate) type SharedDownloads = Arc<Mutex<NativeDownloads>>;

pub(crate) struct Newest {
    pub id: u64,
    pub started_ms: u64,
    pub done: bool,
    pub url: String,
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Temp file a backend hands its download API as the destination.
/// The engine needs a path that does not exist yet.
pub(crate) fn temp_path(id: u64) -> PathBuf {
    std::env::temp_dir().join(format!("vs-dl-{}-{id}", std::process::id()))
}

/// The `filename=` of a `Content-Disposition` header, RFC 5987 form
/// first. Mirrors `nameFromDisposition` in the shim.
pub(crate) fn name_from_disposition(cd: &str) -> Option<String> {
    let lower = cd.to_ascii_lowercase();
    if let Some(i) = lower.find("filename*=") {
        let v = cd[i + 10..].split(';').next().unwrap_or("").trim();
        if let Some(enc) = v.rsplit('\'').next() {
            let decoded = percent_decode(enc);
            if !decoded.is_empty() {
                return Some(decoded);
            }
        }
    }
    let mut from = 0;
    while let Some(off) = lower[from..].find("filename=") {
        let i = from + off;
        from = i + 9;
        if i > 0 && lower.as_bytes()[i - 1] == b'*' {
            continue;
        }
        let v = cd[i + 9..].trim_start();
        let name = if let Some(q) = v.strip_prefix('"') {
            q.split('"').next().unwrap_or("")
        } else {
            v.split(';').next().unwrap_or("").trim()
        };
        if !name.is_empty() {
            return Some(name.to_string());
        }
    }
    None
}

/// Whether a `Content-Disposition` value asks for a download.
/// WebView2 makes that call itself, so Windows does not use this.
#[cfg_attr(target_os = "windows", allow(dead_code))]
pub(crate) fn is_attachment(cd: &str) -> bool {
    cd.trim_start()
        .get(..10)
        .is_some_and(|s| s.eq_ignore_ascii_case("attachment"))
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Some(v) = s
                .get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
            {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Last path segment of a URL, for a response with no filename.
pub(crate) fn name_from_url(url: &str) -> String {
    let path = url.split(['?', '#']).next().unwrap_or("");
    let path = path.split_once("://").map_or(path, |(_, rest)| {
        rest.split_once('/').map_or("", |(_, p)| p)
    });
    let last = path.rsplit('/').find(|s| !s.is_empty()).unwrap_or("");
    if last.is_empty() {
        "download".to_string()
    } else {
        percent_decode(last)
    }
}

impl NativeDownloads {
    /// Record a download the engine started. Returns its id.
    pub(crate) fn begin(&mut self, url: &str, filename: &str, mime: &str) -> u64 {
        self.seq += 1;
        let id = NATIVE_ID_BASE + self.seq;
        self.items.push_back(Item {
            entry: DownloadEntry {
                id,
                filename: if filename.is_empty() {
                    name_from_url(url)
                } else {
                    filename.to_string()
                },
                mime: mime.to_string(),
                url: url.to_string(),
                size: 0,
                error: None,
                done: false,
            },
            bytes: Vec::new(),
            started_ms: now_ms(),
        });
        while self.items.len() > MAX_ENTRIES {
            self.items.pop_front();
        }
        id
    }

    /// Settle a download with its bytes or the reason it failed.
    pub(crate) fn finish(&mut self, id: u64, result: Result<Vec<u8>, String>) {
        let Some(item) = self.items.iter_mut().find(|i| i.entry.id == id) else {
            return;
        };
        // First result wins: some engines report a failure and then
        // finish the same download.
        if item.entry.done {
            return;
        }
        match result {
            Ok(bytes) if bytes.len() as u64 > MAX_BYTES => {
                item.entry.error = Some(format!(
                    "download exceeds {MAX_BYTES} bytes ({})",
                    bytes.len()
                ));
            }
            Ok(bytes) => {
                item.entry.size = bytes.len() as u64;
                item.bytes = bytes;
            }
            Err(e) => item.entry.error = Some(e),
        }
        item.entry.done = true;
    }

    /// Rename once the engine knows the server's suggested name.
    #[cfg_attr(target_os = "windows", allow(dead_code))]
    pub(crate) fn set_name(&mut self, id: u64, filename: &str) {
        if filename.is_empty() {
            return;
        }
        if let Some(item) = self.items.iter_mut().find(|i| i.entry.id == id) {
            item.entry.filename = filename.to_string();
        }
    }

    pub(crate) fn entries(&self) -> Vec<DownloadEntry> {
        self.items.iter().map(|i| i.entry.clone()).collect()
    }

    /// Newest entry: `(id, started_ms, done)`.
    #[cfg(test)]
    pub(crate) fn newest(&self) -> Option<(u64, u64, bool)> {
        self.items
            .back()
            .map(|i| (i.entry.id, i.started_ms, i.entry.done))
    }

    pub(crate) fn newest_with_url(&self) -> Option<Newest> {
        self.items.back().map(|i| Newest {
            id: i.entry.id,
            started_ms: i.started_ms,
            done: i.entry.done,
            url: i.entry.url.clone(),
        })
    }

    pub(crate) fn is_done(&self, id: u64) -> Option<bool> {
        self.items
            .iter()
            .find(|i| i.entry.id == id)
            .map(|i| i.entry.done)
    }

    /// Remove a finished entry and hand back its payload.
    pub(crate) fn take(&mut self, id: u64) -> Option<Result<Download, String>> {
        let pos = self
            .items
            .iter()
            .position(|i| i.entry.id == id && i.entry.done)?;
        let item = self.items.remove(pos)?;
        Some(match item.entry.error {
            Some(e) => Err(e),
            None => Ok(Download {
                bytes: item.bytes,
                filename: item.entry.filename,
                mime: item.entry.mime,
                url: item.entry.url,
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disposition_names() {
        assert_eq!(
            name_from_disposition("attachment; filename=\"Jahresabrechnung 2025.pdf\"").as_deref(),
            Some("Jahresabrechnung 2025.pdf")
        );
        assert_eq!(
            name_from_disposition("attachment; filename=a.ris").as_deref(),
            Some("a.ris")
        );
        assert_eq!(
            name_from_disposition("attachment; filename=\"x.txt\"; filename*=UTF-8''%C3%BC.txt")
                .as_deref(),
            Some("ü.txt")
        );
        assert_eq!(name_from_disposition("attachment"), None);
        assert!(is_attachment("Attachment; filename=a"));
        assert!(!is_attachment("inline"));
    }

    #[test]
    fn url_names() {
        assert_eq!(name_from_url("https://h/a/b/export.ris?x=1"), "export.ris");
        assert_eq!(name_from_url("https://h/"), "download");
        assert_eq!(name_from_url("https://h/a%20b.pdf"), "a b.pdf");
    }

    #[test]
    fn buffer_lifecycle() {
        let mut d = NativeDownloads::default();
        let id = d.begin("https://h/f.ris", "", "text/plain");
        assert!(id > NATIVE_ID_BASE);
        assert_eq!(d.newest().map(|n| n.2), Some(false));
        assert!(d.take(id).is_none(), "not done yet");
        d.finish(id, Ok(b"TY  - JOUR".to_vec()));
        let got = d.take(id).unwrap().unwrap();
        assert_eq!(got.filename, "f.ris");
        assert_eq!(got.bytes, b"TY  - JOUR");
        assert!(d.newest().is_none());
    }
}
