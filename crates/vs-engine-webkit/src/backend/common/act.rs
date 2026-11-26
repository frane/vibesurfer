//! Actions, waits and layout shared by every backend. Each function
//! takes the backend's `eval` closure (`Fn(&str, Duration) ->
//! EngineResult<String>`) and builds the JS on top, so a backend's
//! `Engine` impl shrinks to a page lookup and one call.
//!
//! `eval` is `Fn` so callers can borrow per-page state across several
//! calls (`run_wait` polls a JS predicate until it holds or the budget
//! runs out).

use std::time::Duration;

use vs_protocol::Ref;

use crate::engine::{ActTarget, Action, EngineError, EngineResult, LayoutBox, WaitCondition};

use super::json_string;

/// Full mouse-click event sequence for `Action::Click`, dispatched on
/// the resolved element `el`. Coordinates are the element's center so
/// pointer-position-aware handlers behave. The `button`/`buttons`
/// fields follow real semantics (held during down, released by up).
/// Bind the DOM constructors an action script uses to the element's own
/// window. An element inside a same-origin frame belongs to that
/// frame's realm, where `el instanceof HTMLInputElement` against the
/// top window's constructor is false and events built from the top
/// window's constructors carry the wrong `view`.
const REALM: &str = "const W = (el.ownerDocument && el.ownerDocument.defaultView) || window; \
var HTMLInputElement = W.HTMLInputElement, HTMLTextAreaElement = W.HTMLTextAreaElement, \
HTMLSelectElement = W.HTMLSelectElement, Event = W.Event, KeyboardEvent = W.KeyboardEvent, \
MouseEvent = W.MouseEvent, PointerEvent = W.PointerEvent;";

const CLICK_SEQUENCE_JS: &str = "\
const rc = el.getBoundingClientRect(); \
const x = rc.left + rc.width / 2, y = rc.top + rc.height / 2; \
const mk = (b) => ({ bubbles: true, cancelable: true, composed: true, view: W, clientX: x, clientY: y, screenX: x, screenY: y, button: 0, buttons: b }); \
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
        "(function() {{ const el = (window.__vsFindRef ? window.__vsFindRef({r}) : null) || document.querySelector('[data-vs-ref=\"{r}\"]'); if (!el) return 'err:not_found'; {REALM} {body} }})()",
        r = r.0
    )
}

/// Build the JS that fires the HTML5 drag-and-drop event chain for
/// `vs drag`. Browsers' HTML5 dnd pipeline (dragstart → dragenter →
/// dragover → drop → dragend on a `DataTransfer`) needs a real drag
/// session. Synthetic OS-level mouse events can trip WebKit's
/// start-drag heuristic (macOS 27 does), but the headless web view
/// refuses the system drag session (see `webkit/headless_webview.rs`),
/// so no drop ever arrives natively. To cover the
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

/// Where a native click on ref `r` lands: the center of its box in
/// top-level viewport CSS px, after scrolling it into view. `None` when
/// the ref is gone or the box is empty (a hidden duplicate): the JS act
/// path dispatches on the element whatever its geometry, so callers
/// fall back to it. macOS has its own (`webkit::input::ref_rect`), which
/// its NSEvent click takes as a rect.
#[cfg(not(target_os = "macos"))]
pub(crate) fn ref_click_point<F>(eval: F, r: Ref) -> EngineResult<Option<(f64, f64)>>
where
    F: Fn(&str, Duration) -> EngineResult<String>,
{
    let js = format!(
        r#"(function() {{
            var el = (window.__vsFindRef ? window.__vsFindRef({r}) : null) || document.querySelector('[data-vs-ref="{r}"]');
            if (!el) return 'null';
            try {{ el.scrollIntoView({{behavior: 'instant', block: 'center', inline: 'center'}}); }}
            catch (e) {{ el.scrollIntoView(); }}
            var b = window.__vsTopRect ? window.__vsTopRect(el) : el.getBoundingClientRect();
            return JSON.stringify([b.x, b.y, b.width, b.height]);
        }})()"#,
        r = r.0,
    );
    let result = eval(&js, Duration::from_secs(5))?;
    let unwrapped = serde_json::from_str::<String>(&result).unwrap_or(result);
    let Ok([left, top, width, height]) = serde_json::from_str::<[f64; 4]>(&unwrapped) else {
        return Ok(None);
    };
    Ok((width > 0.5 && height > 0.5).then(|| (left + width / 2.0, top + height / 2.0)))
}

pub(crate) fn run_act<F>(eval: F, target: &ActTarget, action: &Action) -> EngineResult<()>
where
    F: Fn(&str, Duration) -> EngineResult<String>,
{
    let ActTarget::Ref(r) = target;
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
                detail: "",
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
                const rect = window.__vsTopRect ? window.__vsTopRect(el) : el.getBoundingClientRect();
                const cs = (el.ownerDocument.defaultView || window).getComputedStyle(el);
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
