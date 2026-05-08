//! Drive the daemon over its real Unix socket. Spawns the server in a
//! tokio runtime, connects, exchanges line-delimited frames.

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::unix::OwnedReadHalf;
use tokio::net::UnixStream;

use vs_daemon::{daemon::Daemon, server};
use vs_engine_webkit::{test_support::TestEngine, Engine, EngineRuntime};
use vs_store::Store;

/// Read one full response from the daemon: every line up to (and
/// excluding) the blank-line terminator.
async fn read_response(reader: &mut Lines<BufReader<OwnedReadHalf>>) -> Vec<String> {
    let mut lines = Vec::new();
    loop {
        match reader.next_line().await.unwrap() {
            Some(l) if l.is_empty() => return lines,
            Some(l) => lines.push(l),
            None => return lines,
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wire_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("daemon.sock");

    let store = Store::open(dir.path().join("state.db")).unwrap();
    let runtime =
        EngineRuntime::spawn(|| Ok(Box::new(TestEngine::new()) as Box<dyn Engine>)).unwrap();
    let daemon = Daemon::new(store, Arc::new(runtime));

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let socket_clone = socket.clone();
    let server_task =
        tokio::spawn(async move { server::serve(daemon, socket_clone, shutdown_rx).await });

    // Wait for the socket to bind.
    for _ in 0..40 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(socket.exists(), "socket never appeared");

    let stream = UnixStream::connect(&socket).await.unwrap();
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read).lines();

    // 1. Open a session.
    write
        .write_all(b"vs_session_open --policy=default\n")
        .await
        .unwrap();
    let r = read_response(&mut reader).await;
    assert_eq!(r.len(), 2, "session_open got: {r:?}");
    assert!(r[0].starts_with('@'), "envelope: {}", r[0]);
    let session_id = r[1].clone();
    assert!(session_id.starts_with("s_"));

    // 2. Open a page.
    write
        .write_all(format!("vs_open https://example.com/login --session={session_id}\n").as_bytes())
        .await
        .unwrap();
    let r = read_response(&mut reader).await;
    assert_eq!(r.len(), 2, "open got: {r:?}");
    assert!(r[0].starts_with('@'), "envelope: {}", r[0]);
    let page_id = r[1].clone();
    assert!(page_id.starts_with("p_"));

    // 3. View the page — first view is full.
    write
        .write_all(format!("vs_view {page_id} --session={session_id}\n").as_bytes())
        .await
        .unwrap();
    let r = read_response(&mut reader).await;
    assert!(r[0].starts_with('@'), "envelope: {}", r[0]);
    assert!(
        r.iter().any(|l| l.contains("doc")),
        "tree body missing doc: {r:?}"
    );

    // 4. Bad primitive yields BAD_REQUEST envelope.
    write.write_all(b"vs_nope_nope\n").await.unwrap();
    let r = read_response(&mut reader).await;
    assert_eq!(r.len(), 1, "bad-primitive response: {r:?}");
    assert!(r[0].starts_with("! BAD_REQUEST"), "got: {}", r[0]);

    // Clean shutdown.
    drop(write);
    let _ = shutdown_tx.send(());
    let _ = server_task.await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wire_stale_token_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("daemon.sock");

    let store = Store::open(dir.path().join("state.db")).unwrap();
    let runtime =
        EngineRuntime::spawn(|| Ok(Box::new(TestEngine::new()) as Box<dyn Engine>)).unwrap();
    let daemon = Daemon::new(store, Arc::new(runtime));

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let socket_clone = socket.clone();
    let server_task =
        tokio::spawn(async move { server::serve(daemon, socket_clone, shutdown_rx).await });

    for _ in 0..40 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let stream = UnixStream::connect(&socket).await.unwrap();
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read).lines();

    write.write_all(b"vs_session_open\n").await.unwrap();
    let r = read_response(&mut reader).await;
    let session_id = r[1].clone();
    write
        .write_all(format!("vs_open https://x --session={session_id}\n").as_bytes())
        .await
        .unwrap();
    let r = read_response(&mut reader).await;
    let page_id = r[1].clone();
    write
        .write_all(format!("vs_view {page_id} --session={session_id}\n").as_bytes())
        .await
        .unwrap();
    let _ = read_response(&mut reader).await;

    let stale = "ffffffffffffffff";
    write
        .write_all(
            format!("vs_act {page_id} 4 click --session={session_id} --token={stale}\n").as_bytes(),
        )
        .await
        .unwrap();
    let r = read_response(&mut reader).await;
    assert!(
        r[0].starts_with("! STALE_TOKEN"),
        "expected STALE_TOKEN, got: {}",
        r[0]
    );

    drop(write);
    let _ = shutdown_tx.send(());
    let _ = server_task.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wire_idempotent_replay_returns_warning() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("daemon.sock");

    let store = Store::open(dir.path().join("state.db")).unwrap();
    let runtime =
        EngineRuntime::spawn(|| Ok(Box::new(TestEngine::new()) as Box<dyn Engine>)).unwrap();
    let daemon = Daemon::new(store, Arc::new(runtime));

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let socket_clone = socket.clone();
    let server_task =
        tokio::spawn(async move { server::serve(daemon, socket_clone, shutdown_rx).await });

    for _ in 0..40 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let stream = UnixStream::connect(&socket).await.unwrap();
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read).lines();

    write.write_all(b"vs_session_open\n").await.unwrap();
    let r = read_response(&mut reader).await;
    let session_id = r[1].clone();
    write
        .write_all(format!("vs_open https://x --session={session_id}\n").as_bytes())
        .await
        .unwrap();
    let r = read_response(&mut reader).await;
    let page_id = r[1].clone();
    write
        .write_all(format!("vs_view {page_id} --session={session_id}\n").as_bytes())
        .await
        .unwrap();
    let r = read_response(&mut reader).await;
    let token = r[0].strip_prefix('@').unwrap().to_string();

    write
        .write_all(
            format!("vs_act {page_id} 4 click --session={session_id} --token={token}\n").as_bytes(),
        )
        .await
        .unwrap();
    let r1 = read_response(&mut reader).await;
    assert!(r1[0].starts_with('@'), "first call envelope: {}", r1[0]);

    write
        .write_all(
            format!("vs_act {page_id} 4 click --session={session_id} --token={token}\n").as_bytes(),
        )
        .await
        .unwrap();
    let r2 = read_response(&mut reader).await;
    assert!(
        r2.iter().any(|l| l.starts_with("? idempotent_hit")),
        "missing idempotent_hit warning: {r2:?}"
    );
    assert!(
        r2.iter().any(|l| l.starts_with('@')),
        "missing success envelope: {r2:?}"
    );

    drop(write);
    let _ = shutdown_tx.send(());
    let _ = server_task.await;
}
