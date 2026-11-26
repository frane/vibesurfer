#![allow(clippy::too_many_lines, clippy::needless_pass_by_value)]
//! Turning an MCP `tools/call` into a `crate::commands::Cli`, so a call
//! dispatches through the same code path as the CLI binary. No parallel
//! engine logic. The schemas live in [`super::schema`].

use anyhow::{anyhow, Context as _, Result};
use serde_json::Value;

use crate::commands::{Cli, Command};

/// Per-call presentation options that ride alongside the `Cli`.
#[derive(Debug, Default)]
pub struct CallOpts {
    /// Attach a screenshot image block after the call. Set by the
    /// `capture` arg on vs_act / vs_open, or forced by `VS_THUMBS=1`.
    pub thumb: bool,
    /// Page to screenshot. `None` for vs_open — the page id is only
    /// known from the response body.
    pub thumb_page: Option<String>,
}

/// Build a `crate::commands::Cli` from an MCP tool call. Returns an
/// error if the arguments don't match what the primitive needs.
pub fn build_cli(name: &str, args: &Value) -> Result<(Cli, CallOpts)> {
    let thumbs_env = std::env::var("VS_THUMBS").is_ok_and(|v| v == "1");
    let mut opts = CallOpts::default();
    if matches!(name, "vs_act" | "vs_open") {
        opts.thumb = opt_bool(args, "capture").unwrap_or(thumbs_env);
        opts.thumb_page = opt_str(args, "page");
    }
    let cmd = match name {
        "vs_session_open" => Command::SessionOpen {
            policy: opt_str(args, "policy"),
            isolated: opt_bool(args, "isolated").unwrap_or(false),
            intercept: opt_bool(args, "intercept").unwrap_or(false),
            popups: opt_bool(args, "popups").unwrap_or(false),
            ignore_https_errors: opt_bool(args, "ignore_https_errors").unwrap_or(false),
        },
        "vs_session_close" => Command::SessionClose {
            all: opt_bool(args, "all").unwrap_or(false),
            idle_for: opt_str(args, "idle_for"),
        },
        "vs_open" => Command::Open {
            url: req_str(args, "url")?,
        },
        "vs_goto" => Command::Goto {
            page: req_str(args, "page")?,
            url: req_str(args, "url")?,
            html: None,
        },
        "vs_close" => Command::Close {
            page: req_str(args, "page")?,
        },
        "vs_view" => Command::View {
            page: req_str(args, "page")?,
            full: opt_bool(args, "full").unwrap_or(false),
        },
        "vs_read" => Command::Read {
            page: req_str(args, "page")?,
            r: req_u32(args, "ref")?,
        },
        "vs_act" => Command::Act {
            page: req_str(args, "page")?,
            // Either addressing works; `mark` wins if both are given.
            r: match opt_str(args, "mark") {
                Some(name) => format!("mark:{name}"),
                None => req_u32(args, "ref")?.to_string(),
            },
            op: req_str(args, "op")?,
            value: opt_str(args, "value"),
            token: req_str(args, "token")?,
            group: opt_str(args, "group"),
            mode: opt_str(args, "mode").unwrap_or_else(|| "careful".into()),
        },
        "vs_find" => Command::Find {
            query: req_str(args, "query")?,
        },
        "vs_wait" => Command::Wait {
            page: req_str(args, "page")?,
            cond: req_str(args, "cond")?,
            value: opt_str(args, "value"),
            timeout: opt_u64(args, "timeout").unwrap_or(5000),
        },
        "vs_extract" => Command::Extract {
            page: req_str(args, "page")?,
            schema: req_str(args, "schema")?,
            token: req_str(args, "token")?,
        },
        "vs_mark" => Command::Mark {
            page: req_str(args, "page")?,
            r: req_u32(args, "ref")?,
            name: req_str(args, "name")?,
            token: req_str(args, "token")?,
        },
        "vs_annotate" => Command::Annotate {
            target: req_str(args, "target")?,
            key: req_str(args, "key")?,
            value: opt_str(args, "value"),
        },
        "vs_status" => Command::Status,
        "vs_log" => Command::Log {
            page: opt_str(args, "page"),
            group: opt_str(args, "group"),
            since: opt_i64(args, "since"),
            limit: opt_i64(args, "limit"),
        },
        "vs_skill" => Command::Skill {
            sub: opt_str(args, "sub"),
            name: opt_str(args, "name"),
        },
        "vs_capture" => Command::Capture {
            clean: None,
            page: Some(req_str(args, "page")?),
            r: opt_u32(args, "ref"),
            full_page: opt_bool(args, "full_page").unwrap_or(false),
            // Default base64 ON over MCP — agents need bytes inline.
            base64: opt_bool(args, "base64").unwrap_or(true),
        },
        "vs_download" => Command::Download {
            page: req_str(args, "page")?,
            url: opt_str(args, "url"),
            dest: opt_str(args, "dest"),
            list: opt_bool(args, "list").unwrap_or(false),
            id: opt_u64(args, "id"),
            timeout_ms: opt_u64(args, "timeout_ms").unwrap_or(30_000),
        },
        "vs_viewport" => Command::Viewport {
            page: req_str(args, "page")?,
            spec: req_str(args, "spec")?,
            dpr: opt_u32(args, "dpr").unwrap_or(2),
            zoom: args.get("zoom").and_then(Value::as_f64),
        },
        "vs_layout" => {
            let refs_arr = args
                .get("refs")
                .and_then(Value::as_array)
                .ok_or_else(|| anyhow!("vs_layout: missing refs array"))?;
            let refs: Vec<u32> = refs_arr
                .iter()
                .map(|v| {
                    v.as_u64()
                        .and_then(|n| u32::try_from(n).ok())
                        .ok_or_else(|| anyhow!("vs_layout: refs must be positive integers"))
                })
                .collect::<Result<_>>()?;
            Command::Layout {
                page: req_str(args, "page")?,
                refs,
            }
        }
        "vs_auth" => {
            let rest = args
                .get("rest")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect::<Vec<String>>()
                })
                .unwrap_or_default();
            Command::Auth {
                sub: req_str(args, "sub")?,
                rest,
            }
        }
        "vs_move_to" => Command::MoveTo {
            page: req_str(args, "page")?,
            x: req_f64(args, "x")?,
            y: req_f64(args, "y")?,
            mode: opt_str(args, "mode").unwrap_or_else(|| "human".into()),
        },
        "vs_click_at" => Command::ClickAt {
            page: req_str(args, "page")?,
            x: req_f64(args, "x")?,
            y: req_f64(args, "y")?,
            token: req_str(args, "token")?,
            mode: opt_str(args, "mode").unwrap_or_else(|| "human".into()),
            button: opt_str(args, "button"),
            count: args
                .get("count")
                .and_then(Value::as_u64)
                .and_then(|c| u8::try_from(c).ok()),
            modifiers: opt_str(args, "modifiers"),
        },
        "vs_type" => Command::Type {
            page: req_str(args, "page")?,
            text: req_str(args, "text")?,
            secret: opt_bool(args, "secret").unwrap_or(false),
            mode: opt_str(args, "mode").unwrap_or_else(|| "human".into()),
        },
        "vs_hover_at" => Command::HoverAt {
            page: req_str(args, "page")?,
            x: req_f64(args, "x")?,
            y: req_f64(args, "y")?,
            mode: opt_str(args, "mode").unwrap_or_else(|| "human".into()),
        },
        "vs_cookies" => Command::Cookies {
            page: req_str(args, "page")?,
            op: req_str(args, "op")?,
            data: opt_str(args, "json"),
        },
        "vs_inspect" => Command::Inspect {
            page: req_str(args, "page")?,
            kind: req_str(args, "kind")?,
            rest: str_list(args, "args"),
            since: opt_str(args, "since"),
            level: opt_str(args, "level"),
            status: opt_str(args, "status"),
            max: opt_str(args, "max"),
            full: opt_bool(args, "full").unwrap_or(false),
            frame: opt_str(args, "frame"),
            unsafe_log: opt_bool(args, "unsafe_log").unwrap_or(false),
        },
        "vs_init_script" => Command::InitScript {
            page: req_str(args, "page")?,
            source: req_str(args, "source")?,
        },
        "vs_net" => Command::Net {
            sub: net_sub(args)?,
        },
        "vs_drop" => Command::Drop {
            page: req_str(args, "page")?,
            x: req_f64(args, "x")?,
            y: req_f64(args, "y")?,
            files: args
                .get("files")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
            data: args
                .get("data")
                .and_then(Value::as_object)
                .map(|o| {
                    o.iter()
                        .filter_map(|(k, v)| v.as_str().map(|v| format!("{k}={v}")))
                        .collect()
                })
                .unwrap_or_default(),
        },
        "vs_wheel" => Command::Wheel {
            page: req_str(args, "page")?,
            x: req_f64(args, "x")?,
            y: req_f64(args, "y")?,
            dx: req_f64(args, "dx")?,
            dy: req_f64(args, "dy")?,
            mode: opt_str(args, "mode").unwrap_or_else(|| "human".into()),
        },
        "vs_drag" => Command::Drag {
            page: req_str(args, "page")?,
            x1: req_f64(args, "x1")?,
            y1: req_f64(args, "y1")?,
            x2: req_f64(args, "x2")?,
            y2: req_f64(args, "y2")?,
            token: req_str(args, "token")?,
            mode: opt_str(args, "mode").unwrap_or_else(|| "human".into()),
        },
        "vs_prompt_input" => Command::PromptInputQueue {
            // MCP route: enqueues a pending entry, blocks until the
            // local user runs `vs pending fulfill`. The MCP subprocess
            // has no tty so the local-CLI tty path won't work.
            page: req_str(args, "page")?,
            r: req_u32(args, "ref")?,
            message: req_str(args, "message")?,
            secret: opt_bool(args, "secret").unwrap_or(false),
            token: req_str(args, "token")?,
            group: opt_str(args, "group"),
            timeout_ms: opt_u32(args, "timeout_ms").map_or(300_000, u64::from),
        },
        "vs_prompt_form" => {
            let fields = args
                .get("fields")
                .and_then(Value::as_array)
                .ok_or_else(|| anyhow!("vs_prompt_form: missing fields array"))?
                .iter()
                .map(|f| {
                    let r = f
                        .get("ref")
                        .and_then(Value::as_u64)
                        .ok_or_else(|| anyhow!("vs_prompt_form: field without ref"))?;
                    let label = f
                        .get("label")
                        .and_then(Value::as_str)
                        .ok_or_else(|| anyhow!("vs_prompt_form: field without label"))?;
                    let secret = f.get("secret").and_then(Value::as_bool).unwrap_or(false);
                    Ok(format!(
                        "{r}={label}{}",
                        if secret { ",secret" } else { "" }
                    ))
                })
                .collect::<Result<Vec<_>>>()?;
            Command::PromptForm {
                page: req_str(args, "page")?,
                fields,
                token: req_str(args, "token")?,
                group: opt_str(args, "group"),
                open: false,
                // MCP is a two-step dance: return the URL now, park in
                // vs_prompt_form_wait after the agent relays it.
                no_wait: true,
                timeout_ms: 300_000,
            }
        }
        "vs_prompt_form_wait" => Command::PromptFormWait {
            form: req_str(args, "form")?,
            timeout_ms: opt_u32(args, "timeout_ms").map_or(300_000, u64::from),
        },
        "vs_watch" => Command::Watch {
            page: req_str(args, "page")?,
            open: false,
        },
        "vs_record_start" => Command::Record {
            sub: crate::commands::RecordSub::Start {
                page: req_str(args, "page")?,
                fps: opt_u32(args, "fps").unwrap_or(24),
                width: opt_u32(args, "width"),
                retina: opt_bool(args, "retina").unwrap_or(false),
                format: opt_str(args, "format"),
            },
        },
        "vs_record_stop" => Command::Record {
            sub: crate::commands::RecordSub::Stop {
                page: req_str(args, "page")?,
            },
        },
        "vs_pending_url" => Command::Pending {
            sub: crate::commands::PendingSub::Url,
        },
        "vs_prompt_confirm" => Command::PromptConfirm {
            page: req_str(args, "page")?,
            message: req_str(args, "message")?,
        },
        other => return Err(anyhow!("unknown tool: {other}")),
    };
    Ok((
        Cli {
            session: None,
            socket: None,
            home: None,
            no_spawn: false,
            json: false,
            command: cmd,
        },
        opts,
    ))
}

