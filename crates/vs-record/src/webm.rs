//! WebM (VP8) recording: frames encoded with libvpx, muxed here.
//!
//! The container is written in one piece at `finish`: EBML header, then
//! a Segment with Info (duration), Tracks (one VP8 video track),
//! Clusters of SimpleBlocks (a new cluster at each keyframe, and at
//! least every 30 s so block timecodes fit in 16 bits) and Cues (one
//! per cluster) so players can seek. Every element size is written as
//! an 8-byte EBML number, which every reader accepts.

#![allow(
    unsafe_code,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]

use std::ffi::c_int;
use std::path::{Path, PathBuf};

/// libvpx, as build.rs generated it.
#[allow(
    non_upper_case_globals,
    non_camel_case_types,
    non_snake_case,
    dead_code,
    clippy::all,
    clippy::pedantic
)]
mod vpx {
    include!(concat!(env!("OUT_DIR"), "/vpx.rs"));
}

use crate::{RecordError, Result};

/// Real-time VP8 recorder writing a WebM file.
pub struct WebmRecorder {
    ctx: Box<vpx::vpx_codec_ctx_t>,
    img: *mut vpx::vpx_image_t,
    width: usize,
    height: usize,
    fps: u32,
    pts: i64,
    /// (time in ms, keyframe, VP8 frame).
    frames: Vec<(u64, bool, Vec<u8>)>,
    path: PathBuf,
}

// The codec context and image are owned and only touched through
// `&mut self`; the recorder moves between threads but is never shared.
unsafe impl Send for WebmRecorder {}

impl WebmRecorder {
    /// A recorder for `width`x`height` (rounded down to even) frames at
    /// `fps`, writing `path` on `finish`.
    pub fn create(path: &Path, width: usize, height: usize, fps: u32) -> Result<Self> {
        let width = width & !1;
        let height = height & !1;
        if width == 0 || height == 0 {
            return Err(RecordError::Frame("empty frame size"));
        }
        let fps = fps.max(1);
        unsafe {
            let iface = vpx::vpx_codec_vp8_cx();
            // libvpx fills both structs; some of their fields are enums
            // with no zero value, so they start uninitialized.
            let mut cfg_slot = std::mem::MaybeUninit::<vpx::vpx_codec_enc_cfg_t>::uninit();
            if vpx::vpx_codec_enc_config_default(iface, cfg_slot.as_mut_ptr(), 0)
                != vpx::vpx_codec_err_t::VPX_CODEC_OK
            {
                return Err(RecordError::Encode(
                    "vpx_codec_enc_config_default failed".into(),
                ));
            }
            let mut cfg = cfg_slot.assume_init();
            cfg.g_w = width as u32;
            cfg.g_h = height as u32;
            cfg.g_timebase.num = 1;
            cfg.g_timebase.den = fps as c_int;
            cfg.g_lag_in_frames = 0;
            cfg.g_threads = 4;
            cfg.rc_end_usage = vpx::vpx_rc_mode::VPX_VBR;
            // About 0.1 bit per pixel per frame: sharp text at screen sizes.
            cfg.rc_target_bitrate =
                ((width * height) as u64 * u64::from(fps) / 10_000).clamp(300, 8000) as u32;
            cfg.kf_max_dist = fps * 5;
            let mut slot = Box::<vpx::vpx_codec_ctx_t>::new_uninit();
            let err = vpx::vpx_codec_enc_init_ver(
                slot.as_mut_ptr(),
                iface,
                &raw const cfg,
                0,
                vpx::VPX_ENCODER_ABI_VERSION as c_int,
            );
            if err != vpx::vpx_codec_err_t::VPX_CODEC_OK {
                return Err(RecordError::Encode(format!("vpx_codec_enc_init: {err:?}")));
            }
            let mut ctx = slot.assume_init();
            // Fastest real-time setting: recording runs alongside the page.
            vpx::vpx_codec_control_(
                &raw mut *ctx,
                vpx::vp8e_enc_control_id::VP8E_SET_CPUUSED as c_int,
                8 as c_int,
            );
            let img = vpx::vpx_img_alloc(
                std::ptr::null_mut(),
                vpx::vpx_img_fmt::VPX_IMG_FMT_I420,
                width as u32,
                height as u32,
                1,
            );
            if img.is_null() {
                vpx::vpx_codec_destroy(&raw mut *ctx);
                return Err(RecordError::Encode("vpx_img_alloc failed".into()));
            }
            Ok(Self {
                ctx,
                img,
                width,
                height,
                fps,
                pts: 0,
                frames: Vec::new(),
                path: path.to_path_buf(),
            })
        }
    }

