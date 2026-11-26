#![allow(clippy::many_single_char_names)]
//! End-to-end integration: drive primitives 1–9 against a daemon
//! backed by a temp `Store` and the `TestEngine`. Covers the in-process
//! [`Daemon`] API; wire-format dispatch lives in `tests/wire.rs`.

use std::sync::Arc;
use std::time::Duration;

use vs_daemon::{daemon::Daemon, page_state::ViewForm};
use vs_engine_webkit::{self as engine, test_support::TestEngine, Engine, EngineRuntime};
use vs_protocol::{Ref, StateToken};
use vs_store::Store;

mod inspect;

fn make_daemon() -> (Daemon, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("state.db")).expect("store");
    let runtime = EngineRuntime::spawn(|| Ok(Box::new(TestEngine::new()) as Box<dyn Engine>))
        .expect("engine");
    let daemon = Daemon::new(store, Arc::new(runtime));
    (daemon, dir)
}

#[test]
fn full_primitive_sequence() {
    let (d, _dir) = make_daemon();
    let s = d.session_open(Some("default")).unwrap();
    assert!(s.session_id.starts_with("s_"));

    let p = d.open(&s.session_id, "https://example.com/login").unwrap();
    assert!(p.page_id.starts_with("p_"));

    let v1 = d.view(&s.session_id, &p.page_id, false).unwrap();
    // First view after `open` is a re-baseline → Full.
    assert!(matches!(v1.form, ViewForm::Full(_)));

    // Second view with no engine state change → NoChange.
    let v2 = d.view(&s.session_id, &p.page_id, false).unwrap();
    assert!(matches!(v2.form, ViewForm::NoChange));

    // Forced full re-emit.
    let v3 = d.view(&s.session_id, &p.page_id, true).unwrap();
    assert!(matches!(v3.form, ViewForm::Full(_)));

    let r = d.read(&s.session_id, &p.page_id, Ref(1)).unwrap();
    assert!(r.body.contains("[1] doc"));

    let act_args = vec!["click".into()];
    let args_hash = vs_daemon::tokens::args_hash("vs_act", &act_args);
    let after_act = d
        .act(vs_daemon::daemon::ActCall {
            session_id: s.session_id.clone(),
            page_id: p.page_id.clone(),
            target: engine::ActTarget::Ref(Ref(4)),
            action: engine::Action::Click,
            before_token: v3.token,
            args_hash,
            args_redacted: "click 4".into(),
            mode: vs_engine_webkit::engine::InputMode::Careful,
            group_label: Some("login-flow".into()),
        })
        .unwrap();
    assert_eq!(after_act.warnings.len(), 0);

    // The token returned by act is the post-action token.
    let f = d.find(&s.session_id, "Test").unwrap();
    assert!(!f.hits.is_empty());

    d.wait(
        &s.session_id,
        &p.page_id,
        engine::WaitCondition::Stable,
        Duration::from_millis(0),
    )
    .unwrap();

    d.close(&s.session_id, &p.page_id).unwrap();
    d.session_close(&s.session_id).unwrap();
}

#[test]
fn stale_token_rejected() {
    let (d, _dir) = make_daemon();
    let s = d.session_open(None).unwrap();
    let p = d.open(&s.session_id, "https://x").unwrap();
    let _ = d.view(&s.session_id, &p.page_id, false).unwrap();
    let stale = vs_protocol::StateToken::from_bytes([0xff; 8]);
    let err = d
        .act(vs_daemon::daemon::ActCall {
            session_id: s.session_id.clone(),
            page_id: p.page_id.clone(),
            target: engine::ActTarget::Ref(Ref(4)),
            action: engine::Action::Click,
            before_token: stale,
            args_hash: "h".into(),
            args_redacted: "click 4".into(),
            mode: vs_engine_webkit::engine::InputMode::Careful,
            group_label: None,
        })
        .unwrap_err();
    assert!(matches!(err, vs_daemon::DaemonError::StaleToken { .. }));
}

