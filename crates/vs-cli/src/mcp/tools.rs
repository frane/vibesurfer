#![allow(clippy::too_many_lines, clippy::needless_pass_by_value)]
//! MCP tool definitions for the 19 vibesurfer primitives.
//!
//! Two responsibilities:
//! 1. [`list`] — emits the JSON Schema array for `tools/list`.
//! 2. [`build_cli`] — turns an `(name, arguments)` MCP request into a
//!    `crate::commands::Cli` so we can dispatch through the same
//!    code path the CLI binary uses. No parallel engine logic.

use anyhow::{anyhow, Context as _, Result};
use serde_json::{json, Value};

use crate::commands::{Cli, Command};

/// JSON Schema list for `tools/list`. Each tool's name matches the
/// vibesurfer wire primitive (`vs_open`, `vs_view`, …), so an MCP
/// client can recognize them by their wire name.
pub fn list() -> Vec<Value> {
    vec![
        tool("vs_session_open", "Create a vibesurfer session. Writes the active-session pointer.", obj(&[
            ("policy", str_prop("Optional policy id (e.g. \"strict\", \"default\").", false)),
        ])),
        tool("vs_session_close", "Close the active session.", obj(&[])),
        tool("vs_open", "Open a page in the active session.", obj(&[
            ("url", str_prop("URL to navigate to.", true)),
            ("capture", bool_prop("Attach a screenshot image block to the result. Default false; VS_THUMBS=1 forces on.", false)),
        ])),
        tool("vs_close", "Close a page.", obj(&[
            ("page", str_prop("Page id (e.g. p_xxxxxx).", true)),
        ])),
        tool("vs_view", "Snapshot the a11y tree of a page. First call after open is a full tree; subsequent calls are deltas unless --full is set.", obj(&[
            ("page", str_prop("Page id.", true)),
            ("full", bool_prop("Force a full re-baseline instead of a delta.", false)),
        ])),
        tool("vs_read", "Read the full text of a single ref on a page.", obj(&[
            ("page", str_prop("Page id.", true)),
            ("ref", uint_prop("Ref number from a previous vs_view.", true)),
        ])),
        tool("vs_act", "Perform an action on a ref. Requires the state token from the most recent vs_view (optimistic concurrency).", obj(&[
            ("page", str_prop("Page id.", true)),
            ("ref", uint_prop("Ref number.", true)),
            ("op", str_prop("One of: click, fill, scroll, key, submit, hover, focus.", true)),
            ("value", str_prop("Optional value (text for fill, key chord for key, etc.).", false)),
            ("token", str_prop("State token from the most recent read.", true)),
            ("group", str_prop("Optional audit-group label (e.g. \"login-flow\").", false)),
            ("capture", bool_prop("Attach a screenshot image block to the result. Default false; VS_THUMBS=1 forces on.", false)),
        ])),
        tool("vs_prompt_input", "Prompt the human at the local terminal for a value, then fill it into a ref. The agent that invokes this never sees the value the user types — vs reads from /dev/tty itself and ships the bytes to the daemon, which writes them into the field via the trusted prototype-setter fill path. Use --secret for passwords, TANs, credit-card numbers, and anything else the agent should not see.", obj(&[
            ("page", str_prop("Page id.", true)),
            ("ref", uint_prop("Ref number of the input field.", true)),
            ("message", str_prop("Prompt text shown to the human. Include the field label and any disambiguating context (e.g. \"Flatex Kundennummer:\", \"TAN sent to your phone:\").", true)),
            ("secret", bool_prop("Disable terminal echo while reading. Default: false. Set true for any credential.", false)),
            ("token", str_prop("State token from the most recent read.", true)),
            ("group", str_prop("Optional audit-group label.", false)),
        ])),
        tool("vs_prompt_form", "Ask the human for several values at once (e.g. a full login) via a browser form. Returns `form\\t<id>` and `url\\t<single-use localhost URL>` immediately. Relay the URL to the user verbatim, then call vs_prompt_form_wait with the form id. The daemon fills each ref on submit; you never see the values.", obj(&[
            ("page", str_prop("Page id.", true)),
            ("fields", json!({
                "type": "array",
                "description": "Fields in fill order.",
                "required": true,
                "items": {
                    "type": "object",
                    "properties": {
                        "ref": {"type": "integer", "description": "Ref number of the input to fill."},
                        "label": {"type": "string", "description": "Human-facing label shown on the form (e.g. \"Flatex Kundennummer\")."},
                        "secret": {"type": "boolean", "description": "Render as a masked password input. Default false."}
                    },
                    "required": ["ref", "label"]
                }
            })),
            ("token", str_prop("State token from the most recent read.", true)),
            ("group", str_prop("Optional audit-group label.", false)),
        ])),
        tool("vs_prompt_form_wait", "Park until the vs_prompt_form form is submitted, then fill the refs in order. Returns the new state token. Call right after relaying the URL.", obj(&[
            ("form", str_prop("Form id from vs_prompt_form.", true)),
            ("timeout_ms", uint_prop("How long to wait, in milliseconds. Default 300000 (5 min).", false)),
        ])),
        tool("vs_pending_url", "Mint a single-use localhost URL where the human can fulfill every pending prompt entry as one browser form. Returns `url\\t<URL>`.", obj(&[])),
        tool("vs_prompt_confirm", "Block until the human at the local terminal presses Enter. Returns `ok` on confirm or aborts on EOF / Ctrl-C. Use as a human-in-loop gate before a sensitive vs_act click (e.g. \"about to transfer $5000 — Enter to confirm\"). No state change; the next call after this still uses whatever state token the agent already had.", obj(&[
            ("page", str_prop("Page id (passed for audit context; the primitive itself does not touch the page).", true)),
            ("message", str_prop("Prompt text shown to the human. State what they are confirming.", true)),
        ])),
        tool("vs_move_to", "Move the cursor to (x, y) along a humanized Bezier path. Trusted NSEvent dispatch on macOS; ENGINE_UNSUPPORTED on Linux/Windows for now.", obj(&[
            ("page", str_prop("Page id.", true)),
            ("x", num_prop("Target X in CSS pixels (client space, top-left origin).", true)),
            ("y", num_prop("Target Y in CSS pixels.", true)),
            ("mode", str_prop("Input mode: human | careful | robotic. Default: human.", false)),
        ])),
        tool("vs_click_at", "Click at exact coordinates. Trusted (isTrusted=true) on macOS; the Bezier lead-in from the last cursor position simulates real user motion.", obj(&[
            ("page", str_prop("Page id.", true)),
            ("x", num_prop("Click X in CSS pixels.", true)),
            ("y", num_prop("Click Y in CSS pixels.", true)),
            ("token", str_prop("State token from the most recent read.", true)),
            ("mode", str_prop("Input mode: human | careful | robotic. Default: human.", false)),
        ])),
        tool("vs_hover_at", "Hover at coordinates without clicking.", obj(&[
            ("page", str_prop("Page id.", true)),
            ("x", num_prop("Hover X in CSS pixels.", true)),
            ("y", num_prop("Hover Y in CSS pixels.", true)),
            ("mode", str_prop("Input mode: human | careful | robotic. Default: human.", false)),
        ])),
        tool("vs_drag", "Press at (x1, y1), drag along a humanized path to (x2, y2), release.", obj(&[
            ("page", str_prop("Page id.", true)),
            ("x1", num_prop("Drag start X.", true)),
            ("y1", num_prop("Drag start Y.", true)),
            ("x2", num_prop("Drag end X.", true)),
            ("y2", num_prop("Drag end Y.", true)),
            ("token", str_prop("State token from the most recent read.", true)),
            ("mode", str_prop("Input mode: human | careful | robotic. Default: human.", false)),
        ])),
        tool("vs_find", "Search across all open pages in the session.", obj(&[
            ("query", str_prop("Substring or pattern.", true)),
        ])),
        tool("vs_wait", "Wait for a condition on a page (stable | text | ref-appears | ref-gone | net-idle | token-change).", obj(&[
            ("page", str_prop("Page id.", true)),
            ("cond", str_prop("Condition name.", true)),
            ("value", str_prop("Optional argument (text substring, ref number).", false)),
            ("timeout", uint_prop("Max wait in milliseconds (default 5000).", false)),
        ])),
        tool("vs_extract", "Extract structured data using a known schema (list, table; form/jsonld/webmcp not yet implemented in the stub).", obj(&[
            ("page", str_prop("Page id.", true)),
            ("schema", str_prop("Schema name.", true)),
            ("token", str_prop("State token from the most recent read.", true)),
        ])),
        tool("vs_mark", "Persist a ref as a named anchor in the session.", obj(&[
            ("page", str_prop("Page id.", true)),
            ("ref", uint_prop("Ref number.", true)),
            ("name", str_prop("Anchor name (unique per session).", true)),
            ("token", str_prop("State token from the most recent read.", true)),
        ])),
        tool("vs_annotate", "Attach a (key, value) annotation to a target. Target is one of \"ref:N\", \"mark:NAME\", or \"page\".", obj(&[
            ("target", str_prop("Annotation target (ref:N | mark:NAME | page).", true)),
            ("key", str_prop("Key.", true)),
            ("value", str_prop("Optional value.", false)),
        ])),
        tool("vs_status", "Active session + open pages + backend capabilities.", obj(&[])),
        tool("vs_log", "Slice the audit log.", obj(&[
            ("page", str_prop("Filter by page id.", false)),
            ("group", str_prop("Filter by audit group label.", false)),
            ("since", uint_prop("Filter to actions started at or after this epoch second.", false)),
            ("limit", uint_prop("Max number of rows.", false)),
        ])),
        tool("vs_skill", "Skill bundle management (list, show <name>).", obj(&[
            ("sub", str_prop("Subcommand: list or show.", false)),
            ("name", str_prop("Skill name (for show).", false)),
        ])),
        tool("vs_capture", "Take a screenshot. Defaults to viewport scope; pass a ref to capture an element, or full_page=true.", obj(&[
            ("page", str_prop("Page id.", true)),
            ("ref", uint_prop("Optional ref to capture instead of the viewport.", false)),
            ("full_page", bool_prop("Capture the full document, not just the viewport.", false)),
        ])),
        tool("vs_viewport", "Set the viewport. Spec is a preset (mobile, desktop, etc.) or WxH.", obj(&[
            ("page", str_prop("Page id.", true)),
            ("spec", str_prop("Preset name or WxH.", true)),
            ("dpr", uint_prop("Device-pixel ratio (default 2).", false)),
        ])),
        tool("vs_layout", "Compute layout boxes (getBoundingClientRect) for one or more refs.", obj(&[
            ("page", str_prop("Page id.", true)),
            ("refs", json!({
                "type": "array",
                "items": { "type": "integer", "minimum": 1 },
                "minItems": 1,
                "description": "Ref numbers."
            })),
        ])),
        tool("vs_auth", "Persist or restore per-origin auth (cookies + storage). Sub: save | load | list | clear.", obj(&[
            ("sub", str_prop("save | load | list | clear.", true)),
            ("rest", json!({
                "type": "array",
                "items": { "type": "string" },
                "description": "Remaining positional args (e.g. PAGE NAME for save/load).",
                "default": []
            })),
        ])),
    ]
}

