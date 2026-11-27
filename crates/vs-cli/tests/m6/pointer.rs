//! Pointer cells: native drags, coordinate input (move, click, wheel),
//! click variants, new-window navigation, zoom and native drops.

use crate::helpers::{eval_js, open_fixture, ref_for, settle};
use crate::support::{
    assert_ok, body_first, body_rest, each_available_backend, token_of, TestContext,
};

// A trusted drag over a `draggable` element must not start a system
// drag session. On macOS 27 it did: WebKit handed the drag to the
// window server, the daemon's main thread wedged waiting for a
// hardware mouse-up, and a drag window showed on the user's screen
// (#vibesurfer 01M75J1S / 01M75X). On Linux GTK's drag began after
// the drag's release and took the next release on the display, so
// another page's click was lost. The drag must return, the page must
// see the drop, and this page and others must take input afterwards.
#[test]
fn cell_drag_native_draggable_does_not_wedge() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (s, page, token) = open_fixture(&ctx, "/native-drag.html");
        let r = ctx.vs(&["open", &ctx.url("/form.html"), "--session", &s]);
        assert_ok("open second", &r);
        let other = body_first(&r);
        eval_js(
            &ctx,
            &other,
            "(function(){document.body.innerHTML='<input type=checkbox id=c style=\"position:fixed;left:40px;top:40px;width:20px;height:20px\">';return 'ok';})()",
        );
        // Run the drag off-thread: a regression hangs the CLI call
        // forever, and the test should fail, not hang the suite.
        let home = ctx.home_path().to_path_buf();
        let p = page.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let out = crate::support::vs(
                &home,
                &[
                    "drag",
                    &p,
                    "100",
                    "100",
                    "450",
                    "100",
                    &format!("--token={token}"),
                ],
            );
            let _ = tx.send(out);
        });
        let Ok(r) = rx.recv_timeout(std::time::Duration::from_secs(30)) else {
            let log =
                std::fs::read_to_string(ctx.home_path().join("daemon.log")).unwrap_or_default();
            panic!("vs drag over a draggable element must not hang\n--- daemon.log ---\n{log}");
        };
        assert_ok("drag", &r);
        let token = token_of(&r);
        let r = ctx.vs(&["click-at", &page, "100", "320", &format!("--token={token}")]);
        assert_ok("click after drag", &r);
        let log = eval_js(&ctx, &page, "window.__vsLog.join(',')");
        assert!(
            log.contains("drop"),
            "drop target must see the drop, got {log:?}"
        );
        assert!(
            log.contains("click-trusted"),
            "page must take trusted input after the drag, got {log:?}"
        );
        // Another page: a checkbox clicked twice is unchecked again.
        let r = ctx.vs(&["view", &other, "--full"]);
        let mut token = token_of(&r);
        for _ in 0..2 {
            let r = ctx.vs(&[
                "click-at",
                &other,
                "50",
                "50",
                "-M",
                "robotic",
                &format!("--token={token}"),
            ]);
            assert_ok("click-at other page", &r);
            token = token_of(&r);
        }
        settle(200);
        assert_eq!(
            eval_js(&ctx, &other, "String(document.getElementById('c').checked)").trim(),
            "false",
            "both clicks on the other page must land"
        );
    }
}

// A target="_blank" link, a target="_blank" POST form, and
// window.open() all used to go nowhere: the headless page has no
// second window, WebKit dropped the navigation, and the click looked
// like a no-op. ScienceDirect's institution login is a blank-target
// form, so agents could not sign in through it. The navigation now
// loads in the same page, with a POST body intact on the WebKit
// engines. WebView2 hands over only the URL, so Windows skips the POST
// case (docs/known-issues.md).
#[test]
fn cell_new_window_navigation_loads_in_place() {
    for _ in each_available_backend() {
        let cases: [(&str, &str, &str, &str); 3] = [
            (
                "lnk",
                "New-window link",
                "/multi-page-2.html",
                "Multi-page B",
            ),
            ("btn", "New-window POST", "/echo", "entity=escp-idp"),
            ("btn", "Window open", "/multi-page-2.html", "Multi-page B"),
        ];
        for (role, label, path, text) in cases {
            if cfg!(target_os = "windows") && label == "New-window POST" {
                continue;
            }
            let ctx = TestContext::start();
            let (_s, page, _t) = open_fixture(&ctx, "/popup.html");
            let r = ctx.vs(&["view", &page, "--full"]);
            assert_ok("view", &r);
            let target = ref_for(&body_rest(&r), role, label);
            let r = ctx.vs(&[
                "act",
                &page,
                &target.to_string(),
                "click",
                &format!("--token={}", token_of(&r)),
            ]);
            assert_ok(label, &r);
            settle(1500);
            let got_path = eval_js(&ctx, &page, "location.pathname");
            assert!(
                got_path.contains(path),
                "{label}: page must navigate to {path}, got {got_path:?}"
            );
            let body = eval_js(&ctx, &page, "document.body.innerText");
            assert!(
                body.contains(text),
                "{label}: expected {text:?} in body, got {body:?}"
            );
        }
    }
}

