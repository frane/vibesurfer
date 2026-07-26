//! Session video recording.
//!
//! `record_start` prefers *inline* recording: the engine grabs a frame
//! at every input step (mouse move, keystroke, click) and streams them
//! with the cursor position, so the recorder composites a pointer and
//! encodes continuous motion instead of a before/after slideshow. During
//! idle stretches (page loads, deliberate pauses) the drain loop refreshes
//! from the live page so navigations still show. Backends without inline
//! support fall back to the original time-based polling capture. One
//! recording per page.

// Timing/geometry arithmetic (ms clocks, frame counts, cursor rests).
// The casts are deliberate and bounded; the pedantic lints are noise.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::too_many_lines,
    clippy::ptr_arg
)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use vs_engine_webkit::RecFrame;

use super::Daemon;
use crate::error::{DaemonError, Result};

pub(crate) struct RecorderHandle {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<std::result::Result<PathBuf, String>>>,
}

impl Daemon {
    /// Start recording `page_id` to an H.264 MP4 file at about `fps`
    /// (clamped 1..=30). Returns the output path. Errors if the page
    /// is already recording.
    /// `max_width` downscales each frame so its width is at most that
    /// many pixels (aspect preserved); `0` keeps the full device
    /// resolution (retina). Downscaling is the default because a retina
    /// screen recording is huge to capture and encode.
    pub fn record_start(
        &self,
        session_id: &str,
        page_id: &str,
        fps: u32,
        max_width: u32,
    ) -> Result<PathBuf> {
        self.require_session(session_id)?;
        let handle = self.engine_handle_for(session_id, page_id)?;
        if self
            .inner
            .recorders
            .lock()
            .expect("poisoned")
            .contains_key(page_id)
        {
            return Err(DaemonError::BadRequest(format!(
                "already recording {page_id}"
            )));
        }
        std::fs::create_dir_all(&self.inner.captures_dir).map_err(DaemonError::Io)?;
        let out = self.inner.captures_dir.join(format!("rec-{page_id}.mp4"));
        let fps = fps.clamp(1, 30);

        // Prefer inline recording. The engine keeps the sender and emits
        // a frame per input step; if the backend can't (non-Cocoa), it
        // returns an error and we fall back to polling.
        let (tx, rx) = std::sync::mpsc::channel::<RecFrame>();
        let inline = self
            .inner
            .engine
            .record_begin(handle, tx, fps, max_width)
            .is_ok();

        let stop = Arc::new(AtomicBool::new(false));
        let stop_t = stop.clone();
        let daemon = self.clone();
        let page = page_id.to_string();
        let out_t = out.clone();
        let thread = std::thread::Builder::new()
            .name("vs-record".into())
            .spawn(move || -> std::result::Result<PathBuf, String> {
                let result = if inline {
                    record_inline(&daemon, &page, &out_t, fps, max_width, &stop_t, &rx)
                } else {
                    record_polling(&daemon, &page, &out_t, fps, max_width, &stop_t)
                };
                if inline {
                    let _ = daemon.inner.engine.record_end(handle);
                }
                result
            })
            .map_err(DaemonError::Io)?;
        self.inner.recorders.lock().expect("poisoned").insert(
            page_id.to_string(),
            RecorderHandle {
                stop,
                thread: Some(thread),
            },
        );
        Ok(out)
    }

    /// Stop recording `page_id`, flush the encoder, and return the
    /// written file path.
    pub fn record_stop(&self, page_id: &str) -> Result<PathBuf> {
        let mut handle = self
            .inner
            .recorders
            .lock()
            .expect("poisoned")
            .remove(page_id)
            .ok_or_else(|| DaemonError::BadRequest(format!("not recording {page_id}")))?;
        handle.stop.store(true, Ordering::Relaxed);
        let joined = handle
            .thread
            .take()
            .expect("thread present")
            .join()
            .map_err(|_| DaemonError::Other(anyhow::anyhow!("record thread panicked")))?;
        joined.map_err(|e| DaemonError::Other(anyhow::anyhow!(e)))
    }
}

