//! macOS: `NSApplication` on the main thread with the real `WKWebView`
//! backend, tokio and the daemon on a worker thread.

use std::sync::Arc;

use anyhow::{Context as _, Result};
use vs_daemon::{server, Daemon};

use super::{
    init_tracing, install_panic_hook, instance_or_exit, resolve_master_key, run_stop,
    wait_terminate, ServeArgs,
};

/// Wakes the Cocoa main thread's run loop from a worker thread.
///
/// The engine's job queue is an `mpsc` channel, which a run loop cannot
/// wait on. Handing the daemon's workers a way to signal the loop lets
/// the main thread block on `runMode` until there is real work, instead
/// of waking on a short timer to poll for it.
///
/// This is a real `CFRunLoopSource`, not a bare `CFRunLoopWakeUp`.
/// `CFRunLoopWakeUp` alone only makes the loop re-evaluate its sources;
/// with nothing to process it goes straight back to sleep and
/// `runMode:beforeDate:` still does not return until the date passes
/// (measured: every engine hop took the full slice). A signalled
/// source has something to *handle*, which is what makes `runMode`
/// return promptly.
#[derive(Clone, Copy)]
struct MainRunLoopWaker {
    run_loop: *mut std::ffi::c_void,
    source: *mut std::ffi::c_void,
}

// SAFETY: `CFRunLoopSourceSignal` and `CFRunLoopWakeUp` are documented
// as safe to call from a thread other than the one running the loop.
// Both pointers refer to the process's main run loop and a source
// owned by it, which outlive every worker holding this waker.
unsafe impl Send for MainRunLoopWaker {}
unsafe impl Sync for MainRunLoopWaker {}

#[repr(C)]
struct CFRunLoopSourceContext {
    version: isize,
    info: *mut std::ffi::c_void,
    retain: *const std::ffi::c_void,
    release: *const std::ffi::c_void,
    copy_description: *const std::ffi::c_void,
    equal: *const std::ffi::c_void,
    hash: *const std::ffi::c_void,
    schedule: *const std::ffi::c_void,
    cancel: *const std::ffi::c_void,
    perform: Option<extern "C" fn(*mut std::ffi::c_void)>,
}

extern "C" {
    fn CFRunLoopGetCurrent() -> *mut std::ffi::c_void;
    fn CFRunLoopWakeUp(rl: *mut std::ffi::c_void);
    fn CFRunLoopSourceSignal(source: *mut std::ffi::c_void);
    fn CFRunLoopSourceCreate(
        allocator: *const std::ffi::c_void,
        order: isize,
        context: *mut CFRunLoopSourceContext,
    ) -> *mut std::ffi::c_void;
    fn CFRunLoopAddSource(
        rl: *mut std::ffi::c_void,
        source: *mut std::ffi::c_void,
        mode: *const std::ffi::c_void,
    );
    static kCFRunLoopDefaultMode: *const std::ffi::c_void;
}

/// The source's only job is to exist and be signalled — handling it is
/// what returns control to our loop, which then drains the real queue.
extern "C" fn waker_perform(_info: *mut std::ffi::c_void) {}

impl MainRunLoopWaker {
    /// Install a wake source on the calling thread's run loop. Must be
    /// called on the thread that will run the loop.
    fn install() -> Self {
        let mut ctx = CFRunLoopSourceContext {
            version: 0,
            info: std::ptr::null_mut(),
            retain: std::ptr::null(),
            release: std::ptr::null(),
            copy_description: std::ptr::null(),
            equal: std::ptr::null(),
            hash: std::ptr::null(),
            schedule: std::ptr::null(),
            cancel: std::ptr::null(),
            perform: Some(waker_perform),
        };
        unsafe {
            let run_loop = CFRunLoopGetCurrent();
            let source = CFRunLoopSourceCreate(std::ptr::null(), 0, &raw mut ctx);
            if !source.is_null() {
                CFRunLoopAddSource(run_loop, source, kCFRunLoopDefaultMode);
            }
            Self { run_loop, source }
        }
    }

