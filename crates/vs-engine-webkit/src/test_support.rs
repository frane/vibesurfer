//! In-process `TestEngine` — a fake `Engine` impl used by tests.
//!
//! [`TestEngine`] satisfies the [`Engine`](crate::engine::Engine) trait
//! without any FFI. It maintains an in-memory map of pages and serves
//! a canned a11y tree on `snapshot`. Daemon tests use it to exercise
//! request/response, audit, store, and ref-resolution code paths
//! without booting a real WebKit. Sealed behind the `test-support` Cargo feature so production binaries cannot link it.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Duration;

use vs_protocol::{Node, Op, Ref, Role, Tree};

use crate::engine::{
    ActTarget, Action, AuthBlob, CaptureScope, Engine, EngineCapabilities, EngineError,
    EngineResult, LayoutBox, PageHandle, Viewport, WaitCondition,
};

/// 1×1 transparent PNG. Used by the test engine so the protocol's
/// `vs_capture` path can be exercised end-to-end without a real
/// renderer; meaningful pixels arrive with M3b/M3c.
const ONE_BY_ONE_PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x62, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE,
    0x42, 0x60, 0x82,
];
/// Per-page state held by [`TestEngine`].
#[derive(Debug, Clone)]
struct TestPage {
    url: String,
    viewport: Viewport,
    /// Auth blob "applied" to this page. TestEngine doesn't actually do
    /// anything with it — round-trip only.
    auth: Option<AuthBlob>,
    /// Wall-clock when the page was opened. Used as the base for
    /// synthetic console / network entry timestamps so they have
    /// stable timestamps across multiple `console_entries` calls.
    opened_at: std::time::SystemTime,
}

/// In-process engine for tests and daemon plumbing. Sealed behind the `test-support` feature.
#[derive(Debug, Default)]
pub struct TestEngine {
    next_handle: u64,
    pages: BTreeMap<PageHandle, TestPage>,
    capture_dir: Option<std::path::PathBuf>,
    capture_seq: u64,
}

impl TestEngine {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Configure where `capture` writes its output. The default
    /// (`None`) uses `std::env::temp_dir()`. The daemon sets this to
    /// `~/.vibesurfer/captures` at startup.
    #[must_use]
    pub fn with_capture_dir(mut self, dir: impl Into<std::path::PathBuf>) -> Self {
        self.capture_dir = Some(dir.into());
        self
    }

    fn alloc_handle(&mut self) -> PageHandle {
        self.next_handle += 1;
        PageHandle(self.next_handle)
    }

    fn page(&self, page: PageHandle) -> EngineResult<&TestPage> {
        self.pages.get(&page).ok_or(EngineError::NotFound {
            kind: "page",
            id: page.0.to_string(),
        })
    }

    fn page_mut(&mut self, page: PageHandle) -> EngineResult<&mut TestPage> {
        self.pages.get_mut(&page).ok_or(EngineError::NotFound {
            kind: "page",
            id: page.0.to_string(),
        })
    }

    /// Build the canned a11y tree we serve from `snapshot`. The tree
    /// reflects the current page's URL in the document label.
    fn canned_tree(url: &str) -> Tree {
        let mut hd = Node::leaf(Ref(2), Role::Hd, "Test Page");
        hd.attrs.insert("level".into(), "1".into());

        let mut input = Node::leaf(Ref(3), Role::Tf, "");
        input.ops.insert(Op::Fill);
        input.ops.insert(Op::Focus);
        input.attrs.insert("placeholder".into(), "type here".into());

        let mut button = Node::leaf(Ref(4), Role::Btn, "Submit");
        button.ops.insert(Op::Click);

        let mut link = Node::leaf(Ref(5), Role::Lnk, "More info");
        link.ops.insert(Op::Click);
        link.attrs.insert("href".into(), url.to_string());

        let doc = Node {
            r: Ref(1),
            role: Role::Doc,
            label: format!("Test: {url}"),
            ops: BTreeSet::new(),
            attrs: BTreeMap::new(),
            children: vec![hd, input, button, link],
        };
        Tree::from_root(doc)
    }
}

