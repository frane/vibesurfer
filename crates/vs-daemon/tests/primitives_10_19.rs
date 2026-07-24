//! Integration coverage for primitives 10–19 added during M5.

#![allow(clippy::many_single_char_names)]

use std::sync::Arc;

use vs_daemon::{daemon::Daemon, ActCall};
use vs_engine_webkit::{
    self as engine, test_support::TestEngine, CaptureScope, Engine, EngineRuntime, Viewport,
};
use vs_protocol::Ref;
use vs_store::{AnnotationTarget, MasterKey, Store};

fn make_daemon() -> (Daemon, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("state.db")).unwrap();
    // Isolate the TestEngine's capture files per test. Without this the
    // engine writes `test-engine-<page>-<seq>.png` into the shared
    // system temp dir, and parallel tests (page handle 1, seq 1) race
    // the same path — a flaky corrupt-PNG read.
    let cap_dir = dir.path().join("engine-captures");
    let runtime = EngineRuntime::spawn(move || {
        Ok(Box::new(TestEngine::new().with_capture_dir(&cap_dir)) as Box<dyn Engine>)
    })
    .unwrap();
    let daemon = Daemon::new(store, Arc::new(runtime))
        .with_captures_dir(dir.path().join("captures"))
        .with_skills_dir(dir.path().join("skills"))
        .with_master_key(MasterKey::from_bytes([7u8; 32]));
    (daemon, dir)
}

#[test]
fn extract_table_schema() {
    // The stub canned tree has no `tbl`, so extract returns an empty
    // record set — but the request must still succeed and stamp the
    // current token.
    let (d, _dir) = make_daemon();
    let s = d.session_open(None).unwrap();
    let p = d.open(&s.session_id, "https://example.com").unwrap();
    let v = d.view(&s.session_id, &p.page_id, false).unwrap();
    let ex = d
        .extract(&s.session_id, &p.page_id, "table", v.token)
        .unwrap();
    assert_eq!(ex.token, v.token);
    assert!(ex.records.is_empty());
}

#[test]
fn extract_unknown_schema_errors() {
    let (d, _dir) = make_daemon();
    let s = d.session_open(None).unwrap();
    let p = d.open(&s.session_id, "https://x").unwrap();
    let v = d.view(&s.session_id, &p.page_id, false).unwrap();
    assert!(d
        .extract(&s.session_id, &p.page_id, "nope", v.token)
        .is_err());
}

#[test]
fn mark_persists_in_store_and_audit() {
    let (d, _dir) = make_daemon();
    let s = d.session_open(None).unwrap();
    let p = d.open(&s.session_id, "https://x").unwrap();
    let v = d.view(&s.session_id, &p.page_id, false).unwrap();
    let m = d
        .mark(&s.session_id, &p.page_id, Ref(4), "submit-btn", v.token)
        .unwrap();
    assert!(m.mark_id.starts_with("m_"));
    let rows = d.audit_log(&vs_store::ActionFilter::default()).unwrap();
    assert!(rows.iter().any(|r| r.primitive == "vs_mark"));
}

#[test]
fn annotate_targets() {
    let (d, _dir) = make_daemon();
    let s = d.session_open(None).unwrap();
    d.annotate(
        &s.session_id,
        &AnnotationTarget::Ref(7),
        "color",
        Some("blue"),
    )
    .unwrap();
    d.annotate(
        &s.session_id,
        &AnnotationTarget::Page,
        "owner",
        Some("frane"),
    )
    .unwrap();
    let rows = d.audit_log(&vs_store::ActionFilter::default()).unwrap();
    assert_eq!(
        rows.iter().filter(|r| r.primitive == "vs_annotate").count(),
        2
    );
}

#[test]
fn log_filters_by_group() {
    let (d, _dir) = make_daemon();
    let s = d.session_open(None).unwrap();
    let p = d.open(&s.session_id, "https://x").unwrap();
    let v = d.view(&s.session_id, &p.page_id, false).unwrap();
    let h = vs_daemon::tokens::args_hash("vs_act", &["click".into()]);
    d.act(ActCall {
        session_id: s.session_id.clone(),
        page_id: p.page_id.clone(),
        target: engine::ActTarget::Ref(Ref(4)),
        action: engine::Action::Click,
        before_token: v.token,
        args_hash: h,
        args_redacted: "click 4".into(),
        mode: vs_engine_webkit::engine::InputMode::Careful,
        group_label: Some("login-flow".into()),
    })
    .unwrap();
    let only_group = d
        .log(&s.session_id, None, Some("login-flow".into()), None, None)
        .unwrap();
    assert_eq!(only_group.rows.len(), 1);
    assert_eq!(only_group.rows[0].primitive, "vs_act");
}

#[test]
fn skill_list_empty_dir_is_ok() {
    let (d, _dir) = make_daemon();
    let s = d.session_open(None).unwrap();
    // skills_dir is the temp dir's `skills/` which doesn't exist; the
    // primitive must succeed with empty.
    let listed = d.skill_list(&s.session_id).unwrap();
    assert!(listed.names.is_empty());
}

#[test]
fn skill_show_missing_skill_errors() {
    let (d, _dir) = make_daemon();
    let s = d.session_open(None).unwrap();
    let err = d.skill_show(&s.session_id, "no-such").unwrap_err();
    assert!(matches!(err, vs_daemon::DaemonError::BadRequest(_)));
}

