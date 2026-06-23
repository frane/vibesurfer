//! Engine thread + channel bridge.
//!
//! WebKit on either platform must be driven from the thread that owns
//! its run loop (`GMainLoop` on Linux, `CFRunLoop` on macOS). The
//! daemon's Tokio workers can not call WebKit directly. [`EngineRuntime`]
//! owns a dedicated OS thread, lets the platform construct the engine
//! on that thread, and dispatches every call from the daemon over an
//! `mpsc` channel.
//!
//! Every wrapper method on [`EngineRuntime`] (e.g. [`EngineRuntime::open`])
//! is synchronous and blocks until the engine thread replies. The
//! daemon wraps these in `tokio::task::spawn_blocking` to keep them
//! off Tokio's worker threads.

#![allow(clippy::result_unit_err, clippy::must_use_candidate)]

use std::path::PathBuf;
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use vs_protocol::{Ref, Tree};

use crate::engine::{
    ActTarget, Action, AuthBlob, CaptureScope, Engine, EngineCapabilities, EngineError,
    EngineResult, LayoutBox, PageHandle, Viewport, WaitCondition,
};

/// One unit of work for the engine thread.
type Job = Box<dyn FnOnce(&mut dyn Engine) + Send>;

/// Extract a human-readable message from a `catch_unwind` payload.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    let detail = payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".to_string());
    format!("engine panicked: {detail}")
}

/// Owns the engine thread and exposes a synchronous facade.
///
/// Drop semantics: dropping the runtime closes the command channel,
/// which causes the engine thread to exit its loop and drop the
/// engine. The destructor joins the thread.
pub struct EngineRuntime {
    sender: Option<mpsc::Sender<Job>>,
    handle: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for EngineRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineRuntime")
            .field("running", &self.sender.is_some())
            .finish_non_exhaustive()
    }
}

impl EngineRuntime {
    /// Spawn the engine thread and construct the engine *on that thread*
    /// via `make`. This is the only correct construction point: the
    /// platform run loop must be initialized on the same thread that
    /// later drives WebKit.
    ///
    /// Returns once the engine thread has signaled it is ready.
    pub fn spawn<F>(make: F) -> EngineResult<Self>
    where
        F: FnOnce() -> EngineResult<Box<dyn Engine>> + Send + 'static,
    {
        let (tx, rx) = mpsc::channel::<Job>();
        let (ready_tx, ready_rx) = mpsc::sync_channel::<EngineResult<()>>(1);

        let handle = thread::Builder::new()
            .name("vibesurfer-engine".into())
            .spawn(move || {
                let mut engine = match make() {
                    Ok(e) => {
                        let _ = ready_tx.send(Ok(()));
                        e
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };

                while let Ok(job) = rx.recv() {
                    job(engine.as_mut());
                }
                // Channel closed: drop the engine on this thread and exit.
                drop(engine);
            })
            .map_err(|e| EngineError::Other(format!("spawn engine thread: {e}")))?;

        match ready_rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                let _ = handle.join();
                return Err(e);
            }
            Err(_) => {
                let _ = handle.join();
                return Err(EngineError::Crashed);
            }
        }

