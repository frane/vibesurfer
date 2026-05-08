//! End-to-end smoke test: drive a full primitive sequence
//! (open → snapshot → act → save_auth → close) through the
//! [`EngineRuntime`] + stub backend.

use std::time::Duration;

use vs_engine_webkit::{
    backend::stub::StubEngine, ActTarget, Action, Engine, EngineRuntime, WaitCondition,
};
use vs_protocol::Ref;

#[test]
fn full_primitive_sequence_via_runtime() {
    let rt =
        EngineRuntime::spawn(|| Ok(Box::new(StubEngine::new()) as Box<dyn Engine>)).expect("spawn");

    let page = rt.open("https://example.com/login").expect("open");
    rt.wait(page, WaitCondition::Stable, Duration::from_millis(0))
        .expect("wait stable");

    let tree = rt.snapshot(page).expect("snapshot");
    assert_eq!(tree.roots.len(), 1);
    assert!(tree.roots[0].label.contains("https://example.com/login"));

    rt.act(
        page,
        ActTarget::Ref(Ref(3)),
        Action::Fill {
            value: "frane@example.com".into(),
        },
    )
    .expect("fill");
    rt.act(page, ActTarget::Ref(Ref(4)), Action::Click)
        .expect("click");

    let auth = rt.save_auth(page).expect("save_auth");
    assert_eq!(auth.bytes, b"https://example.com/login");
    rt.load_auth(page, auth).expect("load_auth");

    let caps = rt.capabilities().expect("caps");
    assert_eq!(caps.name, "stub");

    rt.close(page).expect("close");
    // Closing twice is fine.
    rt.close(page).expect("close idempotent");
}

#[test]
fn dispatch_serializes_calls() {
    // The channel guarantees one-job-at-a-time on the engine thread,
    // so there is no need for the engine impl itself to be `Sync`. We
    // verify by issuing many calls in tight succession from the same
    // caller thread; the stub's stateful counter (`next_handle`) must
    // advance monotonically.
    let rt =
        EngineRuntime::spawn(|| Ok(Box::new(StubEngine::new()) as Box<dyn Engine>)).expect("spawn");
    let mut handles = Vec::new();
    for i in 0..32 {
        let p = rt.open(&format!("https://example.com/{i}")).unwrap();
        handles.push(p);
    }
    // All handles distinct.
    let mut sorted = handles.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), handles.len());
    for p in handles {
        rt.close(p).unwrap();
    }
}
