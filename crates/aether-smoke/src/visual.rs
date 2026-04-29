//! Visual assertions over decoded frame pixels. PNGs come back from
//! `TestBench::capture` as bytes; this module decodes once and runs
//! O(n) checks against the pixel buffer. Assertion functions take a
//! `&Image` so a single capture can drive many asserts without
//! re-decoding.

use thiserror::Error;

/// Decoded frame: RGBA8 pixels in row-major top-down order, width
/// and height in pixels. The chassis renders at the size requested
/// at boot (`TestBench::start_with_size`); decoded `width`/`height`
/// always match.
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

#[derive(Debug, Error)]
pub enum ImageError {
    #[error("PNG decode failed: {0}")]
    Decode(String),
    #[error("unsupported PNG color type: {0:?}")]
    UnsupportedColor(png::ColorType),
}

/// Decode a captured PNG byte stream into an `Image`. The chassis
/// always emits 8-bit RGBA, so non-RGBA decodes are flagged as
/// `UnsupportedColor` rather than silently coerced.
pub fn decode_png(bytes: &[u8]) -> Result<Image, ImageError> {
    let decoder = png::Decoder::new(bytes);
    let mut reader = decoder
        .read_info()
        .map_err(|e| ImageError::Decode(e.to_string()))?;
    let info = reader.info();
    let width = info.width;
    let height = info.height;
    let color = info.color_type;
    if color != png::ColorType::Rgba {
        return Err(ImageError::UnsupportedColor(color));
    }
    let mut buf = vec![0u8; reader.output_buffer_size()];
    reader
        .next_frame(&mut buf)
        .map_err(|e| ImageError::Decode(e.to_string()))?;
    Ok(Image {
        width,
        height,
        rgba: buf,
    })
}

/// Asserts at least one pixel has a non-zero RGB component. Alpha
/// is ignored — a fully-cleared depth-test frame can have alpha 1.0
/// everywhere yet still be visually black, and a transparent overlay
/// shouldn't count as "drew something". Returns a one-line failure
/// string suitable for `StepReport::Fail`.
pub fn not_all_black(image: &Image) -> Result<(), String> {
    for chunk in image.rgba.chunks_exact(4) {
        if chunk[0] != 0 || chunk[1] != 0 || chunk[2] != 0 {
            return Ok(());
        }
    }
    Err(format!(
        "all {}x{} pixels are black (RGB=0,0,0)",
        image.width, image.height
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthesize an Image from a fill color so asserts can run
    /// without going through the chassis. RGBA bytes laid out
    /// row-major, top-down.
    fn solid(width: u32, height: u32, rgba: [u8; 4]) -> Image {
        let mut buf = Vec::with_capacity((width * height * 4) as usize);
        for _ in 0..(width * height) {
            buf.extend_from_slice(&rgba);
        }
        Image {
            width,
            height,
            rgba: buf,
        }
    }

    #[test]
    fn not_all_black_passes_on_any_color() {
        let img = solid(4, 4, [0, 0, 1, 255]);
        assert!(not_all_black(&img).is_ok());
    }

    #[test]
    fn not_all_black_fails_on_pure_black() {
        let img = solid(4, 4, [0, 0, 0, 255]);
        let err = not_all_black(&img).unwrap_err();
        assert!(err.contains("4x4"));
    }

    #[test]
    fn not_all_black_ignores_alpha() {
        // Fully-transparent black is still "all black" — alpha doesn't
        // count as drawn pixels.
        let img = solid(2, 2, [0, 0, 0, 0]);
        assert!(not_all_black(&img).is_err());
    }

    #[test]
    fn not_all_black_passes_when_one_pixel_lit() {
        let mut img = solid(2, 2, [0, 0, 0, 255]);
        img.rgba[8] = 1; // R channel of pixel index 2
        assert!(not_all_black(&img).is_ok());
    }
}
