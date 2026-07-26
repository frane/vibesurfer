//! Cross-platform local-socket server: accept connections, dispatch
//! wire requests, write wire responses.
//!
//! Uses [`interprocess`] to abstract the platform IPC primitive:
//! AF_UNIX socket files on Unix, named pipes on Windows. Each
//! connection is a separate Tokio task. Per-primitive handlers live
//! in submodules; this file owns the listener loop, the
//! per-connection reader, and the dispatch table.

mod engine_ops;
mod helpers;
mod lifecycle;
mod page_ops;
mod store_ops;

use std::path::Path;
use std::sync::Arc;

use interprocess::local_socket::tokio::{prelude::*, Listener, Stream};
use interprocess::local_socket::ListenerOptions;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use vs_protocol::{ErrorCode, Request};

use crate::daemon::Daemon;
use helpers::format_error;

/// Bind a local socket at `path` and serve `daemon` on it. On Unix
/// `path` is the AF_UNIX socket file; on Windows it's used to derive
/// a stable namespaced pipe name (see [`crate::transport::path_to_name`]).
/// Loops until `shutdown` resolves.
pub async fn serve(
    daemon: Daemon,
    path: impl AsRef<Path>,
    mut shutdown: tokio::sync::oneshot::Receiver<()>,
) -> std::io::Result<()> {
    let path = path.as_ref();
    // On Unix, a stale socket file blocks bind. Best-effort cleanup.
    #[cfg(unix)]
    if path.exists() {
        let _ = std::fs::remove_file(path);
    }
    let name = crate::transport::path_to_name(path).map_err(|e| {
        tracing::error!(?path, error = %e, "could not derive ipc name from socket path");
        e
    })?;
    let listener: Listener = ListenerOptions::new()
        .name(name)
        .create_tokio()
        .map_err(|e| {
            // On Unix, AF_UNIX sun_path is 104 bytes — paths beyond
            // that fail with ENAMETOOLONG. Log the path + the OS
            // error so callers can see why the socket never
            // appeared instead of the daemon dying silently.
            tracing::error!(
                ?path,
                len = path.as_os_str().len(),
                error = %e,
                "failed to bind local socket"
            );
            e
        })?;
    // Only the owning user may talk to the daemon: the socket carries
    // cookie/auth state and drives a real browser. The parent dir is
    // 0700 (see `Paths::ensure_root`), but the socket itself may live
    // under a custom path, so tighten it explicitly too.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
            tracing::warn!(?path, error = %e, "could not restrict socket permissions");
        }
    }
    tracing::info!(?path, "vibesurfer daemon listening");

    let daemon = Arc::new(daemon);
    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown => {
                tracing::info!("shutdown requested");
                break;
            }
            accept = listener.accept() => {
                let stream = accept?;
                let daemon = daemon.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_connection(daemon, stream).await {
                        tracing::warn!(error = %e, "connection ended");
                    }
                });
            }
        }
    }

    #[cfg(unix)]
    let _ = std::fs::remove_file(path);
    Ok(())
}

/// Drive one client connection: read lines, dispatch, write responses.
async fn handle_connection(daemon: Arc<Daemon>, stream: Stream) -> std::io::Result<()> {
    let (read, mut write) = stream.split();
    let mut reader = BufReader::new(read).lines();
    while let Some(line) = reader.next_line().await? {
        if line.is_empty() {
            continue;
        }
        let resp_text = match Request::parse(&line) {
            Ok(req) => {
                tracing::info!(primitive = %req.primitive, "dispatch start");
                let primitive = req.primitive.clone();
                let daemon = daemon.clone();
                let result = tokio::task::spawn_blocking(move || {
                    let mut outcomes = daemon.dispatch(&[req]);
                    outcomes.pop().map_or_else(String::new, |o| o.wire)
                })
                .await;
                tracing::info!(primitive = %primitive, ok = result.is_ok(), "dispatch end");
                result.unwrap_or_else(|join_err| {
                    tracing::error!(primitive = %primitive, error = %join_err, "dispatch panic");
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
        "vs_goto" => lifecycle::handle_goto(daemon, req),
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
        "vs_move_to" => engine_ops::handle_cursor(daemon, req, "move_to"),
        "vs_click_at" => engine_ops::handle_cursor(daemon, req, "click_at"),
        "vs_hover_at" => engine_ops::handle_cursor(daemon, req, "hover_at"),
        "vs_drag" => engine_ops::handle_cursor(daemon, req, "drag"),
        "vs_type" => engine_ops::handle_type(daemon, req),
        "vs_prompt_input_queue" => engine_ops::handle_prompt_input_queue(daemon, req),
        "vs_prompt_form" => engine_ops::handle_prompt_form(daemon, req),
        "vs_prompt_form_wait" => engine_ops::handle_prompt_form_wait(daemon, req),
        "vs_pending_url" => engine_ops::handle_pending_url(daemon, req),
        "vs_watch" => engine_ops::handle_watch(daemon, req),
        "vs_record" => engine_ops::handle_record(daemon, req),
        "vs_frame" => engine_ops::handle_frame(daemon, req),
        "vs_pending_list" => engine_ops::handle_pending_list(daemon, req),
        "vs_pending_fulfill" => engine_ops::handle_pending_fulfill(daemon, req),
        "vs_pending_cancel" => engine_ops::handle_pending_cancel(daemon, req),
        "vs_pending_peek" => engine_ops::handle_pending_peek(daemon, req),
        other => format_error(
            ErrorCode::BadRequest,
            vec![format!("unknown primitive: {other}")],
        ),
    }
}