// `vs move-to` / `vs hover-at` dispatch trusted mouse moves. On macOS
// they used to reach WKWebView and change nothing: no mouseover, no
// :hover. Menus, tooltips and hover cards that open on a real pointer
// never opened.
#[test]
fn cell_move_to_hovers_with_trusted_events() {
    for _ in each_available_backend() {
        for verb in ["move-to", "hover-at"] {
            let ctx = TestContext::start();
            let (_s, page, _t) = open_fixture(&ctx, "/hover.html");
            let r = ctx.vs(&[verb, &page, "160", "120", "--mode=robotic"]);
            assert_ok(verb, &r);
            settle(100);
            let hovered = eval_js(
                &ctx,
                &page,
                "String(document.querySelector('button:hover') && document.querySelector('button:hover').id)",
            );
            assert_eq!(hovered.trim(), "a", "{verb}: button must match :hover");
            let log = eval_js(&ctx, &page, "window.__vsLog.join(',')");
            for ev in [
                "mouseover-trusted",
                "mouseenter-trusted",
                "mousemove-trusted",
            ] {
                assert!(log.contains(ev), "{verb}: missing {ev}, got {log:?}");
            }
        }
    }
}

// Refs live in the walker's maps, not in DOM attributes: after a view
// and an act, the page's markup is untouched, and the ref still
// resolves to the element.
#[test]
fn cell_refs_stay_out_of_the_markup() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, _t) = open_fixture(&ctx, "/form.html");
        let r = ctx.vs(&["view", &page, "--full"]);
        let body = body_rest(&r);
        let token = token_of(&r);
        let n_email = ref_for(&body, "tf", "");
        let r_fill = ctx.vs(&[
            "act",
            &page,
            &n_email.to_string(),
            "fill",
            "user@example.com",
            &format!("--token={token}"),
        ]);
        assert_ok("fill email", &r_fill);
        assert_eq!(
            eval_js(
                &ctx,
                &page,
                "String(document.documentElement.outerHTML.includes('data-vs-ref'))"
            ),
            "false",
            "refs leaked into the page's markup"
        );
        assert_eq!(
            eval_js(
                &ctx,
                &page,
                &format!("String(window.__vsFindRef({n_email}).value)")
            ),
            "user@example.com"
        );
    }
}

// Each page has its own pointer, as each Playwright page does: a page
// opens unhovered, and another page's input does not take its hover
// away. On Linux the X pointer and window stacking are shared, so page
// windows must not overlap (each gets a screen tile).
#[test]
fn cell_pages_keep_their_own_hover() {
    const MENU: &str = "(function(){document.body.innerHTML='<style>#m{display:none}#w:hover #m{display:block}</style><div id=w style=\"position:fixed;left:20px;top:20px;width:200px;height:80px\"><span>Account</span><a id=m href=#out>Sign out</a></div><button id=b style=\"position:fixed;left:20px;top:200px;width:120px;height:40px\">other</button>';return 'ok';})()";
    let shown = "String(getComputedStyle(document.getElementById('m')).display)";
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (s, first, _t) = open_fixture(&ctx, "/form.html");
        eval_js(&ctx, &first, MENU);
        let r = ctx.vs(&["open", &ctx.url("/form.html"), "--session", &s]);
        assert_ok("open second", &r);
        let second = body_first(&r);
        eval_js(&ctx, &second, MENU);
        // A fresh page is not hovered.
        assert_eq!(
            eval_js(&ctx, &second, shown).trim(),
            "none",
            "fresh page hovered"
        );
        let r = ctx.vs(&["hover-at", &first, "60", "40", "-M", "robotic"]);
        assert_ok("hover first", &r);
        assert_eq!(
            eval_js(&ctx, &first, shown).trim(),
            "block",
            "first hovered"
        );
        // The other page's input must not take the first page's hover.
        let r = ctx.vs(&["view", &second, "--full"]);
        let token = token_of(&r);
        let r = ctx.vs(&[
            "click-at",
            &second,
            "60",
            "220",
            "-M",
            "robotic",
            &format!("--token={token}"),
        ]);
        assert_ok("click second", &r);
        settle(200);
        assert_eq!(
            eval_js(&ctx, &first, shown).trim(),
            "block",
            "first page lost its hover to the second page's click"
        );
    }
}

