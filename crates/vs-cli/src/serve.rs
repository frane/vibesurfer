//! `vs serve` — host the daemon in this process.
//!
//! Folded into the `vs` binary so the project ships exactly one CLI
//! surface; the M0 `vibesurferd` binary is gone. Auto-spawn re-execs
//! `vs serve` instead.
//!
//! # Threading model
//!
//! On **macOS**, `WKWebView` is hard-pinned to the Cocoa main thread.
//! [`run`] therefore stays on the OS main thread, initializes
//! `NSApplication`, constructs the `WkBackend` here, and spawns a
//! worker thread that runs the tokio runtime + the daemon. Engine calls
//! issued by the daemon (on tokio workers) flow through an mpsc
//! channel back to main, where they're drained between NSRunLoop
//! ticks. See [`vs_engine_webkit::runtime::MainThreadDispatcher`].
//!
//! On **Linux**, the same shape applies with a GLib main context and
//! WebKitGTK 6. On **Windows**, with a Win32 message pump and
//! WebView2.

use std::sync::Arc;

use anyhow::{Context as _, Result};
use vs_daemon::{config::Paths as DaemonPaths, server, Daemon};

/// Args specific to `vs serve`. `paths` is the resolved daemon home.
pub struct ServeArgs {
    pub paths: DaemonPaths,
}

// =============================================================================
// macOS: NSApp on main, tokio on worker, real WKWebView backend.
// =============================================================================

#[cfg(target_os = "macos")]
pub fn run(args: &ServeArgs) -> Result<()> {
    use objc2::MainThreadMarker;
    use objc2_app_kit::NSApplication;
    use objc2_foundation::{NSDate, NSDefaultRunLoopMode, NSRunLoop};
    use vs_engine_webkit::{backend::webkit::WkBackend, Engine, EngineRuntime};

    init_tracing();
    args.paths.ensure_root().context("ensure ~/.vibesurfer")?;

    let mtm = MainThreadMarker::new()
        .ok_or_else(|| anyhow::anyhow!("vs serve must be invoked from the OS main thread"))?;
    // Initialize NSApp; required for WKWebView even though we don't run
    // the AppKit event loop directly.
    let _app = NSApplication::sharedApplication(mtm);

    let store = vs_store::Store::open(args.paths.db()).context("open state.db")?;
    let captures_dir = args.paths.captures();
    let skills_dir = args.paths.root.join("skills");

    // Engine lives on this thread (the Cocoa main thread). Construct
    // the WkBackend here and hand it to `EngineRuntime::dispatcher`,
    // which gives us back a runtime handle (for the daemon) and a
    // dispatcher we drive in this thread's run loop.
    let backend = WkBackend::new(mtm).with_capture_dir(captures_dir.clone());
    let engine_box: Box<dyn Engine> = Box::new(backend);
    let (engine_runtime, mut dispatcher) = EngineRuntime::dispatcher(engine_box);
    let engine_runtime = Arc::new(engine_runtime);

    let mut daemon = Daemon::new(store, engine_runtime.clone())
        .with_captures_dir(captures_dir)
        .with_skills_dir(skills_dir);

    if let Ok(k) = vs_store::MasterKey::resolve(args.paths.key_file()) {
        daemon = daemon.with_master_key(k);
    } else {
        tracing::warn!(
            "no master key (keyring entry missing and {} not present); vs_auth save|load will fail",
            args.paths.key_file().display()
        );
    }

    let socket = args.paths.socket();

    // Spawn the tokio runtime on a worker. It owns the daemon and the
    // socket server; ctrl-c on the worker triggers a graceful shutdown
    // by closing `shutdown_rx` and dropping the runtime, which closes
    // our engine channel and pops us out of the run-loop below.
    let server_thread = std::thread::Builder::new()
        .name("vs-daemon-tokio".into())
        .spawn(move || -> Result<()> {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(4)
                .enable_all()
                .build()
                .context("build tokio runtime")?;
            rt.block_on(async move {
                let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
                let server =
                    tokio::spawn(async move { server::serve(daemon, socket, shutdown_rx).await });
                let _ = tokio::signal::ctrl_c().await;
                tracing::info!("ctrl-c received, shutting down");
                let _ = shutdown_tx.send(());
                let _ = server.await;
            });
            // Dropping `rt` and the moved `engine_runtime` (held by the
            // daemon) closes the engine channel, which signals the main
            // loop to exit.
            drop(rt);
            Ok(())
        })
        .context("spawn vs-daemon-tokio thread")?;

    // Main run-loop: drain engine jobs, then pump NSRunLoop briefly.
    // Exit when the channel closes (the worker dropped the daemon).
    let runloop = NSRunLoop::currentRunLoop();
    'main: loop {
        // Drain all queued jobs.
        loop {
            match dispatcher.tick() {
                Ok(true) => {}
                Ok(false) => break,
                Err(()) => break 'main,
            }
        }
        // Pump the runloop briefly so WKWebView delegates / JS
        // completion handlers fire on this thread.
        let slice = NSDate::dateWithTimeIntervalSinceNow(0.05);
        unsafe { runloop.runMode_beforeDate(NSDefaultRunLoopMode, &slice) };
    }

    let _ = server_thread.join();
    drop(engine_runtime); // explicit, even though it's already dead
    Ok(())
}

// =============================================================================
// Linux: GTK on main, tokio on worker, real WebKitGTK 6 backend.
// =============================================================================

