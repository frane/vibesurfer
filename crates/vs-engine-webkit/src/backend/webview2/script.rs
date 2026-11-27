//! Running script in a web view: `ExecuteScript`, DevTools protocol
//! calls (clicks and keys), document-start scripts and the inspector
//! message bridge.

use webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2;
use webview2_com::{
    take_pwstr, AddScriptToExecuteOnDocumentCreatedCompletedHandler,
    CallDevToolsProtocolMethodCompletedHandler, ExecuteScriptCompletedHandler,
    WebMessageReceivedEventHandler,
};
use windows::core::{HSTRING, PCWSTR, PWSTR};

use crate::backend::inspector_bridge::{
    self, InspectorSlots, NetworkIngestSlot, CONSOLE_HANDLER, NETWORK_HANDLER,
};
use crate::engine::{EngineError, EngineResult};

use super::wait::{failed, run_async, CALL_BUDGET};

/// Call `webview.ExecuteScript(js)` and return the JSON-encoded
/// result, pumping messages while it runs (see [`run_async`]).
pub(super) fn execute_script(web_view: &ICoreWebView2, js: &str) -> EngineResult<String> {
    let js = HSTRING::from(js);
    run_async(
        "ExecuteScript",
        CALL_BUDGET,
        |tx| {
            ExecuteScriptCompletedHandler::create(Box::new(move |error, json| {
                let _ = tx.send(
                    error
                        .map(|()| json)
                        .map_err(|e| failed("ExecuteScript", &e)),
                );
                Ok(())
            }))
        },
        |h| unsafe { web_view.ExecuteScript(PCWSTR(js.as_ptr()), h) },
    )
}

/// Call a DevTools Protocol method on `web_view` and return its JSON
/// result.
///
/// This is how keyboard input reaches the page on Windows. WebView2's
/// visual hosting has `SendMouseInput` but no keyboard equivalent —
/// in that mode the host is expected to forward Win32 key messages,
/// which a headless daemon with no focused window cannot do. The
/// DevTools protocol dispatches straight into the renderer instead,
/// producing `isTrusted=true` key events without needing window
/// focus, which is what the primitive is for.
pub(super) fn call_cdp(
    web_view: &ICoreWebView2,
    method: &str,
    params_json: &str,
) -> EngineResult<String> {
    let (m, p) = (HSTRING::from(method), HSTRING::from(params_json));
    let what = format!("CallDevToolsProtocolMethod({method})");
    run_async(
        "CallDevToolsProtocolMethod",
        CALL_BUDGET,
        |tx| {
            CallDevToolsProtocolMethodCompletedHandler::create(Box::new(move |error, json| {
                let _ = tx.send(error.map(|()| json).map_err(|e| failed(&what, &e)));
                Ok(())
            }))
        },
        |h| unsafe {
            web_view.CallDevToolsProtocolMethod(PCWSTR(m.as_ptr()), PCWSTR(p.as_ptr()), h)
        },
    )
}

/// A click with another button, count or modifiers, as DevTools input
/// (trusted, like `SendMouseInput`, which has no Alt or Meta): one
/// pressed/released pair per click, with `clickCount` rising, so the
/// page sees `dblclick` and `detail`.
pub(super) fn cdp_click(
    web_view: &ICoreWebView2,
    at: vs_humanize::Point,
    opts: crate::engine::ClickOpts,
) -> EngineResult<()> {
    use crate::engine::MouseButton;
    let button = match opts.button {
        MouseButton::Left => "left",
        MouseButton::Right => "right",
        MouseButton::Middle => "middle",
    };
    let m = opts.modifiers;
    // Alt=1, Ctrl=2, Meta=4, Shift=8.
    let modifiers = u8::from(m.alt)
        | (u8::from(m.ctrl) << 1)
        | (u8::from(m.meta) << 2)
        | (u8::from(m.shift) << 3);
    for n in 1..=opts.count.max(1) {
        for ty in ["mousePressed", "mouseReleased"] {
            let params = serde_json::json!({
                "type": ty, "x": at.x, "y": at.y, "button": button,
                "clickCount": n, "modifiers": modifiers,
            });
            call_cdp(web_view, "Input.dispatchMouseEvent", &params.to_string())?;
        }
    }
    Ok(())
}

