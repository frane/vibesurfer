//! Guess a request's resource type from what reaches the proxy.
//!
//! The proxy sees bytes, not the engine's request object, so the type
//! is inferred, in this order:
//!
//! 1. `Sec-Fetch-Dest` and `Sec-Fetch-Mode`, when the browser sends
//!    them (Chromium always; WebKit only to potentially trustworthy
//!    origins such as `*.localhost`, not to plain-http hosts).
//! 2. The `Accept` field: `text/html` means a document, `text/css` a
//!    stylesheet, `image/` first an image.
//! 3. The URL path's extension.
//! 4. Otherwise `other`.
//!
//! `fetch` covers both fetch() and XMLHttpRequest: neither sends a
//! field that tells them apart. A document is a navigation (`nav=1`);
//! its frame is `sub` only when `Sec-Fetch-Dest` says iframe or frame,
//! so without fetch metadata every document reports `main`.

use super::http::{get, Headers};

/// Resource type, navigation flag, and (for documents) frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Kind {
    pub(crate) ty: &'static str,
    pub(crate) nav: bool,
    /// `Some("main" | "sub")` for documents.
    pub(crate) frame: Option<&'static str>,
}

pub(crate) fn classify(method: &str, url: &str, headers: &Headers) -> Kind {
    let dest = get(headers, "sec-fetch-dest").map(str::to_ascii_lowercase);
    let mode = get(headers, "sec-fetch-mode").map(str::to_ascii_lowercase);
    if let Some(dest) = dest.as_deref() {
        let nav = mode.as_deref() == Some("navigate");
        let (ty, frame) = match dest {
            "document" => ("document", Some("main")),
            "iframe" | "frame" => ("document", Some("sub")),
            "script" | "worker" | "sharedworker" | "serviceworker" | "audioworklet"
            | "paintworklet" => ("script", None),
            "style" => ("stylesheet", None),
            "image" => ("image", None),
            "font" => ("font", None),
            "audio" | "video" | "track" => ("media", None),
            "manifest" => ("manifest", None),
            "websocket" => ("websocket", None),
            "empty" => ("fetch", None),
            _ => ("other", None),
        };
        return Kind { ty, nav, frame };
    }
    let accept = get(headers, "accept").unwrap_or("").to_ascii_lowercase();
    let get_or_post = method.eq_ignore_ascii_case("GET") || method.eq_ignore_ascii_case("POST");
    if get_or_post && (accept.starts_with("text/html") || accept.starts_with("application/xhtml")) {
        return Kind {
            ty: "document",
            nav: true,
            frame: Some("main"),
        };
    }
    if get(headers, "upgrade").is_some_and(|v| v.eq_ignore_ascii_case("websocket")) {
        return simple("websocket");
    }
    if accept.starts_with("text/css") {
        return simple("stylesheet");
    }
    if accept.starts_with("image/") {
        return simple("image");
    }
    simple(by_extension(url))
}

fn simple(ty: &'static str) -> Kind {
    Kind {
        ty,
        nav: false,
        frame: None,
    }
}

fn by_extension(url: &str) -> &'static str {
    let path = url
        .split(['?', '#'])
        .next()
        .unwrap_or(url)
        .rsplit('/')
        .next()
        .unwrap_or("");
    let ext = path
        .rsplit_once('.')
        .map_or("", |(_, e)| e)
        .to_ascii_lowercase();
    match ext.as_str() {
        "js" | "mjs" | "cjs" => "script",
        "css" => "stylesheet",
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "avif" | "svg" | "ico" | "bmp" => "image",
        "woff" | "woff2" | "ttf" | "otf" | "eot" => "font",
        "mp4" | "webm" | "mp3" | "ogg" | "wav" | "m4a" => "media",
        "html" | "htm" => "document",
        _ => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(pairs: &[(&str, &str)]) -> Headers {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn fetch_metadata_wins() {
        let k = classify(
            "GET",
            "http://a/f",
            &h(&[("Sec-Fetch-Dest", "iframe"), ("Sec-Fetch-Mode", "navigate")]),
        );
        assert_eq!(
            k,
            Kind {
                ty: "document",
                nav: true,
                frame: Some("sub")
            }
        );
        let k = classify("GET", "http://a/x.png", &h(&[("Sec-Fetch-Dest", "empty")]));
        assert_eq!(k.ty, "fetch");
    }

    #[test]
    fn falls_back_to_accept_then_extension() {
        let doc = h(&[("Accept", "text/html,application/xhtml+xml,*/*;q=0.8")]);
        assert_eq!(classify("GET", "http://a/", &doc).ty, "document");
        assert!(classify("GET", "http://a/", &doc).nav);
        let img = h(&[("Accept", "image/webp,image/png,*/*;q=0.5")]);
        assert_eq!(classify("GET", "http://a/p", &img).ty, "image");
        assert_eq!(
            classify("GET", "http://a/s.js?v=1", &h(&[("Accept", "*/*")])).ty,
            "script"
        );
        assert_eq!(classify("GET", "http://a/api", &h(&[])).ty, "other");
    }
}
