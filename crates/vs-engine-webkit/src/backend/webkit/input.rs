//! Trusted mouse-input dispatch on macOS.
//!
//! Calling `el.click()` from injected JS produces a `MouseEvent` with
//! `event.isTrusted = false`. Most sites ignore that flag, but
//! anti-bot pipelines (Google, Cloudflare, hCaptcha) do not — they
//! treat untrusted clicks as automated and block.
//!
//! The fix is to drive the OS event pipeline. We construct an
//! `NSEvent` of type `LeftMouseDown`/`LeftMouseUp` and dispatch it
//! to the webview's `NSResponder` directly via `mouseDown:` /
//! `mouseUp:`. The event flows into WebKit's internal event
//! dispatcher and out as a JS `click` with `event.isTrusted = true`
//! — indistinguishable from a real user click.
//!
//! Why direct-to-responder, not `NSWindow::sendEvent`: our hosting
//! window is offscreen (no `orderFront`), so its windowNumber is 0
//! and the macOS window server filters our synthesized event out
//! before it reaches the responder chain. Hopping the window server
//! by calling `mouseDown:` directly on the webview gets the event
//! into WebKit anyway. The `NSWindow` is still required as a
//! container — without it the webview has no responder context and
//! `mouseDown:` is a no-op.
//!
//! Coordinate quirk: `NSEvent` location is in window-local
//! coordinates with origin bottom-left. Web rects come from JS
//! `getBoundingClientRect()` in client (top-left origin). We flip Y
//! against the webview's height to bridge the two.

use std::time::Duration;

use objc2::rc::Retained;
use objc2_app_kit::{NSEvent, NSEventModifierFlags, NSEventType, NSWindow};
use objc2_foundation::NSPoint;
use objc2_web_kit::WKWebView;

use crate::engine::{EngineError, EngineResult};

use super::eval::{eval_js_string, run_loop_until};

/// Axis-aligned bounding box in CSS pixels, top-left origin.
#[derive(Debug, Clone, Copy)]
pub(super) struct ClientRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// Resolve the bounding rect of `data-vs-ref="r"` via JS, scrolling
/// the element into view first so the rect is inside the viewport.
/// A real user clicks something they can see; if the element is
/// below the fold, the WebKit input pipeline no-ops the click
/// because hit-testing at the synthesized location finds nothing.
/// Returns `None` if the element isn't in the DOM.
pub(super) fn ref_rect(
    web_view: &Retained<WKWebView>,
    r: vs_protocol::Ref,
) -> EngineResult<Option<ClientRect>> {
    let js = format!(
        r#"(function() {{
            var el = document.querySelector('[data-vs-ref="{r}"]');
            if (!el) return 'null';
            // Scroll into the viewport's vertical center if it's
            // off-screen. `instant` keeps the test deterministic
            // (no smooth-scroll animation racing the rect read).
            try {{
                el.scrollIntoView({{behavior: 'instant', block: 'center', inline: 'center'}});
            }} catch (e) {{
                el.scrollIntoView();
            }}
            var b = el.getBoundingClientRect();
            return JSON.stringify({{x: b.x, y: b.y, w: b.width, h: b.height}});
        }})()"#,
        r = r.0,
    );
    let result = eval_js_string(web_view, &js, Duration::from_secs(5))?;
    let unwrapped = serde_json::from_str::<String>(&result).unwrap_or(result);
    if unwrapped == "null" {
        return Ok(None);
    }
    let v: serde_json::Value = serde_json::from_str(&unwrapped)
        .map_err(|e| EngineError::Other(format!("ref_rect parse: {e}")))?;
    Ok(Some(ClientRect {
        x: v["x"].as_f64().unwrap_or(0.0),
        y: v["y"].as_f64().unwrap_or(0.0),
        width: v["w"].as_f64().unwrap_or(0.0),
        height: v["h"].as_f64().unwrap_or(0.0),
    }))
}

/// Dispatch a trusted left-click at the center of `rect`. See module
/// docs for why we route through `WKWebView::mouseDown:` directly
/// instead of `NSWindow::sendEvent:`.
pub(super) fn click_at_rect(
    web_view: &Retained<WKWebView>,
    window: &Retained<NSWindow>,
    rect: ClientRect,
    webview_height: f64,
    start: vs_humanize::Point,
    mode: vs_humanize::InputMode,
    seed: u64,
) -> EngineResult<vs_humanize::Point> {
    // Target center in client (top-left origin).
    let cx = rect.x + rect.width / 2.0;
    let cy = rect.y + rect.height / 2.0;
    let end = vs_humanize::Point { x: cx, y: cy };
    let window_number = window.windowNumber();

    let make_event = |ty: NSEventType, p: vs_humanize::Point| -> EngineResult<Retained<NSEvent>> {
        // Cocoa is bottom-left origin; flip against view height.
        let loc = NSPoint::new(p.x, webview_height - p.y);
        NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
            ty,
            loc,
            NSEventModifierFlags::empty(),
            0.0,
            window_number,
            None,
            0,
            1,
            1.0,
        )
        .ok_or_else(|| EngineError::Other(format!("NSEvent::mouseEventWithType returned nil for {ty:?}")))
    };

    // Humanized lead-in: dispatch a sequence of MouseMoved events along
    // a Bezier path from `start` to `end`. The native dispatch keeps
    // every event's `isTrusted = true`, so the visible mouse motion
    // looks indistinguishable from a real cursor reaching the target
    // before the click. `Robotic` returns an empty path; `Careful` a
    // single move; `Human` a full Bezier with Fitts arrival timing.
    let path = vs_humanize::mouse_path(start, end, mode, seed);
    let mut prev_ms: u128 = 0;
    for step in &path {
        // `Down`/`Up`/`Click` from the humanize sequence are not
        // dispatched here — the trusted click below sends the canonical
        // down/up pair so click-count and pressure stay consistent. The
        // path only contributes the move sequence.
        if step.kind == vs_humanize::MouseStepKind::Move {
            let mv = make_event(NSEventType::MouseMoved, step.point)?;
            web_view.mouseMoved(&mv);
            let now_ms = step.at.as_millis();
            let delta = now_ms.saturating_sub(prev_ms);
            if delta > 0 {
                let _ = run_loop_until(
                    || false,
                    Duration::from_millis(u64::try_from(delta).unwrap_or(0)),
                );
            }
            prev_ms = now_ms;
        }
    }

    let down = make_event(NSEventType::LeftMouseDown, end)?;
    let up = make_event(NSEventType::LeftMouseUp, end)?;
    web_view.mouseDown(&down);
    let _ = run_loop_until(|| false, Duration::from_millis(15));
    web_view.mouseUp(&up);
    let _ = run_loop_until(|| false, Duration::from_millis(30));
    Ok(end)
}
