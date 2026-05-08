//! Integration-style unit tests over [`Store`]. Each test exercises
//! one or more tables together; they share an in-memory fixture to
//! keep setup cheap.

use super::*;
use crate::auth::MasterKey;
use crate::types::{
    ActionFilter, ActionInsert, AnnotationTarget, SessionStatus, SkillEntry, StoredRef,
};
use crate::StoreError;

fn fresh_store() -> Store {
    Store::open_in_memory().unwrap()
}

#[test]
fn open_in_memory_runs_migrations() {
    let s = fresh_store();
    let actions = s.list_actions(&ActionFilter::default()).unwrap();
    assert!(actions.is_empty());
}

#[test]
fn session_lifecycle() {
    let mut s = fresh_store();
    let opened = s.create_session("sess-1", Some("default")).unwrap();
    assert_eq!(opened.status, SessionStatus::Open);
    let again = s.get_session("sess-1").unwrap().unwrap();
    assert_eq!(again, opened);
    let closed = s.close_session("sess-1").unwrap();
    assert_eq!(closed.status, SessionStatus::Closed);
    assert!(closed.closed_at.is_some());
    assert_eq!(s.list_sessions().unwrap().len(), 1);
    assert!(matches!(
        s.close_session("sess-1"),
        Err(StoreError::NotFound { .. })
    ));
}

#[test]
fn page_lifecycle() {
    let mut s = fresh_store();
    s.create_session("sess", None).unwrap();
    let p = s
        .create_page("page", "sess", "https://example.com")
        .unwrap();
    assert_eq!(p.url, "https://example.com");
    s.update_page_token("page", "abcdef0123456789", "deadbeef", Some("Title"))
        .unwrap();
    let again = s.get_page("page").unwrap().unwrap();
    assert_eq!(again.last_token.as_deref(), Some("abcdef0123456789"));
    assert_eq!(again.title.as_deref(), Some("Title"));
    s.close_page("page").unwrap();
    assert!(s.get_page("page").unwrap().unwrap().closed_at.is_some());
}

#[test]
fn refs_record_and_retire() {
    let mut s = fresh_store();
    s.create_session("sess", None).unwrap();
    s.create_page("page", "sess", "https://x").unwrap();
    let sr = StoredRef {
        session_id: "sess".into(),
        page_id: "page".into(),
        r: 7,
        dom_path: "html>body>main".into(),
        role: "btn".into(),
        content_hash: "0xabc".into(),
        created_at: epoch_secs(),
        retired_at: None,
    };
    s.record_ref(&sr).unwrap();
    s.retire_ref("sess", "page", 7).unwrap();
    let listed = s.list_refs("sess", "page").unwrap();
    assert_eq!(listed.len(), 1);
    assert!(listed[0].retired_at.is_some());
}

#[test]
fn marks_unique_per_session() {
    let mut s = fresh_store();
    s.create_session("sess", None).unwrap();
    s.create_page("page", "sess", "https://x").unwrap();
    s.create_mark("m1", "sess", "page", "checkout", "html>body", None, None)
        .unwrap();
    let dup = s.create_mark("m2", "sess", "page", "checkout", "html>body", None, None);
    assert!(matches!(dup, Err(StoreError::Conflict(_))));

    s.create_session("sess2", None).unwrap();
    s.create_page("page2", "sess2", "https://x").unwrap();
    s.create_mark("m3", "sess2", "page2", "checkout", "html>body", None, None)
        .unwrap();
}

#[test]
fn annotations_target_round_trip() {
    let mut s = fresh_store();
    s.add_annotation("a1", &AnnotationTarget::Ref(42), "color", Some("blue"))
        .unwrap();
    s.add_annotation(
        "a2",
        &AnnotationTarget::Mark("checkout".into()),
        "note",
        None,
    )
    .unwrap();
    let by_ref = s.list_annotations(&AnnotationTarget::Ref(42)).unwrap();
    assert_eq!(by_ref.len(), 1);
    assert_eq!(by_ref[0].value.as_deref(), Some("blue"));
    let by_mark = s
        .list_annotations(&AnnotationTarget::Mark("checkout".into()))
        .unwrap();
    assert_eq!(by_mark.len(), 1);
    assert!(by_mark[0].value.is_none());
}

