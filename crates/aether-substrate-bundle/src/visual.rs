//! Visual assertions over decoded frame pixels. PNGs come back from
//! `TestBench::capture` as bytes; this module decodes once and runs
//! O(n) checks against the pixel buffer. Assertion functions take a
//! `&Image` so a single capture can drive many asserts without
//! re-decoding.

use std::io::Cursor;

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
    // png 0.18 requires `BufRead + Seek` on the reader. Wrap the byte
    // slice in a `Cursor` to satisfy both bounds (the slice itself is
    // already `Read` but neither `BufRead` nor `Seek`).
    let decoder = png::Decoder::new(Cursor::new(bytes));
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
    // png 0.18 returns `Option<usize>` here (None on size overflow);
    // surface it as a decode error rather than panicking.
    let buf_size = reader
        .output_buffer_size()
        .ok_or_else(|| ImageError::Decode("output buffer size overflowed".to_string()))?;
    let mut buf = vec![0u8; buf_size];
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

/// A pixel is "lit" when at least one of its RGB channels diverges
/// from the reference background `bg` by more than `tol`. This is the
/// per-pixel predicate shared by `differs_from_background` and the
/// silhouette reductions (`coverage` / `centroid` / `bounding_box`):
/// they all partition the frame into the same lit/unlit mask, so the
/// "what counts as drawn" rule lives in exactly one place. `rgb` is
/// the leading three bytes of an RGBA chunk; alpha is ignored.
fn is_lit(rgb: &[u8], bg: [u8; 3], tol: u8) -> bool {
    rgb[0].abs_diff(bg[0]) > tol || rgb[1].abs_diff(bg[1]) > tol || rgb[2].abs_diff(bg[2]) > tol
}

/// Asserts at least one pixel differs from the top-left pixel by
/// more than `tolerance` per RGB channel. The top-left pixel is the
/// "background reference" — for chassis-rendered scenes it's almost
/// always the clear color (geometry sits in the middle), so a passing
/// check means "something was drawn on top of the clear pass." Alpha
/// is ignored. Returns a one-line failure string identifying the
/// reference color, suitable for `StepReport::Fail`.
pub fn differs_from_background(image: &Image, tolerance: u8) -> Result<(), String> {
    if image.rgba.len() < 4 {
        return Err(format!(
            "image too small to sample background: {}x{}",
            image.width, image.height
        ));
    }
    let bg = [image.rgba[0], image.rgba[1], image.rgba[2]];
    for chunk in image.rgba.chunks_exact(4) {
        if is_lit(chunk, bg, tolerance) {
            return Ok(());
        }
    }
    Err(format!(
        "all {}x{} pixels within tolerance ±{} of top-left ({},{},{})",
        image.width, image.height, tolerance, bg[0], bg[1], bg[2]
    ))
}

/// Axis-aligned pixel extent of a lit region, inclusive on both
/// corners: `min`/`max` are the smallest and largest lit column (`x`)
/// and row (`y`). A single lit pixel yields `min == max`. Returned by
/// `bounding_box`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub min_x: u32,
    pub min_y: u32,
    pub max_x: u32,
    pub max_y: u32,
}

/// Fraction of the frame that is lit relative to background `bg` at
/// per-channel tolerance `tol`, in `[0.0, 1.0]` (lit pixels divided by
/// `width * height`). Unlike `differs_from_background`, which only
/// answers "did *anything* draw," coverage constrains *how much* of
/// the frame the geometry occupies — a tight band rules out both an
/// all-background miss and an all-filled clear-color mismatch. The
/// background is passed explicitly so a caller that knows the boot
/// clear color can pin it rather than inferring from the top-left
/// pixel; pass `background_top_left(image)` to keep that convention.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn coverage(image: &Image, bg: [u8; 3], tol: u8) -> f32 {
    let total = image.width as usize * image.height as usize;
    if total == 0 {
        return 0.0;
    }
    let lit = image
        .rgba
        .chunks_exact(4)
        .filter(|chunk| is_lit(chunk, bg, tol))
        .count();
    lit as f32 / total as f32
}