#[test]
fn idempotent_replay() {
    let (d, _dir) = make_daemon();
    let s = d.session_open(None).unwrap();
    let p = d.open(&s.session_id, "https://x").unwrap();
    let v = d.view(&s.session_id, &p.page_id, false).unwrap();
    let h = vs_daemon::tokens::args_hash("vs_act", &["click".into()]);

    let r1 = d
        .act(vs_daemon::daemon::ActCall {
            session_id: s.session_id.clone(),
            page_id: p.page_id.clone(),
            target: engine::ActTarget::Ref(Ref(4)),
            action: engine::Action::Click,
            before_token: v.token,
            args_hash: h.clone(),
            args_redacted: "click 4".into(),
            mode: vs_engine_webkit::engine::InputMode::Careful,
            group_label: None,
        })
        .unwrap();
    let r2 = d
        .act(vs_daemon::daemon::ActCall {
            session_id: s.session_id.clone(),
            page_id: p.page_id.clone(),
            target: engine::ActTarget::Ref(Ref(4)),
            action: engine::Action::Click,
            before_token: v.token,
            args_hash: h,
            args_redacted: "click 4".into(),
            mode: vs_engine_webkit::engine::InputMode::Careful,
            group_label: None,
        })
        .unwrap();
    // Second call should be marked as an idempotency hit.
    assert!(r1.warnings.is_empty());
    assert_eq!(r2.warnings.len(), 1);
    assert_eq!(r2.warnings[0].code, vs_protocol::WarningCode::IdempotentHit);
}

#[test]
fn audit_log_records_every_call() {
    let (d, _dir) = make_daemon();
    let s = d.session_open(None).unwrap();
    let p = d.open(&s.session_id, "https://x").unwrap();
    d.view(&s.session_id, &p.page_id, false).unwrap();
    d.close(&s.session_id, &p.page_id).unwrap();
    d.session_close(&s.session_id).unwrap();

    let rows = d.audit_log(&vs_store::ActionFilter::default()).unwrap();
    let primitives: Vec<&str> = rows.iter().map(|r| r.primitive.as_str()).collect();
    assert_eq!(
        primitives,
        vec![
            "vs_session_open",
            "vs_open",
            "vs_view",
            "vs_close",
            "vs_session_close"
        ]
    );
}

#[test]
fn unknown_session_errors_without_engine_call() {
    let (d, _dir) = make_daemon();
    let err = d.open("nope", "https://x").unwrap_err();
    assert!(matches!(err, vs_daemon::DaemonError::UnknownSession(_)));
}

// =============================================================================
// M5.5 PR1: dispatcher contract tests.
// =============================================================================

use vs_daemon::Primitive;
use vs_protocol::Request;

#[test]
fn dispatch_runs_two_primitives_in_order() {
    let (d, _dir) = make_daemon();
    let s = d.session_open(None).unwrap();
    let open = Request::new("vs_open")
        .arg("https://example.com/order".to_string())
        .flag_value("session", s.session_id.clone());
    let status = Request::new("vs_status").flag_value("session", s.session_id.clone());

    let outcomes: Vec<Primitive> = vec![open, status];
    let results = d.dispatch(&outcomes);

    assert_eq!(results.len(), 2);
    assert!(
        results[0].wire.starts_with('@'),
        "open: {:?}",
        results[0].wire
    );
    assert!(
        results[1].wire.starts_with('@'),
        "status: {:?}",
        results[1].wire
    );
    assert!(results[0].wire.contains("p_"));
    assert!(results[1].wire.contains(&s.session_id));
}

#[test]
fn dispatch_continues_after_error() {
    let (d, _dir) = make_daemon();
    let s = d.session_open(None).unwrap();
    let bad_open = Request::new("vs_open")
        .arg("https://example.com/".to_string())
        .flag_value("session", "s_does_not_exist".to_string());
    let good_status = Request::new("vs_status").flag_value("session", s.session_id.clone());

    let results = d.dispatch(&[bad_open, good_status]);

    assert_eq!(results.len(), 2);
    assert!(
        results[0].wire.starts_with('!'),
        "bad_open should fail: {:?}",
        results[0].wire
    );
    assert!(
        results[1].wire.starts_with('@'),
        "good_status: {:?}",
        results[1].wire
    );
}

// =============================================================================
// M5.5 PR2: vs_open --view composite.
// =============================================================================

