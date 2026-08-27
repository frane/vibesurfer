//! Backend-shared helpers — no platform deps.
//!
//! All three real backends (`webkit`, `wpe`, `webview2`) inject the
//! same DOM-walker JS and parse the same JSON shape into a
//! [`Tree`]. This module holds the parser so each backend doesn't
//! carry its own copy.
//!
//! The module is unconditionally compiled (no `cfg`) so the helpers
//! are available regardless of target.

use std::collections::{BTreeMap, BTreeSet};

use vs_protocol::{Node, Op, Ref, Role, Tree};

pub(crate) fn parse_role(s: &str) -> Role {
    match s {
        "doc" => Role::Doc,
        "btn" => Role::Btn,
        "lnk" => Role::Lnk,
        "tf" => Role::Tf,
        "ta" => Role::Ta,
        "sel" => Role::Sel,
        "chk" => Role::Chk,
        "rad" => Role::Rad,
        "img" => Role::Img,
        "hd" => Role::Hd,
        "p" => Role::P,
        "li" => Role::Li,
        "lst" => Role::Lst,
        "tbl" => Role::Tbl,
        "row" => Role::Row,
        "cell" => Role::Cell,
        "hdr" => Role::Hdr,
        "nav" => Role::Nav,
        "frm" => Role::Frm,
        "dlg" => Role::Dlg,
        "itm" => Role::Itm,
        "sec" => Role::Sec,
        "art" => Role::Art,
        "mn" => Role::Mn,
        "ifr" => Role::Ifr,
        _ => Role::El,
    }
}

pub(crate) fn ops_for_role(role: Role) -> BTreeSet<Op> {
    let mut s = BTreeSet::new();
    match role {
        Role::Btn | Role::Lnk | Role::Chk | Role::Rad => {
            s.insert(Op::Click);
            s.insert(Op::Focus);
        }
        Role::Tf | Role::Ta => {
            s.insert(Op::Fill);
            s.insert(Op::Focus);
        }
        Role::Sel => {
            s.insert(Op::Focus);
        }
        Role::Frm => {
            s.insert(Op::Submit);
        }
        _ => {}
    }
    s
}

/// Parse the JSON output of [`SNAPSHOT_JS`](super) into a [`Tree`].
/// `json` is whatever the backend's JS-eval returned — either a raw
/// JSON object or a doubly-encoded JSON string. We unwrap one level
/// of string quoting if present.
pub(crate) fn parse_snapshot(json: &str) -> Result<Tree, String> {
    let unwrapped: String =
        serde_json::from_str::<String>(json).unwrap_or_else(|_| json.to_string());
    let v: serde_json::Value =
        serde_json::from_str(&unwrapped).map_err(|e| format!("invalid snapshot json: {e}"))?;
    let root = build_node(&v).ok_or_else(|| "missing root".to_string())?;
    Ok(Tree { roots: vec![root] })
}

fn build_node(v: &serde_json::Value) -> Option<Node> {
    // A ref beyond u32 means the page emitted a snapshot we can't
    // represent — drop the node rather than silently aliasing Ref(0).
    let r = u32::try_from(v.get("r")?.as_u64()?).ok()?;
    let role_s = v.get("role")?.as_str()?;
    let label = v.get("label").and_then(|x| x.as_str()).unwrap_or("");
    let role = parse_role(role_s);
    let mut children = Vec::new();
    if let Some(arr) = v.get("children").and_then(|x| x.as_array()) {
        for c in arr {
            if let Some(n) = build_node(c) {
                children.push(n);
            }
        }
    }
    let mut attrs = BTreeMap::new();
    if v.get("hid").and_then(serde_json::Value::as_u64) == Some(1) {
        attrs.insert("hid".to_string(), "1".to_string());
    }
    Some(Node {
        r: Ref(r),
        role,
        label: label.to_string(),
        ops: ops_for_role(role),
        attrs,
        children,
    })
}

/// JS that snapshots cookies + localStorage + sessionStorage as a JSON
/// blob. Used by `save_auth` on every backend.
#[allow(dead_code)] // used by wpe + webview2; WkBackend moved to host-side cookie store
pub(crate) const AUTH_SAVE_JS: &str = include_str!("auth_save.js");

/// JS that restores the JSON blob produced by [`AUTH_SAVE_JS`]. The
/// caller wraps this in an IIFE that defines `blob` from
/// `JSON.parse(<payload>)`.
#[allow(dead_code)]
pub(crate) const AUTH_LOAD_BODY_JS: &str = include_str!("auth_load_body.js");

/// JS that snapshots only localStorage + sessionStorage. The cookie
/// portion is handled by the host-side cookie store API (which sees
/// HttpOnly cookies; `document.cookie` does not).
pub(crate) const STORAGE_SAVE_JS: &str = include_str!("storage_save.js");

/// JS that restores localStorage + sessionStorage from `payload`.
pub(crate) const STORAGE_LOAD_BODY_JS: &str = include_str!("storage_load_body.js");

/// Shared DOM-walker JS payload. All three real backends evaluate this
/// and parse the result with [`parse_snapshot`].
pub(crate) const SNAPSHOT_DOM_WALKER_JS: &str = include_str!("snapshot_dom_walker.js");

/// Download-capture shim. Injected at document-start into every frame
/// on every backend, and re-evaluated on demand by [`run_download`] for
/// documents that predate the injection. See the file header for why
/// interception happens above the engine's download machinery.
pub(crate) const DOWNLOAD_SHIM_JS: &str = include_str!("download_shim.js");

// =============================================================================
// Shared primitive logic — dispatched through each backend's eval_js
// =============================================================================
//
// All three real backends (`webkit`, `wpe`, `webview2`) implement a
// per-platform `eval_js(&WebView, js, budget) -> EngineResult<String>`
// helper. The generic functions below take an `eval` closure with that
// signature and implement `act`, `wait`, `layout`, `save_auth`, and
// `load_auth` on top — so the per-backend `Engine` impls shrink to a
// page-lookup + a single call.
//
// `eval` is taken as `Fn` so callers can borrow per-page state across
// multiple invocations (used by `wait`, which polls a JS predicate
// every ~150ms until satisfied or the budget runs out).

use std::time::Duration;

use crate::engine::{
    ActTarget, Action, AuthBlob, Download, DownloadEntry, DownloadSource, EngineError,
    EngineResult, LayoutBox, WaitCondition,
};