#[test]
fn actions_audit_log() {
    let mut s = fresh_store();
    s.create_session("sess", None).unwrap();
    let now = epoch_secs();
    let id1 = s
        .record_action(&ActionInsert {
            session_id: "sess".into(),
            page_id: Some("page".into()),
            primitive: "vs_open".into(),
            args_redacted: "https://x".into(),
            args_hash: "h1".into(),
            before_token: None,
            after_token: Some("token1".into()),
            idempotency_hit: false,
            result_summary: None,
            latency_ms: 12,
            group_label: None,
            started_at: now,
            finished_at: now,
            error_code: None,
        })
        .unwrap();
    assert!(id1 > 0);
    let listed = s.list_actions(&ActionFilter::default()).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].primitive, "vs_open");
}

#[test]
fn idempotency_window() {
    let mut s = fresh_store();
    s.create_session("sess", None).unwrap();
    let now = epoch_secs();
    let row = ActionInsert {
        session_id: "sess".into(),
        page_id: Some("page".into()),
        primitive: "vs_act".into(),
        args_redacted: "click 7".into(),
        args_hash: "h-click7".into(),
        before_token: Some("t-pre".into()),
        after_token: Some("t-post".into()),
        idempotency_hit: false,
        result_summary: None,
        latency_ms: 8,
        group_label: None,
        started_at: now - 5,
        finished_at: now - 5,
        error_code: None,
    };
    s.record_action(&row).unwrap();
    let hit = s
        .lookup_idempotent("page", "t-pre", "h-click7", now, IDEMPOTENCY_TTL_SECS)
        .unwrap()
        .expect("idempotent hit");
    assert_eq!(hit.after_token.as_deref(), Some("t-post"));
    let miss = s
        .lookup_idempotent("page", "t-pre", "h-click7", now + 60, IDEMPOTENCY_TTL_SECS)
        .unwrap();
    assert!(miss.is_none());
    let mut errored = row.clone();
    errored.args_hash = "h-fail".into();
    errored.error_code = Some("TIMEOUT".into());
    s.record_action(&errored).unwrap();
    let miss2 = s
        .lookup_idempotent("page", "t-pre", "h-fail", now, IDEMPOTENCY_TTL_SECS)
        .unwrap();
    assert!(miss2.is_none());
}

#[test]
fn auth_blob_round_trip_via_store() {
    let mut s = fresh_store();
    let key = MasterKey::from_bytes([5u8; 32]);
    s.save_auth("github", &key, b"cookies=session_abc").unwrap();
    let plain = s.load_auth("github", &key).unwrap();
    assert_eq!(plain, b"cookies=session_abc".to_vec());
    let listed = s.list_auth().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name, "github");
    assert!(listed[0].last_used_at.is_some());
}

#[test]
fn auth_load_with_wrong_key_errors() {
    let mut s = fresh_store();
    let k1 = MasterKey::from_bytes([1u8; 32]);
    let k2 = MasterKey::from_bytes([2u8; 32]);
    s.save_auth("x", &k1, b"secret").unwrap();
    assert!(matches!(s.load_auth("x", &k2), Err(StoreError::Crypto(_))));
}

#[test]
fn auth_overwrite_replaces_blob() {
    let mut s = fresh_store();
    let k = MasterKey::from_bytes([3u8; 32]);
    s.save_auth("x", &k, b"v1").unwrap();
    s.save_auth("x", &k, b"v2").unwrap();
    assert_eq!(s.load_auth("x", &k).unwrap(), b"v2".to_vec());
}

#[test]
fn auth_delete() {
    let mut s = fresh_store();
    let k = MasterKey::from_bytes([4u8; 32]);
    s.save_auth("x", &k, b"v").unwrap();
    s.delete_auth("x").unwrap();
    assert!(s.list_auth().unwrap().is_empty());
    assert!(matches!(
        s.delete_auth("x"),
        Err(StoreError::NotFound { .. })
    ));
}

#[test]
fn skill_cache_round_trip() {
    let mut s = fresh_store();
    let entry = SkillEntry {
        name: "responsive-review".into(),
        version: "1.0.0".into(),
        sha: "deadbeef".into(),
        manifest: "{}".into(),
        last_used_at: Some(epoch_secs()),
    };
    s.upsert_skill(&entry).unwrap();
    let again = s.get_skill("responsive-review").unwrap().unwrap();
    assert_eq!(again, entry);
    let listed = s.list_skills().unwrap();
    assert_eq!(listed.len(), 1);
}