#[test]
fn skill_show_returns_body() {
    let (d, dir) = make_daemon();
    let skills = dir.path().join("skills/responsive-review");
    std::fs::create_dir_all(&skills).unwrap();
    std::fs::write(skills.join("SKILL.md"), "# responsive review\n").unwrap();
    let s = d.session_open(None).unwrap();
    let shown = d.skill_show(&s.session_id, "responsive-review").unwrap();
    assert!(shown.body.contains("responsive review"));
}

#[test]
fn capture_writes_a_png() {
    let (d, _dir) = make_daemon();
    let s = d.session_open(None).unwrap();
    let p = d.open(&s.session_id, "https://x").unwrap();
    let cap = d
        .capture(&s.session_id, &p.page_id, CaptureScope::Viewport)
        .unwrap();
    assert!(cap.path.exists());
    let bytes = std::fs::read(&cap.path).unwrap();
    assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
}

#[test]
fn viewport_rebaselines_view() {
    let (d, _dir) = make_daemon();
    let s = d.session_open(None).unwrap();
    let p = d.open(&s.session_id, "https://x").unwrap();
    let _v0 = d.view(&s.session_id, &p.page_id, false).unwrap();
    let vp = d
        .viewport(&s.session_id, &p.page_id, Viewport::MOBILE)
        .unwrap();
    assert_eq!(vp.warnings.len(), 1);
    assert_eq!(
        vp.warnings[0].code,
        vs_protocol::WarningCode::ViewportChanged
    );
    // The next view should be a fresh full re-baseline.
    let v_after = d.view(&s.session_id, &p.page_id, false).unwrap();
    assert!(matches!(v_after.form, vs_daemon::ViewForm::Full(_)));
}

#[test]
fn layout_returns_one_box_per_ref() {
    let (d, _dir) = make_daemon();
    let s = d.session_open(None).unwrap();
    let p = d.open(&s.session_id, "https://x").unwrap();
    let _v = d.view(&s.session_id, &p.page_id, false).unwrap();
    let lay = d
        .layout(&s.session_id, &p.page_id, vec![Ref(2), Ref(4)])
        .unwrap();
    assert_eq!(lay.boxes.len(), 2);
}

#[test]
fn auth_save_load_round_trip() {
    let (d, _dir) = make_daemon();
    let s = d.session_open(None).unwrap();
    let p = d.open(&s.session_id, "https://github.com").unwrap();
    let _v = d.view(&s.session_id, &p.page_id, false).unwrap();
    d.auth_save(&s.session_id, &p.page_id, "github").unwrap();
    let listed = d.auth_list(&s.session_id).unwrap();
    assert_eq!(listed.names, vec!["github"]);
    let loaded = d.auth_load(&s.session_id, &p.page_id, "github").unwrap();
    assert_eq!(loaded.warnings.len(), 1);
    assert_eq!(
        loaded.warnings[0].code,
        vs_protocol::WarningCode::AuthLoaded
    );
    d.auth_clear(&s.session_id, "github").unwrap();
    assert!(d.auth_list(&s.session_id).unwrap().names.is_empty());
}

#[test]
fn auth_save_without_master_key_errors() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("state.db")).unwrap();
    let runtime =
        EngineRuntime::spawn(|| Ok(Box::new(TestEngine::new()) as Box<dyn Engine>)).unwrap();
    // No master key configured.
    let d = Daemon::new(store, Arc::new(runtime));
    let s = d.session_open(None).unwrap();
    let p = d.open(&s.session_id, "https://x").unwrap();
    let _v = d.view(&s.session_id, &p.page_id, false).unwrap();
    let err = d.auth_save(&s.session_id, &p.page_id, "x").unwrap_err();
    assert!(matches!(err, vs_daemon::DaemonError::BadRequest(_)));
}

#[test]
fn record_plumbing_validates_page_and_dedupes() {
    // Frame encoding is covered by vs-record's own unit tests (it needs
    // real page-sized frames; TestEngine only stubs a 1x1 PNG). Here we
    // pin the daemon plumbing: page validation, one-recording-per-page,
    // and teardown so a page can be recorded again after stop.
    let (d, _dir) = make_daemon();
    let s = d.session_open(None).unwrap();
    let p = d.open(&s.session_id, "https://x").unwrap();

    // An unaddressable page cannot be recorded.
    assert!(d
        .record_start(&s.session_id, "no-such-page", 10, 1280)
        .is_err());
    // Stopping a page that is not recording is a BadRequest.
    let err = d.record_stop(&p.page_id).unwrap_err();
    assert!(matches!(err, vs_daemon::DaemonError::BadRequest(_)));

    // First start registers the recorder; a second is rejected.
    let out = d.record_start(&s.session_id, &p.page_id, 10, 1280).unwrap();
    assert!(out.ends_with(format!("rec-{}.ivf", p.page_id).as_str()));
    let err = d
        .record_start(&s.session_id, &p.page_id, 10, 1280)
        .unwrap_err();
    assert!(matches!(err, vs_daemon::DaemonError::BadRequest(_)));

    // Stop tears the recorder down (the stub-frame encode may error;
    // we only assert the handle is released).
    let _ = d.record_stop(&p.page_id);
    // Which means the page can be recorded again.
    let _ = d.record_start(&s.session_id, &p.page_id, 10, 1280).unwrap();
    let _ = d.record_stop(&p.page_id);
}