/// Build a `crate::commands::Cli` from an MCP tool call. Returns an
/// error if the arguments don't match what the primitive needs.
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
        },
        "vs_session_close" => Command::SessionClose,
        "vs_open" => Command::Open {
            url: req_str(args, "url")?,
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
            r: req_u32(args, "ref")?,
            op: req_str(args, "op")?,
            value: opt_str(args, "value"),
            token: req_str(args, "token")?,
            group: opt_str(args, "group"),
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
        "vs_viewport" => Command::Viewport {
            page: req_str(args, "page")?,
            spec: req_str(args, "spec")?,
            dpr: opt_u32(args, "dpr").unwrap_or(2),
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
        },
        "vs_hover_at" => Command::HoverAt {
            page: req_str(args, "page")?,
            x: req_f64(args, "x")?,
            y: req_f64(args, "y")?,
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

// =============================================================================
// JSON Schema helpers
// =============================================================================

fn tool(name: &str, description: &str, schema: Value) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": schema,
    })
}

fn obj(props: &[(&str, Value)]) -> Value {
    let mut properties = serde_json::Map::new();
    let mut required = Vec::new();
    for (name, schema) in props {
        let mut schema = schema.clone();
        // The `*_prop` helpers tag the property with a sentinel
        // `required: true`. We hoist that into the parent object's
        // `required` array (where JSON Schema 2020-12 expects it)
        // and strip it from the property itself — leaving it inline
        // produces a schema that the Anthropic API rejects as
        // invalid.
        let is_required = schema
            .as_object_mut()
            .and_then(|m| m.remove("required"))
            .and_then(|v| v.as_bool())
            == Some(true);
        properties.insert((*name).into(), schema);
        if is_required {
            required.push(Value::String((*name).into()));
        }
    }
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

fn str_prop(desc: &str, required: bool) -> Value {
    let mut v = json!({ "type": "string", "description": desc });
    if required {
        v.as_object_mut()
            .unwrap()
            .insert("required".into(), Value::Bool(true));
    }
    v
}

fn uint_prop(desc: &str, required: bool) -> Value {
    let mut v = json!({ "type": "integer", "minimum": 0, "description": desc });
    if required {
        v.as_object_mut()
            .unwrap()
            .insert("required".into(), Value::Bool(true));
    }
    v
}

fn num_prop(desc: &str, required: bool) -> Value {
    let mut v = json!({ "type": "number", "description": desc });
    if required {
        v.as_object_mut()
            .unwrap()
            .insert("required".into(), Value::Bool(true));
    }
    v
}

fn bool_prop(desc: &str, required: bool) -> Value {
    let mut v = json!({ "type": "boolean", "description": desc });
    if required {
        v.as_object_mut()
            .unwrap()
            .insert("required".into(), Value::Bool(true));
    }
    v
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

    /// Regression: every emitted tool schema must be plain JSON
    /// Schema 2020-12. The `*_prop` helpers used to leak a per-property
    /// `required: true` sentinel into the published schema, which the
    /// Anthropic API rejects (`tools.N.custom.input_schema: JSON
    /// schema is invalid`). The `obj` builder now hoists the sentinel
    /// into the parent `required` array and removes it from the
    /// property — this test pins that contract.
    #[test]
    fn no_property_carries_required_sentinel() {
        for tool in list() {
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
}
