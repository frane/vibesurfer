//! Session video recording for vibesurfer.
//!
//! Turns a sequence of RGB frames (captured from a page) into a real
//! video. Two encoders, both cross-platform:
//!
//! - [`H264Recorder`]: real-time H.264 via openh264 (built from vendored
//!   source), muxed to MP4 with muxide (pure Rust). Fast enough to keep
//!   up with capture and produces a file that plays everywhere with no
//!   remux. This is the default.
//! - [`Recorder`]: pure-Rust AV1 via rav1e in an IVF container. No build
//!   tools, but far too slow for real time at screen resolution. Kept as
//!   a portable fallback.
//!
//! Both are fed decoded RGB frames (see [`Recorder::push_png`] for the
//! capture path, which decodes vibesurfer's PNG captures). All frames in
//! one recording must share dimensions; the first fixes them.

// This crate is pixel and signal math: cursor rasterization, RGB<->YUV,
// bitrate/dimension arithmetic. The numeric casts are deliberate and
// range-checked by construction, so the pedantic cast lints are noise
// here.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::cast_lossless,
    clippy::too_many_arguments,
    clippy::many_single_char_names,
    clippy::map_unwrap_or
)]

use std::io::{Seek as _, Write};
use std::path::Path;

use rav1e::config::SpeedSettings;
use rav1e::prelude::*;

