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
use objc2::MainThreadMarker;
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
/// Settle waits (ms) after mouseDown and mouseUp, per input mode.
/// Robotic skips the inter-event delays for a fast trusted click;
/// human and careful keep enough for the click to deliver and its
/// handlers to run before the caller reads state.
fn click_settle(mode: vs_humanize::InputMode) -> (u64, u64) {
    match mode {
        vs_humanize::InputMode::Robotic => (2, 6),
        _ => (15, 30),
    }
}

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
    let (down_ms, up_ms) = click_settle(mode);
    web_view.mouseDown(&down);
    let _ = run_loop_until(|| false, Duration::from_millis(down_ms));
    web_view.mouseUp(&up);
    let _ = run_loop_until(|| false, Duration::from_millis(up_ms));
    Ok(end)
}

/// Trusted MouseMoved sequence from `start` to `end` along a Bezier
/// path. Used by `cursor_op` (MoveTo / HoverAt) and as the drag
/// trajectory between mouseDown and mouseUp.
#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_arguments)]
pub(super) fn move_along_path(
    web_view: &Retained<WKWebView>,
    window: &Retained<NSWindow>,
    webview_height: f64,
    start: vs_humanize::Point,
    end: vs_humanize::Point,
    mode: vs_humanize::InputMode,
    seed: u64,
    button_down: bool,
    rec: Option<(&super::record::RecSink, MainThreadMarker)>,
) -> EngineResult<vs_humanize::Point> {
    let window_number = window.windowNumber();
    let make_event = |ty: NSEventType, p: vs_humanize::Point| -> EngineResult<Retained<NSEvent>> {
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
            if button_down { 1.0 } else { 0.0 },
        )
        .ok_or_else(|| EngineError::Other(format!("NSEvent::mouseEventWithType returned nil for {ty:?}")))
    };
    let path = vs_humanize::mouse_path(start, end, mode, seed);
    let mut prev_ms: u128 = 0;
    let move_type = if button_down {
        NSEventType::LeftMouseDragged
    } else {
        NSEventType::MouseMoved
    };
    for step in &path {
        if step.kind == vs_humanize::MouseStepKind::Move {
            let mv = make_event(move_type, step.point)?;
            if button_down {
                web_view.mouseDragged(&mv);
            } else {
                web_view.mouseMoved(&mv);
            }
            let now_ms = step.at.as_millis();
            let delta = now_ms.saturating_sub(prev_ms);
            if let Some((sink, mtm)) = rec {
                sink.step(web_view, mtm, step.point.x, step.point.y, delta as f64);
            }
            if delta > 0 {
                let _ = run_loop_until(
                    || false,
                    Duration::from_millis(u64::try_from(delta).unwrap_or(0)),
                );
            }
            prev_ms = now_ms;
        }
    }
    // Final settling move so the cursor ends exactly at `end`.
    let final_mv = make_event(move_type, end)?;
    if button_down {
        web_view.mouseDragged(&final_mv);
    } else {
        web_view.mouseMoved(&final_mv);
    }
    if let Some((sink, mtm)) = rec {
        sink.frame(web_view, mtm, end.x, end.y, 0.0, 16);
    }
    Ok(end)
}

/// Trusted click at exact coordinates. Routes through `move_along_path`
/// for the humanized lead-in, then dispatches the down/up pair at
/// `target`.
#[allow(clippy::too_many_arguments)]
pub(super) fn click_at_xy(
    web_view: &Retained<WKWebView>,
    window: &Retained<NSWindow>,
    webview_height: f64,
    start: vs_humanize::Point,
    target: vs_humanize::Point,
    mode: vs_humanize::InputMode,
    seed: u64,
    rec: Option<(&super::record::RecSink, MainThreadMarker)>,
) -> EngineResult<vs_humanize::Point> {
    let landed = move_along_path(
        web_view,
        window,
        webview_height,
        start,
        target,
        mode,
        seed,
        false,
        rec,
    )?;
    let window_number = window.windowNumber();
    let loc = NSPoint::new(target.x, webview_height - target.y);
    let make = |ty: NSEventType| -> EngineResult<Retained<NSEvent>> {
        NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
            ty, loc, NSEventModifierFlags::empty(), 0.0, window_number, None, 0, 1, 1.0,
        ).ok_or_else(|| EngineError::Other(format!("NSEvent::mouseEventWithType returned nil for {ty:?}")))
    };
    let (down_ms, up_ms) = click_settle(mode);
    let down = make(NSEventType::LeftMouseDown)?;
    if let Some((sink, mtm)) = rec {
        sink.frame(web_view, mtm, target.x, target.y, 1.0, 60);
    }
    web_view.mouseDown(&down);
    let _ = run_loop_until(|| false, Duration::from_millis(down_ms));
    if let Some((sink, mtm)) = rec {
        sink.frame(web_view, mtm, target.x, target.y, 0.55, down_ms as u32);
    }
    let up = make(NSEventType::LeftMouseUp)?;
    web_view.mouseUp(&up);
    let _ = run_loop_until(|| false, Duration::from_millis(up_ms));
    if let Some((sink, mtm)) = rec {
        sink.frame(web_view, mtm, target.x, target.y, 0.2, up_ms as u32);
    }
    Ok(landed)
}