fn json_string(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

/// Full mouse-click event sequence for `Action::Click`, dispatched on
/// the resolved element `el`. Coordinates are the element's center so
/// pointer-position-aware handlers behave. The `button`/`buttons`
/// fields follow real semantics (held during down, released by up).
const CLICK_SEQUENCE_JS: &str = "\
const rc = el.getBoundingClientRect(); \
const x = rc.left + rc.width / 2, y = rc.top + rc.height / 2; \
const mk = (b) => ({ bubbles: true, cancelable: true, composed: true, view: window, clientX: x, clientY: y, screenX: x, screenY: y, button: 0, buttons: b }); \
const mkp = (b) => Object.assign(mk(b), { pointerId: 1, pointerType: 'mouse', isPrimary: true, width: 1, height: 1 }); \
try { \
  el.dispatchEvent(new PointerEvent('pointerover', mkp(0))); \
  el.dispatchEvent(new MouseEvent('mouseover', mk(0))); \
  el.dispatchEvent(new PointerEvent('pointermove', mkp(0))); \
  el.dispatchEvent(new PointerEvent('pointerdown', mkp(1))); \
  el.dispatchEvent(new MouseEvent('mousedown', mk(1))); \
  if (typeof el.focus === 'function') { try { el.focus(); } catch (e) {} } \
  el.dispatchEvent(new PointerEvent('pointerup', mkp(0))); \
  el.dispatchEvent(new MouseEvent('mouseup', mk(0))); \
  el.dispatchEvent(new MouseEvent('click', mk(0))); \
} catch (e) { el.click(); } \
return 'ok';";

/// Build the `act` JS for a single ref. Returns the literal `"ok"` on
/// success, `"err:not_found"` if the selector misses, or panics on
/// parser misuse (we trust the inputs).
fn build_act_js(r: Ref, action: &Action) -> String {
    let body = match action {
        // Emit a full, realistic mouse-click event sequence rather than
        // a bare `el.click()` (which fires *only* a synthetic `click`).
        // Libraries that gate behavior on pointer events — Radix UI's
        // Select/dismissable-layer most visibly — select on `pointerup`
        // and dismiss on `pointerdown`; with click-only, the value
        // updated but the popover never closed and its focus-trap
        // overlay then swallowed every later click, wedging the page.
        // This mirrors what a real cursor (and the macOS native path)
        // delivers. Falls back to `el.click()` if `PointerEvent` can't
        // be constructed. NB: macOS routes `Ref+click` through native
        // NSEvents and never reaches here; this is the Linux/Windows
        // (and any JS-dispatched) path.
        Action::Click => CLICK_SEQUENCE_JS.to_string(),
        Action::Fill { value } => format!(
            "el.focus(); \
             if (el instanceof HTMLSelectElement) {{ \
               var want = {v}; \
               var opt = Array.prototype.find.call(el.options, function(o){{ return o.value === want || o.text === want || o.label === want; }}) \
                      || Array.prototype.find.call(el.options, function(o){{ return (o.text || '').trim() === (want || '').trim(); }}); \
               if (!opt) return 'err:no_matching_option'; \
               var ss = Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype, 'value').set; \
               ss.call(el, opt.value); \
               el.dispatchEvent(new Event('input', {{bubbles: true}})); \
               el.dispatchEvent(new Event('change', {{bubbles: true}})); \
               return 'ok'; \
             }} \
             var p = (el instanceof HTMLTextAreaElement) ? HTMLTextAreaElement.prototype : (el instanceof HTMLInputElement ? HTMLInputElement.prototype : null); if (p) {{ Object.getOwnPropertyDescriptor(p, 'value').set.call(el, {v}); }} else {{ el.value = {v}; }} el.dispatchEvent(new Event('input', {{bubbles: true}})); el.dispatchEvent(new Event('change', {{bubbles: true}})); return 'ok';",
            v = json_string(value)
        ),
        Action::Scroll => {
            "el.scrollIntoView({behavior: 'instant', block: 'center'}); return 'ok';".into()
        }
        Action::Key { chord } => format!(
            "el.focus(); el.dispatchEvent(new KeyboardEvent('keydown', {{key: {c}, bubbles: true}})); el.dispatchEvent(new KeyboardEvent('keyup', {{key: {c}, bubbles: true}})); return 'ok';",
            c = json_string(chord)
        ),
        Action::Submit => "if (el.form) { el.form.submit(); } else if (typeof el.click === 'function') { el.click(); } return 'ok';".into(),
        Action::Hover => "el.dispatchEvent(new MouseEvent('mouseenter', {bubbles: true})); el.dispatchEvent(new MouseEvent('mouseover', {bubbles: true})); return 'ok';".into(),
        Action::Focus => "el.focus(); return 'ok';".into(),
    };
    format!(
        "(function() {{ const el = (window.__vsFindRef ? window.__vsFindRef({r}) : null) || document.querySelector('[data-vs-ref=\"{r}\"]'); if (!el) return 'err:not_found'; {body} }})()",
        r = r.0
    )
}

/// Build the JS that fires the HTML5 drag-and-drop event chain for
/// `vs drag`. Browsers' HTML5 dnd pipeline (dragstart → dragenter →
/// dragover → drop → dragend on a `DataTransfer`) only fires on real
/// hardware input — synthetic OS-level mouse events don't trip the
/// browser's start-drag heuristic. To cover the
/// `react-dnd` HTML5 backend, native `draggable="true"` widgets, and
/// React-Flow nodes wired to HTML5 dnd, every backend's `CursorOp::Drag`
/// runs the OS-level mouse path (for canvas / mouse-tracking widgets)
/// AND evaluates this JS afterwards. The JS dispatches untrusted but
/// otherwise well-formed `DragEvent`s with a `DataTransfer` attached;
/// library code (react-dnd, react-flow, native handlers) responds to
/// them the same as a real drag because none of them gate on
/// `isTrusted`. A page with no drag handlers absorbs the events as
/// no-ops.
pub(crate) fn build_html5_drag_js(x1: f64, y1: f64, x2: f64, y2: f64) -> String {
    format!(
        r"(function() {{
            var x1 = {x1}, y1 = {y1}, x2 = {x2}, y2 = {y2};
            var src = document.elementFromPoint(x1, y1);
            var dst = document.elementFromPoint(x2, y2);
            if (!src) return 'err:no_source';
            if (!dst) return 'err:no_target';
            var dt;
            try {{ dt = new DataTransfer(); }} catch (e) {{ return 'err:no_datatransfer'; }}
            function fire(target, type, cx, cy) {{
                var ev;
                try {{
                    ev = new DragEvent(type, {{
                        bubbles: true, cancelable: true, composed: true,
                        dataTransfer: dt, clientX: cx, clientY: cy, view: window,
                    }});
                }} catch (_) {{
                    // Fallback for engines where DragEvent's `dataTransfer`
                    // option is unsupported: build a MouseEvent and pin
                    // `dataTransfer` afterwards. Most modern WebKit /
                    // WebView2 builds support the constructor option, so
                    // this branch rarely runs.
                    ev = new MouseEvent(type, {{
                        bubbles: true, cancelable: true, composed: true,
                        clientX: cx, clientY: cy, view: window,
                    }});
                    try {{ Object.defineProperty(ev, 'dataTransfer', {{ value: dt }}); }} catch (_) {{}}
                }}
                target.dispatchEvent(ev);
                return ev;
            }}
            var startEv = fire(src, 'dragstart', x1, y1);
            // If the dragstart handler preventDefault'd, the page is
            // refusing to start a drag — report success but skip the
            // rest of the chain so we don't fabricate a drop the page
            // explicitly opted out of.
            if (startEv.defaultPrevented) return 'ok:cancelled';
            fire(dst, 'dragenter', x2, y2);
            fire(dst, 'dragover', x2, y2);
            // HTML5 spec: `drop` only fires if a handler preventDefault'd
            // the preceding `dragover`. We dispatch it unconditionally —
            // most react-dnd / react-flow targets call preventDefault on
            // dragover; for the rest the drop is a no-op event.
            fire(dst, 'drop', x2, y2);
            fire(src, 'dragend', x2, y2);
            return 'ok';
        }})()"
    )
}