/// `vs_net` arguments as the matching `vs net` subcommand.
fn net_sub(args: &Value) -> Result<crate::commands::NetSub> {
    use crate::commands::{NetOverrides, NetSub};
    let id = || opt_u64(args, "id").ok_or_else(|| anyhow!("vs_net: this op needs id"));
    let o = || NetOverrides {
        url: opt_str(args, "url"),
        method: opt_str(args, "method"),
        headers: str_list(args, "headers"),
        body_b64: opt_str(args, "body_b64"),
    };
    Ok(match req_str(args, "op")?.as_str() {
        "poll" => NetSub::Poll {
            wait_ms: opt_u64(args, "wait_ms").unwrap_or(0),
        },
        "route" => NetSub::Route {
            state: req_str(args, "state")?,
        },
        "continue" => NetSub::Continue { id: id()?, o: o() },
        "fetch" => NetSub::Fetch { id: id()?, o: o() },
        "fulfill" => NetSub::Fulfill {
            id: id()?,
            status: opt_u64(args, "status")
                .and_then(|s| u16::try_from(s).ok())
                .unwrap_or(200),
            headers: str_list(args, "headers"),
            body_b64: opt_str(args, "body_b64"),
        },
        "abort" => NetSub::Abort {
            id: id()?,
            error: opt_str(args, "error"),
        },
        "body" => NetSub::Body { id: id()? },
        "dialog" => NetSub::Dialog {
            id: id()?,
            action: req_str(args, "action")?,
            text: opt_str(args, "text"),
        },
        other => return Err(anyhow!("vs_net: unknown op {other}")),
    })
}

