//! Unix-socket server: accept connections, dispatch wire requests,
//! write wire responses.
//!
//! Each connection is a separate Tokio task. Per-primitive handlers
//! live in submodules; this file owns the listener loop, the
//! per-connection reader, and the dispatch table.

mod engine_ops;
mod helpers;
mod lifecycle;
mod page_ops;
mod store_ops;

use std::path::Path;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use vs_protocol::{ErrorCode, Request};

use crate::daemon::Daemon;
use helpers::format_error;

/// Bind a Unix socket at `path` and serve `daemon` on it. Loops until
/// `shutdown` resolves.
pub async fn serve(
    daemon: Daemon,
    path: impl AsRef<Path>,
    mut shutdown: tokio::sync::oneshot::Receiver<()>,
) -> std::io::Result<()> {
    let path = path.as_ref();
    if path.exists() {
        let _ = std::fs::remove_file(path);
    }
    let listener = UnixListener::bind(path)?;
    tracing::info!(?path, "vibesurferd listening");

    let daemon = Arc::new(daemon);
    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown => {
                tracing::info!("shutdown requested");
                break;
            }
            accept = listener.accept() => {
                let (stream, _peer) = accept?;
                let daemon = daemon.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_connection(daemon, stream).await {
                        tracing::warn!(error = %e, "connection ended");
                    }
                });
            }
        }
    }

    let _ = std::fs::remove_file(path);
    Ok(())
}

/// Drive one client connection: read lines, dispatch, write responses.
async fn handle_connection(daemon: Arc<Daemon>, stream: UnixStream) -> std::io::Result<()> {
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read).lines();
    while let Some(line) = reader.next_line().await? {
        if line.is_empty() {
            continue;
        }
        let resp_text = match Request::parse(&line) {
            Ok(req) => {
                let daemon = daemon.clone();
                tokio::task::spawn_blocking(move || {
                    let mut outcomes = daemon.dispatch(&[req]);
                    outcomes.pop().map_or_else(String::new, |o| o.wire)
                })
                .await
                .unwrap_or_else(|join_err| {
                    format_error(
                        ErrorCode::EngineCrash,
                        vec![format!("dispatch panic: {join_err}")],
                    )
                })
            }
            Err(parse_err) => format_error(ErrorCode::BadRequest, vec![format!("{parse_err}")]),
        };
        write.write_all(resp_text.as_bytes()).await?;
        write.write_all(b"\n").await?;
    }
    Ok(())
}

/// Translate a parsed [`Request`] into a wire response (warnings +
/// envelope + body, terminated by `\n` per the protocol spec — the
/// caller adds the final blank line).
#[must_use]
pub fn dispatch(daemon: &Daemon, req: &Request) -> String {
    match req.primitive.as_str() {
        "vs_session_open" => lifecycle::handle_session_open(daemon, req),
        "vs_session_close" => lifecycle::handle_session_close(daemon, req),
        "vs_open" => lifecycle::handle_open(daemon, req),
        "vs_close" => lifecycle::handle_close(daemon, req),
        "vs_view" => page_ops::handle_view(daemon, req),
        "vs_read" => page_ops::handle_read(daemon, req),
        "vs_act" => page_ops::handle_act(daemon, req),
        "vs_find" => page_ops::handle_find(daemon, req),
        "vs_wait" => page_ops::handle_wait(daemon, req),
        "vs_status" => page_ops::handle_status(daemon, req),
        "vs_extract" => store_ops::handle_extract(daemon, req),
        "vs_mark" => store_ops::handle_mark(daemon, req),
        "vs_annotate" => store_ops::handle_annotate(daemon, req),
        "vs_log" => store_ops::handle_log(daemon, req),
        "vs_skill" => engine_ops::handle_skill(daemon, req),
        "vs_capture" => engine_ops::handle_capture(daemon, req),
        "vs_viewport" => engine_ops::handle_viewport(daemon, req),
        "vs_layout" => engine_ops::handle_layout(daemon, req),
        "vs_auth" => engine_ops::handle_auth(daemon, req),
        "vs_inspect" => engine_ops::handle_inspect(daemon, req),
        other => format_error(
            ErrorCode::BadRequest,
            vec![format!("unknown primitive: {other}")],
        ),
    }
}