/// Trusted drag from `start` to `target`: mouseDown at `start`, a
/// humanized dragged path to `target`, mouseUp at `target`.
#[allow(clippy::too_many_arguments)]
pub(super) fn drag_xy(
    web_view: &Retained<WKWebView>,
    window: &Retained<NSWindow>,
    webview_height: f64,
    start: vs_humanize::Point,
    target: vs_humanize::Point,
    mode: vs_humanize::InputMode,
    seed: u64,
    rec: Option<(&super::record::RecSink, MainThreadMarker)>,
) -> EngineResult<vs_humanize::Point> {
    let window_number = window.windowNumber();
    let make = |ty: NSEventType, p: vs_humanize::Point| -> EngineResult<Retained<NSEvent>> {
        let loc = NSPoint::new(p.x, webview_height - p.y);
        NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
            ty, loc, NSEventModifierFlags::empty(), 0.0, window_number, None, 0, 1, 1.0,
        ).ok_or_else(|| EngineError::Other(format!("NSEvent::mouseEventWithType returned nil for {ty:?}")))
    };
    let down = make(NSEventType::LeftMouseDown, start)?;
    web_view.mouseDown(&down);
    let _ = run_loop_until(|| false, Duration::from_millis(15));
    let landed = move_along_path(
        web_view,
        window,
        webview_height,
        start,
        target,
        mode,
        seed,
        true,
        rec,
    )?;
    let up = make(NSEventType::LeftMouseUp, target)?;
    web_view.mouseUp(&up);
    let _ = run_loop_until(|| false, Duration::from_millis(30));
    Ok(landed)
}

/// Type `text` into the focused element with trusted per-character
/// key events. Each char becomes a KeyDown+KeyUp NSEvent whose
/// `characters` field carries the glyph; WebKit runs its full text
/// insertion pipeline (keydown → beforeinput → input) with
/// isTrusted=true, so DraftJS/ProseMirror/contenteditable and
/// framework-controlled inputs accept it where the prototype-setter
/// `fill` path is ignored. The caller places the caret first.
///
/// `mode` controls inter-keystroke delay so the cadence isn't
/// robotically uniform: Human jitters ~30-90ms, Careful ~120ms,
/// Robotic fires as fast as the run loop drains.
pub(super) fn type_text(
    web_view: &Retained<WKWebView>,
    text: &str,
    mode: vs_humanize::InputMode,
    seed: u64,
    rec: Option<(&super::record::RecSink, MainThreadMarker)>,
    cursor: vs_humanize::Point,
) -> EngineResult<()> {
    let make_key = |ty: NSEventType, ch: &str| -> EngineResult<Retained<NSEvent>> {
        let chars = objc2_foundation::NSString::from_str(ch);
        // keyCode 0 is fine for text insertion: WebKit inserts from
        // the `characters` string, not the virtual keycode, for the
        // printable path. Modifier flags empty — we type literal
        // glyphs, not chords (chords stay on `vs_act key`).
        NSEvent::keyEventWithType_location_modifierFlags_timestamp_windowNumber_context_characters_charactersIgnoringModifiers_isARepeat_keyCode(
            ty,
            NSPoint::new(0.0, 0.0),
            NSEventModifierFlags::empty(),
            0.0,
            0,
            None,
            &chars,
            &chars,
            false,
            0,
        )
        .ok_or_else(|| EngineError::Other(format!("keyEventWithType returned nil for {ch:?}")))
    };

    let base_delay = match mode {
        vs_humanize::InputMode::Robotic => 0u64,
        vs_humanize::InputMode::Careful => 120,
        vs_humanize::InputMode::Human => 45,
    };
    let mut jitter = seed | 1;
    for (i, ch) in text.chars().enumerate() {
        let s = ch.to_string();
        let down = make_key(NSEventType::KeyDown, &s)?;
        let up = make_key(NSEventType::KeyUp, &s)?;
        web_view.keyDown(&down);
        // Between down and up, and between chars, drain the run loop
        // so WebKit processes the insertion before the next event.
        let hold = if base_delay == 0 { 1 } else { base_delay / 3 };
        let _ = run_loop_until(|| false, Duration::from_millis(hold.max(1)));
        web_view.keyUp(&up);
        if base_delay > 0 {
            // xorshift jitter in ±40% of base, Human only.
            jitter ^= jitter << 13;
            jitter ^= jitter >> 7;
            jitter ^= jitter << 17;
            let span = if matches!(mode, vs_humanize::InputMode::Human) {
                base_delay * 4 / 10
            } else {
                0
            };
            let extra = if span == 0 {
                0
            } else {
                jitter % (span * 2 + 1)
            };
            let wait = base_delay.saturating_sub(span).saturating_add(extra);
            let _ = run_loop_until(|| false, Duration::from_millis(wait.max(1)));
            if let Some((sink, mtm)) = rec {
                sink.frame(web_view, mtm, cursor.x, cursor.y, 0.0, (hold + wait) as u32);
            }
        } else if i % 8 == 0 {
            // Robotic: still yield occasionally so a long string
            // doesn't starve the main thread.
            let _ = run_loop_until(|| false, Duration::from_millis(1));
        }
    }
    // Drain the run loop several times after the final keystroke so
    // the web-content process delivers the last keyUp -> beforeinput
    // -> input before the caller reads the page. One block is not
    // enough under load (CI mac dropped the last char or two);
    // repeated short ticks give the content process more scheduling
    // opportunities.
    for _ in 0..6 {
        let _ = run_loop_until(|| false, Duration::from_millis(25));
    }
    Ok(())
}
