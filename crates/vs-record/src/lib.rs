//! Session video recording for vibesurfer.
//!
//! Turns a sequence of RGB frames (captured from a page) into a real
//! AV1 video, encoded with rav1e in pure Rust so nothing external is
//! required. Output is wrapped in an IVF container, the simplest
//! widely-supported wrapper for raw AV1, playable by ffmpeg, VLC, and
//! dav1d-based players. A later pass can add mp4 or webm muxing and an
//! optional ffmpeg path for smaller files when ffmpeg is present.
//!
//! The recorder is fed decoded RGB frames (see [`Recorder::push_png`]
//! for the capture path, which decodes vibesurfer's PNG captures). All
//! frames in one recording must share dimensions; the first frame
//! fixes them and later frames of a different size are rejected.

use std::io::Write as _;

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

/// Encodes RGB frames to AV1 and writes an IVF file.
pub struct Recorder {
    width: usize,
    height: usize,
    fps: u32,
    ctx: Context<u8>,
    /// Encoded AV1 frames (OBU payloads), in order.
    packets: Vec<Vec<u8>>,
}

impl Recorder {
    /// Start a recording at `width` x `height` (both rounded down to
    /// even, required by 4:2:0 chroma) and `fps`. `speed` is 0..=10;
    /// higher is faster and lower quality. 8 is a good default for
    /// screen capture where encode time matters more than size.
    pub fn new(width: usize, height: usize, fps: u32, speed: u8) -> Result<Self> {
        let width = width & !1;
        let height = height & !1;
        if width == 0 || height == 0 {
            return Err(RecordError::Frame("zero-sized recording"));
        }
        let enc = EncoderConfig {
            width,
            height,
            time_base: Rational::new(1, u64::from(fps.max(1))),
            chroma_sampling: ChromaSampling::Cs420,
            speed_settings: SpeedSettings::from_preset(speed.min(10)),
            ..Default::default()
        };
        let cfg = Config::new().with_encoder_config(enc).with_threads(1);
        let ctx: Context<u8> = cfg
            .new_context()
            .map_err(|e| RecordError::Encode(format!("{e:?}")))?;
        Ok(Self {
            width,
            height,
            fps,
            ctx,
            packets: Vec::new(),
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

    /// Pull whatever packets are ready and stop when the encoder
    /// wants more input or reports it is drained. Safe to call after
    /// each send and after flush.
    fn drain(&mut self) -> Result<()> {
        loop {
            match self.ctx.receive_packet() {
                Ok(pkt) => self.packets.push(pkt.data),
                Err(EncoderStatus::Encoded) => {}
                Err(EncoderStatus::NeedMoreData | EncoderStatus::LimitReached) => return Ok(()),
                Err(e) => return Err(RecordError::Encode(format!("receive_packet: {e:?}"))),
            }
        }
    }

    /// Flush the encoder and write the IVF file to `path`.
    pub fn finish_to_file(mut self, path: &std::path::Path) -> Result<usize> {
        self.ctx.flush();
        self.drain()?;
        let bytes = self.into_ivf();
        let mut f = std::fs::File::create(path)?;
        f.write_all(&bytes)?;
        Ok(bytes.len())
    }

    /// Flush and return the IVF bytes (used by tests).
    pub fn finish_to_vec(mut self) -> Result<Vec<u8>> {
        self.ctx.flush();
        self.drain()?;
        Ok(self.into_ivf())
    }

    fn into_ivf(self) -> Vec<u8> {
        let mut out = Vec::new();
        // IVF file header, 32 bytes.
        out.extend_from_slice(b"DKIF");
        out.extend_from_slice(&0u16.to_le_bytes()); // version
        out.extend_from_slice(&32u16.to_le_bytes()); // header length
        out.extend_from_slice(b"AV01"); // codec fourcc
        out.extend_from_slice(&u16::try_from(self.width).unwrap_or(0).to_le_bytes());
        out.extend_from_slice(&u16::try_from(self.height).unwrap_or(0).to_le_bytes());
        out.extend_from_slice(&self.fps.max(1).to_le_bytes()); // timebase den
        out.extend_from_slice(&1u32.to_le_bytes()); // timebase num
        out.extend_from_slice(&u32::try_from(self.packets.len()).unwrap_or(0).to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // unused
                                                    // Per-frame: 12-byte header (size u32, timestamp u64) + data.
        for (i, pkt) in self.packets.iter().enumerate() {
            out.extend_from_slice(&u32::try_from(pkt.len()).unwrap_or(0).to_le_bytes());
            out.extend_from_slice(&(i as u64).to_le_bytes());
            out.extend_from_slice(pkt);
        }
        out
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