#[derive(Debug, thiserror::Error)]
pub enum RecordError {
    #[error("rav1e: {0}")]
    Encode(String),
    #[error("frame size {got:?} does not match recording size {want:?}")]
    SizeMismatch {
        got: (usize, usize),
        want: (usize, usize),
    },
    #[error("bad frame: {0}")]
    Frame(&'static str),
    #[error("png decode: {0}")]
    Png(#[from] image::ImageError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

type Result<T> = std::result::Result<T, RecordError>;

/// Decode just the dimensions of a PNG, so a caller can size a
/// [`Recorder`] from the first captured frame.
pub fn png_dimensions(png: &[u8]) -> Result<(usize, usize)> {
    let img = image::load_from_memory_with_format(png, image::ImageFormat::Png)?;
    Ok((img.width() as usize, img.height() as usize))
}

/// Decode a PNG and downscale it so its width is at most `max_width`
/// (aspect preserved), returning `(width, height, rgb)`. Screen captures
/// arrive at the device backing scale (retina 2x), which is needlessly
/// large for a recording: it slows capture and multiplies the encoder's
/// per-frame memory. Downscaling to a sane width keeps recordings small
/// and light. A frame already within `max_width` is returned unscaled.
pub fn png_to_scaled_rgb(png: &[u8], max_width: u32) -> Result<(usize, usize, Vec<u8>)> {
    let img = image::load_from_memory_with_format(png, image::ImageFormat::Png)?;
    let img = if img.width() > max_width && max_width > 0 {
        let h = u64::from(max_width) * u64::from(img.height()) / u64::from(img.width());
        let h = u32::try_from(h).unwrap_or(u32::MAX).max(1);
        img.resize_exact(max_width, h, image::imageops::FilterType::Triangle)
    } else {
        img
    };
    let rgb = img.to_rgb8();
    Ok((rgb.width() as usize, rgb.height() as usize, rgb.into_raw()))
}

/// macOS-style arrow pointer as a polygon, tip at the origin, in a
/// nominal ~16 px tall unit space (scaled per frame). A headless snapshot
/// never contains the OS pointer, so the recorder draws it from the
/// position the engine reported for each frame — anti-aliased with a fill,
/// a dark border, and a soft drop shadow so it reads like a real cursor
/// rather than a blocky sprite.
const ARROW: [(f64, f64); 7] = [
    (0.0, 0.0),
    (0.0, 15.6),
    (3.6, 12.1),
    (5.7, 17.4),
    (7.7, 16.6),
    (5.6, 11.2),
    (10.2, 11.0),
];
/// Nominal arrow height in unit space (for border-width scaling).
const ARROW_H: f64 = 15.6;

/// Blend `(r,g,b)` at `alpha` (0..1) into the RGB frame at `(px,py)`.
fn blend_px(rgb: &mut [u8], w: usize, h: usize, px: i64, py: i64, r: u8, g: u8, b: u8, alpha: f64) {
    if px < 0 || py < 0 || px as usize >= w || py as usize >= h || alpha <= 0.0 {
        return;
    }
    let a = alpha.clamp(0.0, 1.0);
    let i = (py as usize * w + px as usize) * 3;
    rgb[i] = (f64::from(r) * a + f64::from(rgb[i]) * (1.0 - a)) as u8;
    rgb[i + 1] = (f64::from(g) * a + f64::from(rgb[i + 1]) * (1.0 - a)) as u8;
    rgb[i + 2] = (f64::from(b) * a + f64::from(rgb[i + 2]) * (1.0 - a)) as u8;
}

/// Even-odd point-in-polygon test.
fn in_poly(px: f64, py: f64, pts: &[(f64, f64)]) -> bool {
    let mut inside = false;
    let n = pts.len();
    let mut j = n - 1;
    for i in 0..n {
        let (xi, yi) = pts[i];
        let (xj, yj) = pts[j];
        if (yi > py) != (yj > py) && px < (xj - xi) * (py - yi) / (yj - yi) + xi {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// Coverage of pixel `(px,py)` by `pts`, via a 4x4 sub-sample grid, for
/// anti-aliased edges.
fn coverage(px: i64, py: i64, pts: &[(f64, f64)]) -> f64 {
    let mut hit = 0u32;
    for sy in 0..4 {
        for sx in 0..4 {
            let x = px as f64 + (sx as f64 + 0.5) / 4.0;
            let y = py as f64 + (sy as f64 + 0.5) / 4.0;
            if in_poly(x, y, pts) {
                hit += 1;
            }
        }
    }
    f64::from(hit) / 16.0
}

/// Draw an expanding click ripple centred at `(cx,cy)`. `phase` runs
/// `1.0` (just pressed, tight bright ring) down to `0.0` (gone).
fn draw_click_ring(rgb: &mut [u8], w: usize, h: usize, cx: f64, cy: f64, phase: f32, scale: f64) {
    let phase = f64::from(phase);
    let base = 13.0 * scale;
    let radius = base * (1.7 - phase);
    let thickness = 2.2 * scale;
    let alpha = phase * 0.6;
    let lo = radius - thickness;
    let hi = radius + thickness;
    let x0 = (cx - hi).floor() as i64;
    let x1 = (cx + hi).ceil() as i64;
    let y0 = (cy - hi).floor() as i64;
    let y1 = (cy + hi).ceil() as i64;
    for py in y0..=y1 {
        for px in x0..=x1 {
            let dx = px as f64 + 0.5 - cx;
            let dy = py as f64 + 0.5 - cy;
            let d = (dx * dx + dy * dy).sqrt();
            if d >= lo && d <= hi {
                let edge = 1.0 - ((d - radius).abs() / thickness);
                blend_px(rgb, w, h, px, py, 66, 133, 244, alpha * edge.max(0.0));
            }
        }
    }
}

/// Anti-aliased arrow pointer with tip at `(cx,cy)`, scaled by `scale`.
/// Draws a soft shadow, a dark border, then the white fill.
fn draw_arrow(rgb: &mut [u8], w: usize, h: usize, cx: f64, cy: f64, scale: f64) {
    // Fill polygon in frame space.
    let fill: Vec<(f64, f64)> = ARROW
        .iter()
        .map(|&(x, y)| (cx + x * scale, cy + y * scale))
        .collect();
    // Centroid, to grow a slightly larger border polygon around the fill.
    let (mut mx, mut my) = (0.0, 0.0);
    for &(x, y) in &fill {
        mx += x;
        my += y;
    }
    mx /= fill.len() as f64;
    my /= fill.len() as f64;
    let grow = 1.05 / (ARROW_H * scale); // ~1 px of border
    let border: Vec<(f64, f64)> = fill
        .iter()
        .map(|&(x, y)| (x + (x - mx) * grow, y + (y - my) * grow))
        .collect();
    let shadow: Vec<(f64, f64)> = border.iter().map(|&(x, y)| (x + 0.9, y + 1.2)).collect();

    let pad = 3.0 * scale + 3.0;
    let x0 = (cx - pad).floor() as i64;
    let x1 = (cx + 13.0 * scale + pad).ceil() as i64;
    let y0 = (cy - pad).floor() as i64;
    let y1 = (cy + 20.0 * scale + pad).ceil() as i64;
    for py in y0..=y1 {
        for px in x0..=x1 {
            let cs = coverage(px, py, &shadow);
            if cs > 0.0 {
                blend_px(rgb, w, h, px, py, 0, 0, 0, cs * 0.22);
            }
            let cb = coverage(px, py, &border);
            if cb > 0.0 {
                blend_px(rgb, w, h, px, py, 30, 30, 32, cb);
            }
            let cf = coverage(px, py, &fill);
            if cf > 0.0 {
                blend_px(rgb, w, h, px, py, 252, 252, 252, cf);
            }
        }
    }
}

/// Composite the cursor (and any click ripple) onto an RGB frame at
/// `(x,y)` frame pixels. The pointer scale tracks the frame width so it
/// looks the same size across recording resolutions.
pub fn composite_cursor(rgb: &mut [u8], w: usize, h: usize, x: f64, y: f64, click: f32) {
    // ~20 px tall at 1440 wide, matching a real macOS pointer.
    let scale = (w as f64 / 1440.0 * 1.15).max(1.0);
    if click > 0.0 {
        draw_click_ring(rgb, w, h, x, y, click, scale);
    }
    draw_arrow(rgb, w, h, x, y, scale);
}

/// Where encoded frames go. A file-backed recorder streams each frame
/// straight to disk so memory stays at ~one frame regardless of how long
/// the recording runs; the in-memory sink is only for tests.
enum Sink {
    File {
        writer: std::io::BufWriter<std::fs::File>,
        frames: u32,
    },
    Mem(Vec<Vec<u8>>),
}

/// The 32-byte IVF file header. `frame_count` is patched in at finish
/// for the streaming path (written as 0 up front, seeked back later).
fn ivf_header(width: usize, height: usize, fps: u32, frame_count: u32) -> [u8; 32] {
    let mut h = [0u8; 32];
    h[0..4].copy_from_slice(b"DKIF");
    h[6..8].copy_from_slice(&32u16.to_le_bytes()); // header length (version 0)
    h[8..12].copy_from_slice(b"AV01");
    h[12..14].copy_from_slice(&u16::try_from(width).unwrap_or(0).to_le_bytes());
    h[14..16].copy_from_slice(&u16::try_from(height).unwrap_or(0).to_le_bytes());
    h[16..20].copy_from_slice(&fps.max(1).to_le_bytes()); // timebase den
    h[20..24].copy_from_slice(&1u32.to_le_bytes()); // timebase num
    h[24..28].copy_from_slice(&frame_count.to_le_bytes());
    h
}

/// One IVF frame record: 12-byte header (size u32, timestamp u64) + data.
fn write_frame_record(out: &mut impl Write, pkt: &[u8], ts: u64) -> std::io::Result<()> {
    out.write_all(&u32::try_from(pkt.len()).unwrap_or(0).to_le_bytes())?;
    out.write_all(&ts.to_le_bytes())?;
    out.write_all(pkt)
}

/// Encodes RGB frames to AV1 and writes an IVF file.
pub struct Recorder {
    width: usize,
    height: usize,
    fps: u32,
    ctx: Context<u8>,
    sink: Sink,
}

impl Recorder {
    fn build_ctx(
        width: usize,
        height: usize,
        fps: u32,
        speed: u8,
    ) -> Result<(usize, usize, Context<u8>)> {
        let width = width & !1;
        let height = height & !1;
        if width == 0 || height == 0 {
            return Err(RecordError::Frame("zero-sized recording"));
        }
        // Low-latency, no lookahead: encode and emit each frame as it
        // arrives. Without this, rav1e buffers its whole reorder /
        // lookahead window of *uncompressed* frames — tens of MB each at
        // screen resolution — which grew unbounded during a live
        // recording (~90 MB/s). Screen capture doesn't benefit from the
        // lookahead anyway.
        let mut speed_settings = SpeedSettings::from_preset(speed.min(10));
        speed_settings.rdo_lookahead_frames = 1;
        // Tile the frame so rav1e can encode it across threads. A screen
        // recording at 1440p is far too heavy for the single-threaded
        // encoder to keep up with real-time capture (~1 s/frame); tiling
        // into a 2x2 grid and giving it real threads brings that down by
        // roughly the tile count. Tiles cost a little quality and memory,
        // both of which a headless screen recording can spare.
        let enc = EncoderConfig {
            width,
            height,
            time_base: Rational::new(1, u64::from(fps.max(1))),
            chroma_sampling: ChromaSampling::Cs420,
            low_latency: true,
            tile_cols: 2,
            tile_rows: 2,
            // Lower base quantizer = higher quality. A screen recording of
            // text needs sharp edges; the default (100) smears them. The
            // encode is offline (after `stop`), so we can afford it.
            quantizer: 70,
            speed_settings,
            ..Default::default()
        };
        let threads = std::thread::available_parallelism()
            .map(|n| n.get().saturating_sub(1).clamp(2, 8))
            .unwrap_or(4);
        let cfg = Config::new().with_encoder_config(enc).with_threads(threads);
        let ctx: Context<u8> = cfg
            .new_context()
            .map_err(|e| RecordError::Encode(format!("{e:?}")))?;
        Ok((width, height, ctx))
    }

    /// Start an in-memory recording at `width` x `height` (both rounded
    /// down to even, required by 4:2:0 chroma) and `fps`. `speed` is
    /// 0..=10; higher is faster and lower quality. For tests — real
    /// recordings should use [`Recorder::create`], which streams to disk.
    pub fn new(width: usize, height: usize, fps: u32, speed: u8) -> Result<Self> {
        let (width, height, ctx) = Self::build_ctx(width, height, fps, speed)?;
        Ok(Self {
            width,
            height,
            fps,
            ctx,
            sink: Sink::Mem(Vec::new()),
        })
    }

    /// Start a recording that streams straight to the IVF file at `path`.
    /// Each encoded frame is written and dropped, so memory stays at
    /// about one frame no matter how long the recording runs. Finalize
    /// with [`Recorder::finish`].
    pub fn create(
        path: &std::path::Path,
        width: usize,
        height: usize,
        fps: u32,
        speed: u8,
    ) -> Result<Self> {
        let (width, height, ctx) = Self::build_ctx(width, height, fps, speed)?;
        let mut f = std::fs::File::create(path)?;
        f.write_all(&ivf_header(width, height, fps, 0))?;
        Ok(Self {
            width,
            height,
            fps,
            ctx,
            sink: Sink::File {
                writer: std::io::BufWriter::new(f),
                frames: 0,
            },
        })
    }

    /// Decode a vibesurfer PNG capture and push it as a frame.
    pub fn push_png(&mut self, png: &[u8]) -> Result<()> {
        let img = image::load_from_memory_with_format(png, image::ImageFormat::Png)?;
        let rgb = img.to_rgb8();
        self.push_rgb(rgb.width() as usize, rgb.height() as usize, &rgb)
    }

    /// Push one RGB frame (`w*h*3` bytes, row-major, no padding).
    pub fn push_rgb(&mut self, w: usize, h: usize, rgb: &[u8]) -> Result<()> {
        // Crop to the recording size if the source is a couple pixels
        // larger from the even-rounding; reject a real mismatch.
        if w < self.width || h < self.height {
            return Err(RecordError::SizeMismatch {
                got: (w, h),
                want: (self.width, self.height),
            });
        }
        if rgb.len() < w * h * 3 {
            return Err(RecordError::Frame("rgb buffer too small"));
        }
        let mut frame = self.ctx.new_frame();
        let (yp, up, vp) = rgb_to_yuv420(rgb, w, self.width, self.height);
        frame.planes[0].copy_from_raw_u8(&yp, self.width, 1);
        frame.planes[1].copy_from_raw_u8(&up, self.width / 2, 1);
        frame.planes[2].copy_from_raw_u8(&vp, self.width / 2, 1);

        self.ctx
            .send_frame(frame)
            .map_err(|e| RecordError::Encode(format!("send_frame: {e:?}")))?;
        self.drain()
    }

    /// Pull whatever packets are ready and route them to the sink,
    /// stopping when the encoder wants more input or reports it is
    /// drained. Safe to call after each send and after flush.
    fn drain(&mut self) -> Result<()> {
        loop {
            match self.ctx.receive_packet() {
                Ok(pkt) => match &mut self.sink {
                    Sink::File { writer, frames } => {
                        write_frame_record(writer, &pkt.data, u64::from(*frames))?;
                        *frames += 1;
                    }
                    Sink::Mem(packets) => packets.push(pkt.data),
                },
                Err(EncoderStatus::Encoded) => {}
                Err(EncoderStatus::NeedMoreData | EncoderStatus::LimitReached) => return Ok(()),
                Err(e) => return Err(RecordError::Encode(format!("receive_packet: {e:?}"))),
            }
        }
    }

    /// Flush the encoder and finalize the streamed IVF file: patch the
    /// real frame count into the header. Use with [`Recorder::create`].
    pub fn finish(mut self) -> Result<()> {
        self.ctx.flush();
        self.drain()?;
        match self.sink {
            Sink::File { writer, frames } => {
                let mut f = writer
                    .into_inner()
                    .map_err(std::io::IntoInnerError::into_error)?;
                f.seek(std::io::SeekFrom::Start(24))?;
                f.write_all(&frames.to_le_bytes())?;
                f.flush()?;
                Ok(())
            }
            Sink::Mem(_) => Err(RecordError::Frame(
                "finish() on an in-memory recorder; use finish_to_vec",
            )),
        }
    }

    /// Flush and return the IVF bytes (in-memory recorders / tests).
    pub fn finish_to_vec(mut self) -> Result<Vec<u8>> {
        self.ctx.flush();
        self.drain()?;
        match self.sink {
            Sink::Mem(packets) => {
                let mut out = Vec::new();
                out.extend_from_slice(&ivf_header(
                    self.width,
                    self.height,
                    self.fps,
                    u32::try_from(packets.len()).unwrap_or(0),
                ));
                for (i, pkt) in packets.iter().enumerate() {
                    write_frame_record(&mut out, pkt, i as u64)?;
                }
                Ok(out)
            }
            Sink::File { .. } => Err(RecordError::Frame(
                "finish_to_vec() on a file recorder; use finish",
            )),
        }
    }
}

/// Convert row-major RGB (`src_stride` pixels wide) to planar YUV420,
/// cropping to `w` x `h`. BT.601 limited range. Returns (Y, U, V).
#[allow(clippy::many_single_char_names)]
// The clamp guarantees the 0..=255 range, so the u8 cast cannot
// truncate or lose sign; clippy cannot see the guarantee.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn rgb_to_yuv420(rgb: &[u8], src_w: usize, w: usize, h: usize) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let mut y = vec![0u8; w * h];
    let mut u = vec![0u8; (w / 2) * (h / 2)];
    let mut v = vec![0u8; (w / 2) * (h / 2)];
    let px = |x: usize, row: usize| -> (f32, f32, f32) {
        let idx = (row * src_w + x) * 3;
        (
            f32::from(rgb[idx]),
            f32::from(rgb[idx + 1]),
            f32::from(rgb[idx + 2]),
        )
    };
    let clamp = |val: f32| -> u8 { val.round().clamp(0.0, 255.0) as u8 };
    for row in 0..h {
        for x in 0..w {
            let (r, g, b) = px(x, row);
            y[row * w + x] = clamp(0.257 * r + 0.504 * g + 0.098 * b + 16.0);
        }
    }
    for cy in 0..h / 2 {
        for cx in 0..w / 2 {
            // Average the 2x2 block for chroma.
            let mut rs = 0.0;
            let mut gs = 0.0;
            let mut bs = 0.0;
            for dy in 0..2 {
                for dx in 0..2 {
                    let (r, g, b) = px(cx * 2 + dx, cy * 2 + dy);
                    rs += r;
                    gs += g;
                    bs += b;
                }
            }
            let (r, g, b) = (rs / 4.0, gs / 4.0, bs / 4.0);
            u[cy * (w / 2) + cx] = clamp(-0.148 * r - 0.291 * g + 0.439 * b + 128.0);
            v[cy * (w / 2) + cx] = clamp(0.439 * r - 0.368 * g - 0.071 * b + 128.0);
        }
    }
    (y, u, v)
}

/// Real-time H.264 recorder: encodes RGB frames with openh264 and muxes
/// them into an MP4 with muxide. Unlike [`Recorder`] (rav1e/AV1, far too
/// slow for real time), this keeps up with capture and writes a file
/// that plays natively everywhere without a remux. The MP4 is finalized
/// on [`finish`](Self::finish); the muxer is built lazily on the first
/// frame because it needs the SPS/PPS that the encoder emits with it.
pub struct H264Recorder {
    enc: openh264::encoder::Encoder,
    width: usize,
    height: usize,
    /// Milliseconds each frame is shown (1000 / fps); the muxer PTS
    /// advances by this per frame.
    frame_ms: u32,
    fps: f64,
    path: std::path::PathBuf,
    muxer: Option<muxide::api::Muxer<std::io::BufWriter<std::fs::File>>>,
}

impl H264Recorder {
    /// Create a recorder for `width` x `height` at `fps`, writing an MP4
    /// to `path` when finished. Dimensions are rounded down to even
    /// (required by 4:2:0 chroma).
    pub fn create(path: &Path, width: usize, height: usize, fps: u32) -> Result<Self> {
        use openh264::encoder::{BitRate, EncoderConfig, FrameRate, UsageType};
        let width = width & !1;
        let height = height & !1;
        if width == 0 || height == 0 {
            return Err(RecordError::Frame("zero-sized recording"));
        }
        let fps = fps.max(1);
        // Generous bitrate ceiling for sharp text; screen content is
        // mostly static so the encoder spends far less on average.
        #[allow(clippy::cast_possible_truncation)]
        let bitrate = ((width as u64 * height as u64 * u64::from(fps)) / 5)
            .clamp(1_000_000, 12_000_000) as u32;
        let config = EncoderConfig::new()
            // Screen-content mode keeps UI text and edges crisp.
            .usage_type(UsageType::ScreenContentRealTime)
            .max_frame_rate(FrameRate::from_hz(fps as f32))
            .bitrate(BitRate::from_bps(bitrate));
        let enc = openh264::encoder::Encoder::with_api_config(
            openh264::OpenH264API::from_source(),
            config,
        )
        .map_err(|e| RecordError::Encode(format!("openh264 init: {e:?}")))?;
        Ok(Self {
            enc,
            width,
            height,
            frame_ms: 1000 / fps,
            fps: f64::from(fps),
            path: path.to_path_buf(),
            muxer: None,
        })
    }

    /// Encode and mux one RGB frame (`w*h*3` bytes). A source a couple
    /// pixels larger than the recording size (even-rounding) is cropped;
    /// a smaller one is rejected.
    pub fn push_rgb(&mut self, w: usize, h: usize, rgb: &[u8]) -> Result<()> {
        use openh264::formats::{RgbSliceU8, YUVBuffer};
        if w < self.width || h < self.height {
            return Err(RecordError::SizeMismatch {
                got: (w, h),
                want: (self.width, self.height),
            });
        }
        if rgb.len() < w * h * 3 {
            return Err(RecordError::Frame("rgb buffer too small"));
        }
        // Crop to the exact (even) encoder size if the source is larger.
        let cropped;
        let src: &[u8] = if w == self.width && h == self.height {
            rgb
        } else {
            let mut out = Vec::with_capacity(self.width * self.height * 3);
            for row in 0..self.height {
                let start = row * w * 3;
                out.extend_from_slice(&rgb[start..start + self.width * 3]);
            }
            cropped = out;
            &cropped
        };
        let yuv = YUVBuffer::from_rgb8_source(RgbSliceU8::new(src, (self.width, self.height)));
        let bitstream = self
            .enc
            .encode(&yuv)
            .map_err(|e| RecordError::Encode(format!("openh264 encode: {e:?}")))?;
        let annexb = bitstream.to_vec();
        // A skipped frame (rate control) yields no bytes; drop it.
        if annexb.is_empty() {
            return Ok(());
        }
        if self.muxer.is_none() {
            let cfg = muxide::codec::h264::extract_avc_config(&annexb)
                .ok_or_else(|| RecordError::Encode("no SPS/PPS in first frame".into()))?;
            let file = std::io::BufWriter::new(std::fs::File::create(&self.path)?);
            let muxer = muxide::api::MuxerBuilder::new(file)
                .video(
                    muxide::api::VideoCodec::H264,
                    self.width as u32,
                    self.height as u32,
                    self.fps,
                )
                .with_sps(cfg.sps)
                .with_pps(cfg.pps)
                // moov at the front so the file streams / plays while
                // downloading (browsers need this for <video>).
                .with_fast_start(true)
                .build()
                .map_err(|e| RecordError::Encode(format!("mp4 mux init: {e}")))?;
            self.muxer = Some(muxer);
        }
        // muxide takes Annex-B and converts to AVCC internally (it also
        // detects the IDR keyframe by scanning start codes), so pass the
        // raw bitstream, not a pre-converted one.
        self.muxer
            .as_mut()
            .expect("muxer set above")
            .encode_video(&annexb, self.frame_ms)
            .map_err(|e| RecordError::Encode(format!("mp4 write: {e}")))?;
        Ok(())
    }

    /// Flush and finalize the MP4. Errors if no frame was ever pushed.
    pub fn finish(mut self) -> Result<()> {
        match self.muxer.take() {
            Some(mut m) => m
                .finish_in_place()
                .map_err(|e| RecordError::Encode(format!("mp4 finish: {e}"))),
            None => Err(RecordError::Frame("no frames encoded")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid_rgb(w: usize, h: usize, c: [u8; 3]) -> Vec<u8> {
        let mut v = Vec::with_capacity(w * h * 3);
        for _ in 0..w * h {
            v.extend_from_slice(&c);
        }
        v
    }

    #[test]
    fn encodes_frames_to_a_valid_ivf() {
        let (w, h) = (64, 48);
        let mut rec = Recorder::new(w, h, 5, 9).expect("recorder");
        // A few frames of changing color, so there is real motion.
        for c in [[200, 30, 30], [30, 200, 30], [30, 30, 200], [220, 220, 30]] {
            rec.push_rgb(w, h, &solid_rgb(w, h, c)).expect("push");
        }
        let ivf = rec.finish_to_vec().expect("finish");
        assert_eq!(&ivf[0..4], b"DKIF", "IVF signature");
        assert_eq!(&ivf[8..12], b"AV01", "codec fourcc");
        let frame_count = u32::from_le_bytes(ivf[24..28].try_into().unwrap());
        assert!(
            frame_count >= 1,
            "at least one encoded frame, got {frame_count}"
        );
        // Header plus at least one framed packet.
        assert!(
            ivf.len() > 32 + 12,
            "encoded body present, got {} bytes",
            ivf.len()
        );
    }

    #[test]
    fn odd_dimensions_are_rounded_even() {
        let rec = Recorder::new(65, 49, 5, 9).expect("recorder");
        assert_eq!((rec.width, rec.height), (64, 48));
    }
}
