//! MCP result-content shaping: turn wire text + optional screenshot
//! bytes into MCP content blocks.
//!
//! Two jobs, both about tokens:
//! 1. A `base64=<png>` body line (from `vs_capture --base64`) used to
//!    be shipped as *text* content — hundreds of KB of base64 fed to
//!    the model as text. It becomes a proper image block (hosts render
//!    it; the model sees vision tokens, orders of magnitude cheaper).
//! 2. Action thumbnails (`capture: true` on vs_act / vs_open, or
//!    `VS_THUMBS=1`) are downscaled to [`THUMB_WIDTH`] JPEG before
//!    the image block, so the per-action cost stays ~100 vision
//!    tokens instead of a full-resolution screenshot.

use anyhow::{Context as _, Result};
use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use serde_json::{json, Value};

/// Thumbnail target width in pixels. Height follows the aspect ratio.
const THUMB_WIDTH: u32 = 400;
/// JPEG quality for thumbnails.
const THUMB_QUALITY: u8 = 60;

/// Build the `content` array for a tool result. `thumb_jpeg` is an
/// already-encoded JPEG to append as an image block.
pub fn shape(resp_text: &str, thumb_jpeg: Option<&[u8]>) -> Value {
    let mut blocks: Vec<Value> = Vec::new();
    let mut text_lines: Vec<&str> = Vec::new();
    let mut capture_png: Option<&str> = None;
    for line in resp_text.lines() {
        match line.strip_prefix("base64=") {
            Some(b64) => capture_png = Some(b64),
            None => text_lines.push(line),
        }
    }
    let text = text_lines.join("\n");
    if !text.is_empty() {
        blocks.push(json!({ "type": "text", "text": text }));
    }
    if let Some(b64) = capture_png {
        blocks.push(json!({
            "type": "image",
            "data": b64,
            "mimeType": "image/png",
        }));
    }
    if let Some(jpeg) = thumb_jpeg {
        blocks.push(json!({
            "type": "image",
            "data": STANDARD.encode(jpeg),
            "mimeType": "image/jpeg",
        }));
    }
    if blocks.is_empty() {
        blocks.push(json!({ "type": "text", "text": "" }));
    }
    Value::Array(blocks)
}

/// Downscale a PNG to a [`THUMB_WIDTH`]-wide JPEG thumbnail.
pub fn thumbnail_jpeg(png: &[u8]) -> Result<Vec<u8>> {
    scaled_jpeg(png, THUMB_WIDTH, THUMB_QUALITY)
}

/// Live-panel frame: wider and cleaner than an action thumbnail —
/// it is looked at by a human, not billed to a model.
pub fn frame_jpeg(png: &[u8]) -> Result<Vec<u8>> {
    scaled_jpeg(png, 800, 70)
}

fn scaled_jpeg(png: &[u8], width: u32, quality: u8) -> Result<Vec<u8>> {
    let img = image::load_from_memory_with_format(png, image::ImageFormat::Png)
        .context("decode capture png")?;
    let img = if img.width() > width {
        let h = (u64::from(img.height()) * u64::from(width) / u64::from(img.width())).max(1);
        img.resize(
            width,
            u32::try_from(h).unwrap_or(u32::MAX),
            image::imageops::FilterType::Triangle,
        )
    } else {
        img
    };
    let mut out = Vec::new();
    let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality);
    enc.encode_image(&img.into_rgb8()).context("encode jpeg")?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_line_becomes_image_block() {
        let v = shape("path=/tmp/x.png\nbase64=aGk=", None);
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["type"], "text");
        assert_eq!(arr[0]["text"], "path=/tmp/x.png");
        assert_eq!(arr[1]["type"], "image");
        assert_eq!(arr[1]["data"], "aGk=");
        assert_eq!(arr[1]["mimeType"], "image/png");
    }

    #[test]
    fn plain_text_untouched() {
        let v = shape("@0011223344556677\nok\n", None);
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["type"], "text");
    }

    #[test]
    fn thumbnail_downscales_wide_png() {
        // 800x200 solid PNG -> 400x100 JPEG.
        let img = image::DynamicImage::new_rgb8(800, 200);
        let mut png = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let jpeg = thumbnail_jpeg(&png).unwrap();
        let back = image::load_from_memory(&jpeg).unwrap();
        assert_eq!((back.width(), back.height()), (400, 100));
        let v = shape("ok", Some(&jpeg));
        let arr = v.as_array().unwrap();
        assert_eq!(arr[1]["mimeType"], "image/jpeg");
    }
}
