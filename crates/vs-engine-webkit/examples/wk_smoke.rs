//! Real macOS WKWebView smoke test.
//!
//! Run with:
//! ```sh
//! cargo run --example wk_smoke -- https://example.com
//! ```
//!
//! Constructs a real `WKWebView` on the OS main thread (where this
//! `main` is running), navigates to the supplied URL (or `data:text/html`
//! default), then injects a JS DOM walker and prints the resulting
//! [`Tree`] in the canonical wire format. Proves end-to-end that
//! `WkBackend::open` and `WkBackend::snapshot` actually work against
//! the real browser.

#[cfg(target_os = "macos")]
fn main() {
    use objc2::MainThreadMarker;
    use vs_engine_webkit::backend::webkit::WkBackend;
    use vs_engine_webkit::Engine;

    // We are `main()`; this thread is the OS main thread by definition.
    // `MainThreadMarker::new()` returns `Some(_)` here.
    let mtm = MainThreadMarker::new().expect("main() runs on the main thread");

    let url = std::env::args().nth(1).unwrap_or_else(|| {
        "data:text/html,<h1 id=t>hi</h1><button>Submit</button><a href=#>more</a>".to_string()
    });

    eprintln!("[wk_smoke] opening: {url}");
    let mut backend = WkBackend::new(mtm);
    let page = match backend.open(&url) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[wk_smoke] open failed: {e}");
            std::process::exit(1);
        }
    };
    eprintln!("[wk_smoke] navigated. PageHandle = {page:?}");

    let tree = match backend.snapshot(page) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("[wk_smoke] snapshot failed: {e}");
            std::process::exit(1);
        }
    };
    eprintln!("[wk_smoke] snapshot OK — {} root(s)", tree.roots.len());

    // Print the canonical wire form.
    println!("{}", tree.encode());

    use vs_engine_webkit::{
        engine::{ActTarget, Action, Viewport, WaitCondition},
        CaptureScope,
    };

    // Find the first link or button ref in the tree to act on.
    let actionable = find_actionable(&tree);
    if let Some(r) = actionable {
        eprintln!("[wk_smoke] hover ref={r:?}");
        if let Err(e) = backend.act(page, ActTarget::Ref(r), Action::Hover) {
            eprintln!("[wk_smoke] hover failed: {e}");
        }
        eprintln!("[wk_smoke] focus ref={r:?}");
        if let Err(e) = backend.act(page, ActTarget::Ref(r), Action::Focus) {
            eprintln!("[wk_smoke] focus failed: {e}");
        }
        eprintln!("[wk_smoke] layout for ref={r:?}");
        match backend.layout(page, &[r]) {
            Ok(boxes) => {
                for b in &boxes {
                    eprintln!(
                        "[wk_smoke]   {:?} @ ({:.1},{:.1}) {:.1}x{:.1} visible={} z={}",
                        b.r, b.x, b.y, b.width, b.height, b.visible, b.z_index
                    );
                }
            }
            Err(e) => eprintln!("[wk_smoke] layout failed: {e}"),
        }
    } else {
        eprintln!("[wk_smoke] no actionable ref found in snapshot");
    }

    eprintln!("[wk_smoke] wait stable");
    if let Err(e) = backend.wait(
        page,
        WaitCondition::Stable,
        std::time::Duration::from_secs(2),
    ) {
        eprintln!("[wk_smoke] wait failed: {e}");
    }

    eprintln!("[wk_smoke] set_viewport mobile (390x844)");
    if let Err(e) = backend.set_viewport(page, Viewport::MOBILE) {
        eprintln!("[wk_smoke] set_viewport failed: {e}");
    }

    match backend.capture(page, CaptureScope::Viewport) {
        Ok(p) => {
            eprintln!("[wk_smoke] capture written: {}", p.display());
            println!("CAPTURE_PATH={}", p.display());
        }
        Err(e) => eprintln!("[wk_smoke] capture failed: {e}"),
    }

    let _ = backend.close(page);
    eprintln!("[wk_smoke] done.");
}

#[cfg(target_os = "macos")]
fn find_actionable(tree: &vs_protocol::Tree) -> Option<vs_protocol::Ref> {
    use vs_protocol::{Node, Ref, Role};
    fn walk(n: &Node) -> Option<Ref> {
        if matches!(n.role, Role::Lnk | Role::Btn | Role::Tf | Role::Ta) {
            return Some(n.r);
        }
        for c in &n.children {
            if let Some(r) = walk(c) {
                return Some(r);
            }
        }
        None
    }
    for r in &tree.roots {
        if let Some(found) = walk(r) {
            return Some(found);
        }
    }
    None
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("wk_smoke is macOS-only");
    std::process::exit(0);
}
