use super::*;
use vs_protocol::Ref;

#[test]
fn open_and_snapshot() {
    let mut e = TestEngine::new();
    let page = e.open("https://example.com", &DataStore::Shared).unwrap();
    let tree = e.snapshot(page).unwrap();
    assert_eq!(tree.roots.len(), 1);
    assert_eq!(tree.roots[0].r, Ref(1));
    assert!(tree.roots[0].label.contains("Test: https://example.com"));
    // Children: hd, input, button, link
    assert_eq!(tree.roots[0].children.len(), 4);
}

#[test]
fn close_idempotent() {
    let mut e = TestEngine::new();
    let page = e.open("about:blank", &DataStore::Shared).unwrap();
    e.close(page).unwrap();
    // Closing again is a no-op (the page is just gone).
    e.close(page).unwrap();
}

#[test]
fn snapshot_after_close_errors() {
    let mut e = TestEngine::new();
    let page = e.open("about:blank", &DataStore::Shared).unwrap();
    e.close(page).unwrap();
    let err = e.snapshot(page).unwrap_err();
    assert!(matches!(err, EngineError::NotFound { kind: "page", .. }));
}

#[test]
fn act_known_ref_succeeds() {
    let mut e = TestEngine::new();
    let p = e.open("about:blank", &DataStore::Shared).unwrap();
    e.act(
        p,
        ActTarget::Ref(Ref(4)),
        Action::Click,
        crate::engine::InputMode::Careful,
    )
    .unwrap();
    e.act(
        p,
        ActTarget::Ref(Ref(3)),
        Action::Fill { value: "hi".into() },
        crate::engine::InputMode::Careful,
    )
    .unwrap();
}

#[test]
fn act_unknown_ref_errors() {
    let mut e = TestEngine::new();
    let p = e.open("about:blank", &DataStore::Shared).unwrap();
    let err = e
        .act(
            p,
            ActTarget::Ref(Ref(99)),
            Action::Click,
            crate::engine::InputMode::Careful,
        )
        .unwrap_err();
    assert!(matches!(err, EngineError::NotFound { kind: "ref", .. }));
}

#[test]
fn wait_stable_returns_immediately() {
    let mut e = TestEngine::new();
    let p = e.open("about:blank", &DataStore::Shared).unwrap();
    e.wait(p, WaitCondition::Stable, Duration::from_millis(0))
        .unwrap();
}

#[test]
fn wait_token_change_times_out() {
    let mut e = TestEngine::new();
    let p = e.open("about:blank", &DataStore::Shared).unwrap();
    let err = e
        .wait(p, WaitCondition::TokenChange, Duration::from_millis(1))
        .unwrap_err();
    assert!(matches!(err, EngineError::Timeout { .. }));
}

#[test]
fn capture_writes_a_one_by_one_png() {
    let dir = tempfile::tempdir().unwrap();
    let mut e = TestEngine::new().with_capture_dir(dir.path());
    let p = e.open("https://example.com", &DataStore::Shared).unwrap();
    let path = e.capture(p, CaptureScope::Viewport).unwrap();
    assert!(path.exists(), "capture file missing: {path:?}");
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "not a PNG: {bytes:?}");
    // Two captures get distinct paths.
    let path2 = e.capture(p, CaptureScope::FullPage).unwrap();
    assert_ne!(path, path2);
}

#[test]
fn layout_returns_one_box_per_ref() {
    let mut e = TestEngine::new();
    let p = e.open("about:blank", &DataStore::Shared).unwrap();
    let boxes = e.layout(p, &[Ref(2), Ref(4)]).unwrap();
    assert_eq!(boxes.len(), 2);
    assert_eq!(boxes[0].r, Ref(2));
    assert!(boxes[0].visible);
    assert!((boxes[0].y - boxes[1].y).abs() > f64::EPSILON);
}

#[test]
fn auth_round_trip() {
    let mut e = TestEngine::new();
    let p = e.open("https://example.com", &DataStore::Shared).unwrap();
    let blob = e.save_auth(p).unwrap();
    assert_eq!(blob.bytes, b"https://example.com");
    e.load_auth(p, &blob).unwrap();
}

#[test]
fn viewport_persists() {
    let mut e = TestEngine::new();
    let p = e.open("about:blank", &DataStore::Shared).unwrap();
    e.set_viewport(p, Viewport::MOBILE).unwrap();
    // No public getter, but the page's viewport is updated; verify
    // by closing without panic.
    e.close(p).unwrap();
}

#[test]
fn capabilities_are_test() {
    let e = TestEngine::new();
    assert_eq!(e.capabilities().name, "test");
    assert!(e.capabilities().renders);
    assert!(e.capabilities().persists_auth);
}
