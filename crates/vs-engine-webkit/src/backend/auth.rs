//! Auth-blob schema and JSON codec shared across backends.
//!
//! v1 (shipped through v0.1.1) was a JSON object with `cookies` as a
//! single `document.cookie`-style string. JavaScript's `document.cookie`
//! API cannot see or write cookies marked `HttpOnly`; the bug report
//! at v0.1.2 was that every modern web app's auth cookie silently
//! dropped on save and load.
//!
//! v2 carries cookies as a structured array. The per-backend code now
//! reads and writes those via the host-side cookie store
//! (`WKHTTPCookieStore` on macOS, `WebKitCookieManager` on Linux,
//! `ICoreWebView2CookieManager` on Windows), which sees `HttpOnly`
//! cookies because the attribute hides them from JS, not from the
//! engine. `localStorage` and `sessionStorage` are still pulled and
//! pushed via the JS shim; those buckets are JS-accessible by design.
//!
//! Loaders accept both v1 and v2 so saved blobs from v0.1.1 (no
//! `HttpOnly` cookies in them anyway) still round-trip.

use serde::{Deserialize, Serialize};

use crate::engine::{AuthBlob, EngineError, EngineResult};

/// On-wire representation of one cookie. Field names use snake_case
/// for the JSON; the engines convert to and from their native cookie
/// types.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CookieData {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    /// Seconds-since-epoch expiration. `None` means a session cookie
    /// (`Discard` in NSHTTPCookie / no `expires` attribute).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_unix: Option<i64>,
    #[serde(default)]
    pub secure: bool,
    #[serde(default)]
    pub http_only: bool,
    /// `Strict` / `Lax` / `None`. `None` here means "absent in source";
    /// most engines treat that as Lax under modern defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub same_site: Option<String>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthBlobV2 {
    /// Schema version. Always `2` on save in v0.1.2+.
    pub version: u32,
    /// URL the blob was captured against. Informational; loaders do
    /// not consult it.
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub origin: String,
    #[serde(default)]
    pub cookies: Vec<CookieData>,
    #[serde(default, rename = "localStorage")]
    pub local_storage: std::collections::BTreeMap<String, String>,
    #[serde(default, rename = "sessionStorage")]
    pub session_storage: std::collections::BTreeMap<String, String>,
}

/// v1 shape kept for back-compat parsing. We never write v1.
#[derive(Debug, Deserialize)]
struct AuthBlobV1Raw {
    #[serde(default)]
    cookies: String,
    #[serde(default, rename = "localStorage")]
    local_storage: std::collections::BTreeMap<String, String>,
    #[serde(default, rename = "sessionStorage")]
    session_storage: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    url: String,
    #[serde(default)]
    origin: String,
}

/// Serialize a v2 blob to bytes for at-rest storage.
pub fn encode(blob: &AuthBlobV2) -> EngineResult<AuthBlob> {
    let json = serde_json::to_string(blob)
        .map_err(|e| EngineError::Other(format!("encode auth blob: {e}")))?;
    Ok(AuthBlob {
        bytes: json.into_bytes(),
    })
}

/// Parse bytes into a v2 blob. Accepts v1 (auto-migrating the
/// document.cookie string into structured cookies with empty
/// attributes) and v2 (the modern shape).
pub fn decode(blob: &AuthBlob) -> EngineResult<AuthBlobV2> {
    let text = std::str::from_utf8(&blob.bytes)
        .map_err(|e| EngineError::Other(format!("auth blob not utf8: {e}")))?;
    let value: serde_json::Value = serde_json::from_str(text)
        .map_err(|e| EngineError::Other(format!("auth blob json: {e}")))?;
    // v2: `cookies` is an array. v1: `cookies` is a string.
    if value
        .get("cookies")
        .is_some_and(serde_json::Value::is_array)
    {
        return serde_json::from_value(value)
            .map_err(|e| EngineError::Other(format!("auth blob v2 decode: {e}")));
    }
    let v1: AuthBlobV1Raw = serde_json::from_value(value)
        .map_err(|e| EngineError::Other(format!("auth blob v1 decode: {e}")))?;
    let cookies = parse_document_cookie_string(&v1.cookies);
    Ok(AuthBlobV2 {
        version: 2,
        url: v1.url,
        origin: v1.origin,
        cookies,
        local_storage: v1.local_storage,
        session_storage: v1.session_storage,
    })
}

/// Best-effort parse of a `document.cookie` style string into structured
/// cookies. v1 only knew name=value pairs; domain/path/attributes are
/// blank, so on load the engine has to backfill from the page's origin.
fn parse_document_cookie_string(s: &str) -> Vec<CookieData> {
    s.split(';')
        .filter_map(|piece| {
            let t = piece.trim();
            if t.is_empty() {
                return None;
            }
            let (name, value) = t.split_once('=').unwrap_or((t, ""));
            Some(CookieData {
                name: name.trim().to_string(),
                value: value.trim().to_string(),
                domain: String::new(),
                path: "/".to_string(),
                expires_unix: None,
                secure: false,
                http_only: false,
                same_site: None,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v2_round_trip() {
        let blob = AuthBlobV2 {
            version: 2,
            url: "https://example.com/app".into(),
            origin: "https://example.com".into(),
            cookies: vec![CookieData {
                name: "access_token".into(),
                value: "abc".into(),
                domain: "example.com".into(),
                path: "/".into(),
                expires_unix: Some(1_800_000_000),
                secure: true,
                http_only: true,
                same_site: Some("Lax".into()),
            }],
            local_storage: [("k".to_string(), "v".to_string())].into(),
            session_storage: std::collections::BTreeMap::new(),
        };
        let bytes = encode(&blob).unwrap();
        let back = decode(&bytes).unwrap();
        assert_eq!(back.cookies.len(), 1);
        assert_eq!(back.cookies[0].name, "access_token");
        assert!(back.cookies[0].http_only);
        assert_eq!(back.local_storage.get("k").unwrap(), "v");
    }

    #[test]
    fn v1_blob_decodes_into_v2_shape() {
        // v0.1.1 saved this shape — cookies field is a string.
        let raw = r#"{
            "version": 1,
            "url": "https://example.com/",
            "origin": "https://example.com",
            "cookies": "session=xyz; pref=dark",
            "localStorage": {"theme": "dark"},
            "sessionStorage": {}
        }"#;
        let blob = AuthBlob {
            bytes: raw.as_bytes().to_vec(),
        };
        let v2 = decode(&blob).unwrap();
        assert_eq!(v2.cookies.len(), 2);
        assert_eq!(v2.cookies[0].name, "session");
        assert_eq!(v2.cookies[0].value, "xyz");
        assert!(!v2.cookies[0].http_only, "v1 had no notion of HttpOnly");
        assert_eq!(v2.local_storage.get("theme").unwrap(), "dark");
    }

    #[test]
    fn v1_with_empty_cookies_decodes() {
        let raw = r#"{"version":1,"cookies":"","localStorage":{},"sessionStorage":{}}"#;
        let blob = AuthBlob {
            bytes: raw.as_bytes().to_vec(),
        };
        let v2 = decode(&blob).unwrap();
        assert!(v2.cookies.is_empty());
    }

    #[test]
    fn malformed_json_returns_error() {
        let blob = AuthBlob {
            bytes: b"not json at all".to_vec(),
        };
        let err = decode(&blob).unwrap_err();
        assert!(format!("{err}").contains("auth blob"));
    }
}