pub(crate) fn run_act<F>(eval: F, target: &ActTarget, action: &Action) -> EngineResult<()>
where
    F: Fn(&str, Duration) -> EngineResult<String>,
{
    let r = match target {
        ActTarget::Ref(r) => r,
        ActTarget::Mark(_) => {
            return Err(EngineError::NotImplemented {
                engine: "shared",
                primitive: "act:mark-target",
            });
        }
    };
    let js = build_act_js(*r, action);
    let result = eval(&js, Duration::from_secs(5))?;
    let unwrapped = serde_json::from_str::<String>(&result).unwrap_or(result);
    if unwrapped == "ok" {
        Ok(())
    } else if let Some(rest) = unwrapped.strip_prefix("err:") {
        Err(EngineError::NotFound {
            kind: "ref",
            id: rest.to_string(),
        })
    } else {
        Err(EngineError::Other(format!(
            "unexpected act result: {unwrapped}"
        )))
    }
}

/// Build the JS predicate for a wait condition. Always returns a
/// well-formed expression — every condition has a real implementation
/// after M6.
fn build_wait_predicate(cond: &WaitCondition) -> String {
    match cond {
        WaitCondition::Stable => {
            "(function() { return document.readyState === 'complete' ? '1' : '0'; })()".into()
        }
        WaitCondition::Text(t) => format!(
            "(function() {{ return document.body && document.body.innerText && document.body.innerText.indexOf({q}) >= 0 ? '1' : '0'; }})()",
            q = json_string(t)
        ),
        WaitCondition::RefAppears(r) => format!(
            "(function() {{ const el = (window.__vsFindRef ? window.__vsFindRef({r}) : null) || document.querySelector('[data-vs-ref=\"{r}\"]'); return el ? '1' : '0'; }})()",
            r = r.0
        ),
        WaitCondition::RefGone(r) => format!(
            "(function() {{ const el = (window.__vsFindRef ? window.__vsFindRef({r}) : null) || document.querySelector('[data-vs-ref=\"{r}\"]'); return el ? '0' : '1'; }})()",
            r = r.0
        ),
        WaitCondition::NetIdle => {
            // Net-idle = no resource activity in the last 500ms. The
            // self-installing watcher tracks both PerformanceObserver
            // resource ticks and a fetch wrap. First call is the
            // installer; that initializes lastActivity to "now" so the
            // first poll always returns "0" — the predicate must wait
            // for a real quiet window before reporting idle.
            r"(function() {
              if (!window.__vsNetWatch) {
                window.__vsNetWatch = { lastActivity: performance.now() };
                if (window.PerformanceObserver) {
                  try {
                    var obs = new PerformanceObserver(function(list) {
                      window.__vsNetWatch.lastActivity = performance.now();
                    });
                    obs.observe({ type: 'resource', buffered: true });
                  } catch (e) {}
                }
                var origFetch = window.fetch;
                if (origFetch) {
                  window.fetch = function() {
                    window.__vsNetWatch.lastActivity = performance.now();
                    return origFetch.apply(this, arguments);
                  };
                }
                var XHR = window.XMLHttpRequest;
                if (XHR && XHR.prototype) {
                  var origSend = XHR.prototype.send;
                  XHR.prototype.send = function() {
                    window.__vsNetWatch.lastActivity = performance.now();
                    return origSend.apply(this, arguments);
                  };
                }
              }
              return (performance.now() - window.__vsNetWatch.lastActivity) > 500 ? '1' : '0';
            })()"
                .into()
        }
        WaitCondition::TokenChange => {
            // A new state-token is produced when the DOM observably
            // changes. Approximate via a MutationObserver installed on
            // first call; report '1' once any subtree mutation has
            // fired since installation.
            r"(function() {
              if (!window.__vsTokWatch) {
                window.__vsTokWatch = { changed: false };
                try {
                  var obs = new MutationObserver(function() {
                    window.__vsTokWatch.changed = true;
                  });
                  obs.observe(document.documentElement, {
                    subtree: true,
                    childList: true,
                    attributes: true,
                    characterData: true,
                  });
                } catch (e) {}
              }
              return window.__vsTokWatch.changed ? '1' : '0';
            })()"
                .into()
        }
    }
}

/// Ceiling for a single wait-predicate eval. Generous on purpose: it
/// bounds a wedged eval without turning a merely-slow one into a
/// failed wait. The pacing between polls comes from `tick`, and the
/// wait's own `budget` is what actually ends the loop.
const POLL_EVAL_BUDGET: Duration = Duration::from_secs(5);