    /// Encode one RGB frame (`w*h*3` bytes, at least the recording size;
    /// larger sources are cropped).
    pub fn push_rgb(&mut self, w: usize, h: usize, rgb: &[u8]) -> Result<()> {
        if w < self.width || h < self.height {
            return Err(RecordError::SizeMismatch {
                got: (w, h),
                want: (self.width, self.height),
            });
        }
        if rgb.len() < w * h * 3 {
            return Err(RecordError::Frame("rgb buffer too small"));
        }
        unsafe {
            let img = &*self.img;
            let (ys, us, vs) = (
                img.stride[0] as usize,
                img.stride[1] as usize,
                img.stride[2] as usize,
            );
            let (yp, up, vp) = (img.planes[0], img.planes[1], img.planes[2]);
            for row in 0..self.height {
                for col in 0..self.width {
                    let i = (row * w + col) * 3;
                    let (r, g, b) = (
                        i32::from(rgb[i]),
                        i32::from(rgb[i + 1]),
                        i32::from(rgb[i + 2]),
                    );
                    // BT.601, limited range.
                    let y = ((66 * r + 129 * g + 25 * b + 128) >> 8) + 16;
                    *yp.add(row * ys + col) = y.clamp(0, 255) as u8;
                    if row % 2 == 0 && col % 2 == 0 {
                        let u = ((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128;
                        let v = ((112 * r - 94 * g - 18 * b + 128) >> 8) + 128;
                        *up.add((row / 2) * us + col / 2) = u.clamp(0, 255) as u8;
                        *vp.add((row / 2) * vs + col / 2) = v.clamp(0, 255) as u8;
                    }
                }
            }
        }
        self.encode(Some(self.img))?;
        self.pts += 1;
        Ok(())
    }

    fn encode(&mut self, img: Option<*mut vpx::vpx_image_t>) -> Result<()> {
        unsafe {
            let err = vpx::vpx_codec_encode(
                &raw mut *self.ctx,
                img.unwrap_or(std::ptr::null_mut()),
                self.pts,
                1,
                0,
                vpx::VPX_DL_REALTIME as std::ffi::c_ulong,
            );
            if err != vpx::vpx_codec_err_t::VPX_CODEC_OK {
                return Err(RecordError::Encode(format!("vpx_codec_encode: {err:?}")));
            }
            let mut iter: vpx::vpx_codec_iter_t = std::ptr::null();
            loop {
                let pkt = vpx::vpx_codec_get_cx_data(&raw mut *self.ctx, &raw mut iter);
                if pkt.is_null() {
                    break;
                }
                let pkt = &*pkt;
                if pkt.kind != vpx::vpx_codec_cx_pkt_kind::VPX_CODEC_CX_FRAME_PKT {
                    continue;
                }
                let f = pkt.data.frame;
                let data = std::slice::from_raw_parts(f.buf.cast::<u8>(), f.sz).to_vec();
                let key = f.flags & vpx::VPX_FRAME_IS_KEY != 0;
                let ms = (f.pts.max(0) as u64) * 1000 / u64::from(self.fps);
                self.frames.push((ms, key, data));
            }
        }
        Ok(())
    }

    /// Flush the encoder and write the WebM file. Errors if no frame was
    /// ever encoded.
    pub fn finish(mut self) -> Result<()> {
        self.encode(None)?;
        if self.frames.is_empty() {
            return Err(RecordError::Frame("no frames encoded"));
        }
        let duration = self.frames.last().map_or(0, |f| f.0) + 1000 / u64::from(self.fps);
        let bytes = mux(&self.frames, self.width, self.height, duration);
        std::fs::write(&self.path, bytes)?;
        Ok(())
    }
}

impl Drop for WebmRecorder {
    fn drop(&mut self) {
        unsafe {
            vpx::vpx_img_free(self.img);
            vpx::vpx_codec_destroy(&raw mut *self.ctx);
        }
    }
}

// --- EBML -------------------------------------------------------------

fn element(id: u32, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 12);
    let id_bytes = id.to_be_bytes();
    let skip = id_bytes.iter().position(|b| *b != 0).unwrap_or(3);
    out.extend_from_slice(&id_bytes[skip..]);
    // 8-byte size: 0x01 marker, then 56 bits.
    let size = body.len() as u64 | (1u64 << 56);
    out.extend_from_slice(&size.to_be_bytes());
    out.extend_from_slice(body);
    out
}

fn uint(id: u32, v: u64) -> Vec<u8> {
    let b = v.to_be_bytes();
    let skip = b.iter().position(|x| *x != 0).unwrap_or(7);
    element(id, &b[skip..])
}

fn float(id: u32, v: f64) -> Vec<u8> {
    element(id, &v.to_be_bytes())
}

fn string(id: u32, s: &str) -> Vec<u8> {
    element(id, s.as_bytes())
}

/// A complete WebM file for `frames` (ms, keyframe, VP8 data).
fn mux(frames: &[(u64, bool, Vec<u8>)], width: usize, height: usize, duration_ms: u64) -> Vec<u8> {
    let header = element(
        0x1A45_DFA3,
        &[
            uint(0x4286, 1),
            uint(0x42F7, 1),
            uint(0x42F2, 4),
            uint(0x42F3, 8),
            string(0x4282, "webm"),
            uint(0x4287, 2),
            uint(0x4285, 2),
        ]
        .concat(),
    );
    let info = element(
        0x1549_A966,
        &[
            uint(0x2A_D7B1, 1_000_000),
            string(0x4D80, "vibesurfer"),
            string(0x5741, "vibesurfer"),
            float(0x4489, duration_ms as f64),
        ]
        .concat(),
    );
    let video = element(
        0xE0,
        &[uint(0xB0, width as u64), uint(0xBA, height as u64)].concat(),
    );
    let tracks = element(
        0x1654_AE6B,
        &element(
            0xAE,
            &[
                uint(0xD7, 1),
                uint(0x73C5, 1),
                uint(0x83, 1),
                string(0x86, "V_VP8"),
                video,
            ]
            .concat(),
        ),
    );
    // Clusters, remembering where each starts inside the segment body.
    let mut body = [info, tracks].concat();
    let mut cues = Vec::new();
    let mut i = 0;
    while i < frames.len() {
        let start = frames[i].0;
        let mut blocks = uint(0xE7, start);
        let mut j = i;
        while j < frames.len() && (j == i || (!frames[j].1 && frames[j].0 - start < 30_000)) {
            let (t, key, data) = &frames[j];
            let mut b = vec![0x81];
            b.extend_from_slice(&((*t - start) as i16).to_be_bytes());
            b.push(if *key { 0x80 } else { 0x00 });
            b.extend_from_slice(data);
            blocks.extend(element(0xA3, &b));
            j += 1;
        }
        cues.push(element(
            0xBB,
            &[
                uint(0xB3, start),
                element(
                    0xB7,
                    &[uint(0xF7, 1), uint(0xF1, body.len() as u64)].concat(),
                ),
            ]
            .concat(),
        ));
        body.extend(element(0x1F43_B675, &blocks));
        i = j;
    }
    body.extend(element(0x1C53_BB6B, &cues.concat()));
    [header, element(0x1853_8067, &body)].concat()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_a_playable_webm() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.webm");
        let mut r = WebmRecorder::create(&path, 64, 48, 10).unwrap();
        for k in 0..12u8 {
            let rgb: Vec<u8> = (0..64 * 48)
                .flat_map(|p| [k.wrapping_mul(20), (p % 255) as u8, 128])
                .collect();
            r.push_rgb(64, 48, &rgb).unwrap();
        }
        r.finish().unwrap();
        let b = std::fs::read(&path).unwrap();
        assert_eq!(&b[..4], &[0x1A, 0x45, 0xDF, 0xA3]);
        assert!(b.windows(5).any(|w| w == b"V_VP8"));
        assert!(b.len() > 200);
        if let Ok(keep) = std::env::var("VS_KEEP_WEBM") {
            std::fs::copy(&path, keep).unwrap();
        }
    }
}
