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
    /// If true, do not start a daemon — instead, read the PID file,
    /// send SIGTERM, and wait for the socket to disappear.
    pub stop: bool,
}

/// `vs serve --stop`. Reads the daemon PID file, sends SIGTERM (Unix)
/// or TerminateProcess (Windows), and waits up to 5s for the socket
/// file to disappear. Stale PID files for processes that already exit
/// are cleaned up.
pub fn run_stop(paths: &DaemonPaths) -> Result<()> {
    let pid_file = paths.pid_file();
    let pid_str = match std::fs::read_to_string(&pid_file) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("no daemon running (no PID file at {})", pid_file.display());
            return Ok(());
        }
        Err(e) => return Err(e).context("read pid file"),
    };
    let pid: i32 = pid_str
        .trim()
        .parse()
        .with_context(|| format!("malformed PID file: {pid_str:?}"))?;
    #[cfg(unix)]
    {
        let r = unsafe { libc::kill(pid, libc::SIGTERM) };
        if r != 0 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::ESRCH) {
                let _ = std::fs::remove_file(&pid_file);
                eprintln!("no daemon running (cleaned stale PID file for pid {pid})");
                return Ok(());
            }
            return Err(anyhow::anyhow!("kill({pid}, SIGTERM) failed: {e}"));
        }
    }
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
        let pid_u32 =
            u32::try_from(pid).with_context(|| format!("PID {pid} out of range for a DWORD"))?;
        unsafe {
            let h = OpenProcess(PROCESS_TERMINATE, false, pid_u32)
                .map_err(|e| anyhow::anyhow!("OpenProcess({pid}): {e}"))?;
            let r = TerminateProcess(h, 0);
            let _ = CloseHandle(h);
            r.map_err(|e| anyhow::anyhow!("TerminateProcess({pid}): {e}"))?;
        }
    }
    let socket = paths.socket();
    for _ in 0..50 {
        if !socket.exists() {
            eprintln!("daemon stopped (pid {pid})");
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    Err(anyhow::anyhow!(
        "daemon (pid {pid}) did not exit within 5s; socket still present at {}",
        socket.display()
    ))
}

/// Resolve the daemon master key: OS keyring first, then the fallback
/// file at `~/.vibesurfer/key` (32 raw bytes, 64 hex chars, or base64
/// of 32 bytes). If neither exists, generate a fresh key and persist
/// it to the fallback file (mode 0600) so `vs auth save|load` works
/// out of the box. Returns `None` — with the cause logged — only when
/// the file is present but unusable, or generation/persist fails.
fn resolve_master_key(paths: &DaemonPaths) -> Option<vs_store::MasterKey> {
    use vs_store::MasterKey;
    if let Ok(k) = MasterKey::from_keyring() {
        return Some(k);
    }
    let key_path = paths.key_file();
    if key_path.exists() {
        match MasterKey::from_file(&key_path) {
            Ok(k) => Some(k),
            Err(e) => {
                tracing::warn!(
                    "master key file {} exists but is unusable ({e}); \
                     vs_auth save|load will fail",
                    key_path.display()
                );
                None
            }
        }
    } else {
        match MasterKey::generate().and_then(|k| k.write_to_file(&key_path).map(|()| k)) {
            Ok(k) => {
                tracing::info!(
                    "no master key found; generated one at {} (keyring entry absent)",
                    key_path.display()
                );
                Some(k)
            }
            Err(e) => {
                tracing::warn!(
                    "no master key, and generating one at {} failed ({e}); \
                     vs_auth save|load will fail",
                    key_path.display()
                );
                None
            }
        }
    }
}

/// Future that resolves on SIGTERM (Unix) or never (other platforms).
/// Lets the server loop treat SIGTERM equivalently to ctrl-c.
#[cfg(unix)]
pub async fn wait_terminate() {
    if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        s.recv().await;
    } else {
        std::future::pending::<()>().await;
    }
}

#[cfg(not(unix))]
pub async fn wait_terminate() {
    std::future::pending::<()>().await;
}

// =============================================================================
// macOS: NSApp on main, tokio on worker, real WKWebView backend.
// =============================================================================

#[cfg(target_os = "macos")]
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
    install_seh_handler();
    args.paths.ensure_root().context("ensure ~/.vibesurfer")?;

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
    let engine_runtime = Arc::new(engine_runtime);

    let mut daemon = Daemon::new(store, engine_runtime.clone())
        .with_captures_dir(captures_dir)
        .with_downloads_dir(downloads_dir)
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
        // Pump the runloop briefly so WKWebView delegates / JS
        // completion handlers fire on this thread. The slice also
        // bounds how long a freshly-queued engine job waits to be
        // picked up: an mpsc send from a daemon worker is not a
        // run-loop source, so it can't cut this wait short, and the
        // loop only returns to `tick()` when the slice elapses. At
        // 50ms that was a ~50ms floor on *every* engine hop (measured:
        // a trivial layout eval cost ~50ms). 4ms drops per-hop latency
        // to ~10ms while keeping idle wakeups cheap (runMode sleeps
        // efficiently between them).
        let slice = NSDate::dateWithTimeIntervalSinceNow(0.004);
        unsafe { runloop.runMode_beforeDate(NSDefaultRunLoopMode, &slice) };
    }

    let _ = server_thread.join();
    Ok(())
}