impl Engine for TestEngine {
    fn open(&mut self, url: &str) -> EngineResult<PageHandle> {
        let handle = self.alloc_handle();
        self.pages.insert(
            handle,
            TestPage {
                url: url.to_string(),
                viewport: Viewport::DESKTOP,
                auth: None,
                opened_at: std::time::SystemTime::now(),
            },
        );
        Ok(handle)
    }

    fn navigate(&mut self, page: PageHandle, url: &str) -> EngineResult<()> {
        let p = self.pages.get_mut(&page).ok_or(EngineError::NotFound {
            kind: "page",
            id: page.0.to_string(),
        })?;
        p.url = url.to_string();
        Ok(())
    }

    fn close(&mut self, page: PageHandle) -> EngineResult<()> {
        self.pages.remove(&page);
        Ok(())
    }

    fn snapshot(&mut self, page: PageHandle) -> EngineResult<Tree> {
        let p = self.page(page)?;
        Ok(Self::canned_tree(&p.url))
    }

    fn act(
        &mut self,
        page: PageHandle,
        target: ActTarget,
        action: Action,
        _mode: crate::engine::InputMode,
    ) -> EngineResult<()> {
        // Verify the page exists and the target is plausible.
        let _ = self.page(page)?;
        match target {
            ActTarget::Ref(r) => {
                if !(1..=5).contains(&r.0) {
                    return Err(EngineError::NotFound {
                        kind: "ref",
                        id: r.to_string(),
                    });
                }
            }
            ActTarget::Mark(name) => {
                return Err(EngineError::NotFound {
                    kind: "mark",
                    id: name,
                });
            }
        }
        // TestEngine doesn't actually mutate anything in response to
        // actions; it just acknowledges the call. `Fill` and `Key` carry
        // payload but we don't store them.
        let _ = action;
        Ok(())
    }

    fn wait(
        &mut self,
        page: PageHandle,
        cond: WaitCondition,
        budget: Duration,
    ) -> EngineResult<()> {
        let _ = self.page(page)?;
        // The canned tree is always "stable" — return immediately for
        // the conditions we can satisfy in-memory; reject the rest.
        match cond {
            WaitCondition::Stable | WaitCondition::NetIdle => Ok(()),
            WaitCondition::RefAppears(r) if (1..=5).contains(&r.0) => Ok(()),
            WaitCondition::RefGone(r) if !(1..=5).contains(&r.0) => Ok(()),
            WaitCondition::Text(t) => {
                if t.contains("Test") {
                    Ok(())
                } else {
                    Err(EngineError::Timeout {
                        budget,
                        primitive: "vs_wait",
                    })
                }
            }
            WaitCondition::TokenChange
            | WaitCondition::RefAppears(_)
            | WaitCondition::RefGone(_) => Err(EngineError::Timeout {
                budget,
                primitive: "vs_wait",
            }),
        }
    }

    fn capture(&mut self, page: PageHandle, _scope: CaptureScope) -> EngineResult<PathBuf> {
        let _ = self.page(page)?;
        let dir = self.capture_dir.clone().unwrap_or_else(std::env::temp_dir);
        std::fs::create_dir_all(&dir)
            .map_err(|e| EngineError::Other(format!("create capture dir: {e}")))?;
        self.capture_seq += 1;
        let filename = format!("test-engine-{}-{}.png", page.0, self.capture_seq);
        let path = dir.join(filename);
        std::fs::write(&path, ONE_BY_ONE_PNG)
            .map_err(|e| EngineError::Other(format!("write capture: {e}")))?;
        Ok(path)
    }

