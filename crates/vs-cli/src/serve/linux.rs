//! Linux: GTK on the main thread with the real WebKitGTK 6 backend,
//! tokio and the daemon on a worker thread.

use std::sync::Arc;

use anyhow::{Context as _, Result};
use vs_daemon::{server, Daemon};

use super::{
    init_tracing, install_panic_hook, instance_or_exit, resolve_master_key, run_stop,
    wait_terminate, ServeArgs,
};

#[allow(clippy::too_many_lines)]
pub fn run(args: &ServeArgs) -> Result<()> {
    use vs_engine_webkit::{backend::wpe::WpeBackend, Engine, EngineRuntime};

    init_tracing();
    install_panic_hook();
    args.paths.ensure_root().context("ensure ~/.vibesurfer")?;
    let Some(_instance) = instance_or_exit(&args.paths)? else {
        return Ok(());
    };
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
    // Let the GLib loop below block instead of polling. `wakeup` is
    // thread-safe and is what lets a worker cut the blocking iteration
    // short the moment it queues a job.
    let waker_ctx = glib::MainContext::default();
    let engine_runtime = Arc::new(engine_runtime.with_waker(Box::new(move || waker_ctx.wakeup())));

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

    // Pump the GLib main context on the main thread, draining engine
    // jobs between iterations. Exit when the channel closes.
    //
    // The iteration blocks rather than spinning: workers call
    // `MainContext::wakeup` (thread-safe) when they queue a job, so
    // there is no reason to poll. The 250ms timeout source below is
    // only a backstop bounding staleness if a wakeup were ever missed
    // — same shape as the macOS CFRunLoopSource path.
    let main_ctx = glib::MainContext::default();
    glib::timeout_add_local(std::time::Duration::from_millis(250), || {
        glib::ControlFlow::Continue
    });
    'main: loop {
        loop {
            match dispatcher.tick() {
                Ok(true) => {}
                Ok(false) => break,
                Err(()) => break 'main,
            }
        }
        main_ctx.iteration(true);
    }

    let _ = server_thread.join();
    Ok(())
}
