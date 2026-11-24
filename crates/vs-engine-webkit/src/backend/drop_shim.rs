//! The script half of a native drop. Every engine delivers the drop as
//! trusted OS input; this covers what the OS pasteboard cannot carry:
//! string items of MIME types it has no slot for, strings next to files
//! (WebKit shows a file drag only as `Files`), and the files themselves
//! where the engine delivers none (WebKitGTK from an X11 source).

use base64::Engine as _;

use crate::engine::DropData;

/// MIME types a native drop carries on every engine's pasteboard.
pub(crate) const NATIVE_DROP_TYPES: [&str; 3] = ["text/plain", "text/html", "text/uri-list"];

/// Files larger than this are left to the engine alone.
const FALLBACK_FILE_LIMIT: u64 = 64 << 20;

const SHIM: &str = include_str!("drop_shim.js");

/// Undoes [`install_js`].
pub(crate) const RESTORE_JS: &str = "(window.__vsDropRestore && window.__vsDropRestore(), 'ok')";

/// The script to run before the native drop, or `None` when the drop
/// needs no help.
pub(crate) fn install_js(data: &DropData) -> Option<String> {
    let with_files = !data.files.is_empty();
    let extra: serde_json::Map<String, serde_json::Value> = data
        .items
        .iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), v.clone()))
        .filter(|(k, _)| with_files || !NATIVE_DROP_TYPES.contains(&k.as_str()))
        .map(|(k, v)| (k, serde_json::Value::String(v)))
        .collect();
    let files: Vec<serde_json::Value> = data.files.iter().filter_map(|p| file_spec(p)).collect();
    if extra.is_empty() && files.is_empty() {
        return None;
    }
    Some(
        SHIM.replace("__EXTRA__", &serde_json::Value::Object(extra).to_string())
            .replace("__FILES__", &serde_json::Value::Array(files).to_string()),
    )
}

/// `{name, type, b64}` for one dropped file, or `None` when it can't be
/// read or is too large to inline.
fn file_spec(path: &std::path::Path) -> Option<serde_json::Value> {
    if std::fs::metadata(path).ok()?.len() > FALLBACK_FILE_LIMIT {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    let name = path.file_name()?.to_string_lossy().into_owned();
    Some(serde_json::json!({
        "name": name,
        "type": mime_for(&name),
        "b64": base64::engine::general_purpose::STANDARD.encode(bytes),
    }))
}

/// The MIME type browsers give a dropped file of this name.
fn mime_for(name: &str) -> &'static str {
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "txt" => "text/plain",
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "csv" => "text/csv",
        "js" | "mjs" => "text/javascript",
        "json" => "application/json",
        "xml" => "application/xml",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_alone_needs_no_script() {
        let data = DropData {
            files: vec![],
            items: vec![("text/plain".into(), "hi".into())],
        };
        assert!(install_js(&data).is_none());
    }

    #[test]
    fn custom_types_and_files_are_inlined() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.txt");
        std::fs::write(&f, "x").unwrap();
        let data = DropData {
            files: vec![f],
            items: vec![
                ("text/plain".into(), "hi".into()),
                ("application/x-a".into(), "b".into()),
            ],
        };
        let js = install_js(&data).unwrap();
        assert!(
            js.contains(r#""text/plain":"hi""#),
            "strings ride along with files"
        );
        assert!(js.contains(r#""application/x-a":"b""#));
        assert!(js.contains(r#""name":"a.txt""#) && js.contains(r#""type":"text/plain""#));
        assert!(!js.contains("__EXTRA__") && !js.contains("__FILES__"));
    }
}