    fn layout(&mut self, page: PageHandle, refs: &[Ref]) -> EngineResult<Vec<LayoutBox>> {
        let _ = self.page(page)?;
        // Synthetic boxes: one per *known* ref (1..=5). Unknown refs
        // are filtered out — same shape as real backends (`webkit`,
        // `wpe`) which only report boxes for elements that exist in
        // the DOM. This lets composite primitives (vs_view --layout=)
        // detect missing refs.
        let mut out = Vec::new();
        let mut idx = 0;
        for r in refs {
            if !(1..=5).contains(&r.0) {
                continue;
            }
            #[allow(clippy::cast_precision_loss)]
            let y = f64::from(idx) * 24.0;
            out.push(LayoutBox {
                r: *r,
                x: 0.0,
                y,
                width: 320.0,
                height: 24.0,
                visible: true,
                z_index: 0,
            });
            idx += 1;
        }
        Ok(out)
    }

    fn set_viewport(&mut self, page: PageHandle, viewport: Viewport) -> EngineResult<()> {
        let p = self.page_mut(page)?;
        p.viewport = viewport;
        Ok(())
    }

    fn save_auth(&mut self, page: PageHandle) -> EngineResult<AuthBlob> {
        let p = self.page(page)?;
        // Round-trip-only: serialize the URL as a stand-in for cookies.
        Ok(AuthBlob {
            bytes: p.url.as_bytes().to_vec(),
        })
    }

    fn load_auth(&mut self, page: PageHandle, blob: &AuthBlob) -> EngineResult<()> {
        let p = self.page_mut(page)?;
        p.auth = Some(blob.clone());
        Ok(())
    }

    fn console_entries(
        &mut self,
        page: PageHandle,
    ) -> EngineResult<Vec<crate::inspector::ConsoleEntry>> {
        let p = self.page(page)?;
        // Timestamps anchored at page-open time (not call time) so
        // entries are stable across multiple `console_entries` calls.
        let opened = p.opened_at;
        // Synthetic entries are stamped clearly *before* page open so
        // a `--since=<later-token>` test sees them filtered out.
        let s1 = opened
            .checked_sub(std::time::Duration::from_secs(5))
            .unwrap_or(opened);
        let s2 = opened
            .checked_sub(std::time::Duration::from_secs(3))
            .unwrap_or(opened);
        let s3 = opened
            .checked_sub(std::time::Duration::from_secs(1))
            .unwrap_or(opened);
        Ok(vec![
            crate::inspector::ConsoleEntry {
                timestamp: s1,
                level: crate::inspector::ConsoleLevel::Log,
                message: "test: page loaded".into(),
                stack: None,
            },
            crate::inspector::ConsoleEntry {
                timestamp: s2,
                level: crate::inspector::ConsoleLevel::Warn,
                message: "test: deprecation warning".into(),
                stack: None,
            },
            crate::inspector::ConsoleEntry {
                timestamp: s3,
                level: crate::inspector::ConsoleLevel::Error,
                message: "test: synthetic error".into(),
                stack: Some("at testEngine (test:1:1)".into()),
            },
        ])
    }

    fn network_entries(
        &mut self,
        page: PageHandle,
    ) -> EngineResult<Vec<crate::inspector::NetworkEntry>> {
        let p = self.page(page)?;
        let opened = p.opened_at;
        let url = p.url.clone();
        Ok(vec![
            crate::inspector::NetworkEntry {
                seq: 1,
                timestamp: opened
                    .checked_add(std::time::Duration::from_millis(5))
                    .unwrap_or(opened),
                method: "GET".into(),
                url: url.clone(),
                status: crate::inspector::NetworkStatus::Code(200),
                size: 1234,
                latency_ms: Some(82),
            },
            crate::inspector::NetworkEntry {
                seq: 2,
                timestamp: opened
                    .checked_add(std::time::Duration::from_millis(15))
                    .unwrap_or(opened),
                method: "POST".into(),
                url: format!("{url}/api/checkout"),
                status: crate::inspector::NetworkStatus::Code(401),
                size: 56,
                latency_ms: Some(140),
            },
            crate::inspector::NetworkEntry {
                seq: 3,
                timestamp: opened
                    .checked_add(std::time::Duration::from_millis(25))
                    .unwrap_or(opened),
                method: "GET".into(),
                url: format!("{url}/static/missing.js"),
                status: crate::inspector::NetworkStatus::Code(404),
                size: 0,
                latency_ms: Some(11),
            },
        ])
    }