// With several pages open, a click goes to the page it names. On
// Linux every host window sat at the screen origin and XTest input
// goes by position, so the newest window used to take the click.
#[test]
fn cell_click_at_reaches_its_page_under_another() {
    const BUTTON: &str = "(function(){document.body.innerHTML='<button id=b style=\"position:fixed;left:40px;top:40px;width:120px;height:40px\">hit me</button>';window.__hit=0;document.getElementById('b').addEventListener('click',function(e){window.__hit+=(e.isTrusted?1:100);});return 'ok';})()";
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (s, first, _t) = open_fixture(&ctx, "/form.html");
        eval_js(&ctx, &first, BUTTON);
        let r = ctx.vs(&["open", &ctx.url("/form.html"), "--session", &s]);
        assert_ok("open second", &r);
        let second = body_first(&r);
        eval_js(&ctx, &second, BUTTON);
        settle(300);
        for page in [&first, &second, &first] {
            let r = ctx.vs(&["view", page, "--full"]);
            let token = token_of(&r);
            let before = eval_js(&ctx, page, "String(window.__hit)");
            let r = ctx.vs(&[
                "click-at",
                page,
                "100",
                "60",
                "-M",
                "robotic",
                &format!("--token={token}"),
            ]);
            assert_ok("click-at", &r);
            let after = eval_js(&ctx, page, "String(window.__hit)");
            assert_eq!(
                after.parse::<u32>().unwrap(),
                before.parse::<u32>().unwrap() + 1,
                "click lands on {page}"
            );
        }
        assert_eq!(eval_js(&ctx, &first, "String(window.__hit)"), "2");
        assert_eq!(eval_js(&ctx, &second, "String(window.__hit)"), "1");
    }
}

// A viewport smaller than the host window must not shift pointer
// input. WebKitGTK centered the resized web view in its 1280x800
// window while clicks assumed it sat at the top-left, so every
// click-at after `vs viewport` missed by the gap.
#[test]
fn cell_click_at_lands_after_viewport_resize() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, _t) = open_fixture(&ctx, "/form.html");
        let r = ctx.vs(&["viewport", &page, "1024x600"]);
        assert_ok("viewport", &r);
        eval_js(
            &ctx,
            &page,
            "(function(){document.body.innerHTML='<button id=b style=\"position:fixed;left:40px;top:40px;width:120px;height:40px\">hit me</button>';document.getElementById('b').addEventListener('click',function(e){window.__hit=(window.__hit||0)+(e.isTrusted?1:100);});return 'ok';})()",
        );
        settle(300);
        let r = ctx.vs(&["view", &page, "--full"]);
        let token = token_of(&r);
        let r = ctx.vs(&[
            "click-at",
            &page,
            "100",
            "60",
            "-M",
            "robotic",
            &format!("--token={token}"),
        ]);
        assert_ok("click-at", &r);
        assert_eq!(
            eval_js(&ctx, &page, "String(window.__hit)"),
            "1",
            "one trusted click must land on the button after the resize"
        );
    }
}

// A wheel turn is a native, trusted wheel event: it scrolls what is
// under the pointer (an overflow box, or the page) and the page's
// listener sees isTrusted with the turn's direction.
#[test]
fn cell_wheel_scrolls_what_is_under_the_pointer() {
    let num = |s: &str| s.trim().parse::<f64>().unwrap_or(0.0);
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, _t) = open_fixture(&ctx, "/wheel.html");
        let poll = |expr: &str| {
            let mut v = String::new();
            for _ in 0..40 {
                settle(50);
                v = eval_js(&ctx, &page, expr);
                if num(&v) > 0.0 {
                    break;
                }
            }
            v
        };

        // Over the overflow box, the box scrolls and the page does not.
        let r = ctx.vs(&["wheel", &page, "500", "120", "0", "200", "--mode=robotic"]);
        assert_ok("wheel box", &r);
        let boxed = poll("String(document.getElementById('box').scrollTop)");
        let all = eval_js(&ctx, &page, "JSON.stringify(window.__wheels)");
        assert!(num(&boxed) > 0.0, "box did not scroll: {boxed} {all}");
        assert!(all.contains("\"trusted\":true"), "wheel event: {all}");
        assert!(
            num(&eval_js(&ctx, &page, "String(window.scrollY)")) < 0.5,
            "the page scrolled with the box"
        );

        // Elsewhere the page scrolls.
        let r = ctx.vs(&["wheel", &page, "100", "300", "0", "300", "--mode=robotic"]);
        assert_ok("wheel page", &r);
        let y = poll("String(window.scrollY)");
        assert!(num(&y) > 0.0, "page did not scroll: scrollY={y}");
        let dy = eval_js(
            &ctx,
            &page,
            "String(window.__wheels.reduce((a, w) => a + w.dy, 0))",
        );
        assert!(num(&dy) > 0.0, "deltaY sum: {dy}");

        // Upwards and sideways come out with their signs.
        let r = ctx.vs(&[
            "wheel",
            &page,
            "100",
            "300",
            "-40",
            "-100",
            "--mode=robotic",
        ]);
        assert_ok("wheel up", &r);
        settle(200);
        let all = eval_js(&ctx, &page, "JSON.stringify(window.__wheels)");
        eprintln!("wheels: {all}");
        let up = eval_js(&ctx, &page, "String(window.__wheels.some(w => w.dy < 0))");
        let left = eval_js(&ctx, &page, "String(window.__wheels.some(w => w.dx < 0))");
        assert_eq!((up.trim(), left.trim()), ("true", "true"), "{all}");
    }
}

