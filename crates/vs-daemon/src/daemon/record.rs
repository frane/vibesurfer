//! Session video recording.
//!
//! `record_start` spawns a background thread that captures the page at
//! roughly `fps` via the transient live-frame path and feeds each PNG
//! to a [`vs_record::Recorder`] (pure-Rust AV1). `record_stop` signals
//! the thread, flushes, and returns the written IVF path. One recording
//! per page. Capture goes through the same engine dispatch as every
//! other frame, so it serializes on the platform main thread and never
//! races other primitives.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use super::Daemon;
use crate::error::{DaemonError, Result};

pub(crate) struct RecorderHandle {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<std::result::Result<PathBuf, String>>>,
}

impl Daemon {
    /// Start recording `page_id` to an AV1 IVF file at about `fps`
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
        // Validate the page is addressable in this session.
        let _ = self.engine_handle_for(session_id, page_id)?;
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
        let out = self.inner.captures_dir.join(format!("rec-{page_id}.ivf"));
        let fps = fps.clamp(1, 30);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_t = stop.clone();
        let daemon = self.clone();
        let page = page_id.to_string();
        let out_t = out.clone();
        let thread = std::thread::Builder::new()
            .name("vs-record".into())
            .spawn(move || -> std::result::Result<PathBuf, String> {
                let interval = Duration::from_millis(1000 / u64::from(fps));
                let mut rec: Option<vs_record::Recorder> = None;
                // A `live_frame` capture transiently fails mid-navigation
                // (the page is between documents). That is normal during a
                // recording that spans clicks and gotos, so skip the frame
                // and keep going; only give up once the page has been
                // unreachable for a sustained stretch (~5s), which means it
                // was really closed.
                let mut misses = 0u32;
                let give_up_after = fps * 5;
                // Hard cap so a `record start` that never gets a matching
                // stop can't fill the disk. 30 minutes at the requested
                // fps; frames stream to disk, so memory is bounded either
                // way, but the file size is not.
                let max_frames = u64::from(fps) * 60 * 30;
                let mut frames = 0u64;
                while !stop_t.load(Ordering::Relaxed) && frames < max_frames {
                    let Ok(png) = daemon.live_frame(&page) else {
                        misses += 1;
                        if misses > give_up_after {
                            break;
                        }
                        std::thread::sleep(interval);
                        continue;
                    };
                    misses = 0;
                    // Decode + downscale once, then feed raw RGB to the
                    // encoder (avoids a second PNG round trip and bounds
                    // the encoder's per-frame memory at retina).
                    let Ok((w, h, rgb)) = vs_record::png_to_scaled_rgb(&png, max_width) else {
                        std::thread::sleep(interval);
                        continue;
                    };
                    if rec.is_none() {
                        // Stream straight to the IVF file: memory stays at
                        // ~one frame no matter how long the recording runs.
                        rec = Some(
                            vs_record::Recorder::create(&out_t, w, h, fps, 9)
                                .map_err(|e| e.to_string())?,
                        );
                    }
                    if let Some(r) = rec.as_mut() {
                        // A frame that changed size (viewport change) is
                        // skipped, not fatal.
                        let _ = r.push_rgb(w, h, &rgb);
                        frames += 1;
                    }
                    std::thread::sleep(interval);
                }
                match rec {
                    Some(r) => {
                        r.finish().map_err(|e| e.to_string())?;
                        Ok(out_t)
                    }
                    None => Err("no frames captured".into()),
                }
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
