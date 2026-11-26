//! Loads: waiting for one to finish, capturing the main response for
//! the inspector, and turning responses into downloads.

use std::cell::RefCell;
use std::rc::Rc;

use glib::prelude::*;
use webkit6::prelude::*;
use webkit6::{LoadEvent, WebView};

use super::run_loop_until;
use crate::engine::{EngineError, EngineResult};

use crate::backend::native_download::{
    is_attachment, name_from_disposition, temp_path, SharedDownloads, MAX_BYTES,
};

/// A load that ended because its response became a download left the
/// page as it was and put the file in the download buffer. Not a
/// failure.
pub(super) fn load_result(err: &glib::Error) -> Result<(), String> {
    if err.matches(webkit6::PolicyError::FrameLoadInterruptedByPolicyChange) {
        Ok(())
    } else {
        Err(err.message().to_string())
    }
}

/// Keep the HTTP response of each main-frame navigation.
///
/// `load-changed` fires for the main frame only. The slot is cleared
/// when a load starts, so a failed or non-HTTP load reports nothing,
/// and filled at commit, when the main resource has its response.
pub(super) fn install_response_capture(
    web_view: &WebView,
) -> Rc<RefCell<Option<crate::inspector::MainResponse>>> {
    let slot: Rc<RefCell<Option<crate::inspector::MainResponse>>> = Rc::new(RefCell::new(None));
    let sink = slot.clone();
    web_view.connect_load_changed(move |view, event| match event {
        LoadEvent::Started => {
            sink.borrow_mut().take();
        }
        LoadEvent::Committed => {
            *sink.borrow_mut() = view
                .main_resource()
                .and_then(|r| r.response())
                .and_then(|r| http_response(&r));
        }
        _ => {}
    });
    slot
}

pub(super) fn http_response(resp: &webkit6::URIResponse) -> Option<crate::inspector::MainResponse> {
    let url = resp.uri().map(|u| u.to_string()).unwrap_or_default();
    if !(url.starts_with("http:") || url.starts_with("https:")) {
        return None;
    }
    let status = u16::try_from(resp.status_code()).unwrap_or(0);
    if status == 0 {
        return None;
    }
    let mut headers = Vec::new();
    if let Some(h) = resp.http_headers() {
        h.foreach(|name, value| headers.push((name.to_ascii_lowercase(), value.to_string())));
    }
    Some(crate::inspector::MainResponse {
        url,
        status,
        status_text: crate::inspector::reason_phrase(status).to_string(),
        headers,
    })
}