// =============================================================================
// Linux: GTK on main, tokio on worker, real WebKitGTK 6 backend.
// =============================================================================

#[cfg(target_os = "linux")]
#[allow(clippy::too_many_lines)]
pub fn run(args: &ServeArgs) -> Result<()> {
    use vs_engine_webkit::{backend::wpe::WpeBackend, Engine, EngineRuntime};

    init_tracing();
    install_panic_hook();
    install_seh_handler();
    args.paths.ensure_root().context("ensure ~/.vibesurfer")?;
    if args.stop {
        return run_stop(&args.paths);
    }

    // GTK init must happen on the OS main thread, before any WebView.
    gtk4::init().context("gtk4 init")?;

    let store = vs_store::Store::open(args.paths.db()).context("open state.db")?;
    let captures_dir = args.paths.captures();
    let downloads_dir = args.paths.downloads();
    let skills_dir = args.paths.root.join("skills");

    let backend = WpeBackend::new().with_capture_dir(captures_dir.clone());
    let engine_box: Box<dyn Engine> = Box::new(backend);
    let (engine_runtime, mut dispatcher) = EngineRuntime::dispatcher(engine_box);
    let engine_runtime = Arc::new(engine_runtime);

    let mut daemon = Daemon::new(store, engine_runtime.clone())
        .with_captures_dir(captures_dir)
        .with_downloads_dir(downloads_dir)
        .with_skills_dir(skills_dir);

    if let Some(k) = resolve_master_key(&args.paths) {
        daemon = daemon.with_master_key(k);
    }

    let socket = args.paths.socket();
    let pid_path = args.paths.pid_file();

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
            drop(rt);
            Ok(())
        })
        .context("spawn vs-daemon-tokio thread")?;

    // Drop the main thread's engine-runtime handle so the engine channel
    // can close when the daemon drops on shutdown (see the macOS run for
    // the full rationale — holding it here deadlocked shutdown and
    // orphaned render processes).
    drop(engine_runtime);

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
    Ok(())
}

// =============================================================================
// Windows: WebView2 + Win32 message pump on main, tokio on worker.
// =============================================================================

#[cfg(target_os = "windows")]
#[allow(clippy::too_many_lines)]
pub fn run(args: &ServeArgs) -> Result<()> {
    use vs_engine_webkit::{backend::webview2::Webview2Backend, Engine, EngineRuntime};
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE,
    };

    init_tracing();
    install_panic_hook();
    install_seh_handler();
    args.paths.ensure_root().context("ensure ~/.vibesurfer")?;
    if args.stop {
        return run_stop(&args.paths);
    }

    // SAFETY: required first call on this thread before any
    // WebView2 COM API. RPC_E_CHANGED_MODE on second call is fine.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }

    let store = vs_store::Store::open(args.paths.db()).context("open state.db")?;
    let captures_dir = args.paths.captures();
    let downloads_dir = args.paths.downloads();
    let skills_dir = args.paths.root.join("skills");

    let backend = Webview2Backend::new().with_capture_dir(captures_dir.clone());
    let engine_box: Box<dyn Engine> = Box::new(backend);
    let (engine_runtime, mut dispatcher) = EngineRuntime::dispatcher(engine_box);
    let engine_runtime = Arc::new(engine_runtime);

    let mut daemon = Daemon::new(store, engine_runtime.clone())
        .with_captures_dir(captures_dir)
        .with_downloads_dir(downloads_dir)
        .with_skills_dir(skills_dir);

    if let Some(k) = resolve_master_key(&args.paths) {
        daemon = daemon.with_master_key(k);
    }

    let socket = args.paths.socket();
    let pid_path = args.paths.pid_file();

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
            drop(rt);
            Ok(())
        })
        .context("spawn vs-daemon-tokio thread")?;

    // Drop the main thread's engine-runtime handle so the engine channel
    // can close when the daemon drops on shutdown (see the macOS run for
    // the full rationale — holding it here deadlocked shutdown and
    // orphaned render processes).
    drop(engine_runtime);

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
            while PeekMessageW(&raw mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&raw const msg);
                DispatchMessageW(&raw const msg);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    let _ = server_thread.join();
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