    fn wake(self) {
        if self.source.is_null() || self.run_loop.is_null() {
            return;
        }
        unsafe {
            CFRunLoopSourceSignal(self.source);
            CFRunLoopWakeUp(self.run_loop);
        }
    }
}

#[allow(clippy::too_many_lines)]
pub fn run(args: &ServeArgs) -> Result<()> {
    use objc2::MainThreadMarker;
    use objc2_app_kit::NSApplication;
    use objc2_foundation::{NSDate, NSDefaultRunLoopMode, NSRunLoop};
    use vs_engine_webkit::{backend::webkit::WkBackend, Engine, EngineRuntime};

    if args.stop {
        return run_stop(&args.paths);
    }

    init_tracing();
    install_panic_hook();
    args.paths.ensure_root().context("ensure ~/.vibesurfer")?;
    let Some(_instance) = instance_or_exit(&args.paths)? else {
        return Ok(());
    };

    let mtm = MainThreadMarker::new()
        .ok_or_else(|| anyhow::anyhow!("vs serve must be invoked from the OS main thread"))?;
    // Initialize NSApp; required for WKWebView even though we don't run
    // the AppKit event loop directly.
    let _app = NSApplication::sharedApplication(mtm);

    let store = vs_store::Store::open(args.paths.db()).context("open state.db")?;
    let captures_dir = args.paths.captures();
    let downloads_dir = args.paths.downloads();
    let skills_dir = args.paths.root.join("skills");

    // Engine lives on this thread (the Cocoa main thread). Construct
    // the WkBackend here and hand it to `EngineRuntime::dispatcher`,
    // which gives us back a runtime handle (for the daemon) and a
    // dispatcher we drive in this thread's run loop.
    let backend = WkBackend::new(mtm).with_capture_dir(captures_dir.clone());
    let engine_box: Box<dyn Engine> = Box::new(backend);
    let (engine_runtime, mut dispatcher) = EngineRuntime::dispatcher(engine_box);
    // Let the run loop below sleep instead of polling. An mpsc send is
    // not a run-loop source, so without this the main thread has to
    // wake on a timer just to notice queued work — which cost ~1.5%
    // CPU at idle, forever, on a daemon doing nothing.
    // `CFRunLoopWakeUp` is the one CFRunLoop call documented as safe
    // from another thread, and its signal is a mach port message: if it
    // lands while the loop is between iterations it stays queued, so
    // the next `runMode` returns immediately rather than missing a job.
    let waker = MainRunLoopWaker::install();
    let engine_runtime = Arc::new(engine_runtime.with_waker(Box::new(move || waker.wake())));

    let mut daemon = Daemon::new(store, engine_runtime.clone())
        .with_captures_dir(captures_dir)
        .with_downloads_dir(downloads_dir)
        .with_intercept_ca_dir(args.paths.root.clone())
        .with_skills_dir(skills_dir);

    if let Some(k) = resolve_master_key(&args.paths) {
        daemon = daemon.with_master_key(k);
    }

    let socket = args.paths.socket();
    let pid_path = args.paths.pid_file();

    // Spawn the tokio runtime on a worker. It owns the daemon and the
    // socket server; ctrl-c on the worker triggers a graceful shutdown
    // by closing `shutdown_rx` and dropping the runtime, which closes
    // our engine channel and pops us out of the run-loop below.
    let server_thread = std::thread::Builder::new()
        .name("vs-daemon-tokio".into())
        .spawn(move || -> Result<()> {
            // Restore open sessions from state.db before serving —
            // a restart must not strand agents in WRONG_SESSION.
            let (rs, rp) = daemon.resurrect_sessions(std::time::Duration::from_secs(24 * 3600));
            if rs > 0 {
                tracing::info!("resurrected {rs} session(s) / {rp} page(s) from state.db");
            }
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(4)
                .enable_all()
                .build()
                .context("build tokio runtime")?;
            if let Err(e) = std::fs::write(&pid_path, std::process::id().to_string()) {
                tracing::warn!(?pid_path, error = %e, "write pid file");
            }
            rt.block_on(async move {
                let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
                let mut server =
                    tokio::spawn(async move { server::serve(daemon, socket, shutdown_rx).await });
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {
                        tracing::info!("ctrl-c received, shutting down");
                        let _ = shutdown_tx.send(());
                        if let Ok(Err(e)) = server.await {
                            tracing::error!(error = %e, "server task ended with error");
                        }
                    }
                    () = wait_terminate() => {
                        tracing::info!("SIGTERM received, shutting down");
                        let _ = shutdown_tx.send(());
                        if let Ok(Err(e)) = server.await {
                            tracing::error!(error = %e, "server task ended with error");
                        }
                    }
                    res = &mut server => {
                        // server::serve returned without an external
                        // signal — typically a bind failure. Surface it
                        // before the run loop exits.
                        match res {
                            Ok(Err(e)) => tracing::error!(
                                error = %e,
                                "server task failed before shutdown signal"
                            ),
                            Err(e) => tracing::error!(
                                error = %e,
                                "server task panicked"
                            ),
                            Ok(Ok(())) => {}
                        }
                    }
                }
            });
            let _ = std::fs::remove_file(&pid_path);
            // Dropping `rt` and the moved `engine_runtime` (held by the
            // daemon) closes the engine channel, which signals the main
            // loop to exit.
            drop(rt);
            Ok(())
        })
        .context("spawn vs-daemon-tokio thread")?;

    // Release the main thread's engine-runtime handle now that the daemon
    // (on the tokio worker) holds the only other reference. On shutdown
    // the daemon drops, taking the last EngineRuntime Arc with it and
    // closing the engine channel — so the loop below exits, drops
    // `dispatcher`, and runs WkBackend::drop to reap the web-content
    // processes. Holding this handle here kept the channel open forever
    // and deadlocked the shutdown: the backend never dropped, so every
    // open page's ~90 MB render process orphaned to launchd on exit.
    drop(engine_runtime);

    // Main run-loop: drain engine jobs, then pump NSRunLoop briefly.
    // Exit when the channel closes (the worker dropped the daemon).
    let runloop = NSRunLoop::currentRunLoop();
    'main: loop {
        // Drain all queued jobs inside an autorelease pool. The engine
        // runs on this thread with a hand-rolled run loop that never
        // returns to Cocoa's own pool-draining point, so without an
        // explicit pool every autoreleased Obj-C temporary (crucially, a
        // closed page's WKWebView and its ~90 MB web-content process)
        // stays alive until the daemon exits. Draining per iteration
        // reclaims them right after `close`.
        let keep_going = objc2::rc::autoreleasepool(|_| loop {
            match dispatcher.tick() {
                Ok(true) => {}
                Ok(false) => return true,
                Err(()) => return false,
            }
        });
        if !keep_going {
            break 'main;
        }
        // Pump the runloop so WKWebView delegates / JS completion
        // handlers fire on this thread.
        //
        // The slice used to double as the job-pickup latency: an mpsc
        // send is not a run-loop source, so a queued job waited for the
        // slice to elapse. That forced a bad trade — 50ms slices meant
        // a ~50ms floor on every engine hop, and the 4ms slices that
        // fixed it meant ~250 wakeups/sec and ~1.5% CPU on a daemon
        // sitting idle.
        //
        // `MainRunLoopWaker` removes the trade: a worker signals the
        // loop the moment it queues a job, so this can be a long sleep
        // and still pick work up immediately. The slice is now only a
        // backstop bounding how stale things could get if a wakeup were
        // ever missed, which is why it is not simply `distantFuture`.
        let slice = NSDate::dateWithTimeIntervalSinceNow(0.25);
        unsafe { runloop.runMode_beforeDate(NSDefaultRunLoopMode, &slice) };
    }

    let _ = server_thread.join();
    Ok(())
}
