//! Windows: WebView2 and a Win32 message pump on the main thread, tokio
//! and the daemon on a worker thread; structured exceptions are logged
//! before the process dies.

use std::sync::Arc;

use anyhow::{Context as _, Result};
use vs_daemon::{server, Daemon};

use super::{
    init_tracing, install_panic_hook, instance_or_exit, resolve_master_key, run_stop,
    wait_terminate, ServeArgs,
};

#[allow(clippy::too_many_lines)]
pub fn run(args: &ServeArgs) -> Result<()> {
    use vs_engine_webkit::{backend::webview2::Webview2Backend, Engine, EngineRuntime};
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, MsgWaitForMultipleObjectsEx, PeekMessageW, PostThreadMessageW,
        TranslateMessage, MSG, MWMO_INPUTAVAILABLE, PM_REMOVE, QS_ALLINPUT, WM_NULL,
    };

    init_tracing();
    install_panic_hook();
    install_seh_handler();
    args.paths.ensure_root().context("ensure ~/.vibesurfer")?;
    let Some(_instance) = instance_or_exit(&args.paths)? else {
        return Ok(());
    };
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
    // Let the message loop below wait instead of spinning. A worker
    // posts WM_NULL to this thread when it queues a job, which counts
    // as input to `MsgWaitForMultipleObjectsEx` and returns the loop
    // immediately. `PostThreadMessageW` is safe from any thread.
    let main_tid = unsafe { GetCurrentThreadId() };
    let engine_runtime = Arc::new(engine_runtime.with_waker(Box::new(move || unsafe {
        let _ = PostThreadMessageW(main_tid, WM_NULL, WPARAM(0), LPARAM(0));
    })));

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
        // Drain pending messages (WebView2 callback completions arrive
        // this way), then wait rather than spin.
        let mut msg = MSG::default();
        unsafe {
            while PeekMessageW(&raw mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&raw const msg);
                DispatchMessageW(&raw const msg);
            }
            // `MsgWaitForMultipleObjectsEx` sleeps until a message
            // arrives or the timeout expires. A worker queueing an
            // engine job posts WM_NULL to this thread (see the waker
            // installed above), which counts as a message and returns
            // us immediately — so the 250ms is a staleness backstop,
            // not the pickup path. The old unconditional 10ms sleep
            // was 100 wakeups/sec on an idle daemon.
            MsgWaitForMultipleObjectsEx(None, 250, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
        }
    }

    let _ = server_thread.join();
    Ok(())
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
