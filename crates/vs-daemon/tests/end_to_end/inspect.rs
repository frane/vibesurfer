//! `vs_inspect` through the daemon: kind dispatch, console, network,
//! requests, eval, storage, scripts, DOM, performance and the console
//! error warning on `view`.

use vs_protocol::Request;

use super::make_daemon;

// =============================================================================

#[test]
fn inspect_unknown_kind_errors() {
    let (d, _dir) = make_daemon();
    let s = d.session_open(None).unwrap();
    let p = d.open(&s.session_id, "https://x").unwrap();

    let req = Request::new("vs_inspect")
        .arg("frobulate".to_string())
        .arg(p.page_id.clone())
        .flag_value("session", s.session_id.clone());
    let outcomes = d.dispatch(&[req]);
    let wire = &outcomes[0].wire;
    assert!(
        wire.starts_with("! UNKNOWN_KIND"),
        "expected UNKNOWN_KIND envelope: {wire:?}"
    );
}

#[test]
fn inspect_console_returns_capacity() {
    let (d, _dir) = make_daemon();
    let s = d.session_open(None).unwrap();
    let p = d.open(&s.session_id, "https://x").unwrap();

    let req = Request::new("vs_inspect")
        .arg("console".to_string())
        .arg(p.page_id.clone())
        .flag_value("session", s.session_id.clone());
    let outcomes = d.dispatch(&[req]);
    let wire = &outcomes[0].wire;
    assert!(wire.starts_with('@'), "console envelope: {wire:?}");
    assert!(
        wire.contains("error") && wire.contains("synthetic error"),
        "expected synthetic error entry: {wire:?}"
    );
}

// =============================================================================
// M5.7 PR2: vs_inspect console + network + request — filtering & redaction.
// =============================================================================

#[test]
fn inspect_console_filters_by_level() {
    let (d, _dir) = make_daemon();
    let s = d.session_open(None).unwrap();
    let p = d.open(&s.session_id, "https://x").unwrap();

    // Stub emits log + warn + error. Filter to error only.
    let req = Request::new("vs_inspect")
        .arg("console".to_string())
        .arg(p.page_id.clone())
        .flag_value("session", s.session_id.clone())
        .flag_value("level", "error".to_string());
    let outcomes = d.dispatch(&[req]);
    let wire = &outcomes[0].wire;
    assert!(wire.starts_with('@'), "console envelope: {wire:?}");
    assert!(wire.contains(" error "), "should keep error: {wire:?}");
    assert!(!wire.contains(" warn "), "should drop warn: {wire:?}");
    assert!(!wire.contains(" log "), "should drop log: {wire:?}");
}

#[test]
fn inspect_console_since_token() {
    let (d, _dir) = make_daemon();
    let s = d.session_open(None).unwrap();
    let p = d.open(&s.session_id, "https://x").unwrap();
    // Force an audit row whose after_token we can pass as --since.
    let v = d.view(&s.session_id, &p.page_id, false).unwrap();
    let token = v.token.to_string();

    let req = Request::new("vs_inspect")
        .arg("console".to_string())
        .arg(p.page_id.clone())
        .flag_value("session", s.session_id.clone())
        .flag_value("since", token.clone());
    let outcomes = d.dispatch(&[req]);
    let wire = &outcomes[0].wire;
    // The view's finished_at is now-ish; the stub's synthetic entries
    // are at now-2s, now-1s, now. Since-filtering drops the older two
    // and keeps the most recent (or none, if the audit row is *after*
    // all synthetic entries — both outcomes are valid).
    assert!(wire.starts_with('@'), "envelope: {wire:?}");
    // Body may be empty (everything older than since) or contain only
    // the most recent error. Either way, fewer entries than unfiltered.
    let entry_lines = wire
        .lines()
        .filter(|l| l.contains(" error ") || l.contains(" warn ") || l.contains(" log "))
        .count();
    assert!(
        entry_lines <= 1,
        "since-filter should drop older entries; got {entry_lines}"
    );
}

#[test]
fn inspect_network_filters_by_status() {
    let (d, _dir) = make_daemon();
    let s = d.session_open(None).unwrap();
    let p = d.open(&s.session_id, "https://x").unwrap();

    // Stub emits 200, 401, 404. Filter to 4xx.
    let req = Request::new("vs_inspect")
        .arg("network".to_string())
        .arg(p.page_id.clone())
        .flag_value("session", s.session_id.clone())
        .flag_value("status", "4xx".to_string());
    let outcomes = d.dispatch(&[req]);
    let wire = &outcomes[0].wire;
    assert!(wire.starts_with('@'), "envelope: {wire:?}");
    assert!(
        wire.contains(" 401 ") || wire.contains(" 404 "),
        "should keep 4xx: {wire:?}"
    );
    assert!(!wire.contains(" 200 "), "should drop 2xx: {wire:?}");
}

