//! Store-only primitives: `vs_extract`, `vs_mark`, `vs_annotate`,
//! `vs_log`. These don't touch the engine — they read from the page
//! cache or write to the SQLite store.

use vs_protocol::{Ref, StateToken, Tree};
use vs_store::{ActionFilter, AnnotationTarget};

use super::audit::AuditCtx;
use super::responses::{AnnotateResponse, ExtractResponse, LogResponse, MarkResponse};
use super::{short_id, Daemon};
use crate::error::{DaemonError, Result};
use crate::tokens;

impl Daemon {
    /// Extract structured data from a page using a known schema.
    pub fn extract(
        &self,
        session_id: &str,
        page_id: &str,
        schema: &str,
        before_token: StateToken,
    ) -> Result<ExtractResponse> {
        let ctx = AuditCtx::new("vs_extract", session_id)
            .with_page(page_id)
            .with_args(
                schema.to_string(),
                tokens::args_hash("vs_extract", &[schema.to_string()]),
            )
            .with_before(before_token);
        self.audit_call(ctx, |ctx| {
            let current = self.current_token(session_id, page_id)?;
            if current != before_token {
                return Err(DaemonError::StaleToken {
                    current,
                    reason: "mutate",
                });
            }
            ctx.after_token = Some(current);

            let sessions = self.inner.sessions.lock().expect("poisoned");
            let page = sessions
                .get(session_id)
                .ok_or_else(|| DaemonError::UnknownSession(session_id.to_string()))?
                .pages
                .get(page_id)
                .ok_or_else(|| DaemonError::UnknownPage(page_id.to_string()))?;
            let tree = page.last_tree.as_ref().ok_or_else(|| {
                DaemonError::BadRequest("no tree cached; call vs_view first".into())
            })?;
            let records = match schema {
                "table" => extract_tables(tree),
                "list" => extract_lists(tree),
                "form" | "jsonld" | "webmcp" => {
                    // Pull the engine handle out of the page WHILE we
                    // hold the lock, then drop the lock before calling
                    // the engine — so the engine's main-thread
                    // dispatcher is free to take whatever locks it
                    // needs without deadlocking.
                    drop(sessions);
                    let engine_handle = self.engine_handle_for(session_id, page_id)?;
                    extract_via_engine(&self.inner.engine, engine_handle, schema)?
                }
                other => {
                    return Err(DaemonError::BadRequest(format!("unknown schema: {other}")));
                }
            };
            Ok(ExtractResponse {
                token: current,
                records,
            })
        })
    }

    /// Persist `r` as a named anchor in the session.
    pub fn mark(
        &self,
        session_id: &str,
        page_id: &str,
        r: Ref,
        name: &str,
        before_token: StateToken,
    ) -> Result<MarkResponse> {
        let args = vec![r.to_string(), name.to_string()];
        let ctx = AuditCtx::new("vs_mark", session_id)
            .with_page(page_id)
            .with_args(format!("{r} {name}"), tokens::args_hash("vs_mark", &args))
            .with_before(before_token);
        self.audit_call(ctx, |ctx| {
            let current = self.current_token(session_id, page_id)?;
            if current != before_token {
                return Err(DaemonError::StaleToken {
                    current,
                    reason: "mutate",
                });
            }
            ctx.after_token = Some(current);

            let (dom_path, role, excerpt) = {
                let sessions = self.inner.sessions.lock().expect("poisoned");
                let page = sessions
                    .get(session_id)
                    .ok_or_else(|| DaemonError::UnknownSession(session_id.to_string()))?
                    .pages
                    .get(page_id)
                    .ok_or_else(|| DaemonError::UnknownPage(page_id.to_string()))?;
                let node = page.find_node(r).ok_or(DaemonError::UnknownRef(r.0))?;
                (
                    format!("{}#{}", node.role, r.0),
                    Some(node.role.to_string()),
                    Some(node.label.clone()),
                )
            };

            let mark_id = format!("m_{}", short_id());
            let mut store = self.inner.store.lock().expect("poisoned");
            store.create_mark(
                &mark_id,
                session_id,
                page_id,
                name,
                &dom_path,
                role.as_deref(),
                excerpt.as_deref(),
            )?;
            Ok(MarkResponse {
                mark_id,
                token: current,
            })
        })
    }

    /// Attach `(key, value)` to `target`.
    pub fn annotate(
        &self,
        session_id: &str,
        target: &AnnotationTarget,
        key: &str,
        value: Option<&str>,
    ) -> Result<AnnotateResponse> {
        let target_str = match target {
            AnnotationTarget::Ref(r) => format!("ref:{r}"),
            AnnotationTarget::Mark(name) => format!("mark:{name}"),
            AnnotationTarget::Page => "page".to_string(),
        };
        let args = vec![target_str.clone(), key.to_string()];
        let ctx = AuditCtx::new("vs_annotate", session_id).with_args(
            format!("{target_str} {key}"),
            tokens::args_hash("vs_annotate", &args),
        );
        self.audit_call(ctx, |_ctx| {
            self.require_session(session_id)?;
            let id = format!("an_{}", short_id());
            let mut store = self.inner.store.lock().expect("poisoned");
            let row = store.add_annotation(&id, target, key, value)?;
            Ok(AnnotateResponse { id: row.id })
        })
    }

