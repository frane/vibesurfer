//! Inline recording sink for the Cocoa backend.
//!
//! A [`RecSink`] is attached to a page by `record_begin`. The input
//! primitives (`move_along_path`, `click_at_xy`, `type_text`) call into
//! it as they run, so frames are grabbed *inside* the automation loop on
//! the main thread rather than by a poller that is blocked behind it.
//! That is what makes a recording show continuous mouse motion instead
//! of a before/after slideshow. Each frame carries the cursor position
//! in frame pixels so the recorder can composite a pointer.

use std::cell::Cell;
use std::sync::mpsc::Sender;

use objc2::MainThreadMarker;
use objc2_web_kit::WKWebView;

use super::capture::snapshot_to_png_bytes;
use crate::engine::RecFrame;

pub(super) struct RecSink {
    tx: Sender<RecFrame>,
    /// Snapshot width in pixels passed to WebKit; also the recorded
    /// frame width, so cursor coordinates scale by `scale` below.
    snapshot_width: f64,
    /// frame_px / css_px. Cursor positions arrive in CSS pixels and are
    /// scaled into the frame's pixel space before being sent.
    scale: f64,
    /// Milliseconds per emitted frame (1000 / fps). Motion steps arrive
    /// every ~16 ms; we coalesce them to this cadence so the encoder
    /// gets a steady frame rate without a snapshot per raw step.
    frame_ms: f64,
    accum: Cell<f64>,
}

impl RecSink {
    /// `logical_width` is the web view's CSS width; `max_width` the
    /// requested recording width (`0` = use the logical width at 1x).
    pub(super) fn new(tx: Sender<RecFrame>, logical_width: f64, max_width: u32, fps: u32) -> Self {
        let snapshot_width = if max_width == 0 {
            logical_width.max(1.0)
        } else {
            f64::from(max_width)
        };
        let scale = if logical_width > 0.0 {
            snapshot_width / logical_width
        } else {
            1.0
        };
        let fps = fps.clamp(1, 30);
        Self {
            tx,
            snapshot_width,
            scale,
            frame_ms: 1000.0 / f64::from(fps),
            accum: Cell::new(0.0),
        }
    }

    /// Grab a frame now, cursor at CSS `(x, y)`, with click ripple phase
    /// `click` (`0.0` = none) and intended duration `dt_ms`. Failures are
    /// dropped: a recording must never break automation.
    pub(super) fn frame(
        &self,
        web_view: &WKWebView,
        mtm: MainThreadMarker,
        x: f64,
        y: f64,
        click: f32,
        dt_ms: u32,
    ) {
        if let Ok(png) = snapshot_to_png_bytes(web_view, mtm, Some(self.snapshot_width)) {
            let _ = self.tx.send(RecFrame {
                png,
                cx: x * self.scale,
                cy: y * self.scale,
                click,
                dt_ms,
            });
        }
    }

    /// Called once per motion step with the intended `dt_ms` since the
    /// previous step. Emits a frame only when enough intended time has
    /// accumulated to hit the target frame rate; the emitted frame's
    /// `dt_ms` is that accumulated intended time, so playback ignores the
    /// snapshot cost and moves at natural speed.
    pub(super) fn step(
        &self,
        web_view: &WKWebView,
        mtm: MainThreadMarker,
        x: f64,
        y: f64,
        dt_ms: f64,
    ) {
        let a = self.accum.get() + dt_ms;
        if a >= self.frame_ms {
            self.accum.set(0.0);
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let dt = a.round() as u32;
            self.frame(web_view, mtm, x, y, 0.0, dt);
        } else {
            self.accum.set(a);
        }
    }
}
