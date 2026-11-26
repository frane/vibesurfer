//! Downloads: the in-page shim that captures what a page saves, chunked
//! reads of a URL from inside the page, and merging with the engine's
//! native downloads.

use std::time::Duration;

use crate::engine::{Download, DownloadEntry, DownloadSource, EngineError, EngineResult};

use super::{json_string, DOWNLOAD_SHIM_JS};
use crate::backend::native_download::{SharedDownloads, NATIVE_ID_BASE};

/// Raw bytes pulled per `__vsDl.read` call. 768 KiB in → 1 MiB of
/// base64 out, which keeps every eval round-trip (and the NSString /
/// GVariant / PCWSTR conversion behind it) bounded no matter how large
/// the file is.
const DOWNLOAD_CHUNK: usize = 768 * 1024;

/// Largest download the engine will materialize. Mirrors `MAX_BYTES`
/// in `download_shim.js`; kept in both places because the shim refuses
/// the capture and this refuses the read-back.
const MAX_DOWNLOAD_BYTES: usize = 64 * 1024 * 1024;

/// Strip the one layer of JSON-string quoting some eval bridges wrap
/// around a returned string.
fn eval_str(raw: String) -> String {
    serde_json::from_str::<String>(&raw).unwrap_or(raw)
}

/// Install [`DOWNLOAD_SHIM_JS`] if this document doesn't already have
/// it. Pages opened before the shim was registered as a document-start
/// script — and any backend where that registration silently failed —
/// still get a working `vs_download` this way.
fn ensure_download_shim<F>(eval: &F) -> EngineResult<()>
where
    F: Fn(&str, Duration) -> EngineResult<String>,
{
    let probe = |e: &F| -> EngineResult<String> {
        Ok(eval_str(e(
            "(typeof window.__vsDl)",
            Duration::from_secs(5),
        )?))
    };
    if probe(eval)? == "object" {
        return Ok(());
    }
    eval(DOWNLOAD_SHIM_JS, Duration::from_secs(5))?;
    if probe(eval)? == "object" {
        Ok(())
    } else {
        Err(EngineError::Other(
            "download shim failed to install on this page".into(),
        ))
    }
}

fn parse_download_entry(v: &serde_json::Value) -> DownloadEntry {
    let s = |k: &str| {
        v.get(k)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    DownloadEntry {
        id: v.get("id").and_then(serde_json::Value::as_u64).unwrap_or(0),
        filename: s("name"),
        mime: s("mime"),
        url: s("url"),
        size: v
            .get("size")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0),
        error: v
            .get("err")
            .and_then(serde_json::Value::as_str)
            .map(ToString::to_string),
        done: v
            .get("done")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
    }
}

/// Run `download`: resolve the source to a buffered entry, wait for it
/// to complete, then pull its payload out of the page in chunks.
///
/// The bytes never touch the wire — the daemon writes them to disk and
/// reports only the path. `tick` pumps the platform run loop between
/// polls, exactly as in [`run_wait`].
pub(crate) fn run_download<F, T>(
    eval: F,
    mut tick: T,
    source: &DownloadSource,
    budget: Duration,
    native: &SharedDownloads,
) -> EngineResult<Download>
where
    F: Fn(&str, Duration) -> EngineResult<String>,
    T: FnMut(),
{
    use base64::Engine as _;

    ensure_download_shim(&eval)?;
    let deadline = std::time::Instant::now() + budget;
    let timeout = || EngineError::Timeout {
        budget,
        primitive: "download",
        detail: "",
    };

    let id = match source {
        DownloadSource::Url(url) => {
            let js = format!("window.__vsDl.fetch({}, null)", json_string(url));
            let raw = eval_str(eval(&js, Duration::from_secs(10))?);
            raw.trim().parse::<u64>().map_err(|_| {
                EngineError::Other(format!("download: shim returned no id ({raw:?})"))
            })?
        }
        DownloadSource::Captured { id: Some(id) } if *id >= NATIVE_ID_BASE => {
            return wait_native(native, *id, deadline, timeout, &mut tick);
        }
        DownloadSource::Captured { id: Some(id) } => *id,
        // Newest of either buffer. A server-sent attachment lands in
        // the native buffer, a page-script save in the shim's; an
        // `<a download>` to an http URL can land in both, and then the
        // shim's copy wins so the agent gets one file, not two.
        DownloadSource::Captured { id: None } => {
            match newest_either(&eval, native, deadline, timeout, &mut tick)? {
                Newest::Shim(id) => id,
                Newest::Native(id) => return wait_native(native, id, deadline, timeout, &mut tick),
            }
        }
    };

    let meta = loop {
        let raw = eval_str(eval(
            &format!("window.__vsDl.meta({id})"),
            Duration::from_secs(5),
        )?);
        if raw != "null" && !raw.is_empty() {
            let v: serde_json::Value = serde_json::from_str(&raw)
                .map_err(|e| EngineError::Other(format!("download: bad meta {raw:?}: {e}")))?;
            let entry = parse_download_entry(&v);
            if entry.done {
                break entry;
            }
        }
        if std::time::Instant::now() >= deadline {
            return Err(EngineError::Timeout {
                budget,
                primitive: "download",
                detail: "",
            });
        }
        tick();
    };

    if let Some(err) = meta.error {
        return Err(EngineError::Other(format!("download failed: {err}")));
    }

    // The shim caps a capture at MAX_DOWNLOAD_BYTES, but `size` is
    // read back out of the page and a page can overwrite `__vsDl`
    // wholesale. Re-check before reserving, so a bogus length is an
    // error rather than a multi-gigabyte allocation.
    let size = usize::try_from(meta.size).unwrap_or(usize::MAX);
    if size > MAX_DOWNLOAD_BYTES {
        return Err(EngineError::Other(format!(
            "download too large: {size} bytes (cap {MAX_DOWNLOAD_BYTES})"
        )));
    }
    let mut bytes: Vec<u8> = Vec::with_capacity(size);
    while bytes.len() < size {
        let off = bytes.len();
        let len = DOWNLOAD_CHUNK.min(size - off);
        let js = format!("window.__vsDl.read({}, {off}, {len})", meta.id);
        let b64 = eval_str(eval(&js, Duration::from_secs(30))?);
        if b64.is_empty() {
            break;
        }
        let chunk = base64::engine::general_purpose::STANDARD
            .decode(b64.as_bytes())
            .map_err(|e| EngineError::Other(format!("download: bad base64 chunk: {e}")))?;
        if chunk.is_empty() {
            break;
        }
        bytes.extend_from_slice(&chunk);
    }
    // Release the page-side copy; the buffer is small and holding a
    // second copy of a large file in the web content process is waste.
    let _ = eval(
        &format!("window.__vsDl.drop({})", meta.id),
        Duration::from_secs(5),
    );

    if bytes.len() != size {
        return Err(EngineError::Other(format!(
            "download truncated: read {} of {size} bytes",
            bytes.len()
        )));
    }
    Ok(Download {
        bytes,
        filename: meta.filename,
        mime: meta.mime,
        url: meta.url,
    })
}