        Ok(Self {
            sender: Some(tx),
            handle: Some(handle),
        })
    }

    /// Construct a runtime whose engine runs on the *caller'\''s* thread.
    /// No internal thread is spawned. The returned [`MainThreadDispatcher`]
    /// drives the engine: call [`MainThreadDispatcher::tick`] in the
    /// caller'\''s loop to drain one queued job, or [`MainThreadDispatcher::run_until`]
    /// to block until a stop flag fires.
    ///
    /// Use this when the engine is bound to a specific OS thread (e.g.
    /// the macOS Cocoa main thread, where `WKWebView` must run).
    pub fn dispatcher(engine: Box<dyn Engine>) -> (Self, MainThreadDispatcher) {
        let (tx, rx) = mpsc::channel::<Job>();
        let runtime = Self {
            sender: Some(tx),
            handle: None,
        };
        let dispatcher = MainThreadDispatcher { engine, rx };
        (runtime, dispatcher)
    }

    /// Cleanly shut down: close the channel and join the thread.
    /// Idempotent — calling shutdown twice is a no-op.
    pub fn shutdown(&mut self) {
        drop(self.sender.take());
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }

    fn dispatch<R, F>(&self, f: F) -> EngineResult<R>
    where
        F: FnOnce(&mut dyn Engine) -> EngineResult<R> + Send + 'static,
        R: Send + 'static,
    {
        let sender = self.sender.as_ref().ok_or(EngineError::Closed)?;
        let (reply_tx, reply_rx) = mpsc::sync_channel::<EngineResult<R>>(1);
        let job: Job = Box::new(move |engine| {
            // Isolate a panicking engine op: on macOS the job runs on
            // the Cocoa main thread inside an Obj-C run-loop frame, so an
            // unwinding panic is undefined behavior that can take the
            // whole daemon down ("could not connect to the server" on the
            // next request). Catch it here and hand back a clean, message-
            // bearing error instead — far better than the bare
            // `ENGINE_CRASH` (empty-args) the dropped reply channel would
            // otherwise produce.
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(engine)))
                .unwrap_or_else(|payload| Err(EngineError::Other(panic_message(&payload))));
            let _ = reply_tx.send(result);
        });
        sender.send(job).map_err(|_| EngineError::Closed)?;
        reply_rx.recv().map_err(|_| EngineError::Crashed)?
    }

    // -----------------------------------------------------------------
    // Synchronous facade for every Engine method.
    // -----------------------------------------------------------------

    pub fn open(&self, url: &str) -> EngineResult<PageHandle> {
        let url = url.to_string();
        self.dispatch(move |e| e.open(&url))
    }

    pub fn close(&self, page: PageHandle) -> EngineResult<()> {
        self.dispatch(move |e| e.close(page))
    }

    pub fn snapshot(&self, page: PageHandle) -> EngineResult<Tree> {
        self.dispatch(move |e| e.snapshot(page))
    }

    pub fn act(&self, page: PageHandle, target: ActTarget, action: Action) -> EngineResult<()> {
        self.dispatch(move |e| e.act(page, target, action))
    }

    pub fn wait(
        &self,
        page: PageHandle,
        cond: WaitCondition,
        budget: Duration,
    ) -> EngineResult<()> {
        self.dispatch(move |e| e.wait(page, cond, budget))
    }

    pub fn capture(&self, page: PageHandle, scope: CaptureScope) -> EngineResult<PathBuf> {
        self.dispatch(move |e| e.capture(page, scope))
    }

    pub fn layout(&self, page: PageHandle, refs: Vec<Ref>) -> EngineResult<Vec<LayoutBox>> {
        self.dispatch(move |e| e.layout(page, &refs))
    }

    pub fn set_viewport(&self, page: PageHandle, viewport: Viewport) -> EngineResult<()> {
        self.dispatch(move |e| e.set_viewport(page, viewport))
    }

    pub fn save_auth(&self, page: PageHandle) -> EngineResult<AuthBlob> {
        self.dispatch(move |e| e.save_auth(page))
    }

    pub fn load_auth(&self, page: PageHandle, blob: AuthBlob) -> EngineResult<()> {
        self.dispatch(move |e| e.load_auth(page, &blob))
    }

    pub fn console_entries(
        &self,
        page: PageHandle,
    ) -> EngineResult<Vec<crate::inspector::ConsoleEntry>> {
        self.dispatch(move |e| e.console_entries(page))
    }

    pub fn network_entries(
        &self,
        page: PageHandle,
    ) -> EngineResult<Vec<crate::inspector::NetworkEntry>> {
        self.dispatch(move |e| e.network_entries(page))
    }

    pub fn request_detail(
        &self,
        page: PageHandle,
        seq: u64,
    ) -> EngineResult<Option<crate::inspector::RequestDetail>> {
        self.dispatch(move |e| e.request_detail(page, seq))
    }

    pub fn eval_js(
        &self,
        page: PageHandle,
        expr: &str,
    ) -> EngineResult<crate::inspector::EvalResult> {
        let expr = expr.to_string();
        self.dispatch(move |e| e.eval_js(page, &expr))
    }

    pub fn storage(
        &self,
        page: PageHandle,
        scope: crate::inspector::StorageScope,
    ) -> EngineResult<Vec<crate::inspector::StorageEntry>> {
        self.dispatch(move |e| e.storage(page, scope))
    }

    pub fn cookie_events(
        &self,
        page: PageHandle,
    ) -> EngineResult<Vec<crate::inspector::CookieEvent>> {
        self.dispatch(move |e| e.cookie_events(page))
    }

    pub fn cursor_op(
        &self,
        page: PageHandle,
        op: crate::engine::CursorOp,
        mode: crate::engine::InputMode,
    ) -> EngineResult<()> {
        self.dispatch(move |e| e.cursor_op(page, op, mode))
    }

    pub fn scripts(&self, page: PageHandle) -> EngineResult<Vec<crate::inspector::ScriptEntry>> {
        self.dispatch(move |e| e.scripts(page))
    }

    pub fn script_source(
        &self,
        page: PageHandle,
        seq: u64,
    ) -> EngineResult<Option<crate::inspector::ScriptSource>> {
        self.dispatch(move |e| e.script_source(page, seq))
    }

    pub fn dom(
        &self,
        page: PageHandle,
        r: vs_protocol::Ref,
        extra_props: Vec<String>,
    ) -> EngineResult<Option<crate::inspector::DomDetail>> {
        self.dispatch(move |e| e.dom(page, r, &extra_props))
    }

    pub fn performance(
        &self,
        page: PageHandle,
    ) -> EngineResult<crate::inspector::PerformanceMetrics> {
        self.dispatch(move |e| e.performance(page))
    }

    pub fn capabilities(&self) -> EngineResult<EngineCapabilities> {
        self.dispatch(|e| Ok(e.capabilities()))
    }
}