/// A string array argument; missing or mistyped is empty.
fn str_list(args: &Value, key: &str) -> Vec<String> {
    args.get(key)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

// =============================================================================
// Argument extractors
// =============================================================================

fn req_str(args: &Value, key: &str) -> Result<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .with_context(|| format!("missing required string `{key}`"))
}

fn opt_str(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(Value::as_str).map(str::to_string)
}

fn req_u32(args: &Value, key: &str) -> Result<u32> {
    args.get(key)
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
        .with_context(|| format!("missing required positive integer `{key}`"))
}

fn req_f64(args: &Value, key: &str) -> Result<f64> {
    args.get(key)
        .and_then(Value::as_f64)
        .ok_or_else(|| anyhow!("missing required f64 arg `{key}`"))
}

fn opt_u32(args: &Value, key: &str) -> Option<u32> {
    args.get(key)
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
}

fn opt_u64(args: &Value, key: &str) -> Option<u64> {
    args.get(key).and_then(Value::as_u64)
}

fn opt_i64(args: &Value, key: &str) -> Option<i64> {
    args.get(key).and_then(Value::as_i64)
}

fn opt_bool(args: &Value, key: &str) -> Option<bool> {
    args.get(key).and_then(Value::as_bool)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::schema::list;
    use serde_json::json;

    /// Regression: every emitted tool schema must be plain JSON
    /// Schema 2020-12. The `*_prop` helpers used to leak a per-property
    /// `required: true` sentinel into the published schema, which the
    /// Anthropic API rejects (`tools.N.custom.input_schema: JSON
    /// schema is invalid`). The `obj` builder now hoists the sentinel
    /// into the parent `required` array and removes it from the
    /// property — this test pins that contract.
    #[test]
    fn no_property_carries_required_sentinel() {
        for tool in list(true) {
            let name = tool["name"].as_str().unwrap_or("?");
            let schema = &tool["inputSchema"];
            let props = schema["properties"]
                .as_object()
                .unwrap_or_else(|| panic!("{name}: properties not an object"));
            for (prop_name, prop) in props {
                assert!(
                    prop.get("required").is_none(),
                    "{name}.{prop_name} carries an inline `required` key; \
                     the helpers' sentinel must be stripped by `obj()`",
                );
            }
            // And the parent `required` array exists and is a list.
            assert!(
                schema.get("required").and_then(Value::as_array).is_some(),
                "{name}: parent `required` array missing",
            );
        }
    }

    /// Every listed tool has a builder: a call with no arguments fails on
    /// a missing argument, never as an unknown tool.
    #[test]
    fn every_listed_tool_builds() {
        for tool in list(false) {
            let name = tool["name"].as_str().unwrap_or("?");
            if let Err(e) = build_cli(name, &json!({})) {
                assert!(!e.to_string().contains("unknown tool"), "{name}: {e}");
            }
        }
    }

    /// Every CLI command an agent can use is an MCP tool too. A new
    /// subcommand fails this until it gets one or a reason below.
    #[test]
    fn every_cli_command_is_a_tool() {
        use clap::CommandFactory as _;
        // Not tools: the human side of a prompt (pending fulfill/cancel
        // read the local tty; vs_pending_url is the agent half), daemon
        // plumbing, and flow, which batches CLI calls that MCP makes one
        // tool call each. prompt-scan over MCP is vs_watch plus
        // vs_prompt_confirm.
        let not_tools = [
            "pending",
            "socket",
            "serve",
            "mcp",
            "flow",
            "prompt-scan",
            "help",
        ];
        // One tool for a command with subcommands.
        let renamed = [("record", "vs_record_start")];
        let tools: Vec<String> = list(false)
            .iter()
            .filter_map(|t| t["name"].as_str().map(str::to_string))
            .collect();
        for sub in crate::commands::Cli::command().get_subcommands() {
            let cli = sub.get_name();
            if not_tools.contains(&cli) || sub.is_hide_set() {
                continue;
            }
            let want = renamed.iter().find(|(c, _)| *c == cli).map_or_else(
                || format!("vs_{}", cli.replace('-', "_")),
                |(_, t)| (*t).to_string(),
            );
            assert!(tools.contains(&want), "`vs {cli}` has no MCP tool {want}");
        }
    }

    #[test]
    fn net_dialog_answers_a_prompt() {
        let (cli, _) = build_cli(
            "vs_net",
            &json!({"op": "dialog", "id": 3, "action": "accept", "text": "Ada"}),
        )
        .unwrap();
        let Command::Net {
            sub: crate::commands::NetSub::Dialog { id, action, text },
        } = cli.command
        else {
            panic!("not a dialog: {:?}", cli.command);
        };
        assert_eq!(
            (id, action.as_str(), text.as_deref()),
            (3, "accept", Some("Ada"))
        );
    }
}