/// Run a `wait` primitive: poll a JS predicate until it returns `"1"`
/// or `budget` elapses. The `tick` closure is called between polls so
/// the caller can pump its platform run loop.
pub(crate) fn run_wait<F, T>(
    eval: F,
    cond: &WaitCondition,
    budget: Duration,
    mut tick: T,
) -> EngineResult<()>
where
    F: Fn(&str, Duration) -> EngineResult<String>,
    T: FnMut(),
{
    let predicate = build_wait_predicate(cond);
    let deadline = std::time::Instant::now() + budget;
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err(EngineError::Timeout {
                budget,
                primitive: "wait",
            });
        }
        // Budget for *this* poll's eval, distinct from how often we
        // poll. These used to be the same 150ms value, which made a
        // single slow eval fatal: `eval` returned
        // `Timeout { primitive: "eval" }`, `?` propagated it, and the
        // caller saw `! TIMEOUT 150ms eval` — a failed wait on a page
        // that was merely busy. On a loaded CI runner that was the
        // whole of `cell_wait_gone`'s flakiness.
        let one = POLL_EVAL_BUDGET.min(remaining);
        match eval(&predicate, one) {
            Ok(result) => {
                let unwrapped = serde_json::from_str::<String>(&result).unwrap_or(result);
                if unwrapped == "1" {
                    return Ok(());
                }
            }
            // A poll that timed out is "not satisfied yet", not a
            // failure. Keep polling; the wait's own deadline above is
            // what bounds this loop.
            Err(EngineError::Timeout { .. }) => {}
            Err(e) => return Err(e),
        }
        tick();
    }
}

/// Run a `layout` primitive: query `getBoundingClientRect` for each
/// ref, parse the JSON result.
pub(crate) fn run_layout<F>(eval: F, refs: &[Ref]) -> EngineResult<Vec<LayoutBox>>
where
    F: Fn(&str, Duration) -> EngineResult<String>,
{
    let refs_json = serde_json::to_string(&refs.iter().map(|r| r.0).collect::<Vec<_>>())
        .unwrap_or_else(|_| "[]".into());
    let js = format!(
        r#"(function() {{
            const refs = {refs_json};
            const out = refs.map(r => {{
                const el = (window.__vsFindRef ? window.__vsFindRef(r) : null) || document.querySelector(`[data-vs-ref="${{r}}"]`);
                if (!el) return {{r, found: false}};
                const rect = el.getBoundingClientRect();
                const cs = getComputedStyle(el);
                const z = parseInt(cs.zIndex, 10);
                return {{
                    r, found: true,
                    x: rect.x, y: rect.y, w: rect.width, h: rect.height,
                    visible: rect.width > 0 && rect.height > 0 && cs.visibility !== 'hidden' && cs.display !== 'none',
                    z: Number.isFinite(z) ? z : 0,
                }};
            }});
            return JSON.stringify(out);
        }})()"#
    );
    let json = eval(&js, Duration::from_secs(5))?;
    let unwrapped = serde_json::from_str::<String>(&json).unwrap_or(json);
    let v: serde_json::Value =
        serde_json::from_str(&unwrapped).map_err(|e| EngineError::Other(e.to_string()))?;
    let arr = v
        .as_array()
        .ok_or_else(|| EngineError::Other("expected array".into()))?;
    let mut out = Vec::with_capacity(arr.len());
    for entry in arr {
        let r_v = entry
            .get("r")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let r = Ref(u32::try_from(r_v).unwrap_or(0));
        let found = entry
            .get("found")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        if !found {
            continue;
        }
        out.push(LayoutBox {
            r,
            x: entry
                .get("x")
                .and_then(serde_json::Value::as_f64)
                .unwrap_or(0.0),
            y: entry
                .get("y")
                .and_then(serde_json::Value::as_f64)
                .unwrap_or(0.0),
            width: entry
                .get("w")
                .and_then(serde_json::Value::as_f64)
                .unwrap_or(0.0),
            height: entry
                .get("h")
                .and_then(serde_json::Value::as_f64)
                .unwrap_or(0.0),
            visible: entry
                .get("visible")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            z_index: entry
                .get("z")
                .and_then(serde_json::Value::as_i64)
                .and_then(|n| i32::try_from(n).ok())
                .unwrap_or(0),
        });
    }
    Ok(out)
}

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
) -> EngineResult<Download>
where
    F: Fn(&str, Duration) -> EngineResult<String>,
    T: FnMut(),
{
    use base64::Engine as _;

    ensure_download_shim(&eval)?;
    let deadline = std::time::Instant::now() + budget;

    // `0` addresses "whatever is newest" on the JS side.
    let id = match source {
        DownloadSource::Url(url) => {
            let js = format!("window.__vsDl.fetch({}, null)", json_string(url));
            let raw = eval_str(eval(&js, Duration::from_secs(10))?);
            raw.trim().parse::<u64>().map_err(|_| {
                EngineError::Other(format!("download: shim returned no id ({raw:?})"))
            })?
        }
        DownloadSource::Captured { id } => id.unwrap_or(0),
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
pub(crate) fn run_download_list<F>(eval: F) -> EngineResult<Vec<DownloadEntry>>
where
    F: Fn(&str, Duration) -> EngineResult<String>,
{
    ensure_download_shim(&eval)?;
    let raw = eval_str(eval("window.__vsDl.index()", Duration::from_secs(5))?);
    let v: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|e| EngineError::Other(format!("download list: bad json {raw:?}: {e}")))?;
    Ok(v.as_array()
        .map(|a| a.iter().map(parse_download_entry).collect())
        .unwrap_or_default())
}

/// Run `save_auth`: eval the snapshot JS, capture the JSON blob.
#[allow(dead_code)]
pub(crate) fn run_save_auth<F>(eval: F) -> EngineResult<AuthBlob>
where
    F: Fn(&str, Duration) -> EngineResult<String>,
{
    let json = eval(AUTH_SAVE_JS, Duration::from_secs(5))?;
    let payload = serde_json::from_str::<String>(&json).unwrap_or(json);
    Ok(AuthBlob {
        bytes: payload.into_bytes(),
    })
}

/// Run `load_auth`: rebuild the IIFE with the payload baked in, eval,
/// expect the literal `"ok"`.
#[allow(dead_code)]
pub(crate) fn run_load_auth<F>(eval: F, blob: &AuthBlob) -> EngineResult<()>
where
    F: Fn(&str, Duration) -> EngineResult<String>,
{
    let payload = std::str::from_utf8(&blob.bytes)
        .map_err(|e| EngineError::Other(format!("auth blob not utf8: {e}")))?;
    let payload_lit = serde_json::to_string(payload)
        .map_err(|e| EngineError::Other(format!("auth blob json-encode: {e}")))?;
    let body = AUTH_LOAD_BODY_JS;
    let js = format!("(function() {{ const blob = JSON.parse({payload_lit}); {body} }})()");
    let result = eval(&js, Duration::from_secs(5))?;
    let unwrapped = serde_json::from_str::<String>(&result).unwrap_or(result);
    if unwrapped == "ok" {
        Ok(())
    } else {
        Err(EngineError::Other(format!(
            "load_auth: unexpected: {unwrapped}"
        )))
    }
}

/// Run `eval_js`: wrap the user source in a try/catch that returns a
/// tagged JSON record, then map back to [`EvalResult`].
///
/// Two wrappers are tried in order:
/// 1. **expression mode** — `(function(){ return <expr>; })()`. The
///    common case; works under strict CSP (no `eval`) and returns the
///    value of a single expression.
/// 2. **program mode** — `(0, eval)(<source as string literal>)`,
///    reached only when expression mode fails to even parse (a
///    multiline statement block like `const a=1; f(); a`). Indirect
///    `eval` evaluates the source as a *program* and yields the
///    completion value of its last statement (REPL semantics), and any
///    SyntaxError/throw is caught inside the wrapper so the caller gets
///    a clean `Syntax`/`Thrown` result instead of an opaque engine
///    error. Passing the source as a JSON-encoded string literal keeps
///    its newlines from breaking the wrapper's own parse.
pub(crate) fn run_eval<F>(eval: F, expr: &str) -> EngineResult<crate::inspector::EvalResult>
where
    F: Fn(&str, Duration) -> EngineResult<String>,
{
    let expr_wrapper = format!(
        r"(function() {{
            try {{
                var __v = (function() {{ return {expr}; }})();
                return JSON.stringify({{
                    kind: 'ok',
                    type: typeof __v,
                    value: (typeof __v === 'string') ? __v : JSON.stringify(__v),
                }});
            }} catch (e) {{
                var msg = (e && e.message) || String(e);
                var name = (e && e.name) || 'Error';
                if (name === 'SyntaxError') {{
                    return JSON.stringify({{ kind: 'syntax', message: msg }});
                }}
                return JSON.stringify({{ kind: 'thrown', name: name, message: msg }});
            }}
        }})()"
    );
    // Expression mode. When `expr` is a statement block the wrapper
    // itself fails to compile, and engines signal that differently:
    // WebKit / WebKitGTK return an `Err`, while WebView2's
    // `ExecuteScript` returns the JSON string `"null"` (it reports an
    // uncompilable or throwing script as JSON null rather than an
    // error). Treat *both* — an `Err`, or an `Ok` whose payload doesn't
    // parse as our tagged record — as "expression mode didn't apply"
    // and fall through to program mode.
    if let Ok(json) = eval(&expr_wrapper, Duration::from_secs(5)) {
        if let Ok(result) = parse_eval_json(&json) {
            return Ok(result);
        }
    }

    // Program mode via indirect `eval`: evaluates the source as a
    // program (REPL completion-value semantics) and catches any
    // SyntaxError / throw inside the wrapper, so a statement block
    // resolves and malformed input comes back as a clean
    // Syntax/Thrown record instead of an opaque engine error.
    let src_literal = serde_json::to_string(expr).unwrap_or_else(|_| "\"\"".to_string());
    let program_wrapper = format!(
        r"(function() {{
            try {{
                var __v = (0, eval)({src_literal});
                return JSON.stringify({{
                    kind: 'ok',
                    type: typeof __v,
                    value: (typeof __v === 'string') ? __v : JSON.stringify(__v),
                }});
            }} catch (e) {{
                var msg = (e && e.message) || String(e);
                var name = (e && e.name) || 'Error';
                if (name === 'SyntaxError') {{
                    return JSON.stringify({{ kind: 'syntax', message: msg }});
                }}
                return JSON.stringify({{ kind: 'thrown', name: name, message: msg }});
            }}
        }})()"
    );
    let json = eval(&program_wrapper, Duration::from_secs(5))?;
    parse_eval_json(&json)
}