#[cfg(target_os = "linux")]
pub fn run(args: &ServeArgs) -> Result<()> {
    use vs_engine_webkit::{backend::wpe::WpeBackend, Engine, EngineRuntime};

    init_tracing();
    args.paths.ensure_root().context("ensure ~/.vibesurfer")?;

    // GTK init must happen on the OS main thread, before any WebView.
    gtk4::init().context("gtk4 init")?;

    let store = vs_store::Store::open(args.paths.db()).context("open state.db")?;
    let captures_dir = args.paths.captures();
    let skills_dir = args.paths.root.join("skills");

    let backend = WpeBackend::new().with_capture_dir(captures_dir.clone());
    let engine_box: Box<dyn Engine> = Box::new(backend);
    let (engine_runtime, mut dispatcher) = EngineRuntime::dispatcher(engine_box);
    let engine_runtime = Arc::new(engine_runtime);

    let mut daemon = Daemon::new(store, engine_runtime.clone())
        .with_captures_dir(captures_dir)
        .with_skills_dir(skills_dir);

    if let Ok(k) = vs_store::MasterKey::resolve(args.paths.key_file()) {
        daemon = daemon.with_master_key(k);
    } else {
        tracing::warn!(
            "no master key (keyring entry missing and {} not present); vs_auth save|load will fail",
            args.paths.key_file().display()
        );
    }

    let socket = args.paths.socket();

    let server_thread = std::thread::Builder::new()
        .name("vs-daemon-tokio".into())
        .spawn(move || -> Result<()> {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(4)
                .enable_all()
                .build()
                .context("build tokio runtime")?;
            rt.block_on(async move {
                let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
                let server =
                    tokio::spawn(async move { server::serve(daemon, socket, shutdown_rx).await });
                let _ = tokio::signal::ctrl_c().await;
                tracing::info!("ctrl-c received, shutting down");
                let _ = shutdown_tx.send(());
                let _ = server.await;
            });
            drop(rt);
            Ok(())
        })
        .context("spawn vs-daemon-tokio thread")?;

    // Pump the GLib main context on the main thread, draining engine
    // jobs between iterations. Exit when the channel closes.
    let main_ctx = glib::MainContext::default();
    'main: loop {
        loop {
            match dispatcher.tick() {
                Ok(true) => {}
                Ok(false) => break,
                Err(()) => break 'main,
            }
        }
        // Iterate non-blocking — if the GLib loop has nothing to do,
        // sleep briefly so we don't burn CPU.
        if !main_ctx.iteration(false) {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    let _ = server_thread.join();
    drop(engine_runtime);
    Ok(())
}

// =============================================================================
// Windows: WebView2 + Win32 message pump on main, tokio on worker.
// =============================================================================

#[cfg(target_os = "windows")]
pub fn run(args: &ServeArgs) -> Result<()> {
    use vs_engine_webkit::{backend::webview2::Webview2Backend, Engine, EngineRuntime};
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE,
    };

    init_tracing();
    args.paths.ensure_root().context("ensure ~/.vibesurfer")?;

    // SAFETY: required first call on this thread before any
    // WebView2 COM API. RPC_E_CHANGED_MODE on second call is fine.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }

    let store = vs_store::Store::open(args.paths.db()).context("open state.db")?;
    let captures_dir = args.paths.captures();
    let skills_dir = args.paths.root.join("skills");

    let backend = Webview2Backend::new().with_capture_dir(captures_dir.clone());
    let engine_box: Box<dyn Engine> = Box::new(backend);
    let (engine_runtime, mut dispatcher) = EngineRuntime::dispatcher(engine_box);
    let engine_runtime = Arc::new(engine_runtime);

    let mut daemon = Daemon::new(store, engine_runtime.clone())
        .with_captures_dir(captures_dir)
        .with_skills_dir(skills_dir);

    if let Ok(k) = vs_store::MasterKey::resolve(args.paths.key_file()) {
        daemon = daemon.with_master_key(k);
    } else {
        tracing::warn!(
            "no master key (keyring entry missing and {} not present); vs_auth save|load will fail",
            args.paths.key_file().display()
        );
    }

    let socket = args.paths.socket();

    let server_thread = std::thread::Builder::new()
        .name("vs-daemon-tokio".into())
        .spawn(move || -> Result<()> {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(4)
                .enable_all()
                .build()
                .context("build tokio runtime")?;
            rt.block_on(async move {
                let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
                let server =
                    tokio::spawn(async move { server::serve(daemon, socket, shutdown_rx).await });
                let _ = tokio::signal::ctrl_c().await;
                tracing::info!("ctrl-c received, shutting down");
                let _ = shutdown_tx.send(());
                let _ = server.await;
            });
            drop(rt);
            Ok(())
        })
        .context("spawn vs-daemon-tokio thread")?;

    // Pump Win32 messages on the main thread, draining engine jobs
    // between iterations. Exit when the channel closes.
    let mut shutdown = false;
    while !shutdown {
        loop {
            match dispatcher.tick() {
                Ok(true) => {}
                Ok(false) => break,
                Err(()) => {
                    shutdown = true;
                    break;
                }
            }
        }
        // Non-blocking PeekMessage. If a message exists, dispatch
        // (WebView2 callback completions arrive this way).
        let mut msg = MSG::default();
        unsafe {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    let _ = server_thread.join();
    drop(engine_runtime);
    Ok(())
}

fn init_tracing() {
    if tracing::dispatcher::has_been_set() {
        return;
    }
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("vs_daemon=info,info")),
        )
        .with_writer(std::io::stderr)
        .try_init();
}