#[test]
fn open_with_view_returns_tree() {
    let (d, _dir) = make_daemon();
    let s = d.session_open(None).unwrap();

    let req = Request::new("vs_open")
        .arg("https://example.com/login".to_string())
        .flag("view")
        .flag_value("session", s.session_id.clone());
    let outcomes = d.dispatch(&[req]);
    assert_eq!(outcomes.len(), 1);
    let wire = &outcomes[0].wire;

    // Two envelopes joined by a blank line: open's @<token>\np_xxx
    // followed by view's @<token>\n<tree lines>.
    assert!(wire.starts_with('@'), "open envelope: {wire:?}");
    assert!(wire.contains("p_"), "open body should include page id");

    // Find the inner blank line that separates the two outcomes.
    let split_at = wire
        .find("\n\n")
        .expect("expected blank-line separator between open and view");
    let view_section = &wire[split_at + 2..];
    assert!(
        view_section.starts_with('@'),
        "view envelope after separator: {view_section:?}"
    );
    // TestEngine's tree includes a `doc` root.
    assert!(
        view_section.contains("doc"),
        "view body should include `doc` role: {view_section:?}"
    );
}

#[test]
fn open_with_view_idempotent() {
    let (d, _dir) = make_daemon();
    let s = d.session_open(None).unwrap();

    // Two opens of the same URL within the idempotency window.
    let mk_req = || {
        Request::new("vs_open")
            .arg("https://example.com/order".to_string())
            .flag("view")
            .flag_value("session", s.session_id.clone())
    };
    let r1 = d.dispatch(&[mk_req()]);
    let r2 = d.dispatch(&[mk_req()]);

    let w1 = &r1[0].wire;
    let w2 = &r2[0].wire;
    // Both should succeed; both should carry tree content. The open
    // half may differ in whether it carries an `idempotent_hit`
    // warning, but the view half is always fresh (computed each call).
    assert!(w1.contains("\n\n@"), "first call has open + view");
    assert!(w2.contains("\n\n@"), "second call has open + view");
    assert!(w1.contains("doc") && w2.contains("doc"), "both have tree");
}

// =============================================================================
// M5.5 PR3: vs_wait --view composite.
// =============================================================================

#[test]
fn wait_stable_then_view() {
    let (d, _dir) = make_daemon();
    let s = d.session_open(None).unwrap();
    let p = d.open(&s.session_id, "https://example.com").unwrap();
    // Need at least one view to baseline before the wait.
    d.view(&s.session_id, &p.page_id, false).unwrap();

    let req = Request::new("vs_wait")
        .arg(p.page_id.clone())
        .arg("stable".to_string())
        .flag("view")
        .flag_value("session", s.session_id.clone())
        .flag_value("timeout", "100".to_string());
    let outcomes = d.dispatch(&[req]);
    let wire = &outcomes[0].wire;
    assert!(wire.starts_with('@'), "wait envelope: {wire:?}");
    let split = wire.find("\n\n").expect("expected wait+view separator");
    let view_section = &wire[split + 2..];
    assert!(
        view_section.starts_with('@'),
        "view envelope: {view_section:?}"
    );
}

#[test]
fn wait_timeout_skips_view() {
    let (d, _dir) = make_daemon();
    let s = d.session_open(None).unwrap();
    let p = d.open(&s.session_id, "https://example.com").unwrap();
    d.view(&s.session_id, &p.page_id, false).unwrap();

    // `ref` 999 will never appear in the stub tree; force a timeout.
    let req = Request::new("vs_wait")
        .arg(p.page_id.clone())
        .arg("ref".to_string())
        .arg("999".to_string())
        .flag("view")
        .flag_value("session", s.session_id.clone())
        .flag_value("timeout", "50".to_string());
    let outcomes = d.dispatch(&[req]);
    let wire = &outcomes[0].wire;
    assert!(wire.starts_with('!'), "wait should fail: {wire:?}");
    assert!(
        !wire.contains("\n\n@"),
        "no view section after a wait failure: {wire:?}"
    );
}

// =============================================================================
// M5.5 PR4: vs_act --view composite.
// =============================================================================

fn build_act_req(session: &str, page: &str, r: u32, op: &str, token: &str) -> Request {
    Request::new("vs_act")
        .arg(page.to_string())
        .arg(r.to_string())
        .arg(op.to_string())
        .flag_value("session", session.to_string())
        .flag_value("token", token.to_string())
}

#[test]
fn act_with_view_returns_delta() {
    let (d, _dir) = make_daemon();
    let s = d.session_open(None).unwrap();
    let p = d.open(&s.session_id, "https://x").unwrap();
    let v = d.view(&s.session_id, &p.page_id, false).unwrap();

    let req =
        build_act_req(&s.session_id, &p.page_id, 4, "click", &v.token.to_string()).flag("view");
    let outcomes = d.dispatch(&[req]);
    let wire = &outcomes[0].wire;
    assert!(wire.starts_with('@'), "act envelope: {wire:?}");
    let split = wire.find("\n\n").expect("expected act+view separator");
    let view_section = &wire[split + 2..];
    assert!(
        view_section.starts_with('@'),
        "view envelope after act: {view_section:?}"
    );
}

