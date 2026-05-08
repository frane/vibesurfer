//! Light-weight CLI smoke test: build a `Cli` programmatically, run
//! it against an in-process daemon listening on a temp socket. We
//! don't fork the `vs` binary here — the binary path is a `main.rs`
//! wrapper and is exercised by the wire tests; this test validates
//! the `commands::run` dispatch path.

use std::sync::Arc;
use std::time::Duration;

use vs_cli::active_session;
use vs_cli::commands::{run, Cli, Command};
use vs_daemon::{daemon::Daemon, server};
use vs_engine_webkit::{backend::stub::StubEngine, Engine, EngineRuntime};
use vs_store::Store;

fn boot_daemon(socket: &std::path::Path, db: &std::path::Path) -> tokio::sync::oneshot::Sender<()> {
    let store = Store::open(db).unwrap();
    let runtime =
        EngineRuntime::spawn(|| Ok(Box::new(StubEngine::new()) as Box<dyn Engine>)).unwrap();
    let daemon = Daemon::new(store, Arc::new(runtime));
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let socket = socket.to_path_buf();
    tokio::spawn(async move {
        let _ = server::serve(daemon, socket, shutdown_rx).await;
    });
    shutdown_tx
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_open_then_status() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("daemon.sock");
    let db = dir.path().join("state.db");
    let home = dir.path().to_path_buf();

    let shutdown = boot_daemon(&socket, &db);
    for _ in 0..40 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(socket.exists(), "socket never appeared");

    // session-open
    let cli = Cli {
        session: None,
        socket: Some(socket.clone()),
        home: Some(home.clone()),
        no_spawn: true,
        json: false,
        command: Command::SessionOpen { policy: None },
    };
    let resp = tokio::task::spawn_blocking(move || run(&cli))
        .await
        .unwrap()
        .unwrap();
    assert!(resp.is_ok(), "envelope: {:?}", resp.envelope);
    assert!(!resp.body.is_empty());
    let session_id = resp.body[0].clone();

    // active-session pointer should now exist with that id
    assert_eq!(
        active_session::read(home.join("active-session"))
            .unwrap()
            .as_deref(),
        Some(session_id.as_str())
    );

    // status (uses active session implicitly via the file)
    let cli2 = Cli {
        session: None,
        socket: Some(socket.clone()),
        home: Some(home.clone()),
        no_spawn: true,
        json: false,
        command: Command::Status,
    };
    let resp = tokio::task::spawn_blocking(move || run(&cli2))
        .await
        .unwrap()
        .unwrap();
    assert!(resp.is_ok());
    assert!(resp.body.iter().any(|l| l.contains(&session_id)));

    let _ = shutdown.send(());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_override_via_flag() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("daemon.sock");
    let db = dir.path().join("state.db");
    let home = dir.path().to_path_buf();

    let shutdown = boot_daemon(&socket, &db);
    for _ in 0..40 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // Open a session, save its id, then call status with --session=<id>.
    let cli = Cli {
        session: None,
        socket: Some(socket.clone()),
        home: Some(home.clone()),
        no_spawn: true,
        json: false,
        command: Command::SessionOpen {
            policy: Some("strict".into()),
        },
    };
    let resp = tokio::task::spawn_blocking(move || run(&cli))
        .await
        .unwrap()
        .unwrap();
    let session_id = resp.body[0].clone();

    // Wipe the active-session file. With --session= override, status must still work.
    active_session::clear(home.join("active-session")).unwrap();

    let session_clone = session_id.clone();
    let cli3 = Cli {
        session: Some(session_clone.clone()),
        socket: Some(socket.clone()),
        home: Some(home.clone()),
        no_spawn: true,
        json: false,
        command: Command::Status,
    };
    let resp = tokio::task::spawn_blocking(move || run(&cli3))
        .await
        .unwrap()
        .unwrap();
    assert!(resp.is_ok());
    assert!(resp.body.iter().any(|l| l.contains(&session_clone)));

    let _ = shutdown.send(());
}