/// Run `download_list`: the captured download intents, payloads
/// excluded.
pub(crate) fn run_download_list<F>(
    eval: F,
    native: &SharedDownloads,
) -> EngineResult<Vec<DownloadEntry>>
where
    F: Fn(&str, Duration) -> EngineResult<String>,
{
    ensure_download_shim(&eval)?;
    let raw = eval_str(eval("window.__vsDl.index()", Duration::from_secs(5))?);
    let v: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|e| EngineError::Other(format!("download list: bad json {raw:?}: {e}")))?;
    let mut out: Vec<DownloadEntry> = v
        .as_array()
        .map(|a| a.iter().map(parse_download_entry).collect())
        .unwrap_or_default();
    // An `<a download>` to an http URL can be caught by both; list it once.
    let nat = native.lock().map_err(poisoned)?.entries();
    for e in nat {
        if !out.iter().any(|j| j.url == e.url) {
            out.push(e);
        }
    }
    Ok(out)
}

fn poisoned<T>(_: std::sync::PoisonError<T>) -> EngineError {
    EngineError::Other("download buffer lock poisoned".into())
}

/// The shim's newest entry, done or not.
struct ShimNewest {
    id: u64,
    t0: u64,
    url: String,
}

fn shim_newest<F>(eval: &F) -> EngineResult<Option<ShimNewest>>
where
    F: Fn(&str, Duration) -> EngineResult<String>,
{
    let raw = eval_str(eval("window.__vsDl.meta(0)", Duration::from_secs(5))?);
    if raw == "null" || raw.is_empty() {
        return Ok(None);
    }
    let v: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|e| EngineError::Other(format!("download: bad meta {raw:?}: {e}")))?;
    let num = |k: &str| v.get(k).and_then(serde_json::Value::as_u64).unwrap_or(0);
    Ok(Some(ShimNewest {
        id: num("id"),
        t0: num("t0").max(num("ts")),
        url: v
            .get("url")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string(),
    }))
}

enum Newest {
    Shim(u64),
    Native(u64),
}

/// Wait until either buffer has something to drain and say which.
fn newest_either<F, T>(
    eval: &F,
    native: &SharedDownloads,
    deadline: std::time::Instant,
    timeout: impl Fn() -> EngineError,
    tick: &mut T,
) -> EngineResult<Newest>
where
    F: Fn(&str, Duration) -> EngineResult<String>,
    T: FnMut(),
{
    loop {
        let js = shim_newest(eval)?;
        let nat = native.lock().map_err(poisoned)?.newest_with_url();
        match (nat, js) {
            (Some(n), j)
                if j.as_ref()
                    .is_none_or(|j| n.started_ms >= j.t0 && n.url != j.url) =>
            {
                if n.done {
                    return Ok(Newest::Native(n.id));
                }
            }
            (_, Some(j)) => return Ok(Newest::Shim(j.id)),
            (_, None) => {}
        }
        if std::time::Instant::now() >= deadline {
            return Err(timeout());
        }
        tick();
    }
}

/// Wait for a native download to settle, then take it.
fn wait_native<T: FnMut()>(
    native: &SharedDownloads,
    id: u64,
    deadline: std::time::Instant,
    timeout: impl Fn() -> EngineError,
    tick: &mut T,
) -> EngineResult<Download> {
    loop {
        let done = native.lock().map_err(poisoned)?.is_done(id);
        match done {
            None => {
                return Err(EngineError::Other(format!(
                    "download: no captured download with id {id}"
                )))
            }
            Some(true) => {
                let taken = native.lock().map_err(poisoned)?.take(id);
                return match taken {
                    Some(Ok(d)) => Ok(d),
                    Some(Err(e)) => Err(EngineError::Other(format!("download failed: {e}"))),
                    None => Err(EngineError::Other(format!(
                        "download: no captured download with id {id}"
                    ))),
                };
            }
            Some(false) => {}
        }
        if std::time::Instant::now() >= deadline {
            return Err(timeout());
        }
        tick();
    }
}
