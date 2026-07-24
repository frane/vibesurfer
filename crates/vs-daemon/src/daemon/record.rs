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
    pub fn record_start(&self, session_id: &str, page_id: &str, fps: u32) -> Result<PathBuf> {
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
                while !stop_t.load(Ordering::Relaxed) {
                    let Ok(png) = daemon.live_frame(&page) else {
                        break; // page closed or engine gone
                    };
                    if rec.is_none() {
                        let (w, h) = vs_record::png_dimensions(&png).map_err(|e| e.to_string())?;
                        rec = Some(
                            vs_record::Recorder::new(w, h, fps, 9).map_err(|e| e.to_string())?,
                        );
                    }
                    if let Some(r) = rec.as_mut() {
                        // A frame that changed size (viewport change) is
                        // skipped, not fatal.
                        let _ = r.push_png(&png);
                    }
                    std::thread::sleep(interval);
                }
                match rec {
                    Some(r) => {
                        r.finish_to_file(&out_t).map_err(|e| e.to_string())?;
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
