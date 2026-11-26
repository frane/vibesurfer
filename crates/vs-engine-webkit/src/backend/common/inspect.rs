//! Inspector reads shared by every backend: eval, storage and cookie
//! diffs, scripts, DOM and performance.

use std::time::Duration;

use vs_protocol::Ref;

use crate::engine::{EngineError, EngineResult};

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
