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
                    res = &mut server => {
                        // server::serve returned without ctrl-c — most
                        // commonly a bind failure on the local socket.
                        // Surface it before the run loop exits.
                        match res {
                            Ok(Err(e)) => tracing::error!(
                                error = %e,
                                "server task failed before ctrl-c"
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
    install_panic_hook();
    install_seh_handler();
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
                    res = &mut server => {
                        // server::serve returned without ctrl-c — most
                        // commonly a bind failure on the local socket.
                        // Surface it before the run loop exits.
                        match res {
                            Ok(Err(e)) => tracing::error!(
                                error = %e,
                                "server task failed before ctrl-c"
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
    install_panic_hook();
    install_seh_handler();
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
                    res = &mut server => {
                        // server::serve returned without ctrl-c — most
                        // commonly a bind failure on the local socket.
                        // Surface it before the run loop exits.
                        match res {
                            Ok(Err(e)) => tracing::error!(
                                error = %e,
                                "server task failed before ctrl-c"
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
            while PeekMessageW(&raw mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&raw const msg);
                DispatchMessageW(&raw const msg);
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
        let code = rec.ExceptionCode.0 as u32;
        if code & 0xF0000000 != 0xC0000000 {
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
        if code == 0xC0000005 && rec.NumberParameters >= 2 {
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
                "VIBESURFER_SEH access={} (kind={}) faulting_va=0x{:x}",
                kind_str, kind, va,
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