#[test]
fn stale_act_skips_view() {
    let (d, _dir) = make_daemon();
    let s = d.session_open(None).unwrap();
    let p = d.open(&s.session_id, "https://x").unwrap();
    d.view(&s.session_id, &p.page_id, false).unwrap();

    let stale = "ffffffffffffffff";
    let req = build_act_req(&s.session_id, &p.page_id, 4, "click", stale).flag("view");
    let outcomes = d.dispatch(&[req]);
    let wire = &outcomes[0].wire;
    assert!(wire.starts_with('!'), "act should fail: {wire:?}");
    assert!(
        !wire.contains("\n\n@"),
        "no view section after stale: {wire:?}"
    );
}

#[test]
fn idempotent_act_still_views() {
    let (d, _dir) = make_daemon();
    let s = d.session_open(None).unwrap();
    let p = d.open(&s.session_id, "https://x").unwrap();
    let v = d.view(&s.session_id, &p.page_id, false).unwrap();

    let token = v.token.to_string();
    // Two identical acts within the idempotency window.
    let mk = || build_act_req(&s.session_id, &p.page_id, 4, "click", &token).flag("view");
    let _ = d.dispatch(&[mk()]);
    let outcomes = d.dispatch(&[mk()]);
    let wire = &outcomes[0].wire;
    // Second act should carry the ? idempotent_hit warning before
    // its envelope, AND still produce a view section.
    assert!(
        wire.contains("? idempotent_hit") || wire.contains("?idempotent_hit"),
        "idempotent_hit warning expected: {wire:?}"
    );
    assert!(
        wire.contains("\n\n@"),
        "view section still emitted on idempotent replay: {wire:?}"
    );
}

// =============================================================================
// M5.5 PR5: vs_view --layout=N,M composite.
// =============================================================================

#[test]
fn view_with_layout_includes_boxes() {
    let (d, _dir) = make_daemon();
    let s = d.session_open(None).unwrap();
    let p = d.open(&s.session_id, "https://x").unwrap();

    let req = Request::new("vs_view")
        .arg(p.page_id.clone())
        .flag_value("session", s.session_id.clone())
        .flag_value("layout", "1,2,4".to_string());
    let outcomes = d.dispatch(&[req]);
    let wire = &outcomes[0].wire;
    assert!(wire.starts_with('@'), "view envelope: {wire:?}");
    let split = wire
        .find("\n\n")
        .expect("expected blank-line separator before layout");
    let layout_section = &wire[split + 2..];
    assert!(
        layout_section.contains("x=") && layout_section.contains("w="),
        "layout body should have box lines: {layout_section:?}"
    );
}

#[test]
fn view_layout_unknown_ref_warns() {
    let (d, _dir) = make_daemon();
    let s = d.session_open(None).unwrap();
    let p = d.open(&s.session_id, "https://x").unwrap();

    // Ref 999 doesn't exist in TestEngine's tree.
    let req = Request::new("vs_view")
        .arg(p.page_id.clone())
        .flag_value("session", s.session_id.clone())
        .flag_value("layout", "999".to_string());
    let outcomes = d.dispatch(&[req]);
    let wire = &outcomes[0].wire;
    // View itself succeeds.
    assert!(wire.starts_with('@'), "view envelope: {wire:?}");
    // Layout section contains the ! NOT_FOUND line.
    assert!(
        wire.contains("! NOT_FOUND ref=999"),
        "expected NOT_FOUND for unknown ref: {wire:?}"
    );
}

// =============================================================================
// M5.5 PR6: vs_view --read=N composite.
// =============================================================================

#[test]
fn view_with_read_includes_text() {
    let (d, _dir) = make_daemon();
    let s = d.session_open(None).unwrap();
    let p = d.open(&s.session_id, "https://x").unwrap();

    let req = Request::new("vs_view")
        .arg(p.page_id.clone())
        .flag_value("session", s.session_id.clone())
        .flag_value("read", "1".to_string());
    let outcomes = d.dispatch(&[req]);
    let wire = &outcomes[0].wire;
    assert!(wire.starts_with('@'), "view envelope: {wire:?}");
    assert!(
        wire.contains("\n\n[1] doc"),
        "expected read body after blank separator: {wire:?}"
    );
}

