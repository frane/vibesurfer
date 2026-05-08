#![allow(clippy::needless_pass_by_value)]
//! `vs-mcp` — full MCP (Model Context Protocol) server for vibesurfer.
//!
//! Speaks JSON-RPC 2.0 over stdio. Each of the 19 vibesurfer primitives
//! is exposed as one MCP tool whose name matches the wire primitive
//! (`vs_open`, `vs_view`, etc.). Tool dispatch delegates to
//! [`vs_cli::commands::run`] — the same code path the CLI uses — so
//! there is no parallel engine logic, no shim layer, no drift.
//!
//! Run as: `vs-mcp` (Claude Desktop / Claude Code spawn it via their
//! MCP server config).

#![forbid(unsafe_code)]

mod tools;

use std::sync::Arc;

use anyhow::{Context as _, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Stdout};
use tokio::sync::Mutex;

use vs_cli::commands::Cli;

const MCP_VERSION: &str = "2024-11-05";
const SERVER_NAME: &str = "vibesurfer";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    init_tracing();

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
        let stdout = stdout.clone();
        tokio::spawn(async move {
            let resp = handle_request(&req).await;
            if let Some(r) = resp {
                write_message(&stdout, r).await;
            }
        });
    }
    Ok(())
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("vs_mcp=info,info")),
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

    // Build a vs-cli `Cli` from the tool name + arguments. Then run
    // the same dispatch path the CLI binary uses.
    let cli = tools::build_cli(name, &arguments).map_err(|e| McpError {
        code: -32602,
        message: e.to_string(),
    })?;

    let resp = tokio::task::spawn_blocking(move || run_cli(cli))
        .await
        .map_err(|e| McpError {
            code: -32603,
            message: format!("blocking task: {e}"),
        })?
        .map_err(|e| McpError {
            code: -32603,
            message: format!("vs dispatch: {e:#}"),
        })?;

    Ok(json!({
        "content": [{
            "type": "text",
            "text": resp,
        }],
        "isError": false,
    }))
}

/// Run the CLI dispatch and return the rendered wire response.
fn run_cli(cli: Cli) -> Result<String> {
    let resp = vs_cli::commands::run(&cli).context("vs_cli::commands::run")?;
    Ok(vs_cli::commands::render(&resp, false))
}