/// Install a panic hook that logs the panic via tracing::error
/// before the default hook prints to stderr. The default hook
/// already writes to stderr, but on Windows a panic on a non-main
/// thread can sometimes terminate the process before stderr is
/// flushed; routing through tracing first guarantees the panic
/// reaches the daemon log file the test harness captures.
fn install_panic_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map_or_else(|| "<unknown>".to_string(), ToString::to_string);
        let msg = info
            .payload()
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| info.payload().downcast_ref::<String>().map(String::as_str))
            .unwrap_or("<no message>");
        tracing::error!(at = %location, "PANIC: {msg}");
        prev(info);
    }));
}

/// Install a Win32 vectored exception handler that logs structured
/// exceptions (access violation, stack overflow, etc.) before the
/// process dies. Rust's panic machinery does not catch SEH on
/// Windows by default — when a COM call inside `Webview2Backend`
/// dereferences bad memory, the daemon vanishes with no message,
/// no panic hook firing, no trace.
///
/// This hook writes a single line to stderr containing the
/// exception code + faulting address, then returns
/// `EXCEPTION_CONTINUE_SEARCH` so the OS's default handler runs
/// (process death). Stderr is captured to the daemon log by the
/// test harness, so the line lands in CI output.
///
/// Direct stderr write — not tracing — because the allocator may
/// be in a bad state during an SEH; tracing's formatting could
/// deadlock. `writeln!` to a locked stderr handle is the cheapest
/// thing that can work here.
#[cfg(target_os = "windows")]
fn install_seh_handler() {
    use std::io::Write;
    use windows::Win32::System::Diagnostics::Debug::{
        AddVectoredExceptionHandler, EXCEPTION_POINTERS,
    };

    unsafe extern "system" fn handler(info: *mut EXCEPTION_POINTERS) -> i32 {
        if info.is_null() {
            return 0; // EXCEPTION_CONTINUE_SEARCH
        }
        let info = unsafe { &*info };
        if info.ExceptionRecord.is_null() {
            return 0;
        }
        let rec = unsafe { &*info.ExceptionRecord };
        // Filter to fatal-class exceptions only. Status codes in the
        // 0xC0... range are NTSTATUS errors; lower codes are e.g.
        // C++ EH (0xE06D7363), DLL not found, debug breakpoints —
        // not interesting and not always fatal.
        // NTSTATUS is i32 internally — bit-reinterpret to u32 for
        // hex display + range check. `as u32` would clippy-deny on
        // `cast_sign_loss`; the to_ne_bytes round-trip is bit-exact.
        let code = u32::from_ne_bytes(rec.ExceptionCode.0.to_ne_bytes());
        if code & 0xF000_0000 != 0xC000_0000 {
            return 0;
        }
        let mut err = std::io::stderr().lock();
        let _ = writeln!(
            err,
            "VIBESURFER_SEH code=0x{:08x} ip={:p} flags=0x{:x} params={}",
            code, rec.ExceptionAddress, rec.ExceptionFlags, rec.NumberParameters,
        );
        // For STATUS_ACCESS_VIOLATION, ExceptionInformation carries
        // [access_kind, faulting_va] — log both. access_kind: 0=read,
        // 1=write, 8=DEP/execute. faulting_va is the address that was
        // being read / written / executed at the time of the fault.
        if code == 0xC000_0005 && rec.NumberParameters >= 2 {
            let kind = rec.ExceptionInformation[0];
            let va = rec.ExceptionInformation[1];
            let kind_str = match kind {
                0 => "read",
                1 => "write",
                8 => "execute",
                _ => "?",
            };
            let _ = writeln!(
                err,
                "VIBESURFER_SEH access={kind_str} (kind={kind}) faulting_va=0x{va:x}",
            );
        }
        // Also dump RIP / RSP / a few callee-saved registers from the
        // ContextRecord. RIP confirms the IP from ExceptionAddress;
        // RSP + return-address-on-stack often points at the caller
        // even when ExceptionAddress is 0x0.
        if !info.ContextRecord.is_null() {
            let ctx = unsafe { &*info.ContextRecord };
            #[cfg(target_arch = "x86_64")]
            {
                let _ = writeln!(
                    err,
                    "VIBESURFER_SEH rip=0x{:x} rsp=0x{:x} rbp=0x{:x} rcx=0x{:x} rdx=0x{:x}",
                    ctx.Rip, ctx.Rsp, ctx.Rbp, ctx.Rcx, ctx.Rdx,
                );
            }
            #[cfg(target_arch = "aarch64")]
            {
                let _ = writeln!(
                    err,
                    "VIBESURFER_SEH pc=0x{:x} sp=0x{:x} fp=0x{:x}",
                    ctx.Pc, ctx.Sp, ctx.Fp,
                );
            }
        }
        let _ = err.flush();
        0
    }

    unsafe {
        AddVectoredExceptionHandler(1 /* CALL_FIRST */, Some(handler));
    }
}

/// No-op on non-Windows. The Unix kernels we target deliver crashes
/// as signals (SIGSEGV etc.); Rust's panic + signal handling already
/// covers those.
#[cfg(not(target_os = "windows"))]
fn install_seh_handler() {}
