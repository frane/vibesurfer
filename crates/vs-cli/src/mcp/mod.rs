//! `vs mcp` — full MCP (Model Context Protocol) server for vibesurfer.
//!
//! Speaks JSON-RPC 2.0 over stdio. Each of the 19 vibesurfer
//! primitives is exposed as one MCP tool whose name matches the wire
//! primitive (`vs_open`, `vs_view`, etc.). Tool dispatch delegates to
//! [`crate::commands::run`] — the same code path the CLI uses — so
//! there is no parallel engine logic, no shim, no drift.
//!
//! Run as a subcommand: `vs mcp` (Claude Desktop / Claude Code spawn
//! it via their MCP server config).

mod content;
mod tools;

use std::sync::Arc;

use anyhow::{Context as _, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Stdout};
use tokio::sync::Mutex;

use crate::commands::Cli;

const MCP_VERSION: &str = "2024-11-05";
const SERVER_NAME: &str = "vibesurfer";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Synchronous entry point for `vs mcp`. Owns its own tokio runtime
/// (the rest of the `vs` binary is sync). Returns when stdin closes.
pub fn run() -> Result<()> {
    init_tracing();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .context("build tokio runtime for vs mcp")?;
    rt.block_on(serve())
}

async fn serve() -> Result<()> {
    let stdin = tokio::io::stdin();
    let stdout = Arc::new(Mutex::new(tokio::io::stdout()));
    let mut reader = BufReader::new(stdin).lines();

    while let Some(line) = reader.next_line().await? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("invalid json on stdin: {e}; raw: {line}");
                continue;
            }
        };
        let resp = handle_request(&req).await;
        if let Some(r) = resp {
            write_message(&stdout, r).await;
        }
    }
    Ok(())
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("vs_cli=info,info")),
        )
        .with_writer(std::io::stderr) // stdout is reserved for protocol
        .try_init();
}

async fn write_message(stdout: &Mutex<Stdout>, msg: Value) {
    let bytes = match serde_json::to_vec(&msg) {
        Ok(b) => b,
        Err(e) => {
            tracing::error!("serialize response: {e}");
            return;
        }
    };
    let mut out = stdout.lock().await;
    let _ = out.write_all(&bytes).await;
    let _ = out.write_all(b"\n").await;
    let _ = out.flush().await;
}

/// Top-level dispatch. Returns `Some(response)` for requests,
/// `None` for notifications (no response).
async fn handle_request(req: &Value) -> Option<Value> {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);

    let is_notification = id.is_none();

    let result: Result<Value, McpError> = match method {
        "initialize" => Ok(initialize_result()),
        "initialized" | "notifications/initialized" => return None,
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tools::list() })),
        "tools/call" => call_tool(&params).await,
        other => Err(McpError {
            code: -32601,
            message: format!("method not found: {other}"),
        }),
    };

    if is_notification {
        return None;
    }
    let id = id.unwrap_or(Value::Null);
    Some(match result {
        Ok(value) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": value,
        }),
        Err(err) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": err.code,
                "message": err.message,
            },
        }),
    })
}

fn initialize_result() -> Value {
    json!({
        "protocolVersion": MCP_VERSION,
        "capabilities": {
            "tools": { "listChanged": false },
        },
        "serverInfo": {
            "name": SERVER_NAME,
            "version": SERVER_VERSION,
        },
    })
}

#[derive(Debug)]
struct McpError {
    code: i64,
    message: String,
}

async fn call_tool(params: &Value) -> Result<Value, McpError> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| McpError {
            code: -32602,
            message: "missing tool name".into(),
        })?;
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));

    let (cli, opts) = tools::build_cli(name, &arguments).map_err(|e| McpError {
        code: -32602,
        message: e.to_string(),
    })?;

    let resp = tokio::task::spawn_blocking(move || run_cli(&cli))
        .await
        .map_err(|e| McpError {
            code: -32603,
            message: format!("blocking task: {e}"),
        })?
        .map_err(|e| McpError {
            code: -32603,
            message: format!("vs dispatch: {e:#}"),
        })?;

    // Optional action thumbnail. Failures degrade to text-only — a
    // missing screenshot must never fail the action that succeeded.
    let thumb = if opts.thumb {
        let page = opts.thumb_page.or_else(|| first_page_id(&resp));
        match page {
            Some(p) => tokio::task::spawn_blocking(move || thumb_for_page(&p))
                .await
                .ok()
                .and_then(std::result::Result::ok),
            None => None,
        }
    } else {
        None
    };

    Ok(json!({
        "content": content::shape(&resp, thumb.as_deref()),
        "isError": false,
    }))
}

/// First `p_…` token in a response body — how vs_open's fresh page id
/// is recovered for the thumbnail call.
fn first_page_id(resp: &str) -> Option<String> {
    resp.split_whitespace()
        .find(|w| w.starts_with("p_"))
        .map(ToString::to_string)
}

/// Capture `page` and downscale to a JPEG thumbnail.
fn thumb_for_page(page: &str) -> Result<Vec<u8>> {
    let (cli, _) = tools::build_cli("vs_capture", &json!({ "page": page, "base64": false }))?;
    let resp = run_cli(&cli)?;
    let path = resp
        .lines()
        .find_map(|l| {
            l.strip_prefix("path=").or_else(|| {
                let t = l.trim();
                std::path::Path::new(t)
                    .extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("png"))
                    .then_some(t)
            })
        })
        .context("capture: no png path in response")?;
    let png = std::fs::read(path.trim()).context("read capture png")?;
    content::thumbnail_jpeg(&png)
}

fn run_cli(cli: &Cli) -> Result<String> {
    let resp = crate::commands::run(cli).context("vs_cli::commands::run")?;
    Ok(crate::commands::render(&resp, false))
}