// click-at with --button, --count and --modifiers: native events with
// the right button, detail and modifier flags, all trusted; a right
// click the page does not cancel must not wedge the engine on a
// context menu.
#[test]
#[allow(clippy::too_many_lines)]
fn cell_click_variants_are_trusted() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, token) = open_fixture(&ctx, "/click-variants.html");
        let ev = |ctx: &TestContext| eval_js(ctx, &page, "window.__ev.join(' ')");
        let reset = |ctx: &TestContext| {
            let _ = eval_js(ctx, &page, "(window.__ev = [], 'ok')");
        };
        let tok = format!("--token={token}");
        let r = ctx.vs(&[
            "click-at",
            &page,
            "120",
            "70",
            &tok,
            "--mode=robotic",
            "--count",
            "2",
        ]);
        assert_ok("double click", &r);
        settle(150);
        let got = ev(&ctx);
        assert!(got.contains("dblclick:a:0:2:true"), "double click: {got}");
        assert!(
            got.contains("click:a:0:2:true"),
            "second click has detail 2: {got}"
        );
        reset(&ctx);

        let r = ctx.vs(&[
            "click-at",
            &page,
            "120",
            "70",
            &tok,
            "--mode=robotic",
            "--button",
            "right",
        ]);
        assert_ok("right click", &r);
        settle(150);
        let got = ev(&ctx);
        assert!(
            got.contains("mousedown:a:2:1:true"),
            "right mousedown: {got}"
        );
        assert!(got.contains("contextmenu:a:2:"), "contextmenu: {got}");
        reset(&ctx);

        let r = ctx.vs(&[
            "click-at",
            &page,
            "120",
            "70",
            &tok,
            "--mode=robotic",
            "--modifiers",
            "shift,alt",
        ]);
        assert_ok("shift-alt click", &r);
        settle(150);
        let got = ev(&ctx);
        assert!(
            got.contains("click:a:0:1:true:true:false:true:false"),
            "modifiers: {got}"
        );
        reset(&ctx);

        let r = ctx.vs(&[
            "click-at",
            &page,
            "120",
            "70",
            &tok,
            "--mode=robotic",
            "--button",
            "middle",
        ]);
        assert_ok("middle click", &r);
        settle(150);
        let got = ev(&ctx);
        assert!(
            got.contains("mousedown:a:1:1:true"),
            "middle mousedown: {got}"
        );
        reset(&ctx);

        // An uncancelled right click, then the engine still answers.
        let r = ctx.vs(&[
            "click-at",
            &page,
            "340",
            "70",
            &tok,
            "--mode=robotic",
            "--button",
            "right",
        ]);
        assert_ok("right click with the native menu", &r);
        settle(200);
        let got = ev(&ctx);
        assert!(got.contains("contextmenu:b:2:"), "contextmenu on b: {got}");
        let r = ctx.vs(&["click-at", &page, "120", "70", &tok, "--mode=robotic"]);
        assert_ok("click after the menu", &r);
        settle(150);
        assert!(
            ev(&ctx).contains("click:a:0:1:true"),
            "engine stuck after a context menu"
        );
    }
}