impl Drop for EngineRuntime {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Drives an engine that lives on the caller'\''s thread (typically the
/// OS main thread on macOS, since `WKWebView` is pinned there). The
/// driver owns the engine and a receiver for jobs sent by the runtime
/// handle that the daemon holds.
pub struct MainThreadDispatcher {
    engine: Box<dyn Engine>,
    rx: mpsc::Receiver<Job>,
}

impl MainThreadDispatcher {
    /// Drain one job if available. Returns `Ok(true)` if a job was
    /// executed, `Ok(false)` if the queue is currently empty,
    /// `Err(())` if the channel is closed (the runtime was dropped —
    /// the dispatcher should exit its loop).
    pub fn tick(&mut self) -> Result<bool, ()> {
        match self.rx.try_recv() {
            Ok(job) => {
                job(self.engine.as_mut());
                Ok(true)
            }
            Err(mpsc::TryRecvError::Empty) => Ok(false),
            Err(mpsc::TryRecvError::Disconnected) => Err(()),
        }
    }

    /// Block until a job arrives or the channel is closed; execute one
    /// job. Returns `Ok(true)` after running, `Err(())` on closed.
    pub fn tick_blocking(&mut self) -> Result<bool, ()> {
        match self.rx.recv() {
            Ok(job) => {
                job(self.engine.as_mut());
                Ok(true)
            }
            Err(_) => Err(()),
        }
    }
}

#[cfg(test)]
mod tests {
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
    }

    impl Engine for TestEngine {
        fn open(&mut self, url: &str) -> EngineResult<PageHandle> {
            self.next_handle += 1;
            self.last_url = url.to_string();
            Ok(PageHandle(self.next_handle))
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
        fn act(&mut self, _: PageHandle, _: ActTarget, _: Action) -> EngineResult<()> {
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
        EngineRuntime::spawn(|| Ok(Box::new(TestEngine::default()) as Box<dyn Engine>))
            .expect("spawn")
    }

    #[test]
    fn spawn_and_shutdown_cleanly() {
        let mut rt = spawn_test_runtime();
        rt.shutdown();
        rt.shutdown();
    }

    #[test]
    fn dispatch_blocks_until_reply() {
        let rt = spawn_test_runtime();
        let caps = rt.capabilities().unwrap();
        assert_eq!(caps.name, "test");
    }

    #[test]
    fn engine_construction_failure_reported() {
        let err =
            EngineRuntime::spawn(|| Err::<Box<dyn Engine>, _>(EngineError::Other("nope".into())))
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
        let page = rt.open("https://example.com/login").unwrap();
        rt.wait(page, WaitCondition::Stable, Duration::from_millis(0))
            .unwrap();
        let tree = rt.snapshot(page).unwrap();
        assert!(tree.roots[0].label.contains("https://example.com/login"));
        rt.act(
            page,
            ActTarget::Ref(Ref(3)),
            Action::Fill { value: "x".into() },
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
            handles.push(rt.open(&format!("https://example.com/{i}")).unwrap());
        }
        let mut sorted = handles.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), handles.len());
    }
}
