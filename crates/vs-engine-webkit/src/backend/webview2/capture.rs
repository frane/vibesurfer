//! Capturing what a page loads: the main document's response for the
//! inspector, and responses that become downloads.

use crate::backend::native_download::{
    name_from_disposition, temp_path, SharedDownloads, MAX_BYTES,
};

use webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2;
use webview2_com::{take_pwstr, NavigationCompletedEventHandler};
use windows::core::{Interface, HSTRING, PWSTR};

use crate::engine::{EngineError, EngineResult};

use super::ResponseCapture;

/// Read a `PWSTR` out-parameter; empty on failure or null.
fn out_pwstr(f: impl FnOnce(*mut PWSTR) -> windows::core::Result<()>) -> String {
    let mut p = PWSTR(std::ptr::null_mut());
    if f(&raw mut p).is_ok() && !p.0.is_null() {
        take_pwstr(p)
    } else {
        String::new()
    }
}

/// Track the main document's response for `Engine::main_response`.
///
/// `NavigationStarting` (main frame only) clears the record, response
/// events are kept by URL until `NavigationCompleted`, and completion
/// stores the final `Source` and `HttpStatusCode`. A status of 0 (a
/// failed load, `data:`, `about:blank`) or a non-HTTP source records
/// nothing.
pub(super) fn install_response_capture(
    web_view: &ICoreWebView2,
) -> EngineResult<std::rc::Rc<std::cell::RefCell<ResponseCapture>>> {
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        ICoreWebView2NavigationCompletedEventArgs2, ICoreWebView2_2,
    };
    use webview2_com::{NavigationStartingEventHandler, WebResourceResponseReceivedEventHandler};
    use windows::core::BOOL;

    let state = std::rc::Rc::new(std::cell::RefCell::new(ResponseCapture::default()));

    let s = state.clone();
    let starting = NavigationStartingEventHandler::create(Box::new(move |_sender, _args| {
        let mut st = s.borrow_mut();
        st.in_flight = true;
        st.responses.clear();
        st.done = None;
        Ok(())
    }));
    let mut token: i64 = 0;
    unsafe { web_view.add_NavigationStarting(&starting, &raw mut token) }
        .map_err(|e| EngineError::Other(format!("add_NavigationStarting: {e}")))?;

    let s = state.clone();
    let completed = NavigationCompletedEventHandler::create(Box::new(move |sender, args| {
        let mut st = s.borrow_mut();
        st.in_flight = false;
        let mut status: i32 = 0;
        if let Some(a2) =
            args.and_then(|a| a.cast::<ICoreWebView2NavigationCompletedEventArgs2>().ok())
        {
            let _ = unsafe { a2.HttpStatusCode(&raw mut status) };
        }
        let source = sender
            .map(|v| out_pwstr(|p| unsafe { v.Source(p) }))
            .unwrap_or_default();
        let http = source.starts_with("http:") || source.starts_with("https:");
        st.done = match u16::try_from(status) {
            Ok(code) if code > 0 && http => Some((source.clone(), code)),
            _ => None,
        };
        st.responses.retain(|r| r.url == source);
        Ok(())
    }));
    let mut token: i64 = 0;
    unsafe { web_view.add_NavigationCompleted(&completed, &raw mut token) }
        .map_err(|e| EngineError::Other(format!("add_NavigationCompleted: {e}")))?;

    let v2: ICoreWebView2_2 = web_view
        .cast()
        .map_err(|e| EngineError::Other(format!("cast to ICoreWebView2_2: {e}")))?;
    let s = state.clone();
    let received =
        WebResourceResponseReceivedEventHandler::create(Box::new(move |_sender, args| {
            let Some(args) = args else { return Ok(()) };
            let uri = out_pwstr(|p| unsafe { args.Request()?.Uri(p) });
            {
                let st = s.borrow();
                // While the navigation is in flight, keep everything up to
                // the cap. After it, only a late response for the document.
                let wanted = if st.in_flight {
                    st.responses.len() < ResponseCapture::MAX
                } else {
                    st.done.as_ref().is_some_and(|(u, _)| *u == uri)
                        && !st.responses.iter().any(|r| r.url == uri)
                };
                if !wanted {
                    return Ok(());
                }
            }
            let resp = unsafe { args.Response() }?;
            let mut status: i32 = 0;
            unsafe { resp.StatusCode(&raw mut status) }?;
            let status = u16::try_from(status).unwrap_or(0);
            let mut status_text = out_pwstr(|p| unsafe { resp.ReasonPhrase(p) });
            if status_text.is_empty() {
                status_text = crate::inspector::reason_phrase(status).to_string();
            }
            let mut headers = Vec::new();
            let iter = unsafe { resp.Headers()?.GetIterator() }?;
            loop {
                let mut has = BOOL::default();
                unsafe { iter.HasCurrentHeader(&raw mut has) }?;
                if !has.as_bool() {
                    break;
                }
                let mut name = PWSTR(std::ptr::null_mut());
                let mut value = PWSTR(std::ptr::null_mut());
                unsafe { iter.GetCurrentHeader(&raw mut name, &raw mut value) }?;
                headers.push((take_pwstr(name).to_ascii_lowercase(), take_pwstr(value)));
                let mut next = BOOL::default();
                unsafe { iter.MoveNext(&raw mut next) }?;
            }
            s.borrow_mut()
                .responses
                .push(crate::inspector::MainResponse {
                    url: uri,
                    status,
                    status_text,
                    headers,
                });
            Ok(())
        }));
    let mut token: i64 = 0;
    unsafe { v2.add_WebResourceResponseReceived(&received, &raw mut token) }
        .map_err(|e| EngineError::Other(format!("add_WebResourceResponseReceived: {e}")))?;

    Ok(state)
}