// viewport --zoom: a 390 px wide view at zoom 390/980 lays the page out
// 980 CSS px wide (the mobile default without a viewport tag), and a
// click at view coordinates lands on what is drawn there.
#[test]
fn cell_viewport_zoom_lays_out_wider() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, token) = open_fixture(&ctx, "/click-variants.html");
        let r = ctx.vs(&["viewport", &page, "390x664", "--dpr", "1"]);
        assert_ok("viewport", &r);
        let zoom = 390.0 / 980.0;
        let r = ctx.vs(&["viewport", &page, "keep", "--zoom", &zoom.to_string()]);
        assert_ok("zoom", &r);
        settle(200);
        let w: f64 = eval_js(&ctx, &page, "String(window.innerWidth)")
            .trim()
            .parse()
            .unwrap_or(0.0);
        assert!((975.0..=985.0).contains(&w), "innerWidth {w}");
        // Button a is at CSS (40..200, 40..100): its middle, in view px.
        let (x, y) = (120.0 * zoom, 70.0 * zoom);
        let r = ctx.vs(&[
            "click-at",
            &page,
            &x.to_string(),
            &y.to_string(),
            &format!("--token={token}"),
            "--mode=robotic",
        ]);
        assert_ok("click", &r);
        settle(150);
        let ev = eval_js(&ctx, &page, "window.__ev.join(' ')");
        assert!(ev.contains("click:a:0:1:true"), "{ev}");
    }
}

// vs drop: files and strings from "another application" arrive as a
// trusted drag; a target whose dragover declines sees no drop.
#[test]
fn cell_drop_is_native() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, _token) = open_fixture(&ctx, "/drop.html");
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("note.txt");
        std::fs::write(&file, "from disk").unwrap();
        let file = file.to_string_lossy().into_owned();
        let r = ctx.vs(&[
            "drop",
            &page,
            "160",
            "120",
            "--file",
            &file,
            "--data",
            "text/plain=hello",
            "--data",
            "application/x-vs=custom",
        ]);
        assert_ok("drop", &r);
        assert_eq!(body_first(&r), "accepted", "{}", r.stdout);
        let mut got = String::new();
        for _ in 0..20 {
            got = eval_js(&ctx, &page, "String(window.__drop)");
            if got != "null" {
                break;
            }
            settle(100);
        }
        let ev = eval_js(&ctx, &page, "window.__ev.join(' ')");
        assert!(
            ev.contains("dragenter:true") && ev.contains("drop:true"),
            "events: {ev}"
        );
        assert!(got.contains("\"text\":\"hello\""), "text: {got}");
        assert!(got.contains("\"custom\":\"custom\""), "custom type: {got}");
        assert!(got.contains("note.txt=from disk"), "file: {got}");

        // Text alone travels on the pasteboard.
        let _ = eval_js(
            &ctx,
            &page,
            "(window.__ev = [], window.__drop = null, 'ok')",
        );
        let r = ctx.vs(&["drop", &page, "160", "120", "--data", "text/plain=plain"]);
        assert_ok("text drop", &r);
        let mut got = String::new();
        for _ in 0..20 {
            got = eval_js(&ctx, &page, "String(window.__drop)");
            if got != "null" {
                break;
            }
            settle(100);
        }
        assert!(got.contains("\"text\":\"plain\""), "text drop: {got}");

        let _ = eval_js(&ctx, &page, "(window.__ev = [], 'ok')");
        let r = ctx.vs(&["drop", &page, "160", "310", "--data", "text/plain=x"]);
        assert_ok("refused drop", &r);
        assert_eq!(body_first(&r), "refused", "{}", r.stdout);
        let ev = eval_js(&ctx, &page, "window.__ev.join(' ')");
        assert!(
            !ev.contains("off-drop"),
            "a refused drop must not fire drop: {ev}"
        );
    }
}

// An intercept page that moves between a loopback host and another one
// gets a new web view on macOS (the loopback scheme handler, or none).
// The new view took the host window's size, not the page's viewport,
// so the page laid out at 1440x900 after `goto` in a 500x500 viewport.
// Every engine: the viewport survives the navigation.
#[test]
fn cell_viewport_survives_a_goto_between_loopback_and_named_hosts() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        assert_ok("session-open", &ctx.vs(&["session-open", "--intercept"]));
        let r = ctx.vs(&["open", "about:blank"]);
        assert_ok("open", &r);
        let page = crate::support::body_first(&r);
        let r = ctx.vs(&["viewport", &page, "500x500", "--dpr", "1"]);
        assert_ok("viewport", &r);
        let named = ctx
            .url("/form.html")
            .replace("//127.0.0.1:", "//vs.localhost:");
        for url in [named, ctx.url("/form.html")] {
            assert_ok("goto", &ctx.vs(&["goto", &page, &url]));
            assert_eq!(
                eval_js(&ctx, &page, "innerWidth + 'x' + innerHeight"),
                "500x500",
                "after goto {url}"
            );
        }
    }
}
