//! Backend-shared helpers — no platform deps.
//!
//! All three real backends (`webkit`, `wpe`, `webview2`) inject the
//! same DOM-walker JS and parse the same JSON shape into a
//! [`Tree`]. This module holds the parser and the injected scripts so
//! each backend doesn't carry its own copy; the submodules hold the
//! primitives built on a backend's `eval` closure ([`act`], [`auth`],
//! [`download`], [`inspect`]).
//!
//! The module is unconditionally compiled (no `cfg`) so the helpers
//! are available regardless of target.

mod act;
mod auth;
mod download;
mod input;
mod inspect;

use std::collections::{BTreeMap, BTreeSet};

use vs_protocol::{Node, Op, Ref, Role, Tree};

#[cfg(not(target_os = "macos"))]
pub(crate) use act::ref_click_point;
pub(crate) use act::{build_html5_drag_js, run_act, run_layout, run_wait};
pub(crate) use auth::{run_load_storage_only, run_save_storage_only};
pub(crate) use download::{run_download, run_download_list};
#[cfg(not(target_os = "macos"))]
pub(crate) use input::cursor_seed;
pub(crate) use input::KeyCadence;
pub(crate) use inspect::{
    cookie_to_storage_entry, diff_cookies, run_dom, run_eval, run_performance, run_script_source,
    run_scripts, run_storage,
};

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
    // `level=N` on a heading (h1-h6 or aria-level).
    if let Some(l) = v.get("lvl").and_then(serde_json::Value::as_u64) {
        attrs.insert("level".to_string(), l.to_string());
    }
    // `lbl=<text>` on a form field with a <label> or aria-labelledby.
    // The node's label stays the field's value.
    if let Some(l) = v.get("flbl").and_then(serde_json::Value::as_str) {
        attrs.insert("lbl".to_string(), l.to_string());
    }
    // `checked=1` on a checked checkbox or radio; absent when unchecked.
    if v.get("chk").and_then(serde_json::Value::as_u64) == Some(1) {
        attrs.insert("checked".to_string(), "1".to_string());
    }
    // `challenge=<provider>:<state>` on a bot-check widget. `pending`
    // means unsolved, and is usually one trusted click away from
    // solved — see `challenge_box` below. Providers draw into a closed
    // shadow root, so the widget itself never appears in the tree.
    if let Some(chal) = v.get("chal").and_then(serde_json::Value::as_str) {
        attrs.insert("challenge".to_string(), chal.to_string());
    }
    // `challenge_box=x,y,w,h` — the widget's viewport rect, so an agent
    // can drive `vs click-at` straight at it.
    if let Some(b) = v.get("chalbox").and_then(serde_json::Value::as_str) {
        attrs.insert("challenge_box".to_string(), b.to_string());
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

/// Document-start shim that cancels every trusted `dragstart`, on every
/// backend.
///
/// Synthesized mouse input over anything draggable (a link, an image,
/// `draggable="true"`, selected text) can make the engine start a real
/// system drag. On macOS that shows a drag window and waits for a
/// hardware mouse-up that never comes, wedging the daemon; on Linux
/// GTK's drag began after `vs drag` had released, then took the next
/// release on the display, another page's click. A cancelled
/// `dragstart` is the one point every engine honours before starting
/// that drag. Nobody uses these views by hand, so every trusted drag
/// came from vs. Pages still see the `dragstart`, and `vs drag`
/// delivers the HTML5 chain afterwards as untrusted events, which this
/// leaves alone. See also `headless_webview.rs`, the macOS backstop if
/// a drag starts anyway.
pub(crate) const NO_NATIVE_DRAG_JS: &str = r"
window.addEventListener('dragstart', function (e) {
  if (e.isTrusted) e.preventDefault();
}, true);
";

/// JS that snapshots only localStorage + sessionStorage. The cookie
/// portion is handled by the host-side cookie store API (which sees
/// HttpOnly cookies; `document.cookie` does not).
pub(crate) const STORAGE_SAVE_JS: &str = include_str!("../storage_save.js");

/// JS that restores localStorage + sessionStorage from `payload`.
pub(crate) const STORAGE_LOAD_BODY_JS: &str = include_str!("../storage_load_body.js");

/// Kickoff-and-poll dump of every IndexedDB database on the origin.
pub(crate) const IDB_SAVE_JS: &str = include_str!("../idb_save.js");

/// Kickoff-and-poll restore of the databases `IDB_SAVE_JS` captured.
pub(crate) const IDB_LOAD_BODY_JS: &str = include_str!("../idb_load.js");

/// Shared DOM-walker JS payload. All three real backends evaluate this
/// and parse the result with [`parse_snapshot`].
pub(crate) const SNAPSHOT_DOM_WALKER_JS: &str = include_str!("../snapshot_dom_walker.js");

/// Virtual WebAuthn authenticator shim. Every backend adds it as a
/// document-start script (all frames) and evaluates it once on the
/// document already loaded.
pub(crate) const WEBAUTHN_JS: &str = include_str!("../webauthn_virtual.js");

/// Download-capture shim. Injected at document-start into every frame
/// on every backend, and re-evaluated on demand by [`run_download`] for
/// documents that predate the injection. See the file header for why
/// interception happens above the engine's download machinery.
pub(crate) const DOWNLOAD_SHIM_JS: &str = include_str!("../download_shim.js");

/// How long `open` and `goto` wait for a navigation to finish once it
/// has started.
///
/// 15 seconds is the right answer for a person waiting on a page, and
/// stays the default. It is not the web-process launch allowance: on
/// macOS that is a separate 45s wait that ends at
/// `didStartProvisionalNavigation`, because a cold `WKWebView` can
/// spend longer than 15s just spawning its process. Charging the
/// spawn against this budget produced `! TIMEOUT 15000ms open` for a
/// navigation that had not begun. `VS_NAV_BUDGET_MS` lets a caller buy
/// more patience for the load itself without making a truly hung page
/// take longer to report for everyone else. The integration harness
/// sets it to 45s.
///
/// macOS and Linux only, because those are the backends that bound a
/// navigation at all. Windows waits on `NavigationCompleted` through
/// `wait_with_pump` with no deadline, so a hung navigation hangs the
/// call there — a real gap, but an older and separate one than this.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(crate) fn nav_budget() -> std::time::Duration {
    const DEFAULT_MS: u64 = 15_000;
    let ms = std::env::var("VS_NAV_BUDGET_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .unwrap_or(DEFAULT_MS);
    std::time::Duration::from_millis(ms)
}

/// How long a click waits for the page's new-window request.
///
/// The engine raises that request (WKUIDelegate `createWebView`,
/// WebKitGTK `create`, WebView2 `NewWindowRequested`) over IPC from the
/// content process, and it can land after the click has returned. The
/// wait only happens when the page's hint (`NEW_WINDOW_HINT_JS`) says a
/// request is coming, and ends as soon as it arrives. Every click used
/// to pay this in full, which cost ~250ms per click.
pub(crate) const NEW_WINDOW_SETTLE: std::time::Duration = std::time::Duration::from_secs(1);

/// Read and clear the page's new-window hint (`__vsNw`, set by the
/// document-start shim when a blank-target link or form or
/// `window.open` fires). The reply carries `vsnw:1` when a new-window
/// request is on its way.
pub(crate) const NEW_WINDOW_HINT_JS: &str =
    "(function(){var v=window.__vsNw;try{window.__vsNw=0;}catch(e){}return v?'vsnw:1':'vsnw:0';})()";

/// Whether to wait for a new-window request after a click, given the
/// hint eval's result. A failed eval waits, as before the hint existed.
pub(crate) fn new_window_hinted(result: &Result<String, impl std::fmt::Debug>) -> bool {
    match result {
        Ok(s) => s.contains("vsnw:1"),
        Err(_) => true,
    }
}

/// Whether `action` can open a new window: a click, a submit, or Enter.
pub(crate) fn may_open_window(action: &crate::engine::Action) -> bool {
    use crate::engine::Action;
    match action {
        Action::Click | Action::Submit => true,
        Action::Key { chord } => chord.eq_ignore_ascii_case("enter"),
        _ => false,
    }
}

/// `s` as a JS string literal (`""` if it cannot be encoded).
fn json_string(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}