/// Take over WebView2's own downloads.
///
/// WebView2 turns a `Content-Disposition: attachment` response (an
/// export button, a form POST answering with a file, a hidden-iframe
/// download) into a download with a shelf UI and a file in the user's
/// Downloads folder. A headless page wants neither: write it to a temp
/// file, hide the UI, and move the bytes into `downloads` for
/// `vs_download`. `blob:` and `data:` saves come from page script and
/// the shim already has them, so those are cancelled here.
pub(super) fn install_download_capture(
    web_view: &ICoreWebView2,
    downloads: &SharedDownloads,
) -> EngineResult<()> {
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        ICoreWebView2DownloadOperation, ICoreWebView2_4, COREWEBVIEW2_DOWNLOAD_INTERRUPT_REASON,
        COREWEBVIEW2_DOWNLOAD_STATE, COREWEBVIEW2_DOWNLOAD_STATE_COMPLETED,
        COREWEBVIEW2_DOWNLOAD_STATE_INTERRUPTED,
    };
    use webview2_com::{DownloadStartingEventHandler, StateChangedEventHandler};

    fn read_pwstr(f: impl FnOnce(*mut PWSTR) -> windows::core::Result<()>) -> String {
        let mut p = PWSTR(std::ptr::null_mut());
        if f(&raw mut p).is_ok() && !p.0.is_null() {
            take_pwstr(p)
        } else {
            String::new()
        }
    }

    let v4: ICoreWebView2_4 = web_view
        .cast()
        .map_err(|e| EngineError::Other(format!("cast to ICoreWebView2_4: {e}")))?;
    let downloads = downloads.clone();
    let handler = DownloadStartingEventHandler::create(Box::new(move |_sender, args| {
        let Some(args) = args else { return Ok(()) };
        let op: ICoreWebView2DownloadOperation = unsafe { args.DownloadOperation() }?;
        let url = read_pwstr(|p| unsafe { op.Uri(p) });
        if url.starts_with("blob:") || url.starts_with("data:") {
            unsafe { args.SetCancel(true) }?;
            return Ok(());
        }
        let cd = read_pwstr(|p| unsafe { op.ContentDisposition(p) });
        let mime = read_pwstr(|p| unsafe { op.MimeType(p) });
        let name = name_from_disposition(&cd).unwrap_or_default();
        let Ok(id) = downloads.lock().map(|mut d| d.begin(&url, &name, &mime)) else {
            return Ok(());
        };
        let mut total: i64 = 0;
        let _ = unsafe { op.TotalBytesToReceive(&raw mut total) };
        if u64::try_from(total).is_ok_and(|n| n > MAX_BYTES) {
            if let Ok(mut d) = downloads.lock() {
                d.finish(
                    id,
                    Err(format!("download exceeds {MAX_BYTES} bytes ({total})")),
                );
            }
            unsafe { args.SetCancel(true) }?;
            return Ok(());
        }
        let path = temp_path(id);
        let _ = std::fs::remove_file(&path);
        let wide = HSTRING::from(path.as_os_str());
        unsafe { args.SetResultFilePath(&wide) }?;
        unsafe { args.SetHandled(true) }?;

        let dl = downloads.clone();
        let state_handler = StateChangedEventHandler::create(Box::new(move |op, _| {
            let Some(op) = op else { return Ok(()) };
            let mut state = COREWEBVIEW2_DOWNLOAD_STATE::default();
            unsafe { op.State(&raw mut state) }?;
            let result = if state == COREWEBVIEW2_DOWNLOAD_STATE_COMPLETED {
                let path = temp_path(id);
                let bytes =
                    std::fs::read(&path).map_err(|e| format!("read {}: {e}", path.display()));
                let _ = std::fs::remove_file(&path);
                bytes
            } else if state == COREWEBVIEW2_DOWNLOAD_STATE_INTERRUPTED {
                let _ = std::fs::remove_file(temp_path(id));
                let mut reason = COREWEBVIEW2_DOWNLOAD_INTERRUPT_REASON::default();
                let _ = unsafe { op.InterruptReason(&raw mut reason) };
                Err(format!("download interrupted ({reason:?})"))
            } else {
                return Ok(());
            };
            if let Ok(mut d) = dl.lock() {
                d.finish(id, result);
            }
            Ok(())
        }));
        let mut token: i64 = 0;
        unsafe { op.add_StateChanged(&state_handler, &raw mut token) }?;
        Ok(())
    }));
    let mut token: i64 = 0;
    unsafe { v4.add_DownloadStarting(&handler, &raw mut token) }
        .map_err(|e| EngineError::Other(format!("add_DownloadStarting: {e}")))?;
    Ok(())
}
