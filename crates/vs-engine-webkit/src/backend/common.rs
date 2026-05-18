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
    let r = v.get("r")?.as_u64()?;
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
    Some(Node {
        r: Ref(u32::try_from(r).unwrap_or(0)),
        role,
        label: label.to_string(),
        ops: ops_for_role(role),
        attrs: BTreeMap::new(),
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
    ActTarget, Action, AuthBlob, EngineError, EngineResult, LayoutBox, WaitCondition,
};

fn json_string(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

/// Build the `act` JS for a single ref. Returns the literal `"ok"` on
/// success, `"err:not_found"` if the selector misses, or panics on
/// parser misuse (we trust the inputs).
fn build_act_js(r: Ref, action: &Action) -> String {
    let body = match action {
        Action::Click => "el.click(); return 'ok';".to_string(),
        Action::Fill { value } => format!(
            "el.focus(); var p = (el instanceof HTMLTextAreaElement) ? HTMLTextAreaElement.prototype : (el instanceof HTMLInputElement ? HTMLInputElement.prototype : null); if (p) {{ Object.getOwnPropertyDescriptor(p, 'value').set.call(el, {v}); }} else {{ el.value = {v}; }} el.dispatchEvent(new Event('input', {{bubbles: true}})); el.dispatchEvent(new Event('change', {{bubbles: true}})); return 'ok';",
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
        "(function() {{ const el = document.querySelector('[data-vs-ref=\"{r}\"]'); if (!el) return 'err:not_found'; {body} }})()",
        r = r.0
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
            "(function() {{ return document.querySelector('[data-vs-ref=\"{r}\"]') ? '1' : '0'; }})()",
            r = r.0
        ),
        WaitCondition::RefGone(r) => format!(
            "(function() {{ return document.querySelector('[data-vs-ref=\"{r}\"]') ? '0' : '1'; }})()",
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

/// Run a `wait` primitive: poll a JS predicate every 150ms until it
/// returns `"1"` or `budget` elapses. The `tick` closure is called
/// between polls so the caller can pump its platform run loop.
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
    let slice = Duration::from_millis(150);
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err(EngineError::Timeout {
                budget,
                primitive: "wait",
            });
        }
        let one = if remaining < slice { remaining } else { slice };
        let result = eval(&predicate, one)?;
        let unwrapped = serde_json::from_str::<String>(&result).unwrap_or(result);
        if unwrapped == "1" {
            return Ok(());
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
                const el = document.querySelector('[data-vs-ref="' + r + '"]');
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

/// Run `eval_js`: wrap the user expression in a try/catch that returns
/// a tagged JSON record, then map back to [`EvalResult`].
pub(crate) fn run_eval<F>(eval: F, expr: &str) -> EngineResult<crate::inspector::EvalResult>
where
    F: Fn(&str, Duration) -> EngineResult<String>,
{
    use crate::inspector::EvalResult;
    let wrapped = format!(
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
    match eval(&wrapped, Duration::from_secs(5)) {
        Ok(json) => {
            let unwrapped = serde_json::from_str::<String>(&json).unwrap_or(json);
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
        Err(EngineError::Other(msg)) if msg.contains("SyntaxError") => {
            Ok(EvalResult::Syntax { message: msg })
        }
        Err(e) => Err(e),
    }
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
                // indexedDB.databases() is async (returns a Promise) and
                // WKWebView's evaluateJavaScript can't await it. We
                // install a one-shot watcher on the first call which
                // populates `window.__vsIdbList`; subsequent calls
                // return the cached list. Tests that need the value
                // settle for ~200ms after page load before asking.
                if (!window.__vsIdbList) {{
                    window.__vsIdbList = [];
                    if (indexedDB && indexedDB.databases) {{
                        try {{
                            indexedDB.databases().then(function(dbs) {{
                                window.__vsIdbList = dbs.map(function(d) {{
                                    return {{ name: d.name || '', version: d.version || 0 }};
                                }});
                            }});
                        }} catch (e) {{}}
                    }}
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
            var el = document.querySelector('[data-vs-ref="{r}"]');
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