    /// Slice the audit log for the session.
    pub fn log(
        &self,
        session_id: &str,
        page_id: Option<String>,
        group_label: Option<String>,
        since_started_at: Option<i64>,
        limit: Option<i64>,
    ) -> Result<LogResponse> {
        let ctx = AuditCtx::new("vs_log", session_id)
            .with_args(String::new(), tokens::args_hash("vs_log", &[]));
        self.audit_call(ctx, |_ctx| {
            self.require_session(session_id)?;
            let filter = ActionFilter {
                session_id: Some(session_id.to_string()),
                page_id,
                group_label,
                since_started_at,
                limit,
            };
            let store = self.inner.store.lock().expect("poisoned");
            let rows = store.list_actions(&filter)?;
            Ok(LogResponse { rows })
        })
    }
}

/// Walk the tree and emit one record per `tbl` row. Each record is a
/// flat list of cell labels in document order. Rows can be nested
/// under intermediate `el` placeholders (e.g. THEAD/TBODY collapse to
/// `el` in the snapshot walker), so the row search recurses.
fn extract_tables(tree: &Tree) -> Vec<Vec<String>> {
    fn collect_rows(node: &vs_protocol::Node, out: &mut Vec<Vec<String>>) {
        if matches!(node.role, vs_protocol::Role::Row) {
            let cells: Vec<String> = collect_cells(node);
            if !cells.is_empty() {
                out.push(cells);
            }
            return;
        }
        for c in &node.children {
            collect_rows(c, out);
        }
    }
    fn collect_cells(node: &vs_protocol::Node) -> Vec<String> {
        let mut acc = Vec::new();
        for c in &node.children {
            if matches!(c.role, vs_protocol::Role::Cell | vs_protocol::Role::Hdr) {
                acc.push(c.label.clone());
            } else {
                // Cells nested under intermediate placeholders.
                acc.extend(collect_cells(c));
            }
        }
        acc
    }
    let mut out = Vec::new();
    for node in tree {
        if matches!(node.role, vs_protocol::Role::Tbl) {
            for child in &node.children {
                collect_rows(child, &mut out);
            }
        }
    }
    out
}

/// Walk the tree and emit one record per `lst` item: `[role, label]`.
fn extract_lists(tree: &Tree) -> Vec<Vec<String>> {
    fn collect_items(node: &vs_protocol::Node, out: &mut Vec<Vec<String>>) {
        if matches!(node.role, vs_protocol::Role::Itm | vs_protocol::Role::Li) {
            out.push(vec![node.role.to_string(), node.label.clone()]);
            return;
        }
        for c in &node.children {
            collect_items(c, out);
        }
    }
    let mut out = Vec::new();
    for node in tree {
        if matches!(node.role, vs_protocol::Role::Lst) {
            for child in &node.children {
                collect_items(child, &mut out);
            }
        }
    }
    out
}

/// Run a JS extractor for `schema` against the live page and parse
/// the JSON result into the same `Vec<Vec<String>>` shape the
/// tree-walking extractors return. Used for `form` / `jsonld` /
/// `webmcp` — schemas that need DOM access we don't carry on the tree.
fn extract_via_engine(
    engine: &vs_engine_webkit::EngineRuntime,
    handle: vs_engine_webkit::PageHandle,
    schema: &str,
) -> Result<Vec<Vec<String>>> {
    use vs_engine_webkit::inspector::EvalResult;
    let js = match schema {
        "form" => {
            r"(function() {
            var out = [];
            for (var i = 0; i < document.forms.length; i++) {
                var f = document.forms[i];
                for (var j = 0; j < f.elements.length; j++) {
                    var el = f.elements[j];
                    if (!el.name && !el.id) continue;
                    out.push([
                        f.id || ('form_' + i),
                        el.name || el.id,
                        el.type || el.tagName.toLowerCase(),
                        el.value || '',
                    ]);
                }
            }
            return JSON.stringify(out);
        })()"
        }
        "jsonld" => {
            r#"(function() {
            var nodes = document.querySelectorAll('script[type="application/ld+json"]');
            var out = [];
            for (var i = 0; i < nodes.length; i++) {
                out.push(['jsonld', nodes[i].textContent || '']);
            }
            return JSON.stringify(out);
        })()"#
        }
        "webmcp" => {
            r#"(function() {
            var nodes = document.querySelectorAll('script[type="application/x-webmcp"]');
            var out = [];
            for (var i = 0; i < nodes.length; i++) {
                out.push(['webmcp', nodes[i].textContent || '']);
            }
            return JSON.stringify(out);
        })()"#
        }
        _ => return Err(DaemonError::BadRequest(format!("unknown schema: {schema}"))),
    };
    let result = engine
        .eval_js(handle, js)
        .map_err(|e| DaemonError::BadRequest(format!("engine: {e}")))?;
    let value = match result {
        EvalResult::Ok { value, .. } => value,
        EvalResult::Thrown { kind, message } => {
            return Err(DaemonError::BadRequest(format!(
                "extract {schema}: {kind}: {message}"
            )));
        }
        EvalResult::Syntax { message } => {
            return Err(DaemonError::BadRequest(format!(
                "extract {schema}: syntax: {message}"
            )));
        }
    };
    // The eval helper double-encodes: `value` is already a JSON string
    // representing a JSON-encoded array. Decode once.
    let arr: serde_json::Value = serde_json::from_str(&value)
        .map_err(|e| DaemonError::BadRequest(format!("extract {schema}: parse: {e}")))?;
    let rows = arr.as_array().cloned().unwrap_or_default();
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let cells = row
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|v: &serde_json::Value| {
                        v.as_str().map_or_else(|| v.to_string(), str::to_string)
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        out.push(cells);
    }
    Ok(out)
}
