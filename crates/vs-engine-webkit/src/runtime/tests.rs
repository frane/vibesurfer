use std::path::PathBuf;
use std::time::Duration;

use vs_protocol::{Node, Ref, Role, Tree};

use super::*;
use crate::engine::{
    ActTarget, Action, AuthBlob, CaptureScope, EngineCapabilities, LayoutBox, Viewport,
    WaitCondition,
};

/// Minimal in-process `Engine` impl used only to exercise the
/// runtime's spawn / dispatch / shutdown plumbing. Lives in the
/// same `cfg(test)` block as the tests so it can never be reached
/// from production code.
#[derive(Default)]
struct TestEngine {
    next_handle: u64,
    last_url: String,
    /// When set, opens stay pending until the flag turns true.
    slow_open: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
}

impl Engine for TestEngine {
    fn open(&mut self, url: &str, _store: &DataStore) -> EngineResult<PageHandle> {
        self.next_handle += 1;
        self.last_url = url.to_string();
        Ok(PageHandle(self.next_handle))
    }
    fn open_start(&mut self, url: &str, store: &DataStore) -> EngineResult<OpenStart> {
        if self.slow_open.is_some() {
            return Ok(OpenStart::Pending(1));
        }
        self.open(url, store).map(OpenStart::Done)
    }
    fn open_poll(&mut self, _ticket: u64) -> Option<EngineResult<PageHandle>> {
        let done = self.slow_open.as_ref()?;
        done.load(std::sync::atomic::Ordering::SeqCst)
            .then_some(Ok(PageHandle(7)))
    }
    fn navigate(&mut self, _page: PageHandle, url: &str) -> EngineResult<()> {
        self.last_url = url.to_string();
        Ok(())
    }
    fn enable_webauthn(&mut self, _page: PageHandle) -> EngineResult<()> {
        Ok(())
    }
    fn add_init_script(&mut self, _page: PageHandle, _source: &str) -> EngineResult<()> {
        Ok(())
    }
    fn close(&mut self, _page: PageHandle) -> EngineResult<()> {
        Ok(())
    }
    fn snapshot(&mut self, _page: PageHandle) -> EngineResult<Tree> {
        Ok(Tree::from_root(Node::leaf(
            Ref(1),
            Role::Doc,
            &self.last_url,
        )))
    }
    fn act(
        &mut self,
        _: PageHandle,
        _: ActTarget,
        _: Action,
        _: crate::engine::InputMode,
    ) -> EngineResult<()> {
        Ok(())
    }
    fn wait(&mut self, _: PageHandle, _: WaitCondition, _: Duration) -> EngineResult<()> {
        Ok(())
    }
    fn capture(&mut self, _: PageHandle, _: CaptureScope) -> EngineResult<PathBuf> {
        Ok(PathBuf::from("/tmp/test.png"))
    }
    fn layout(&mut self, _: PageHandle, refs: &[Ref]) -> EngineResult<Vec<LayoutBox>> {
        Ok(refs
            .iter()
            .map(|r| LayoutBox {
                r: *r,
                x: 0.0,
                y: 0.0,
                width: 1.0,
                height: 1.0,
                visible: true,
                z_index: 0,
            })
            .collect())
    }
    fn set_viewport(&mut self, _: PageHandle, _: Viewport) -> EngineResult<()> {
        Ok(())
    }
    fn save_auth(&mut self, _: PageHandle) -> EngineResult<AuthBlob> {
        Ok(AuthBlob {
            bytes: self.last_url.as_bytes().to_vec(),
        })
    }
    fn load_auth(&mut self, _: PageHandle, _: &AuthBlob) -> EngineResult<()> {
        Ok(())
    }
    fn capabilities(&self) -> EngineCapabilities {
        EngineCapabilities {
            renders: false,
            honors_viewport: false,
            measures_layout: false,
            persists_auth: false,
            inspector_console: false,
            inspector_network: false,
            inspector_cookie_events: false,
            name: "test",
            version: "runtime-tests",
        }
    }
}

fn spawn_test_runtime() -> EngineRuntime {
    EngineRuntime::spawn(|| Ok(Box::new(TestEngine::default()) as Box<dyn Engine>)).expect("spawn")
}

#[test]
fn spawn_and_shutdown_cleanly() {
    let mut rt = spawn_test_runtime();
    rt.shutdown();
    rt.shutdown();
}

/// An open whose load is still running does not hold up other
/// calls, and its caller gets the page once the load ends.
#[test]
fn a_pending_open_does_not_hold_up_other_calls() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    let loaded = Arc::new(AtomicBool::new(false));
    let engine = TestEngine {
        slow_open: Some(loaded.clone()),
        ..TestEngine::default()
    };
    let (rt, mut driver) = EngineRuntime::dispatcher(Box::new(engine));
    let rt = Arc::new(rt);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut drive_until = |done: &dyn Fn() -> bool| {
        while !done() && std::time::Instant::now() < deadline {
            let _ = driver.tick();
            std::thread::yield_now();
        }
    };

    let r = rt.clone();
    let opener = std::thread::spawn(move || r.open("about:blank", &DataStore::Shared));
    let r = rt.clone();
    let other = std::thread::spawn(move || r.capabilities());
    drive_until(&|| other.is_finished());
    assert!(other.join().unwrap().is_ok(), "served while the open waits");
    assert!(!opener.is_finished(), "the open waits for its load");

    loaded.store(true, Ordering::SeqCst);
    drive_until(&|| opener.is_finished());
    assert_eq!(opener.join().unwrap().unwrap(), PageHandle(7));
}

#[test]
fn dispatch_blocks_until_reply() {
    let rt = spawn_test_runtime();
    let caps = rt.capabilities().unwrap();
    assert_eq!(caps.name, "test");
}

#[test]
fn engine_construction_failure_reported() {
    let err = EngineRuntime::spawn(|| Err::<Box<dyn Engine>, _>(EngineError::Other("nope".into())))
        .unwrap_err();
    assert!(matches!(err, EngineError::Other(_)));
}

#[test]
fn calls_after_drop_error_with_closed() {
    let mut rt = spawn_test_runtime();
    rt.shutdown();
    let err = rt.capabilities().unwrap_err();
    assert!(matches!(err, EngineError::Closed));
}

/// Cover the round-trip path that used to live in
/// `tests/runtime_round_trip.rs`. The Engine impl is intentionally
/// trivial — this test verifies the dispatch channel, not engine
/// behavior.
#[test]
fn full_primitive_sequence_via_runtime() {
    let rt = spawn_test_runtime();
    let page = rt
        .open("https://example.com/login", &DataStore::Shared)
        .unwrap();
    rt.wait(page, WaitCondition::Stable, Duration::from_millis(0))
        .unwrap();
    let tree = rt.snapshot(page).unwrap();
    assert!(tree.roots[0].label.contains("https://example.com/login"));
    rt.act(
        page,
        ActTarget::Ref(Ref(3)),
        Action::Fill { value: "x".into() },
        crate::engine::InputMode::Careful,
    )
    .unwrap();
    let auth = rt.save_auth(page).unwrap();
    rt.load_auth(page, auth).unwrap();
    rt.close(page).unwrap();
    rt.close(page).unwrap();
}

#[test]
fn dispatch_serializes_calls() {
    let rt = spawn_test_runtime();
    let mut handles = Vec::new();
    for i in 0..32 {
        handles.push(
            rt.open(&format!("https://example.com/{i}"), &DataStore::Shared)
                .unwrap(),
        );
    }
    let mut sorted = handles.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), handles.len());
}