/// Decode the tagged JSON record emitted by the `run_eval` wrappers.
/// Returns `Err` if `json` isn't one of those records — e.g. the bare
/// `"null"` WebView2 hands back when a wrapper fails to compile — which
/// the caller uses as the signal to fall through to program mode.
fn parse_eval_json(json: &str) -> EngineResult<crate::inspector::EvalResult> {
    use crate::inspector::EvalResult;
    let unwrapped = serde_json::from_str::<String>(json).unwrap_or_else(|_| json.to_string());
    let v: serde_json::Value = serde_json::from_str(&unwrapped)
        .map_err(|e| EngineError::Other(format!("eval: invalid wrapper json: {e}")))?;
    let kind = v.get("kind").and_then(|x| x.as_str()).unwrap_or("");
    match kind {
        "ok" => {
            let js_type = v
                .get("type")
                .and_then(|x| x.as_str())
                .unwrap_or("undefined")
                .to_string();
            let value = v
                .get("value")
                .and_then(|x| x.as_str())
                .unwrap_or("undefined")
                .to_string();
            Ok(EvalResult::Ok { value, js_type })
        }
        "thrown" => {
            let kind = v
                .get("name")
                .and_then(|x| x.as_str())
                .unwrap_or("Error")
                .to_string();
            let message = v
                .get("message")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            Ok(EvalResult::Thrown { kind, message })
        }
        "syntax" => {
            let message = v
                .get("message")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            Ok(EvalResult::Syntax { message })
        }
        other => Err(EngineError::Other(format!("eval: unexpected kind {other}"))),
    }
}