    fn request_detail(
        &mut self,
        page: PageHandle,
        seq: u64,
    ) -> EngineResult<Option<crate::inspector::RequestDetail>> {
        let p = self.page(page)?;
        // Match against the synthetic seqs above. seq=2 carries an
        // Authorization header so the redactor has something to redact.
        let detail = match seq {
            1 => crate::inspector::RequestDetail {
                seq: 1,
                method: "GET".into(),
                url: p.url.clone(),
                status: crate::inspector::NetworkStatus::Code(200),
                request_headers: vec![crate::inspector::Header {
                    name: "Accept".into(),
                    value: "text/html".into(),
                }],
                request_body: None,
                response_headers: vec![crate::inspector::Header {
                    name: "Content-Type".into(),
                    value: "text/html".into(),
                }],
                response_body: Some("<!doctype html><h1>test</h1>".into()),
            },
            2 => crate::inspector::RequestDetail {
                seq: 2,
                method: "POST".into(),
                url: format!("{}/api/checkout", p.url),
                status: crate::inspector::NetworkStatus::Code(401),
                request_headers: vec![
                    crate::inspector::Header {
                        name: "Authorization".into(),
                        value: "Bearer secret-token-xyz".into(),
                    },
                    crate::inspector::Header {
                        name: "Content-Type".into(),
                        value: "application/json".into(),
                    },
                ],
                request_body: Some(r#"{"qty":1}"#.into()),
                response_headers: vec![crate::inspector::Header {
                    name: "Content-Type".into(),
                    value: "application/json".into(),
                }],
                response_body: Some(r#"{"error":"unauthorized"}"#.into()),
            },
            _ => return Ok(None),
        };
        Ok(Some(detail))
    }

    fn eval_js(
        &mut self,
        page: PageHandle,
        expr: &str,
    ) -> EngineResult<crate::inspector::EvalResult> {
        let _ = self.page(page)?;
        // Tiny mock evaluator covering the cases tests care about:
        // numeric literals, throw, syntax error, anything else returns
        // a string echo. Lets us prove the protocol path without a JS
        // engine.
        if let Ok(n) = expr.trim().parse::<f64>() {
            return Ok(crate::inspector::EvalResult::Ok {
                value: n.to_string(),
                js_type: "number".into(),
            });
        }
        if expr.contains("throw ") {
            return Ok(crate::inspector::EvalResult::Thrown {
                kind: "TypeError".into(),
                message: "test: simulated throw".into(),
            });
        }
        if expr.contains("###") {
            return Ok(crate::inspector::EvalResult::Syntax {
                message: "test: simulated syntax error".into(),
            });
        }
        Ok(crate::inspector::EvalResult::Ok {
            value: serde_json::to_string(expr).unwrap_or_else(|_| "\"\"".into()),
            js_type: "string".into(),
        })
    }

    fn storage(
        &mut self,
        page: PageHandle,
        scope: crate::inspector::StorageScope,
    ) -> EngineResult<Vec<crate::inspector::StorageEntry>> {
        use crate::inspector::{StorageEntry, StorageScope as S};
        let _ = self.page(page)?;
        Ok(match scope {
            S::Cookies => vec![
                StorageEntry {
                    key: "session_id".into(),
                    value: "abc123".into(),
                    flags: vec!["secure".into(), "httponly".into(), "samesite=lax".into()],
                    sensitive: true,
                },
                StorageEntry {
                    key: "consent".into(),
                    value: "all".into(),
                    flags: vec!["secure".into()],
                    sensitive: false,
                },
            ],
            S::Local => vec![
                StorageEntry {
                    key: "theme".into(),
                    value: "dark".into(),
                    flags: vec![],
                    sensitive: false,
                },
                StorageEntry {
                    key: "auth_token".into(),
                    value: "eyJhbGciOiJIUzI1NiIs.example.long.token".into(),
                    flags: vec![],
                    sensitive: true,
                },
            ],
            S::Session => vec![StorageEntry {
                key: "csrf".into(),
                value: "xyz".into(),
                flags: vec![],
                sensitive: false,
            }],
            S::IndexedDb => vec![StorageEntry {
                key: "vibesurfer_demo_db".into(),
                value: "v=2 stores=cache,history".into(),
                flags: vec![],
                sensitive: false,
            }],
        })
    }

    fn scripts(&mut self, page: PageHandle) -> EngineResult<Vec<crate::inspector::ScriptEntry>> {
        use crate::inspector::{ScriptEntry, ScriptState};
        let p = self.page(page)?;
        Ok(vec![
            ScriptEntry {
                seq: 1,
                source: format!("{}/static/app.js", p.url),
                size: 12_345,
                state: ScriptState::Parsed,
            },
            ScriptEntry {
                seq: 2,
                source: "inline:doc[0]".into(),
                size: 80,
                state: ScriptState::Parsed,
            },
            ScriptEntry {
                seq: 3,
                source: format!("{}/missing.js", p.url),
                size: 0,
                state: ScriptState::Error,
            },
        ])
    }

    fn script_source(
        &mut self,
        page: PageHandle,
        seq: u64,
    ) -> EngineResult<Option<crate::inspector::ScriptSource>> {
        let p = self.page(page)?;
        Ok(match seq {
            1 => Some(crate::inspector::ScriptSource {
                seq: 1,
                source_url: format!("{}/static/app.js", p.url),
                body: "// test-engine: synthetic app.js\nfunction main() {}\n".into(),
            }),
            2 => Some(crate::inspector::ScriptSource {
                seq: 2,
                source_url: "inline:doc[0]".into(),
                body: "console.log('hi from inline');".into(),
            }),
            _ => None,
        })
    }

    fn dom(
        &mut self,
        page: PageHandle,
        r: vs_protocol::Ref,
        extra_props: &[String],
    ) -> EngineResult<Option<crate::inspector::DomDetail>> {
        let _ = self.page(page)?;
        if !(1..=5).contains(&r.0) {
            return Ok(None);
        }
        let mut computed = vec![
            ("display".to_string(), "block".to_string()),
            ("visibility".to_string(), "visible".to_string()),
            ("opacity".to_string(), "1".to_string()),
            ("pointer-events".to_string(), "auto".to_string()),
            ("cursor".to_string(), "default".to_string()),
            ("position".to_string(), "static".to_string()),
            ("z-index".to_string(), "auto".to_string()),
        ];
        for p in extra_props {
            computed.push((p.clone(), format!("(test:{p})")));
        }
        Ok(Some(crate::inspector::DomDetail {
            r: r.0,
            outer_html: format!("<div data-vs-ref=\"{}\">test</div>", r.0),
            computed,
        }))
    }

    fn performance(
        &mut self,
        page: PageHandle,
    ) -> EngineResult<crate::inspector::PerformanceMetrics> {
        let _ = self.page(page)?;
        Ok(crate::inspector::PerformanceMetrics {
            ttfb_ms: 12.0,
            fcp_ms: 88.0,
            lcp_ms: 142.0,
            cls: 0.05,
            fid_ms: 4.0,
            long_tasks: 1,
            total_blocking_ms: 24.0,
            js_heap_mb: 6.5,
            dom_nodes: 42,
        })
    }
    fn capabilities(&self) -> EngineCapabilities {
        EngineCapabilities::TEST
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vs_protocol::Ref;

    #[test]
    fn open_and_snapshot() {
        let mut e = TestEngine::new();
        let page = e.open("https://example.com").unwrap();
        let tree = e.snapshot(page).unwrap();
        assert_eq!(tree.roots.len(), 1);
        assert_eq!(tree.roots[0].r, Ref(1));
        assert!(tree.roots[0].label.contains("Test: https://example.com"));
        // Children: hd, input, button, link
        assert_eq!(tree.roots[0].children.len(), 4);
    }

    #[test]
    fn close_idempotent() {
        let mut e = TestEngine::new();
        let page = e.open("about:blank").unwrap();
        e.close(page).unwrap();
        // Closing again is a no-op (the page is just gone).
        e.close(page).unwrap();
    }

    #[test]
    fn snapshot_after_close_errors() {
        let mut e = TestEngine::new();
        let page = e.open("about:blank").unwrap();
        e.close(page).unwrap();
        let err = e.snapshot(page).unwrap_err();
        assert!(matches!(err, EngineError::NotFound { kind: "page", .. }));
    }

    #[test]
    fn act_known_ref_succeeds() {
        let mut e = TestEngine::new();
        let p = e.open("about:blank").unwrap();
        e.act(
            p,
            ActTarget::Ref(Ref(4)),
            Action::Click,
            crate::engine::InputMode::Careful,
        )
        .unwrap();
        e.act(
            p,
            ActTarget::Ref(Ref(3)),
            Action::Fill { value: "hi".into() },
            crate::engine::InputMode::Careful,
        )
        .unwrap();
    }

    #[test]
    fn act_unknown_ref_errors() {
        let mut e = TestEngine::new();
        let p = e.open("about:blank").unwrap();
        let err = e
            .act(
                p,
                ActTarget::Ref(Ref(99)),
                Action::Click,
                crate::engine::InputMode::Careful,
            )
            .unwrap_err();
        assert!(matches!(err, EngineError::NotFound { kind: "ref", .. }));
    }

    #[test]
    fn wait_stable_returns_immediately() {
        let mut e = TestEngine::new();
        let p = e.open("about:blank").unwrap();
        e.wait(p, WaitCondition::Stable, Duration::from_millis(0))
            .unwrap();
    }

    #[test]
    fn wait_token_change_times_out() {
        let mut e = TestEngine::new();
        let p = e.open("about:blank").unwrap();
        let err = e
            .wait(p, WaitCondition::TokenChange, Duration::from_millis(1))
            .unwrap_err();
        assert!(matches!(err, EngineError::Timeout { .. }));
    }

    #[test]
    fn capture_writes_a_one_by_one_png() {
        let dir = tempfile::tempdir().unwrap();
        let mut e = TestEngine::new().with_capture_dir(dir.path());
        let p = e.open("https://example.com").unwrap();
        let path = e.capture(p, CaptureScope::Viewport).unwrap();
        assert!(path.exists(), "capture file missing: {path:?}");
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "not a PNG: {bytes:?}");
        // Two captures get distinct paths.
        let path2 = e.capture(p, CaptureScope::FullPage).unwrap();
        assert_ne!(path, path2);
    }

    #[test]
    fn layout_returns_one_box_per_ref() {
        let mut e = TestEngine::new();
        let p = e.open("about:blank").unwrap();
        let boxes = e.layout(p, &[Ref(2), Ref(4)]).unwrap();
        assert_eq!(boxes.len(), 2);
        assert_eq!(boxes[0].r, Ref(2));
        assert!(boxes[0].visible);
        assert!((boxes[0].y - boxes[1].y).abs() > f64::EPSILON);
    }

    #[test]
    fn auth_round_trip() {
        let mut e = TestEngine::new();
        let p = e.open("https://example.com").unwrap();
        let blob = e.save_auth(p).unwrap();
        assert_eq!(blob.bytes, b"https://example.com");
        e.load_auth(p, &blob).unwrap();
    }

    #[test]
    fn viewport_persists() {
        let mut e = TestEngine::new();
        let p = e.open("about:blank").unwrap();
        e.set_viewport(p, Viewport::MOBILE).unwrap();
        // No public getter, but the page's viewport is updated; verify
        // by closing without panic.
        e.close(p).unwrap();
    }

    #[test]
    fn capabilities_are_test() {
        let e = TestEngine::new();
        assert_eq!(e.capabilities().name, "test");
        assert!(e.capabilities().renders);
        assert!(e.capabilities().persists_auth);
    }
}