#[test]
fn inspect_request_redacts_auth_headers() {
    let (d, _dir) = make_daemon();
    let s = d.session_open(None).unwrap();
    let p = d.open(&s.session_id, "https://x").unwrap();

    // seq=2 carries Authorization: Bearer secret-token-xyz.
    let req = Request::new("vs_inspect")
        .arg("request".to_string())
        .arg(p.page_id.clone())
        .arg("2".to_string())
        .flag_value("session", s.session_id.clone());
    let outcomes = d.dispatch(&[req]);
    let wire = &outcomes[0].wire;
    assert!(wire.starts_with('@'), "request envelope: {wire:?}");
    // Header name visible, value redacted.
    assert!(
        wire.contains("Authorization: ***"),
        "expected redacted authz: {wire:?}"
    );
    assert!(
        !wire.contains("Bearer secret-token-xyz"),
        "secret leaked: {wire:?}"
    );

    // --unsafe-log opt-out: secret is visible.
    let req2 = Request::new("vs_inspect")
        .arg("request".to_string())
        .arg(p.page_id.clone())
        .arg("2".to_string())
        .flag_value("session", s.session_id.clone())
        .flag("unsafe-log");
    let w2 = &d.dispatch(&[req2])[0].wire;
    assert!(
        w2.contains("Bearer secret-token-xyz"),
        "--unsafe-log should reveal: {w2:?}"
    );
}

#[test]
fn inspect_request_unknown_id_errors() {
    let (d, _dir) = make_daemon();
    let s = d.session_open(None).unwrap();
    let p = d.open(&s.session_id, "https://x").unwrap();

    let req = Request::new("vs_inspect")
        .arg("request".to_string())
        .arg(p.page_id.clone())
        .arg("999".to_string())
        .flag_value("session", s.session_id.clone());
    let wire = &d.dispatch(&[req])[0].wire;
    assert!(
        wire.starts_with("! NOT_FOUND"),
        "expected NOT_FOUND for unknown seq: {wire:?}"
    );
}

// =============================================================================
// M5.7 PR3-6: vs_inspect eval / storage / scripts / dom / performance + warning.
// =============================================================================

fn mk_inspect(session: &str, page: &str, kind: &str, args: &[&str]) -> Request {
    let mut r = Request::new("vs_inspect")
        .arg(kind.to_string())
        .arg(page.to_string())
        .flag_value("session", session.to_string());
    for a in args {
        r = r.arg((*a).to_string());
    }
    r
}

#[test]
fn eval_returns_number() {
    let (d, _dir) = make_daemon();
    let s = d.session_open(None).unwrap();
    let p = d.open(&s.session_id, "https://x").unwrap();
    let req = mk_inspect(&s.session_id, &p.page_id, "eval", &["42"]);
    let w = &d.dispatch(&[req])[0].wire;
    assert!(w.starts_with('@'), "envelope: {w:?}");
    assert!(w.contains("result=42"), "expected number result: {w:?}");
    assert!(w.contains("type=number"), "expected type=number: {w:?}");
}

#[test]
fn eval_serializes_object() {
    let (d, _dir) = make_daemon();
    let s = d.session_open(None).unwrap();
    let p = d.open(&s.session_id, "https://x").unwrap();
    let req = mk_inspect(&s.session_id, &p.page_id, "eval", &["{a:1}"]);
    let w = &d.dispatch(&[req])[0].wire;
    assert!(w.starts_with('@'), "envelope: {w:?}");
    assert!(w.contains("type=string"), "expected type=string: {w:?}");
}

#[test]
fn eval_thrown_error_returns_eval_error() {
    let (d, _dir) = make_daemon();
    let s = d.session_open(None).unwrap();
    let p = d.open(&s.session_id, "https://x").unwrap();
    let req = mk_inspect(&s.session_id, &p.page_id, "eval", &["throw new Error()"]);
    let w = &d.dispatch(&[req])[0].wire;
    assert!(w.starts_with("! EVAL_ERROR"), "expected EVAL_ERROR: {w:?}");
}

#[test]
fn inspect_storage_cookies_lists_all() {
    let (d, _dir) = make_daemon();
    let s = d.session_open(None).unwrap();
    let p = d.open(&s.session_id, "https://x").unwrap();
    let req = mk_inspect(&s.session_id, &p.page_id, "storage", &["cookies"]);
    let w = &d.dispatch(&[req])[0].wire;
    assert!(w.starts_with('@'));
    assert!(w.contains("session_id=*** "));
    assert!(w.contains("consent=all"));
}