/// Map a host-side [`auth::CookieData`] to an [`inspector::StorageEntry`].
/// Used by each backend's `storage(Cookies)` path so `vs inspect storage
/// cookies` surfaces HttpOnly entries that `document.cookie` is blind to.
pub(crate) fn cookie_to_storage_entry(
    c: &crate::backend::auth::CookieData,
) -> crate::inspector::StorageEntry {
    let lower = c.name.to_ascii_lowercase();
    let sensitive = ["session_id", "auth", "token", "secret", "password", "csrf"]
        .iter()
        .any(|needle| lower.contains(needle));
    let mut flags = Vec::new();
    if c.secure {
        flags.push("secure".to_string());
    }
    if c.http_only {
        flags.push("httponly".to_string());
    }
    if let Some(ss) = &c.same_site {
        flags.push(format!("samesite={}", ss.to_ascii_lowercase()));
    }
    if let Some(unix) = c.expires_unix {
        flags.push(format!("expires={unix}"));
    }
    crate::inspector::StorageEntry {
        key: c.name.clone(),
        value: c.value.clone(),
        flags,
        sensitive,
    }
}

/// Diff the current cookie snapshot against `previous` and produce
/// `CookieEvent`s for entries that were added or removed. Identity is
/// `(name, domain, path)` — the same triple WebKit / WebKitGTK /
/// WebView2 use to dedupe cookies in their stores. The `next_seq`
/// counter is incremented for each event so per-page sequences stay
/// monotonic across calls.
pub(crate) fn diff_cookies(
    previous: Option<&[crate::backend::auth::CookieData]>,
    current: &[crate::backend::auth::CookieData],
    next_seq: &mut u64,
) -> Vec<crate::inspector::CookieEvent> {
    use crate::inspector::{CookieAction, CookieEvent};
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(0));
    let mut out = Vec::new();
    let key =
        |c: &crate::backend::auth::CookieData| (c.name.clone(), c.domain.clone(), c.path.clone());
    let cur_keys: std::collections::HashSet<_> = current.iter().map(key).collect();
    let prev_keys: std::collections::HashSet<_> = previous
        .map(|p| p.iter().map(key).collect())
        .unwrap_or_default();
    let entry_flags = |c: &crate::backend::auth::CookieData| {
        let mut flags = Vec::new();
        if c.secure {
            flags.push("secure".to_string());
        }
        if c.http_only {
            flags.push("httponly".to_string());
        }
        if let Some(ss) = &c.same_site {
            flags.push(format!("samesite={}", ss.to_ascii_lowercase()));
        }
        if let Some(unix) = c.expires_unix {
            flags.push(format!("expires={unix}"));
        }
        flags
    };
    for c in current {
        if !prev_keys.contains(&key(c)) {
            *next_seq += 1;
            out.push(CookieEvent {
                seq: *next_seq,
                ts_ms: now_ms,
                action: CookieAction::Added,
                name: c.name.clone(),
                domain: c.domain.clone(),
                path: c.path.clone(),
                flags: entry_flags(c),
            });
        }
    }
    if let Some(prev) = previous {
        for c in prev {
            if !cur_keys.contains(&key(c)) {
                *next_seq += 1;
                out.push(CookieEvent {
                    seq: *next_seq,
                    ts_ms: now_ms,
                    action: CookieAction::Removed,
                    name: c.name.clone(),
                    domain: c.domain.clone(),
                    path: c.path.clone(),
                    flags: entry_flags(c),
                });
            }
        }
    }
    out
}

/// Run `storage`: enumerate cookies / localStorage / sessionStorage /
/// indexeddb databases for the page. The response is a `Vec<(key,
/// value, sensitive)>` where `sensitive` flags credential-shaped keys.
pub(crate) fn run_storage<F>(
    eval: F,
    scope: crate::inspector::StorageScope,
) -> EngineResult<Vec<crate::inspector::StorageEntry>>
where
    F: Fn(&str, Duration) -> EngineResult<String>,
{
    use crate::inspector::StorageScope;
    let scope_name = match scope {
        StorageScope::Cookies => "cookies",
        StorageScope::Local => "local",
        StorageScope::Session => "session",
        StorageScope::IndexedDb => "indexeddb",
    };
    let js = format!(
        r"(function() {{
            var scope = {scope_name:?};
            function isSensitive(k) {{
                var lower = String(k).toLowerCase();
                return lower.includes('session_id')
                    || lower.includes('auth')
                    || lower.includes('token')
                    || lower.includes('secret')
                    || lower.includes('password')
                    || lower.includes('csrf');
            }}
            var entries = [];
            if (scope === 'cookies') {{
                var s = document.cookie || '';
                var parts = s.split(';');
                for (var i = 0; i < parts.length; i++) {{
                    var p = parts[i].trim();
                    if (!p) continue;
                    var idx = p.indexOf('=');
                    var k = idx >= 0 ? p.slice(0, idx) : p;
                    var v = idx >= 0 ? p.slice(idx + 1) : '';
                    entries.push({{ key: k, value: v, sensitive: isSensitive(k) }});
                }}
            }} else if (scope === 'local') {{
                for (var i = 0; i < localStorage.length; i++) {{
                    var k = localStorage.key(i);
                    var v = localStorage.getItem(k) || '';
                    entries.push({{ key: k, value: v, sensitive: isSensitive(k) }});
                }}
            }} else if (scope === 'session') {{
                for (var i = 0; i < sessionStorage.length; i++) {{
                    var k = sessionStorage.key(i);
                    var v = sessionStorage.getItem(k) || '';
                    entries.push({{ key: k, value: v, sensitive: isSensitive(k) }});
                }}
            }} else if (scope === 'indexeddb') {{
                // indexedDB.databases() is async (returns a Promise)
                // and WKWebView's evaluateJavaScript can't await it.
                // Each call returns the last snapshot and re-arms a
                // refresh, so call → settle → call converges on fresh
                // data. (A one-shot watcher used to cache the first
                // resolution forever — an empty list if queried before
                // the page created its db.)
                if (!window.__vsIdbList) {{
                    window.__vsIdbList = [];
                }}
                if (indexedDB && indexedDB.databases) {{
                    try {{
                        indexedDB.databases().then(function(dbs) {{
                            window.__vsIdbList = dbs.map(function(d) {{
                                return {{ name: d.name || '', version: d.version || 0 }};
                            }});
                        }});
                    }} catch (e) {{}}
                }}
                for (var i = 0; i < window.__vsIdbList.length; i++) {{
                    var d = window.__vsIdbList[i];
                    entries.push({{
                        key: d.name,
                        value: String(d.version || ''),
                        sensitive: false,
                    }});
                }}
            }}
            return JSON.stringify(entries);
        }})()"
    );
    let json = eval(&js, Duration::from_secs(5))?;
    Ok(parse_storage_entries(&json))
}

