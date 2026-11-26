//! Native mouse input through `SendMouseInput` on the composition
//! controller: humanized paths, presses and the wheel.

use std::time::Duration;

use webview2_com::Microsoft::Web::WebView2::Win32::{
    ICoreWebView2CompositionController, COREWEBVIEW2_MOUSE_EVENT_KIND,
    COREWEBVIEW2_MOUSE_EVENT_KIND_HORIZONTAL_WHEEL, COREWEBVIEW2_MOUSE_EVENT_KIND_LEFT_BUTTON_DOWN,
    COREWEBVIEW2_MOUSE_EVENT_KIND_LEFT_BUTTON_UP, COREWEBVIEW2_MOUSE_EVENT_KIND_MOVE,
    COREWEBVIEW2_MOUSE_EVENT_KIND_WHEEL, COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS_NONE,
};
use windows::Win32::Foundation::POINT;

use crate::engine::{CursorOp, EngineError, EngineResult};

pub(super) fn wv2_humanize_seed(op: CursorOp) -> u64 {
    let (a, b, c, d) = match op {
        CursorOp::MoveTo { x, y } | CursorOp::HoverAt { x, y } | CursorOp::ClickAt { x, y } => {
            (x, y, 0.0, 0.0)
        }
        CursorOp::Wheel { x, y, dx, dy } => (x, y, dx, dy),
        CursorOp::Click { x, y, .. } => (x, y, 0.0, 0.0),
        CursorOp::Drag { x1, y1, x2, y2 } => (x1, y1, x2, y2),
    };
    let bits = |v: f64| v.to_bits();
    bits(a).wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ bits(b).wrapping_mul(0xBF58_476D_1CE4_E5B9)
        ^ bits(c).wrapping_mul(0x94D0_49BB_1331_11EB)
        ^ bits(d)
}

#[allow(clippy::cast_possible_truncation)]
pub(super) fn point_at(p: vs_humanize::Point) -> POINT {
    POINT {
        x: p.x.round() as i32,
        y: p.y.round() as i32,
    }
}

pub(super) fn wv2_send_mouse(
    comp: &ICoreWebView2CompositionController,
    kind: COREWEBVIEW2_MOUSE_EVENT_KIND,
    point: vs_humanize::Point,
) -> EngineResult<()> {
    unsafe {
        comp.SendMouseInput(
            kind,
            COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS_NONE,
            0,
            point_at(point),
        )
        .map_err(|e| EngineError::Other(format!("SendMouseInput: {e}")))
    }
}

/// A trusted wheel turn of `(dx, dy)` CSS px at `point`. `mouseData`
/// is in 120ths of a notch, positive away from the user (up) for the
/// vertical wheel and to the right for the horizontal one; Chromium
/// scrolls a notch as 100 px, so one CSS px is 1.2 units.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub(super) fn wv2_wheel(
    comp: &ICoreWebView2CompositionController,
    point: vs_humanize::Point,
    dx: f64,
    dy: f64,
) -> EngineResult<()> {
    let units = |px: f64| (px * 1.2).round() as i32;
    for (kind, data) in [
        (COREWEBVIEW2_MOUSE_EVENT_KIND_WHEEL, -units(dy)),
        (COREWEBVIEW2_MOUSE_EVENT_KIND_HORIZONTAL_WHEEL, units(dx)),
    ] {
        if data == 0 {
            continue;
        }
        unsafe {
            comp.SendMouseInput(
                kind,
                COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS_NONE,
                data as u32,
                point_at(point),
            )
            .map_err(|e| EngineError::Other(format!("SendMouseInput wheel: {e}")))?;
        }
    }
    Ok(())
}

pub(super) fn wv2_move_along_path(
    comp: &ICoreWebView2CompositionController,
    start: vs_humanize::Point,
    end: vs_humanize::Point,
    mode: vs_humanize::InputMode,
    seed: u64,
) -> EngineResult<vs_humanize::Point> {
    let path = vs_humanize::mouse_path(start, end, mode, seed);
    let mut prev_ms: u128 = 0;
    for step in &path {
        if step.kind != vs_humanize::MouseStepKind::Move {
            continue;
        }
        wv2_send_mouse(comp, COREWEBVIEW2_MOUSE_EVENT_KIND_MOVE, step.point)?;
        let dt = step.at.as_millis().saturating_sub(prev_ms);
        if dt > 0 {
            std::thread::sleep(Duration::from_millis(u64::try_from(dt).unwrap_or(0)));
        }
        prev_ms = step.at.as_millis();
    }
    // Final settling move ending exactly at `end`.
    wv2_send_mouse(comp, COREWEBVIEW2_MOUSE_EVENT_KIND_MOVE, end)?;
    Ok(end)
}

pub(super) fn wv2_press_release(
    comp: &ICoreWebView2CompositionController,
    at: vs_humanize::Point,
) -> EngineResult<()> {
    wv2_send_mouse(comp, COREWEBVIEW2_MOUSE_EVENT_KIND_LEFT_BUTTON_DOWN, at)?;
    std::thread::sleep(Duration::from_millis(15));
    wv2_send_mouse(comp, COREWEBVIEW2_MOUSE_EVENT_KIND_LEFT_BUTTON_UP, at)?;
    std::thread::sleep(Duration::from_millis(30));
    Ok(())
}