/// How one character is spelled to `Input.dispatchKeyEvent`.
///
/// A printable character identifies itself and is inserted because of
/// `text`. A named key must not carry text, or the renderer inserts a
/// character instead of acting on the key; Enter is the exception,
/// where the text is what submits a form.
struct CdpKey {
    key: String,
    code: Option<&'static str>,
    virtual_key: u32,
    text: String,
}

fn cdp_key(ch: char) -> CdpKey {
    let named = match ch {
        '\n' | '\r' => Some(("Enter", "Enter", 13_u32, "\r")),
        '\t' => Some(("Tab", "Tab", 9, "")),
        '\u{8}' => Some(("Backspace", "Backspace", 8, "")),
        '\u{1b}' => Some(("Escape", "Escape", 27, "")),
        _ => None,
    };
    match named {
        Some((key, code, virtual_key, text)) => CdpKey {
            key: key.to_string(),
            code: Some(code),
            virtual_key,
            text: text.to_string(),
        },
        None => CdpKey {
            key: ch.to_string(),
            code: None,
            virtual_key: 0,
            text: ch.to_string(),
        },
    }
}

thread_local! {
    /// Set while a popup's view is set up inside WebView2's
    /// controller-created callback (`spawn_popup`), where waiting for an
    /// async completion pumps messages inside the callback and never
    /// returns. Scripts are then registered without waiting: the call
    /// takes effect at once, so the popup's first document gets them.
    static NO_WAIT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Run `f` with `add_init_script` not waiting for completions.
pub(super) fn without_waiting<T>(f: impl FnOnce() -> T) -> T {
    NO_WAIT.with(|w| w.set(true));
    let out = f();
    NO_WAIT.with(|w| w.set(false));
    out
}

/// Add `js` to the document-start initializer for `web_view`.
/// Returns the script id, which the caller stores in case we want to
/// remove it later (empty when not waited for; see `NO_WAIT`).
pub(super) fn add_init_script(web_view: &ICoreWebView2, js: &str) -> EngineResult<String> {
    if NO_WAIT.with(std::cell::Cell::get) {
        let done =
            AddScriptToExecuteOnDocumentCreatedCompletedHandler::create(Box::new(|_, _| Ok(())));
        let w = HSTRING::from(js);
        unsafe { web_view.AddScriptToExecuteOnDocumentCreated(PCWSTR(w.as_ptr()), &done) }
            .map_err(|e| EngineError::Other(format!("AddScriptToExecuteOnDocumentCreated: {e}")))?;
        return Ok(String::new());
    }
    let js = HSTRING::from(js);
    run_async(
        "AddScriptToExecuteOnDocumentCreated",
        CALL_BUDGET,
        |tx| {
            AddScriptToExecuteOnDocumentCreatedCompletedHandler::create(Box::new(
                move |error, id| {
                    let _ = tx.send(
                        error
                            .map(|()| id)
                            .map_err(|e| failed("AddScriptToExecuteOnDocumentCreated", &e)),
                    );
                    Ok(())
                },
            ))
        },
        |h| unsafe { web_view.AddScriptToExecuteOnDocumentCreated(PCWSTR(js.as_ptr()), h) },
    )
}

/// Run the inspector install path: subscribe `WebMessageReceived`
/// (one handler that switches on a `kind` field), then add the
/// shared inspector_capture.js as a document-start script. Returns
/// `false` if the env var `VS_DISABLE_INSPECTOR=1` is set, mirroring
/// Mac/Linux for the capability-gate test.
pub(super) fn install_inspector(web_view: &ICoreWebView2, slots: &InspectorSlots) -> bool {
    if std::env::var_os("VS_DISABLE_INSPECTOR").is_some() {
        return false;
    }
    let cs_console = slots.console.clone();
    let cs_network = slots.network.clone();
    let cs_details = slots.details.clone();
    let cs_pending = slots.pending.clone();

    // The shared bridge JS (in inspector_capture.js) calls
    // `window.webkit.messageHandlers.<name>.postMessage(json)` on
    // Mac/Linux. WebView2 only exposes `window.chrome.webview
    // .postMessage(...)` — one global channel — so we install a
    // shim that maps the `webkit.messageHandlers` API onto
    // chrome.webview by tagging the JSON with `__channel`.
    // Wrapped in an IIFE: a top-level `function __vsMakeHandler` here
    // became a global, and `Object.keys(window)` then listed it
    // alongside the page's own properties — the same instrumentation
    // leak the shared shims were fixed for. Caught by the
    // fingerprint cell on windows-latest.
    let shim = r"
      (function () {
        window.webkit = window.webkit || {};
        window.webkit.messageHandlers = window.webkit.messageHandlers || {};
        function __vsMakeHandler(name) {
            return {
                postMessage: function(msg) {
                    var s = (typeof msg === 'string') ? msg : JSON.stringify(msg);
                    // Pass an object, not a string. postMessage of a
                    // string makes WebMessageAsJson on the host side
                    // return a JSON-encoded string literal that our
                    // host parser cannot field-access. The object
                    // form makes WebMessageAsJson return the object,
                    // and our handler reads __channel + body off it.
                    window.chrome.webview.postMessage({ __channel: name, body: s });
                },
            };
        }
        window.webkit.messageHandlers.vsConsole = __vsMakeHandler('vsConsole');
        window.webkit.messageHandlers.vsNetwork = __vsMakeHandler('vsNetwork');
      })();
    ";
    if add_init_script(web_view, shim).is_err() {
        return false;
    }
    if add_init_script(web_view, inspector_bridge::SCRIPT).is_err() {
        return false;
    }

    let handler = WebMessageReceivedEventHandler::create(Box::new(move |_sender, args_opt| {
        let Some(args) = args_opt else {
            return Ok(());
        };
        let mut raw = PWSTR(std::ptr::null_mut());
        if unsafe { args.WebMessageAsJson(&raw mut raw) }.is_err() {
            return Ok(());
        }
        let outer = take_pwstr(raw);
        let v: serde_json::Value = match serde_json::from_str(&outer) {
            Ok(v) => v,
            Err(_) => return Ok(()),
        };
        // The shim wraps every host message as
        // `{__channel: name, body: <json-string>}`. Unwrap.
        let channel = v.get("__channel").and_then(|x| x.as_str()).unwrap_or("");
        let body = v.get("body").and_then(|x| x.as_str()).unwrap_or("");
        match channel {
            CONSOLE_HANDLER => {
                let mut buf = cs_console.borrow_mut();
                inspector_bridge::ingest_console(&mut buf, body);
            }
            NETWORK_HANDLER => {
                let mut entries = cs_network.borrow_mut();
                let mut details = cs_details.borrow_mut();
                let mut pending = cs_pending.borrow_mut();
                inspector_bridge::ingest_network(
                    NetworkIngestSlot {
                        entries: &mut entries,
                        details: &mut details,
                        pending: &mut pending,
                    },
                    body,
                );
            }
            _ => {}
        }
        Ok(())
    }));
    let mut token: i64 = 0;
    unsafe { web_view.add_WebMessageReceived(&handler, &raw mut token) }.is_ok()
}

/// `Input.dispatchKeyEvent` parameters (keyDown, keyUp) typing `ch`.
pub(super) fn cdp_key_events(ch: char) -> (String, String) {
    let spelling = cdp_key(ch);
    let mut down = serde_json::json!({
        "type": "keyDown",
        "key": spelling.key,
        "text": spelling.text,
        "unmodifiedText": spelling.text,
        "windowsVirtualKeyCode": spelling.virtual_key,
    });
    let mut up = serde_json::json!({
        "type": "keyUp",
        "key": spelling.key,
        "windowsVirtualKeyCode": spelling.virtual_key,
    });
    if let Some(code) = spelling.code {
        down["code"] = serde_json::Value::from(code);
        up["code"] = serde_json::Value::from(code);
    }
    (down.to_string(), up.to_string())
}