/// Decode the JSON-encoded array produced by the storage probe.
/// Lenient — bad input drops to an empty list rather than erroring,
/// matching the wider "engine should never crash on bad page JS"
/// principle.
fn parse_storage_entries(json: &str) -> Vec<crate::inspector::StorageEntry> {
    let unwrapped = serde_json::from_str::<String>(json).unwrap_or_else(|_| json.to_string());
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&unwrapped) else {
        return Vec::new();
    };
    let Some(arr) = v.as_array() else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(arr.len());
    for e in arr {
        let key = e
            .get("key")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let value = e
            .get("value")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let sensitive = e
            .get("sensitive")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        out.push(crate::inspector::StorageEntry {
            key,
            value,
            flags: Vec::new(),
            sensitive,
        });
    }
    out
}

/// Run `scripts`: enumerate `<script>` elements with src + state.
pub(crate) fn run_scripts<F>(eval: F) -> EngineResult<Vec<crate::inspector::ScriptEntry>>
where
    F: Fn(&str, Duration) -> EngineResult<String>,
{
    use crate::inspector::{ScriptEntry, ScriptState};
    let js = r"(function() {
        var out = [];
        var els = document.scripts;
        for (var i = 0; i < els.length; i++) {
            var s = els[i];
            out.push({
                seq: i + 1,
                source: s.src ? s.src : ('inline:doc[' + i + ']'),
                size: (s.src ? 0 : (s.text ? s.text.length : 0)),
            });
        }
        return JSON.stringify(out);
    })()";
    let json = eval(js, Duration::from_secs(5))?;
    let unwrapped = serde_json::from_str::<String>(&json).unwrap_or(json);
    let v: serde_json::Value = match serde_json::from_str(&unwrapped) {
        Ok(v) => v,
        Err(_) => return Ok(Vec::new()),
    };
    let Some(arr) = v.as_array() else {
        return Ok(Vec::new());
    };
    let mut out = Vec::with_capacity(arr.len());
    for e in arr {
        let seq = e
            .get("seq")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let source = e
            .get("source")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let size = e
            .get("size")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        out.push(ScriptEntry {
            seq,
            source,
            size,
            state: ScriptState::Parsed,
        });
    }
    Ok(out)
}

/// Run `script_source`: return source text for one script seq (1-based).
pub(crate) fn run_script_source<F>(
    eval: F,
    seq: u64,
) -> EngineResult<Option<crate::inspector::ScriptSource>>
where
    F: Fn(&str, Duration) -> EngineResult<String>,
{
    use crate::inspector::ScriptSource;
    let js = format!(
        r"(function() {{
            var i = {seq} - 1;
            var s = document.scripts[i];
            if (!s) return JSON.stringify(null);
            return JSON.stringify({{
                source_url: s.src || ('inline:doc[' + i + ']'),
                body: s.text || '',
            }});
        }})()"
    );
    let json = eval(&js, Duration::from_secs(5))?;
    let unwrapped = serde_json::from_str::<String>(&json).unwrap_or(json);
    let v: serde_json::Value = match serde_json::from_str(&unwrapped) {
        Ok(v) => v,
        Err(_) => return Ok(None),
    };
    if v.is_null() {
        return Ok(None);
    }
    let source_url = v
        .get("source_url")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let body = v
        .get("body")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    Ok(Some(ScriptSource {
        seq,
        source_url,
        body,
    }))
}

/// Run `dom`: outerHTML + computed style for one ref.
pub(crate) fn run_dom<F>(
    eval: F,
    r: Ref,
    extra_props: &[String],
) -> EngineResult<Option<crate::inspector::DomDetail>>
where
    F: Fn(&str, Duration) -> EngineResult<String>,
{
    use crate::inspector::DomDetail;
    let extras_json = serde_json::to_string(extra_props).unwrap_or_else(|_| "[]".into());
    let js = format!(
        r#"(function() {{
            var el = (window.__vsFindRef ? window.__vsFindRef({r}) : null) || document.querySelector(`[data-vs-ref="${{r}}"]`);
            if (!el) return JSON.stringify(null);
            var cs = getComputedStyle(el);
            var defaultProps = ['display','visibility','position','color','background-color','font-size','z-index'];
            var extras = {extras_json};
            var seen = {{}};
            var pairs = [];
            for (var i = 0; i < defaultProps.length; i++) {{
                var k = defaultProps[i];
                if (seen[k]) continue;
                seen[k] = true;
                pairs.push([k, cs.getPropertyValue(k)]);
            }}
            for (var j = 0; j < extras.length; j++) {{
                var k = extras[j];
                if (seen[k]) continue;
                seen[k] = true;
                pairs.push([k, cs.getPropertyValue(k)]);
            }}
            return JSON.stringify({{
                outer_html: el.outerHTML,
                computed: pairs,
            }});
        }})()"#,
        r = r.0
    );
    let json = eval(&js, Duration::from_secs(5))?;
    let unwrapped = serde_json::from_str::<String>(&json).unwrap_or(json);
    let v: serde_json::Value = match serde_json::from_str(&unwrapped) {
        Ok(v) => v,
        Err(_) => return Ok(None),
    };
    if v.is_null() {
        return Ok(None);
    }
    let outer = v
        .get("outer_html")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let mut computed: Vec<(String, String)> = Vec::new();
    if let Some(arr) = v.get("computed").and_then(|x| x.as_array()) {
        for pair in arr {
            if let Some(p) = pair.as_array() {
                let k = p.first().and_then(|x| x.as_str()).unwrap_or("");
                let val = p.get(1).and_then(|x| x.as_str()).unwrap_or("");
                computed.push((k.to_string(), val.to_string()));
            }
        }
    }
    Ok(Some(DomDetail {
        r: r.0,
        outer_html: outer,
        computed,
    }))
}