/// Default resting cursor position for idle frames, as a fraction of the
/// frame. Used until the first input step reports a real position.
fn rest_cursor(w: usize, h: usize) -> (f64, f64) {
    (w as f64 * 0.82, h as f64 * 0.86)
}

/// One captured, not-yet-encoded frame: the raw snapshot PNG plus the
/// cursor state and the wall-clock time it was taken. Kept compressed
/// (PNG is ~100-200 KB) so a whole recording's worth fits in tens of MB.
struct Cap {
    t_ms: u128,
    png: Vec<u8>,
    cx: f64,
    cy: f64,
    click: f32,
}

/// Inline recording, capture-then-encode. rav1e is far too slow to encode
/// 1440p in real time (~1 s/frame single-tile), so trying to encode on the
/// capture path starved the recording down to a few frames. Instead this
/// *collects* lightweight timestamped PNGs at real-time cadence — motion
/// frames streamed from the input primitives, idle frames snapshotted from
/// the live page during pauses/loads — then, after `stop`, resamples the
/// timeline to an exact `fps` and encodes once. Playback runs at true
/// speed because each output frame is chosen by wall-clock timestamp.
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
fn record_inline(
    daemon: &Daemon,
    page: &str,
    out: &PathBuf,
    fps: u32,
    max_width: u32,
    stop: &AtomicBool,
    rx: &Receiver<RecFrame>,
) -> std::result::Result<PathBuf, String> {
    use std::time::Instant;

    let frame_dt = Duration::from_millis(1000 / u64::from(fps.max(1)));
    let give_up_after = fps.max(1) * 8;
    // Cap collected frames so a `start` with no `stop` can't grow memory
    // without bound: 30 minutes at `fps`, PNGs only.
    let cap_limit = (fps * 60 * 30) as usize;

    // Consecutive idle ticks before snapshotting the live page. Keeps a
    // brief gap between a move's frames from triggering a `live_frame`
    // that would block behind the still-running move on the main thread.
    let refresh_after = (fps / 8).max(2);

    // Real wall-clock start. Every frame is stamped with the actual time
    // it was captured, so the recording is a faithful capture of what
    // happened, at true speed. No re-timing.
    let start = Instant::now();
    let mut caps: Vec<Cap> = Vec::new();
    let mut last_cursor: (f64, f64) = (0.0, 0.0);
    let mut have_cursor = false;
    let mut misses = 0u32;
    let mut idle_ticks = 0u32;

    // Seed on the current page so the recording opens on it immediately.
    if let Ok(png) = daemon.live_frame(page, max_width) {
        if let Ok((w, h, _)) = vs_record::png_to_scaled_rgb(&png, max_width) {
            last_cursor = rest_cursor(w, h);
            have_cursor = true;
            caps.push(Cap {
                t_ms: 0,
                png,
                cx: last_cursor.0,
                cy: last_cursor.1,
                click: 0.0,
            });
        }
    }

    while !stop.load(Ordering::Relaxed) && caps.len() < cap_limit {
        match rx.recv_timeout(frame_dt) {
            // A real input step captured this frame with the cursor on it.
            Ok(frame) => {
                misses = 0;
                idle_ticks = 0;
                last_cursor = (frame.cx, frame.cy);
                have_cursor = true;
                caps.push(Cap {
                    t_ms: start.elapsed().as_millis(),
                    png: frame.png,
                    cx: frame.cx,
                    cy: frame.cy,
                    click: frame.click,
                });
            }
            // Idle: no input running. Snapshot the live page so page loads
            // and pauses are captured too, cursor at its last spot.
            Err(RecvTimeoutError::Timeout) => {
                idle_ticks += 1;
                if idle_ticks < refresh_after {
                    continue;
                }
                idle_ticks = 0;
                if let Ok(png) = daemon.live_frame(page, max_width) {
                    misses = 0;
                    let (cx, cy) = if have_cursor { last_cursor } else { (0.0, 0.0) };
                    caps.push(Cap {
                        t_ms: start.elapsed().as_millis(),
                        png,
                        cx,
                        cy,
                        click: 0.0,
                    });
                } else {
                    misses += 1;
                    if misses > give_up_after {
                        break;
                    }
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    if caps.is_empty() {
        return Err("no frames captured".into());
    }

    // Resample the captured timeline to an exact `fps` and encode once.
    // Decoding/compositing is cached per source frame so a held frame
    // (e.g. a 2 s pause) isn't decoded on every output tick.
    let step_ms = u128::from(1000 / u64::from(fps.max(1)));
    let end_ms = caps.last().map_or(0, |c| c.t_ms);
    let mut rec: Option<vs_record::H264Recorder> = None;
    let mut src = 0usize;
    let mut cached_i: Option<usize> = None;
    let mut base: Vec<u8> = Vec::new();
    let (mut bw, mut bh) = (0usize, 0usize);

    let mut t = 0u128;
    loop {
        while src + 1 < caps.len() && caps[src + 1].t_ms <= t {
            src += 1;
        }
        if cached_i != Some(src) {
            let (w, h, rgb) = vs_record::png_to_scaled_rgb(&caps[src].png, max_width)
                .map_err(|e| e.to_string())?;
            base = rgb;
            bw = w;
            bh = h;
            cached_i = Some(src);
        }
        let mut rgb = base.clone();
        vs_record::composite_cursor(
            &mut rgb,
            bw,
            bh,
            caps[src].cx,
            caps[src].cy,
            caps[src].click,
        );
        if rec.is_none() {
            rec = Some(
                // Offline encode: a slower preset for sharper text/edges.
                vs_record::H264Recorder::create(out, bw, bh, fps).map_err(|e| e.to_string())?,
            );
        }
        if let Some(r) = rec.as_mut() {
            // Fail the recording on a real encode error rather than
            // silently writing an empty file.
            r.push_rgb(bw, bh, &rgb).map_err(|e| e.to_string())?;
        }
        if t >= end_ms {
            break;
        }
        t += step_ms;
    }

    match rec {
        Some(r) => {
            r.finish().map_err(|e| e.to_string())?;
            Ok(out.clone())
        }
        None => Err("no frames captured".into()),
    }
}

/// Fallback recording for backends without inline support: capture the
/// page on a fixed timer. Produces smooth video only when the page
/// animates on its own; synthesized input shows up as discrete states.
fn record_polling(
    daemon: &Daemon,
    page: &str,
    out: &PathBuf,
    fps: u32,
    max_width: u32,
    stop: &AtomicBool,
) -> std::result::Result<PathBuf, String> {
    let interval = Duration::from_millis(1000 / u64::from(fps));
    let mut rec: Option<vs_record::H264Recorder> = None;
    let mut misses = 0u32;
    let give_up_after = fps * 5;
    let max_frames = u64::from(fps) * 60 * 30;
    let mut frames = 0u64;
    while !stop.load(Ordering::Relaxed) && frames < max_frames {
        let Ok(png) = daemon.live_frame(page, max_width) else {
            misses += 1;
            if misses > give_up_after {
                break;
            }
            std::thread::sleep(interval);
            continue;
        };
        misses = 0;
        let Ok((w, h, rgb)) = vs_record::png_to_scaled_rgb(&png, max_width) else {
            std::thread::sleep(interval);
            continue;
        };
        if rec.is_none() {
            rec = Some(vs_record::H264Recorder::create(out, w, h, fps).map_err(|e| e.to_string())?);
        }
        if let Some(r) = rec.as_mut() {
            r.push_rgb(w, h, &rgb).map_err(|e| e.to_string())?;
            frames += 1;
        }
        std::thread::sleep(interval);
    }
    match rec {
        Some(r) => {
            r.finish().map_err(|e| e.to_string())?;
            Ok(out.clone())
        }
        None => Err("no frames captured".into()),
    }
}