/// Save attachments instead of dropping them.
///
/// A `Content-Disposition: attachment` response (an export button, a
/// form POST answering with a file, a hidden-iframe download) never
/// reaches page script. Turn it into a WebKit download, write it to a
/// temp file, and move the bytes into `downloads` for `vs_download`.
/// The network session is shared by every page of the same store, so
/// the `download-started` handler checks the download belongs to this
/// one.
pub(super) fn install_download_capture(
    web_view: &WebView,
    downloads: &SharedDownloads,
) -> Option<(webkit6::NetworkSession, glib::SignalHandlerId)> {
    web_view.connect_decide_policy(|_view, decision, ty| {
        if ty != webkit6::PolicyDecisionType::Response {
            return false;
        }
        let Some(rd) = decision.downcast_ref::<webkit6::ResponsePolicyDecision>() else {
            return false;
        };
        let Some(resp) = rd.response() else {
            return false;
        };
        let uri = resp.uri().map(|u| u.to_string()).unwrap_or_default();
        if !(uri.starts_with("http:") || uri.starts_with("https:")) {
            return false;
        }
        let cd = resp
            .http_headers()
            .and_then(|h| h.one("Content-Disposition"))
            .map(|v| v.to_string())
            .unwrap_or_default();
        if is_attachment(&cd) || !rd.is_mime_type_supported() {
            decision.download();
            return true;
        }
        false
    });

    let session = WebViewExt::network_session(web_view)?;
    let weak = web_view.downgrade();
    let downloads = downloads.clone();
    let id = session.connect_download_started(move |_session, download| {
        let Some(view) = weak.upgrade() else { return };
        if download.web_view().as_ref() != Some(&view) {
            return;
        }
        let resp = download.response();
        let url = resp
            .as_ref()
            .and_then(webkit6::URIResponse::uri)
            .or_else(|| download.request().and_then(|r| r.uri()))
            .map(|u| u.to_string())
            .unwrap_or_default();
        let mime = resp
            .as_ref()
            .and_then(webkit6::URIResponse::mime_type)
            .map(|m| m.to_string())
            .unwrap_or_default();
        let name = resp
            .as_ref()
            .and_then(webkit6::URIResponse::http_headers)
            .and_then(|h| h.one("Content-Disposition"))
            .and_then(|cd| name_from_disposition(&cd))
            .unwrap_or_default();
        let Ok(id) = downloads.lock().map(|mut d| d.begin(&url, &name, &mime)) else {
            return;
        };
        let path = temp_path(id);
        let _ = std::fs::remove_file(&path);

        let dl = downloads.clone();
        download.connect_decide_destination(move |download, suggested| {
            if let Ok(mut d) = dl.lock() {
                if name.is_empty() {
                    d.set_name(id, suggested);
                }
            }
            let expected = download.response().map_or(0, |r| r.content_length());
            if expected > MAX_BYTES {
                if let Ok(mut d) = dl.lock() {
                    d.finish(
                        id,
                        Err(format!("download exceeds {MAX_BYTES} bytes ({expected})")),
                    );
                }
                download.cancel();
                return true;
            }
            download.set_destination(&path.to_string_lossy());
            true
        });
        let dl = downloads.clone();
        download.connect_failed(move |_download, err| {
            if let Ok(mut d) = dl.lock() {
                d.finish(id, Err(err.message().to_string()));
            }
            let _ = std::fs::remove_file(temp_path(id));
        });
        let dl = downloads.clone();
        download.connect_finished(move |_download| {
            let path = temp_path(id);
            // `failed` is followed by `finished`; the first result wins.
            let bytes = std::fs::read(&path).map_err(|e| format!("read {}: {e}", path.display()));
            let _ = std::fs::remove_file(&path);
            if let Ok(mut d) = dl.lock() {
                d.finish(id, bytes);
            }
        });
    });
    Some((session, id))
}

/// Wait for the load already started on `web_view` to finish or fail.
pub(super) fn wait_for_load(web_view: &WebView, primitive: &'static str) -> EngineResult<()> {
    let slot: Rc<RefCell<Option<Result<(), String>>>> = Rc::new(RefCell::new(None));
    let slot_for_signal = slot.clone();
    let signal_id = web_view.connect_load_changed(move |_view, event| {
        if event == LoadEvent::Finished {
            *slot_for_signal.borrow_mut() = Some(Ok(()));
        }
    });
    let slot_fail = slot.clone();
    let fail_id = web_view.connect_load_failed(move |_view, _event, _uri, err| {
        *slot_fail.borrow_mut() = Some(load_result(err));
        true
    });

    let slot_check = slot.clone();
    let budget = crate::backend::common::nav_budget();
    let ok = run_loop_until(move || slot_check.borrow().is_some(), budget);

    web_view.disconnect(signal_id);
    web_view.disconnect(fail_id);

    if !ok {
        return Err(EngineError::Timeout {
            budget,
            primitive,
            detail: "",
        });
    }
    // Taken out of the RefCell before the match, not inside it:
    // as the function's tail expression the borrow guard would
    // otherwise outlive `slot` itself.
    let result = slot.borrow_mut().take();
    match result {
        Some(Ok(())) => Ok(()),
        Some(Err(msg)) => Err(EngineError::Other(format!("navigation failed: {msg}"))),
        None => Err(EngineError::Other(
            "navigation completed without a result".into(),
        )),
    }
}