/// Run `performance`: collect PerformanceObserver-derived Web Vitals.
pub(crate) fn run_performance<F>(eval: F) -> EngineResult<crate::inspector::PerformanceMetrics>
where
    F: Fn(&str, Duration) -> EngineResult<String>,
{
    use crate::inspector::PerformanceMetrics;
    let js = r"(function() {
        var nav = performance.getEntriesByType('navigation')[0] || {};
        var paint = performance.getEntriesByType('paint') || [];
        var fcp = 0;
        for (var i = 0; i < paint.length; i++) {
            if (paint[i].name === 'first-contentful-paint') fcp = paint[i].startTime;
        }
        var lcp = (window.__vsLcp || 0);
        var cls = (window.__vsCls || 0);
        var heap = 0;
        if (performance.memory && performance.memory.usedJSHeapSize) {
            heap = performance.memory.usedJSHeapSize / (1024 * 1024);
        }
        return JSON.stringify({
            ttfb: nav.responseStart || 0,
            fcp: fcp,
            lcp: lcp,
            cls: cls,
            fid: 0,
            long_tasks: 0,
            total_blocking: 0,
            heap_mb: heap,
            dom_nodes: document.getElementsByTagName('*').length,
        });
    })()";
    let json = eval(js, Duration::from_secs(5))?;
    let unwrapped = serde_json::from_str::<String>(&json).unwrap_or(json);
    let v: serde_json::Value = serde_json::from_str(&unwrapped)
        .map_err(|e| EngineError::Other(format!("performance: invalid wrapper json: {e}")))?;
    let f = |k: &str| v.get(k).and_then(serde_json::Value::as_f64).unwrap_or(0.0);
    let u = |k: &str| {
        v.get(k)
            .and_then(serde_json::Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(0)
    };
    Ok(PerformanceMetrics {
        ttfb_ms: f("ttfb"),
        fcp_ms: f("fcp"),
        lcp_ms: f("lcp"),
        cls: f("cls"),
        fid_ms: f("fid"),
        long_tasks: u("long_tasks"),
        total_blocking_ms: f("total_blocking"),
        js_heap_mb: f("heap_mb"),
        dom_nodes: u("dom_nodes"),
    })
}

/// Captured storage state from `STORAGE_SAVE_JS`. Cookies live in a
/// separate host-side payload because `document.cookie` can't see
/// `HttpOnly`.
pub(crate) struct StorageSnapshot {
    pub url: String,
    pub origin: String,
    pub local_storage: std::collections::BTreeMap<String, String>,
    pub session_storage: std::collections::BTreeMap<String, String>,
}

pub(crate) fn run_save_storage_only<F>(eval: F) -> EngineResult<StorageSnapshot>
where
    F: Fn(&str, Duration) -> EngineResult<String>,
{
    let json = eval(STORAGE_SAVE_JS, Duration::from_secs(5))?;
    let unwrapped = serde_json::from_str::<String>(&json).unwrap_or(json);
    let v: serde_json::Value = serde_json::from_str(&unwrapped)
        .map_err(|e| EngineError::Other(format!("save_storage parse: {e}")))?;
    let url = v
        .get("url")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let origin = v
        .get("origin")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let local_storage = parse_storage_map(v.get("localStorage"));
    let session_storage = parse_storage_map(v.get("sessionStorage"));
    Ok(StorageSnapshot {
        url,
        origin,
        local_storage,
        session_storage,
    })
}

fn parse_storage_map(v: Option<&serde_json::Value>) -> std::collections::BTreeMap<String, String> {
    let Some(obj) = v.and_then(serde_json::Value::as_object) else {
        return std::collections::BTreeMap::new();
    };
    obj.iter()
        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
        .collect()
}

pub(crate) fn run_load_storage_only<F>(
    eval: F,
    local: &std::collections::BTreeMap<String, String>,
    session: &std::collections::BTreeMap<String, String>,
) -> EngineResult<()>
where
    F: Fn(&str, Duration) -> EngineResult<String>,
{
    let payload = serde_json::json!({
        "localStorage": local,
        "sessionStorage": session,
    });
    let payload_lit = serde_json::to_string(&payload.to_string())
        .map_err(|e| EngineError::Other(format!("storage payload encode: {e}")))?;
    let body = STORAGE_LOAD_BODY_JS;
    let js = format!("(function() {{ const payload = JSON.parse({payload_lit}); {body} }})()");
    let result = eval(&js, Duration::from_secs(5))?;
    let unwrapped = serde_json::from_str::<String>(&result).unwrap_or(result);
    if unwrapped == "ok" {
        Ok(())
    } else {
        Err(EngineError::Other(format!(
            "load_storage: unexpected: {unwrapped}"
        )))
    }
}

#[cfg(test)]
mod eval_tests {
    use super::run_eval;
    use crate::engine::{EngineError, EngineResult};
    use crate::inspector::EvalResult;
    use std::time::Duration;

    // Program-mode wrappers contain `(0, eval)`; expression-mode ones do
    // not. Tests use that to fake each engine's behavior per mode.
    fn is_program_mode(js: &str) -> bool {
        js.contains("(0, eval)")
    }

    #[test]
    fn single_expression_uses_expression_mode() {
        // A plain expression resolves in expression mode — no fallback.
        let eval = |js: &str, _b: Duration| -> EngineResult<String> {
            assert!(!is_program_mode(js), "should not reach program mode");
            Ok(r#"{"kind":"ok","type":"number","value":"2"}"#.to_string())
        };
        match run_eval(eval, "1 + 1").unwrap() {
            EvalResult::Ok { value, .. } => assert_eq!(value, "2"),
            other => panic!("expected Ok(2), got {other:?}"),
        }
    }

    #[test]
    fn webkit_err_triggers_program_fallback() {
        // WKWebView / WebKitGTK: a statement block makes the
        // expression wrapper a syntax error -> the engine returns Err.
        let eval = |js: &str, _b: Duration| -> EngineResult<String> {
            if is_program_mode(js) {
                Ok(r#"{"kind":"ok","type":"number","value":"6"}"#.to_string())
            } else {
                Err(EngineError::Other("SyntaxError: ...".into()))
            }
        };
        match run_eval(eval, "const a=2;\nconst b=3;\na*b").unwrap() {
            EvalResult::Ok { value, .. } => assert_eq!(value, "6"),
            other => panic!("expected Ok(6), got {other:?}"),
        }
    }

    #[test]
    fn webview2_null_triggers_program_fallback() {
        // WebView2: ExecuteScript returns the bare string "null" for an
        // uncompilable wrapper instead of an Err. run_eval must still
        // fall through to program mode rather than surfacing an error.
        let eval = |js: &str, _b: Duration| -> EngineResult<String> {
            if is_program_mode(js) {
                Ok(r#"{"kind":"ok","type":"number","value":"6"}"#.to_string())
            } else {
                Ok("null".to_string())
            }
        };
        match run_eval(eval, "const a=2;\nconst b=3;\na*b").unwrap() {
            EvalResult::Ok { value, .. } => assert_eq!(value, "6"),
            other => panic!("expected Ok(6), got {other:?}"),
        }
    }
}