/// Mean `(x, y)` pixel coordinate of the lit region relative to
/// background `bg` at tolerance `tol`, where `x` is the column and `y`
/// the row (top-down). This pins *where* the geometry landed — a
/// centroid near the frame center says the blob sits in the interior,
/// not hugging an edge. Returns `None` when no pixel is lit (an empty
/// mask has no centroid). The `bg`/`tol` convention matches
/// `coverage`.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn centroid(image: &Image, bg: [u8; 3], tol: u8) -> Option<(f32, f32)> {
    let width = image.width as usize;
    let mut sum_x = 0u64;
    let mut sum_y = 0u64;
    let mut lit = 0u64;
    for (index, chunk) in image.rgba.chunks_exact(4).enumerate() {
        if is_lit(chunk, bg, tol) {
            sum_x += (index % width) as u64;
            sum_y += (index / width) as u64;
            lit += 1;
        }
    }
    if lit == 0 {
        return None;
    }
    Some((sum_x as f32 / lit as f32, sum_y as f32 / lit as f32))
}

/// Inclusive axis-aligned bounding box of the lit region relative to
/// background `bg` at tolerance `tol`. Together with `coverage` this
/// distinguishes "a large blob centered here" from "a thin streak
/// along one edge" that share a coverage fraction. Returns `None` when
/// no pixel is lit (an empty mask has no extent). The `bg`/`tol`
/// convention matches `coverage`.
#[must_use]
#[allow(clippy::cast_possible_truncation)]
pub fn bounding_box(image: &Image, bg: [u8; 3], tol: u8) -> Option<Rect> {
    let width = image.width as usize;
    let mut min_x = u32::MAX;
    let mut min_y = u32::MAX;
    let mut max_x = 0u32;
    let mut max_y = 0u32;
    let mut any_lit = false;
    for (index, chunk) in image.rgba.chunks_exact(4).enumerate() {
        if !is_lit(chunk, bg, tol) {
            continue;
        }
        any_lit = true;
        let x = (index % width) as u32;
        let y = (index / width) as u32;
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x);
        max_y = max_y.max(y);
    }
    any_lit.then_some(Rect {
        min_x,
        min_y,
        max_x,
        max_y,
    })
}