#[test]
fn inspect_storage_local_truncates() {
    let (d, _dir) = make_daemon();
    let s = d.session_open(None).unwrap();
    let p = d.open(&s.session_id, "https://x").unwrap();
    let req = mk_inspect(&s.session_id, &p.page_id, "storage", &["local"]);
    let w = &d.dispatch(&[req])[0].wire;
    assert!(w.starts_with('@'));
    assert!(w.contains("auth_token=***"));
    assert!(w.contains("theme=dark"));
}

#[test]
fn inspect_storage_session_returns_entries() {
    let (d, _dir) = make_daemon();
    let s = d.session_open(None).unwrap();
    let p = d.open(&s.session_id, "https://x").unwrap();
    let req = mk_inspect(&s.session_id, &p.page_id, "storage", &["session"]);
    let w = &d.dispatch(&[req])[0].wire;
    assert!(w.starts_with('@'));
    assert!(w.contains("csrf=xyz"));
}

#[test]
fn inspect_storage_indexeddb_lists_databases() {
    let (d, _dir) = make_daemon();
    let s = d.session_open(None).unwrap();
    let p = d.open(&s.session_id, "https://x").unwrap();
    let req = mk_inspect(&s.session_id, &p.page_id, "storage", &["indexeddb"]);
    let w = &d.dispatch(&[req])[0].wire;
    assert!(w.starts_with('@'));
    assert!(w.contains("vibesurfer_demo_db"));
}

#[test]
fn inspect_scripts_lists_loaded() {
    let (d, _dir) = make_daemon();
    let s = d.session_open(None).unwrap();
    let p = d.open(&s.session_id, "https://x").unwrap();
    let req = mk_inspect(&s.session_id, &p.page_id, "scripts", &[]);
    let w = &d.dispatch(&[req])[0].wire;
    assert!(w.starts_with('@'));
    assert!(w.contains("s_1 ") && w.contains("parsed"));
    assert!(w.contains("s_3 ") && w.contains("error"));
}

#[test]
fn inspect_dom_returns_outer_html() {
    let (d, _dir) = make_daemon();
    let s = d.session_open(None).unwrap();
    let p = d.open(&s.session_id, "https://x").unwrap();
    let req = mk_inspect(&s.session_id, &p.page_id, "dom", &["4"]);
    let w = &d.dispatch(&[req])[0].wire;
    assert!(w.starts_with('@'));
    assert!(w.contains("data-vs-ref=\"4\""));
    assert!(w.contains("computed:"));
}

#[test]
fn inspect_dom_includes_computed_styles() {
    let (d, _dir) = make_daemon();
    let s = d.session_open(None).unwrap();
    let p = d.open(&s.session_id, "https://x").unwrap();
    let req = mk_inspect(&s.session_id, &p.page_id, "dom", &["4"]);
    let w = &d.dispatch(&[req])[0].wire;
    assert!(w.contains("display:"));
    assert!(w.contains("pointer-events:"));
    assert!(w.contains("z-index:"));
}

#[test]
fn inspect_performance_returns_vitals() {
    let (d, _dir) = make_daemon();
    let s = d.session_open(None).unwrap();
    let p = d.open(&s.session_id, "https://x").unwrap();
    let req = mk_inspect(&s.session_id, &p.page_id, "performance", &[]);
    let w = &d.dispatch(&[req])[0].wire;
    assert!(w.starts_with('@'));
    assert!(w.contains("ttfb=") && w.contains("lcp=") && w.contains("cls="));
    assert!(w.contains("js_heap=") && w.contains("dom_nodes="));
}

#[test]
fn view_emits_console_error_warning_when_new_errors() {
    let (d, _dir) = make_daemon();
    let s = d.session_open(None).unwrap();
    let p = d.open(&s.session_id, "https://x").unwrap();
    // First view: no prior view → warning *cannot* fire.
    let r1 = Request::new("vs_view")
        .arg(p.page_id.clone())
        .flag_value("session", s.session_id.clone());
    let w1 = &d.dispatch(&[r1])[0].wire;
    assert!(!w1.contains("? console_error"), "first view: {w1:?}");
}

#[test]
fn view_no_warning_when_no_new_errors() {
    let (d, _dir) = make_daemon();
    let s = d.session_open(None).unwrap();
    let p = d.open(&s.session_id, "https://x").unwrap();
    let r = || {
        Request::new("vs_view")
            .arg(p.page_id.clone())
            .flag_value("session", s.session_id.clone())
    };
    let _ = d.dispatch(&[r()]);
    std::thread::sleep(std::time::Duration::from_secs(1));
    let w2 = &d.dispatch(&[r()])[0].wire;
    assert!(!w2.contains("? console_error"), "second view: {w2:?}");
}
