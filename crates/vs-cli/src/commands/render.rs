//! Render a [`Response`] to stdout as either canonical wire form
//! (default) or strict RFC-8259 JSON (`--json`).

use crate::client::Response;

/// `--json` emits strict RFC-8259 JSON via `serde_json` (escaping,
/// surrogates, control characters all handled). The default is the
/// canonical wire form that an agent would consume directly.
#[must_use]
pub fn render(resp: &Response, json: bool) -> String {
    if json {
        let warnings: Vec<serde_json::Value> = resp
            .warnings
            .iter()
            .map(|w| {
                serde_json::json!({
                    "code": w.code.to_string(),
                    "args": w.args,
                })
            })
            .collect();
        let envelope = match &resp.envelope {
            vs_protocol::Envelope::Success(t) => serde_json::json!({
                "kind": "success",
                "token": t.to_string(),
            }),
            vs_protocol::Envelope::Error { code, args } => serde_json::json!({
                "kind": "error",
                "code": code.to_string(),
                "args": args,
            }),
        };
        let value = serde_json::json!({
            "warnings": warnings,
            "envelope": envelope,
            "body": resp.body,
        });
        let mut out =
            serde_json::to_string_pretty(&value).expect("serde_json never fails on owned strings");
        out.push('\n');
        out
    } else {
        resp.render_wire()
    }
}