/// RGB of the top-left pixel — the conventional background reference
/// for a chassis-rendered scene, where the clear color fills the
/// corners and geometry sits in the middle. Pass the result as `bg` to
/// `coverage` / `centroid` / `bounding_box` to keep the
/// `differs_from_background` convention. An image with fewer than four
/// bytes (no first pixel) yields `[0, 0, 0]`.
#[must_use]
pub fn background_top_left(image: &Image) -> [u8; 3] {
    if image.rgba.len() < 4 {
        return [0, 0, 0];
    }
    [image.rgba[0], image.rgba[1], image.rgba[2]]
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

    /// Paint a single filled rectangle in `fill` over a solid `bg`
    /// frame, so the silhouette reductions can be checked against
    /// geometry with known corners, area, and center. The rect spans
    /// `[min_x, max_x] × [min_y, max_y]` inclusive, in pixel
    /// coordinates — the same inclusive convention `Rect` and
    /// `bounding_box` use, so a round-trip through `bounding_box`
    /// recovers the exact `rect` passed in.
    fn solid_with_rect(width: u32, height: u32, bg: [u8; 4], fill: [u8; 4], rect: Rect) -> Image {
        let mut image = solid(width, height, bg);
        for y in rect.min_y..=rect.max_y {
            for x in rect.min_x..=rect.max_x {
                let start = ((y * width + x) * 4) as usize;
                image.rgba[start..start + 4].copy_from_slice(&fill);
            }
        }
        image
    }

    #[test]
    fn coverage_matches_rect_area_fraction() {
        let bg = [69, 79, 105];
        // 4×4 lit rect (4..=7 × 6..=9) on a 16×16 frame: 16 of 256 px.
        let rect = Rect {
            min_x: 4,
            min_y: 6,
            max_x: 7,
            max_y: 9,
        };
        let img = solid_with_rect(16, 16, [bg[0], bg[1], bg[2], 255], [200, 32, 32, 255], rect);
        let fraction = coverage(&img, bg, 5);
        assert!(
            (fraction - 16.0 / 256.0).abs() < 1e-6,
            "coverage was {fraction}, expected 16/256",
        );
    }

    #[test]
    fn centroid_lands_at_rect_center() {
        let bg = [69, 79, 105];
        let rect = Rect {
            min_x: 4,
            min_y: 6,
            max_x: 7,
            max_y: 9,
        };
        let img = solid_with_rect(16, 16, [bg[0], bg[1], bg[2], 255], [200, 32, 32, 255], rect);
        let (center_x, center_y) = centroid(&img, bg, 5).expect("a lit mask has a centroid");
        // Mean of the inclusive spans 4..=7 and 6..=9.
        assert!(
            (center_x - 5.5).abs() < 1e-6,
            "centroid x was {center_x}, expected 5.5",
        );
        assert!(
            (center_y - 7.5).abs() < 1e-6,
            "centroid y was {center_y}, expected 7.5",
        );
    }

    #[test]
    fn bounding_box_recovers_rect_corners() {
        let bg = [69, 79, 105];
        let rect = Rect {
            min_x: 4,
            min_y: 6,
            max_x: 7,
            max_y: 9,
        };
        let img = solid_with_rect(16, 16, [bg[0], bg[1], bg[2], 255], [200, 32, 32, 255], rect);
        assert_eq!(bounding_box(&img, bg, 5), Some(rect));
    }

    #[test]
    fn reductions_report_empty_on_all_background() {
        let bg = [69, 79, 105];
        let img = solid(8, 8, [bg[0], bg[1], bg[2], 255]);
        // No pixel diverges from bg, so the mask is empty: zero
        // coverage and no centroid / bounding box to report.
        assert_eq!(coverage(&img, bg, 5), 0.0);
        assert!(centroid(&img, bg, 5).is_none());
        assert!(bounding_box(&img, bg, 5).is_none());
    }

    #[test]
    fn background_top_left_reads_first_pixel() {
        let img = solid(4, 4, [69, 79, 105, 255]);
        assert_eq!(background_top_left(&img), [69, 79, 105]);
        // An image with no first pixel falls back to black rather than
        // indexing out of bounds.
        let empty = Image {
            width: 0,
            height: 0,
            rgba: Vec::new(),
        };
        assert_eq!(background_top_left(&empty), [0, 0, 0]);
    }

    #[test]
    fn not_all_black_passes_on_any_color() {
        let img = solid(4, 4, [0, 0, 1, 255]);
        assert!(not_all_black(&img).is_ok());
    }

    #[test]
    fn not_all_black_fails_on_pure_black() {
        let img = solid(4, 4, [0, 0, 0, 255]);
        let err = not_all_black(&img).expect_err("test setup: solid black must fail");
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

    #[test]
    fn differs_from_background_fails_on_uniform_color() {
        let img = solid(8, 8, [69, 79, 105, 255]);
        let err =
            differs_from_background(&img, 5).expect_err("test setup: uniform background must fail");
        assert!(err.contains("69,79,105"));
        assert!(err.contains("8x8"));
    }

    #[test]
    fn differs_from_background_passes_when_one_pixel_diverges() {
        let mut img = solid(4, 4, [69, 79, 105, 255]);
        img.rgba[20] = 200; // R channel of pixel index 5
        assert!(differs_from_background(&img, 5).is_ok());
    }

    #[test]
    fn differs_from_background_respects_tolerance() {
        // Pixel at idx 5 has R that differs from bg by 4 — within
        // tolerance 5.
        let mut img = solid(4, 4, [69, 79, 105, 255]);
        img.rgba[20] = 73;
        assert!(differs_from_background(&img, 5).is_err());
        // Tolerance 3 — same diff now exceeds.
        assert!(differs_from_background(&img, 3).is_ok());
    }

    #[test]
    fn differs_from_background_handles_tiny_image() {
        let img = Image {
            width: 0,
            height: 0,
            rgba: Vec::new(),
        };
        let err = differs_from_background(&img, 5)
            .expect_err("test setup: empty image must fail with \"too small\"");
        assert!(err.contains("too small"));
    }
}