/// Regression for the check-then-act race: two concurrent `act` calls
/// on the same page must serialize on the page's mutation lock and
/// complete without deadlock. `TestEngine` acts don't change the
/// snapshot, so both succeed; the assertion here is liveness plus
/// distinct audit rows (no lost call).
#[test]
fn concurrent_acts_on_same_page_serialize_without_deadlock() {
    let (d, _dir) = make_daemon();
    let s = d.session_open(None).unwrap();
    let p = d.open(&s.session_id, "https://x").unwrap();
    let v = d.view(&s.session_id, &p.page_id, false).unwrap();

    let call = |hash: &str| vs_daemon::daemon::ActCall {
        session_id: s.session_id.clone(),
        page_id: p.page_id.clone(),
        target: engine::ActTarget::Ref(Ref(4)),
        action: engine::Action::Click,
        before_token: v.token,
        args_hash: hash.into(),
        args_redacted: "click 4".into(),
        mode: vs_engine_webkit::engine::InputMode::Careful,
        group_label: None,
    };

    let d2 = d.clone();
    let c1 = call("hash-a");
    let t = std::thread::spawn(move || d2.act(c1));
    let r2 = d.act(call("hash-b"));
    let r1 = t.join().expect("act thread panicked");
    r1.expect("first concurrent act failed");
    r2.expect("second concurrent act failed");

    let actions = d
        .audit_log(&vs_store::ActionFilter {
            session_id: Some(s.session_id.clone()),
            ..Default::default()
        })
        .unwrap();
    let acts = actions.iter().filter(|a| a.primitive == "vs_act").count();
    assert_eq!(acts, 2, "both concurrent acts must be audited");
}

/// A captured download must land inside the downloads directory under
/// a sanitized name, no matter what the page called it.
///
/// The name the `TestEngine` reports (`../../../escaped.bin`) is the
/// shape a hostile `Content-Disposition` takes: three levels up and
/// into a file of the attacker's choosing. Everything before the last
/// separator has to be discarded before the daemon writes anything.
#[test]
fn captured_download_cannot_escape_the_downloads_dir() {
    let dir = tempfile::tempdir().unwrap();
    let downloads = dir.path().join("downloads");
    let store = Store::open(dir.path().join("state.db")).expect("store");
    let runtime = EngineRuntime::spawn(|| Ok(Box::new(TestEngine::new()) as Box<dyn Engine>))
        .expect("engine");
    let d = Daemon::new(store, Arc::new(runtime)).with_downloads_dir(&downloads);

    let s = d.session_open(Some("default")).unwrap();
    let p = d.open(&s.session_id, "https://example.com/docs").unwrap();

    let got = d
        .download(
            &s.session_id,
            &p.page_id,
            engine::DownloadSource::Captured { id: None },
            None,
            Duration::from_secs(5),
        )
        .unwrap();

    assert_eq!(
        got.path,
        downloads.join("escaped.bin"),
        "traversal survived sanitization"
    );
    assert_eq!(
        std::fs::read(&got.path).unwrap(),
        vs_engine_webkit::test_support::CAPTURED_DOWNLOAD_BODY,
    );
    assert_eq!(got.size, got.path.metadata().unwrap().len());
    assert_eq!(got.mime, "application/pdf");
}

/// A second download of the same name gets a suffix instead of
/// clobbering the first.
#[test]
fn repeated_download_does_not_overwrite() {
    let dir = tempfile::tempdir().unwrap();
    let downloads = dir.path().join("downloads");
    let store = Store::open(dir.path().join("state.db")).expect("store");
    let runtime = EngineRuntime::spawn(|| Ok(Box::new(TestEngine::new()) as Box<dyn Engine>))
        .expect("engine");
    let d = Daemon::new(store, Arc::new(runtime)).with_downloads_dir(&downloads);

    let s = d.session_open(Some("default")).unwrap();
    let p = d.open(&s.session_id, "https://example.com/docs").unwrap();

    let url = engine::DownloadSource::Url("https://example.com/files/report.pdf".into());
    let first = d
        .download(
            &s.session_id,
            &p.page_id,
            url.clone(),
            None,
            Duration::from_secs(5),
        )
        .unwrap();
    let second = d
        .download(&s.session_id, &p.page_id, url, None, Duration::from_secs(5))
        .unwrap();

    assert_eq!(first.path, downloads.join("report.pdf"));
    assert_eq!(second.path, downloads.join("report-1.pdf"));
    assert!(first.path.exists() && second.path.exists());
}

