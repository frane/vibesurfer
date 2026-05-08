//! End-to-end integration tests against a real on-disk SQLite file.
//!
//! Lives at `tests/` so the workspace integration harness (and CI's
//! `--test '*'`) exercise the actual file-based code path: WAL journal,
//! foreign keys, durable migration history.

use std::fs;

use vs_store::{
    auth::MasterKey, ActionFilter, ActionInsert, AnnotationTarget, Store, StoreError, StoredRef,
    IDEMPOTENCY_TTL_SECS,
};

#[test]
fn full_lifecycle_against_disk() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("state.db");

    {
        let mut store = Store::open(&db).unwrap();
        store.create_session("sess", Some("default")).unwrap();
        store
            .create_page("page", "sess", "https://example.com")
            .unwrap();
        store
            .update_page_token("page", "0123456789abcdef", "domhash", Some("Example"))
            .unwrap();
        store
            .record_ref(&StoredRef {
                session_id: "sess".into(),
                page_id: "page".into(),
                r: 1,
                dom_path: "html".into(),
                role: "doc".into(),
                content_hash: "hash".into(),
                created_at: vs_store::epoch_secs(),
                retired_at: None,
            })
            .unwrap();
        store
            .create_mark("m1", "sess", "page", "checkout", "html>main", None, None)
            .unwrap();
        store
            .add_annotation(
                "a1",
                &AnnotationTarget::Mark("checkout".into()),
                "owner",
                Some("frane"),
            )
            .unwrap();
        store
            .record_action(&ActionInsert {
                session_id: "sess".into(),
                page_id: Some("page".into()),
                primitive: "vs_open".into(),
                args_redacted: "https://example.com".into(),
                args_hash: "h-open".into(),
                before_token: None,
                after_token: Some("0123456789abcdef".into()),
                idempotency_hit: false,
                result_summary: Some("ok".into()),
                latency_ms: 42,
                group_label: Some("login-flow".into()),
                started_at: vs_store::epoch_secs(),
                finished_at: vs_store::epoch_secs(),
                error_code: None,
            })
            .unwrap();
        let key = MasterKey::from_bytes([9u8; 32]);
        store.save_auth("ex", &key, b"cookie=session=abc").unwrap();
    }

    // Reopen against the same path: migrations must not double-apply,
    // and prior state must be visible.
    let store = Store::open(&db).unwrap();
    assert!(store.get_session("sess").unwrap().is_some());
    assert_eq!(store.list_pages("sess").unwrap().len(), 1);
    assert_eq!(store.list_refs("sess", "page").unwrap().len(), 1);
    assert_eq!(store.list_marks("sess").unwrap().len(), 1);
    assert_eq!(
        store
            .list_annotations(&AnnotationTarget::Mark("checkout".into()))
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        store.list_actions(&ActionFilter::default()).unwrap().len(),
        1
    );
    let key = MasterKey::from_bytes([9u8; 32]);
    let mut store2 = Store::open(&db).unwrap();
    let plain = store2.load_auth("ex", &key).unwrap();
    assert_eq!(plain, b"cookie=session=abc".to_vec());

    // WAL files exist alongside the main db.
    let entries: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
        .collect();
    assert!(entries.iter().any(|n| n == "state.db"));
    assert!(entries
        .iter()
        .any(|n| n.contains("wal") || n.contains("shm")));
}

#[test]
fn group_filter_and_idempotency_hit_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("state.db");
    let mut store = Store::open(&db).unwrap();
    store.create_session("sess", None).unwrap();

    let now = vs_store::epoch_secs();
    let row = ActionInsert {
        session_id: "sess".into(),
        page_id: Some("page".into()),
        primitive: "vs_act".into(),
        args_redacted: "click 7".into(),
        args_hash: "h7".into(),
        before_token: Some("tA".into()),
        after_token: Some("tB".into()),
        idempotency_hit: false,
        result_summary: None,
        latency_ms: 5,
        group_label: Some("login".into()),
        started_at: now - 1,
        finished_at: now - 1,
        error_code: None,
    };
    store.record_action(&row).unwrap();

    // Second call within window — record as a hit.
    let mut hit_row = row.clone();
    hit_row.idempotency_hit = true;
    hit_row.started_at = now;
    hit_row.finished_at = now;
    store.record_action(&hit_row).unwrap();

    let group_filter = ActionFilter {
        group_label: Some("login".into()),
        ..ActionFilter::default()
    };
    assert_eq!(store.list_actions(&group_filter).unwrap().len(), 2);

    // The idempotency lookup should still return the *original* (the
    // one that actually executed) — not the hit row.
    let cached = store
        .lookup_idempotent("page", "tA", "h7", now, IDEMPOTENCY_TTL_SECS)
        .unwrap()
        .expect("idempotent");
    assert!(!cached.idempotency_hit);
}

#[test]
fn missing_session_close_errors_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("state.db");
    let mut store = Store::open(&db).unwrap();
    let err = store.close_session("nope").unwrap_err();
    assert!(matches!(err, StoreError::NotFound { .. }));
}