/// A relative `--dest` is confined to the downloads directory; an
/// absolute one is honored as given.
#[test]
fn download_dest_is_confined_when_relative() {
    let dir = tempfile::tempdir().unwrap();
    let downloads = dir.path().join("downloads");
    let store = Store::open(dir.path().join("state.db")).expect("store");
    let runtime = EngineRuntime::spawn(|| Ok(Box::new(TestEngine::new()) as Box<dyn Engine>))
        .expect("engine");
    let d = Daemon::new(store, Arc::new(runtime)).with_downloads_dir(&downloads);

    let s = d.session_open(Some("default")).unwrap();
    let p = d.open(&s.session_id, "https://example.com/docs").unwrap();
    let url = engine::DownloadSource::Url("https://example.com/a.bin".into());

    let confined = d
        .download(
            &s.session_id,
            &p.page_id,
            url.clone(),
            Some("../../outside.bin"),
            Duration::from_secs(5),
        )
        .unwrap();
    assert_eq!(confined.path, downloads.join("outside.bin"));

    let explicit = dir.path().join("elsewhere").join("named.bin");
    let absolute = d
        .download(
            &s.session_id,
            &p.page_id,
            url,
            Some(explicit.to_str().unwrap()),
            Duration::from_secs(5),
        )
        .unwrap();
    assert_eq!(absolute.path, explicit);
    assert!(explicit.exists());
}

/// `vs_download --list` surfaces what the page tried to save without
/// reading any payload.
#[test]
fn download_list_reports_captured_intents() {
    let (d, _dir) = make_daemon();
    let s = d.session_open(Some("default")).unwrap();
    let p = d.open(&s.session_id, "https://example.com/docs").unwrap();

    let listed = d.download_list(&s.session_id, &p.page_id).unwrap();
    assert_eq!(listed.entries.len(), 1);
    let e = &listed.entries[0];
    assert!(e.done);
    assert_eq!(e.error, None);
    assert_eq!(e.mime, "application/pdf");
    assert!(e.url.starts_with("blob:"));
}

/// A secure form fill must survive the page changing while the human
/// types.
///
/// `vs_prompt_form` captured the page token, then handed a URL to a
/// person. They take seconds or minutes. Any re-render in that window
/// — which a framework-driven login form does constantly, on
/// validation, focus and timers — advances the token, and the fill
/// then failed the stale-token check and silently never ran: pending
/// entry consumed, form reported fulfilled, field still empty. Secure
/// credential entry into a React or Vue login was impossible, which is
/// what the primitive exists for.
#[test]
fn prompt_form_fill_survives_a_page_change_while_the_human_types() {
    let (d, _dir) = make_daemon();
    let s = d.session_open(Some("default")).unwrap();
    let p = d.open(&s.session_id, "https://example.com/login").unwrap();
    let before = d.view(&s.session_id, &p.page_id, false).unwrap();

    // The agent asks for one secret, quoting the token it just read.
    let (form_id, _url) = d
        .prompt_form_enqueue(
            &p.page_id,
            vec![(Ref(3), "Password".to_string(), true)],
            &before.token.to_string(),
            None,
        )
        .expect("enqueue form");

    // The page moves on while the human is typing. Navigating is the
    // clearest stand-in available against the canned test tree — a
    // re-baseline alone yields an identical token, because the token is
    // derived from tree content and the fake engine's tree never
    // changes. What matters is only that the page's current token no
    // longer equals the one the form captured.
    d.navigate(
        &s.session_id,
        &p.page_id,
        "https://example.com/login?step=2",
        None,
    )
    .unwrap();
    let after = d.view(&s.session_id, &p.page_id, true).unwrap();
    assert_ne!(
        before.token, after.token,
        "test setup: the page must have moved for this to prove anything"
    );

    // The human submits.
    let pending = d.pending_list();
    let entry = pending
        .iter()
        .find(|e| e.form.as_deref() == Some(form_id.as_str()))
        .expect("pending entry present");
    assert!(d.pending_fulfill(&entry.id, "hunter2".into()));

    // The fill must land, not fail on the token it captured minutes ago.
    let token = d
        .prompt_form_wait(&s.session_id, &form_id, Duration::from_secs(5))
        .expect("secure fill must survive the page moving while a human types");
    assert_ne!(token, StateToken::ZERO);
}
