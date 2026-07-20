//! Visual assertions over decoded frame pixels. PNGs come back from
//! `SubstrateHarness::capture` as bytes; this module decodes once and runs
//! O(n) checks against the pixel buffer. Assertion functions take a
//! `&Image` so a single capture can drive many asserts without
//! re-decoding.

use std::io::Cursor;

use aether_kinds::{FrameCheck, FrameCheckResult, FrameRect, FrameReduction, FrameVerdict};
use aether_substrate::capture::ReferenceCapture;
use thiserror::Error;

/// Decoded frame: RGBA8 pixels in row-major top-down order, width
/// and height in pixels. The chassis renders at the size requested
/// at boot (`SubstrateHarness::start_with_size`); decoded `width`/`height`
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
    let mut reader = decoder.read_info().map_err(|e| ImageError::Decode(e.to_string()))?;
    let info = reader.info();
    let width = info.width;
    let height = info.height;
    let color = info.color_type;
    if color != png::ColorType::Rgba {
        return Err(ImageError::UnsupportedColor(color));
    }
    // png 0.18 returns `Option<usize>` here (None on size overflow);
    // surface it as a decode error rather than panicking.
    let buf_size =
        reader.output_buffer_size().ok_or_else(|| ImageError::Decode("output buffer size overflowed".to_string()))?;
    let mut buf = vec![0u8; buf_size];
    reader.next_frame(&mut buf).map_err(|e| ImageError::Decode(e.to_string()))?;
    Ok(Image { width, height, rgba: buf })
}

/// Asserts at least one pixel has a non-zero RGB component. Alpha
/// is ignored — a fully-cleared depth-test frame can have alpha 1.0
/// everywhere yet still be visually black, and a transparent overlay
/// shouldn't count as "drew something". Returns a one-line failure
/// string suitable for `StepReport::Fail`.
pub fn not_all_black(image: &Image) -> Result<(), String> {
    if any_pixel_lit(image, None, [0, 0, 0], 0) == Some(true) {
        return Ok(());
    }
    Err(format!("all {}x{} pixels are black (RGB=0,0,0)", image.width, image.height))
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

/// A pixel "matches" `target` when every one of its RGB channels is
/// within an inclusive `tolerance` of the corresponding target channel —
/// the logical complement of `is_lit` against the same reference color
/// and tolerance (`matches_target(rgb, c, tol) == !is_lit(rgb, c, tol)`),
/// but named and used separately: `is_lit` partitions the frame relative
/// to a *background* for the silhouette reductions, while
/// `matches_target` asks whether a pixel *is* a known *foreground*
/// color for `target_color_stats`. `rgb` is the leading three bytes of
/// an RGBA chunk; alpha is ignored.
fn matches_target(rgb: &[u8], target: [u8; 3], tolerance: u8) -> bool {
    rgb[0].abs_diff(target[0]) <= tolerance
        && rgb[1].abs_diff(target[1]) <= tolerance
        && rgb[2].abs_diff(target[2]) <= tolerance
}

/// Clamp a requested region against the frame bounds, yielding the
/// `Rect` a reduction should walk: `None` (no region requested) maps to
/// the whole frame; `Some(rect)` maps to the frame-clamped
/// intersection. `None` comes back out when the clamped intersection is
/// empty — a zero-size frame, a region entirely outside the frame, or a
/// degenerate `min > max` — so callers score the established
/// empty-mask result (coverage `0.0`, centroid / `bounding_box` `None`)
/// rather than erroring.
fn clamp_region(region: Option<Rect>, width: u32, height: u32) -> Option<Rect> {
    if width == 0 || height == 0 {
        return None;
    }
    let (min_x, min_y, max_x, max_y) = region.map_or_else(
        || (0, 0, width - 1, height - 1),
        |rect| (rect.min_x, rect.min_y, rect.max_x.min(width - 1), rect.max_y.min(height - 1)),
    );
    if min_x > max_x || min_y > max_y {
        return None;
    }
    Some(Rect { min_x, min_y, max_x, max_y })
}

/// Pixel count of an already-clamped region rect (inclusive on both
/// corners).
fn region_pixel_count(rect: Rect) -> u64 {
    u64::from(rect.max_x - rect.min_x + 1) * u64::from(rect.max_y - rect.min_y + 1)
}

/// Walk the `(x, y, &rgb)` triples of every pixel inside `rect`, in
/// frame pixel coordinates, row-major top-down. `rect` must already be
/// clamped to the frame bounds (`clamp_region`'s output) — this is the
/// single place a region turns into a pixel walk, shared by every
/// reduction so the region/clamp math lives in one spot.
fn region_pixels(image: &Image, rect: Rect) -> impl Iterator<Item = (u32, u32, &[u8])> {
    let width = image.width;
    let rgba = image.rgba.as_slice();
    (rect.min_y..=rect.max_y).flat_map(move |y| {
        (rect.min_x..=rect.max_x).map(move |x| {
            let start = ((y * width + x) * 4) as usize;
            (x, y, &rgba[start..start + 4])
        })
    })
}

/// Whether at least one pixel in the frame-clamped `region` is lit
/// relative to `bg`/`tol` — the shared boolean core for `not_all_black`
/// and `differs_from_background` (whole-frame and region-scoped alike).
/// `None` means the region clamped to zero pixels.
fn any_pixel_lit(image: &Image, region: Option<Rect>, bg: [u8; 3], tol: u8) -> Option<bool> {
    clamp_region(region, image.width, image.height)
        .map(|rect| region_pixels(image, rect).any(|(_, _, rgb)| is_lit(rgb, bg, tol)))
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
        return Err(format!("image too small to sample background: {}x{}", image.width, image.height));
    }
    let bg = [image.rgba[0], image.rgba[1], image.rgba[2]];
    if any_pixel_lit(image, None, bg, tolerance) == Some(true) {
        return Ok(());
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

impl From<FrameRect> for Rect {
    fn from(rect: FrameRect) -> Self {
        Self { min_x: rect.min_x, min_y: rect.min_y, max_x: rect.max_x, max_y: rect.max_y }
    }
}

impl From<Rect> for FrameRect {
    fn from(rect: Rect) -> Self {
        Self { min_x: rect.min_x, min_y: rect.min_y, max_x: rect.max_x, max_y: rect.max_y }
    }
}

/// A point in absolute frame pixel coordinates. Coordinates may be
/// fractional when the point is an aggregate such as a pixel centroid;
/// `x` increases across columns and `y` increases down rows.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FramePoint {
    pub x: f32,
    pub y: f32,
}

/// Bounded target-color statistics for a frame-clamped region, returned
/// by `target_color_stats`: how many pixels the region walk sampled, how
/// many matched the requested target within tolerance, the resulting
/// matching fraction, and the matched pixels' centroid and bounding box
/// (both in absolute frame coordinates, mirroring `centroid` /
/// `bounding_box`). The centroid is a [`FramePoint`] whose named `x`
/// and `y` fields make the coordinate order explicit. `centroid` and
/// `bounding_box` are `None` exactly when `matching` is `0` — an empty
/// match set has no location or extent to report.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ColorRegionStats {
    pub sampled: u64,
    pub matching: u64,
    pub fraction: f32,
    pub centroid: Option<FramePoint>,
    pub bounding_box: Option<Rect>,
}

/// Region-scoped core of `coverage`: lit pixels within the
/// frame-clamped `region` divided by the clamped region's pixel count.
/// `region: None` scores the whole frame. Guards the divide-by-zero the
/// same way an empty clamp does — `clamp_region` returning `None`
/// short-circuits to `0.0` before any division happens.
#[allow(clippy::cast_precision_loss)]
fn coverage_in_region(image: &Image, region: Option<Rect>, bg: [u8; 3], tol: u8) -> f32 {
    let Some(rect) = clamp_region(region, image.width, image.height) else {
        return 0.0;
    };
    let total = region_pixel_count(rect);
    let lit = region_pixels(image, rect).filter(|(_, _, rgb)| is_lit(rgb, bg, tol)).count();
    lit as f32 / total as f32
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
    coverage_in_region(image, None, bg, tol)
}

/// Region-scoped core of `centroid`: mean lit-pixel `(x, y)`, reported
/// in frame pixel coordinates (not region-relative) within the
/// frame-clamped `region`. `region: None` scores the whole frame.
#[allow(clippy::cast_precision_loss)]
fn centroid_in_region(image: &Image, region: Option<Rect>, bg: [u8; 3], tol: u8) -> Option<(f32, f32)> {
    let rect = clamp_region(region, image.width, image.height)?;
    let mut sum_x = 0u64;
    let mut sum_y = 0u64;
    let mut lit = 0u64;
    for (x, y, rgb) in region_pixels(image, rect) {
        if is_lit(rgb, bg, tol) {
            sum_x += u64::from(x);
            sum_y += u64::from(y);
            lit += 1;
        }
    }
    if lit == 0 {
        return None;
    }
    Some((sum_x as f32 / lit as f32, sum_y as f32 / lit as f32))
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
    centroid_in_region(image, None, bg, tol)
}

/// Region-scoped core of `bounding_box`, reported in frame pixel
/// coordinates (not region-relative) within the frame-clamped `region`.
/// `region: None` scores the whole frame.
#[allow(clippy::cast_possible_truncation)]
fn bounding_box_in_region(image: &Image, region: Option<Rect>, bg: [u8; 3], tol: u8) -> Option<Rect> {
    let rect = clamp_region(region, image.width, image.height)?;
    let mut min_x = u32::MAX;
    let mut min_y = u32::MAX;
    let mut max_x = 0u32;
    let mut max_y = 0u32;
    let mut any_lit = false;
    for (x, y, rgb) in region_pixels(image, rect) {
        if !is_lit(rgb, bg, tol) {
            continue;
        }
        any_lit = true;
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x);
        max_y = max_y.max(y);
    }
    any_lit.then_some(Rect { min_x, min_y, max_x, max_y })
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
    bounding_box_in_region(image, None, bg, tol)
}

/// Bounded target-color statistics for the frame-clamped `region`: walk
/// the region once, counting pixels whose RGB is within an inclusive
/// per-channel `tolerance` of `target` (`matches_target`; alpha
/// ignored, matching every other reduction's convention), and return
/// the aggregate `ColorRegionStats` — sampled and matching pixel
/// counts, matching fraction, and the matched pixels' centroid and
/// bounding box in absolute frame coordinates. The centroid's named
/// `FramePoint` `x` and `y` fields identify the axes.
/// `region: None` scores the whole frame. An empty or fully out-of-frame
/// region (an empty `clamp_region` result — a zero-size frame, a region
/// entirely outside the frame, or a degenerate `min > max`) yields zero
/// sampled/matching counts, `0.0` fraction, and no centroid/bounding
/// box; a non-empty region with no matching pixel reports its non-zero
/// `sampled` count alongside the same zero `matching`/`fraction`/`None`
/// geometry.
#[must_use]
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
pub fn target_color_stats(image: &Image, target: [u8; 3], tolerance: u8, region: Option<Rect>) -> ColorRegionStats {
    let empty = ColorRegionStats { sampled: 0, matching: 0, fraction: 0.0, centroid: None, bounding_box: None };
    let Some(rect) = clamp_region(region, image.width, image.height) else {
        return empty;
    };
    let sampled = region_pixel_count(rect);
    let mut matching = 0u64;
    let mut sum_x = 0u64;
    let mut sum_y = 0u64;
    let mut min_x = u32::MAX;
    let mut min_y = u32::MAX;
    let mut max_x = 0u32;
    let mut max_y = 0u32;
    for (x, y, rgb) in region_pixels(image, rect) {
        if !matches_target(rgb, target, tolerance) {
            continue;
        }
        matching += 1;
        sum_x += u64::from(x);
        sum_y += u64::from(y);
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x);
        max_y = max_y.max(y);
    }
    if matching == 0 {
        return ColorRegionStats { sampled, ..empty };
    }
    ColorRegionStats {
        sampled,
        matching,
        fraction: matching as f32 / sampled as f32,
        centroid: Some(FramePoint { x: sum_x as f32 / matching as f32, y: sum_y as f32 / matching as f32 }),
        bounding_box: Some(Rect { min_x, min_y, max_x, max_y }),
    }
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

/// Pixel-similarity metric for `mean_absolute_error`. Slots in as the
/// v1 choice in `CaptureFrame.similarity`; `Ssim` and `PHashHamming`
/// are the documented upgrade path for structural / perceptual checks
/// (iamacoffeepot/aether#1780).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Metric {
    /// Mean absolute error per channel, normalized to `[0.0, 1.0]`
    /// (0 = identical, 1 = maximally different). Robust for
    /// "did the demo break" smoke checks once a tolerance absorbs
    /// minor GPU / anti-aliasing nondeterminism.
    MeanAbsoluteError,
}

/// Score the mean absolute error between two `Image`s, normalized to
/// `[0.0, 1.0]` per channel (0 = identical, 1 = maximally different).
/// Only RGB channels contribute; alpha is ignored, matching the
/// `not_all_black` / silhouette reduction conventions.
///
/// Returns `Err` when the images differ in dimensions — the caller
/// should surface this as a `CaptureFrameResult::Err` with a
/// descriptive message rather than silently assigning a score.
///
/// # Unit test coverage
///
/// `tests::mae_identical_images_score_zero` and
/// `tests::mae_known_delta_images_score_expected` verify the two
/// invariants the implementation plan calls out.
#[allow(clippy::cast_precision_loss)]
pub fn mean_absolute_error(a: &Image, b: &Image) -> Result<f32, String> {
    if a.width != b.width || a.height != b.height {
        return Err(format!(
            "dimension mismatch: reference is {}x{} but captured frame is {}x{}",
            b.width, b.height, a.width, a.height,
        ));
    }
    let pixel_count = u64::from(a.width) * u64::from(a.height);
    if pixel_count == 0 {
        return Ok(0.0);
    }
    let total: u64 = a
        .rgba
        .chunks_exact(4)
        .zip(b.rgba.chunks_exact(4))
        .map(|(ac, bc)| {
            u64::from(ac[0].abs_diff(bc[0])) + u64::from(ac[1].abs_diff(bc[1])) + u64::from(ac[2].abs_diff(bc[2]))
        })
        .sum();
    Ok(total as f32 / (pixel_count as f32 * 3.0 * 255.0))
}

/// Score the optional reference-image similarity check (#1780) for a
/// freshly-mapped RGBA frame: decode the reference PNG, compute the
/// normalised MAE against `rgba`, and pass when it is `<= threshold`.
/// Returns `(None, None)` when no reference was requested. Shared by every
/// capture render path so the scoring lives in one place.
pub fn score_similarity(
    rgba: &[u8],
    width: u32,
    height: u32,
    reference: Option<&ReferenceCapture>,
) -> Result<(Option<f32>, Option<bool>), String> {
    let Some(ref_capture) = reference else {
        return Ok((None, None));
    };
    let ref_image =
        decode_png(&ref_capture.png_bytes).map_err(|e| format!("similarity reference decode failed: {e}"))?;
    let captured = Image { width, height, rgba: rgba.to_vec() };
    let score = mean_absolute_error(&captured, &ref_image).map_err(|e| format!("similarity comparison failed: {e}"))?;
    Ok((Some(score), Some(score <= ref_capture.threshold)))
}

/// Score a `CaptureFrame.checks` request against a freshly-mapped
/// RGBA8 frame (the exact bytes the PNG is encoded from) and return the
/// wire [`FrameVerdict`]. This is the substrate-side verdict the MCP
/// `capture_frame` tool surfaces alongside the PNG
/// (iamacoffeepot/aether#1777): the render thread reaches the raw RGBA
/// and the reductions in one place, so a smoke demo asserts on the
/// precise pixels rendered without decoding the returned PNG.
///
/// Each check resolves its own background — explicit when the request
/// pins one, otherwise the frame's top-left pixel
/// (`differs_from_background`'s convention), regardless of `region`.
/// `check.region` restricts every reduction to the frame-clamped
/// intersection of the requested rect (`None` scores the whole frame);
/// coverage divides by the clamped region's pixel count and
/// `centroid`/`bounding_box` report frame (not region-relative)
/// coordinates. `rgba` is consumed to build the working `Image` so the
/// verdict runs without copying the buffer.
#[must_use]
pub fn run_checks(rgba: Vec<u8>, width: u32, height: u32, checks: &[FrameCheck]) -> FrameVerdict {
    let image = Image { width, height, rgba };
    let results = checks
        .iter()
        .map(|check| {
            let bg = check.background.unwrap_or_else(|| background_top_left(&image));
            let region: Option<Rect> = check.region.map(Rect::from);
            match check.reduction {
                FrameReduction::NotAllBlack => {
                    let (passed, detail) = match any_pixel_lit(&image, region, [0, 0, 0], 0) {
                        Some(true) => (true, None),
                        Some(false) => {
                            (false, Some("no pixel in the scored region has a non-zero RGB component".to_string()))
                        }
                        None => (
                            false,
                            Some(format!("region clamps to zero pixels on a {}x{} frame", image.width, image.height)),
                        ),
                    };
                    FrameCheckResult::NotAllBlack { passed, detail }
                }
                FrameReduction::DiffersFromBackground => {
                    let (passed, detail) = if image.rgba.len() < 4 {
                        (false, Some(format!("image too small to sample background: {}x{}", image.width, image.height)))
                    } else {
                        let top_left = [image.rgba[0], image.rgba[1], image.rgba[2]];
                        match any_pixel_lit(&image, region, top_left, check.tolerance) {
                            Some(true) => (true, None),
                            Some(false) => (
                                false,
                                Some(format!(
                                    "no pixel in the scored region diverges from top-left \
                                     ({},{},{}) by more than tolerance ±{}",
                                    top_left[0], top_left[1], top_left[2], check.tolerance
                                )),
                            ),
                            None => (
                                false,
                                Some(format!(
                                    "region clamps to zero pixels on a {}x{} frame",
                                    image.width, image.height
                                )),
                            ),
                        }
                    };
                    FrameCheckResult::DiffersFromBackground { passed, detail }
                }
                FrameReduction::Coverage => FrameCheckResult::Coverage {
                    background: bg,
                    fraction: coverage_in_region(&image, region, bg, check.tolerance),
                },
                FrameReduction::Centroid => FrameCheckResult::Centroid {
                    background: bg,
                    centroid: centroid_in_region(&image, region, bg, check.tolerance).map(<[f32; 2]>::from),
                },
                FrameReduction::BoundingBox => FrameCheckResult::BoundingBox {
                    background: bg,
                    rect: bounding_box_in_region(&image, region, bg, check.tolerance).map(FrameRect::from),
                },
            }
        })
        .collect();
    FrameVerdict { width, height, results }
}

/// RGBA8 diagnostic mask for one `FrameCheck`, at the same dimensions
/// as `image`: a pixel inside the check's frame-clamped region is
/// opaque white when lit and opaque black when unlit — the exact
/// lit/unlit partition `run_checks` scores that reduction against — and
/// a pixel outside the region is fully transparent, so a rendered mask
/// visually separates "not scored" from "scored but background."
/// Mirrors `run_checks`'s per-reduction background resolution exactly,
/// including `FrameReduction::DiffersFromBackground`'s established
/// top-left-only behavior (`check.background` is ignored for that
/// reduction, matching `run_checks`), so a diagnostic artifact can
/// never visualize a different partition than the verdict it explains.
/// Crate-internal: consumed only by `substrate_harness::artifacts` (issue
/// 2914).
#[must_use]
pub(crate) fn diagnostic_mask(image: &Image, check: &FrameCheck) -> Vec<u8> {
    let region: Option<Rect> = check.region.map(Rect::from);
    let (bg, tolerance) = match check.reduction {
        FrameReduction::NotAllBlack => ([0, 0, 0], 0u8),
        FrameReduction::DiffersFromBackground => (background_top_left(image), check.tolerance),
        FrameReduction::Coverage | FrameReduction::Centroid | FrameReduction::BoundingBox => {
            (check.background.unwrap_or_else(|| background_top_left(image)), check.tolerance)
        }
    };
    let mut mask = vec![0u8; image.rgba.len()];
    if let Some(rect) = clamp_region(region, image.width, image.height) {
        for (x, y, rgb) in region_pixels(image, rect) {
            let start = ((y * image.width + x) * 4) as usize;
            let pixel = if is_lit(rgb, bg, tolerance) {
                [255, 255, 255, 255]
            } else {
                [0, 0, 0, 255]
            };
            mask[start..start + 4].copy_from_slice(&pixel);
        }
    }
    mask
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
        Image { width, height, rgba: buf }
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
        let rect = Rect { min_x: 4, min_y: 6, max_x: 7, max_y: 9 };
        let img = solid_with_rect(16, 16, [bg[0], bg[1], bg[2], 255], [200, 32, 32, 255], rect);
        let fraction = coverage(&img, bg, 5);
        assert!((fraction - 16.0 / 256.0).abs() < 1e-6, "coverage was {fraction}, expected 16/256");
    }

    #[test]
    fn centroid_lands_at_rect_center() {
        let bg = [69, 79, 105];
        let rect = Rect { min_x: 4, min_y: 6, max_x: 7, max_y: 9 };
        let img = solid_with_rect(16, 16, [bg[0], bg[1], bg[2], 255], [200, 32, 32, 255], rect);
        let (center_x, center_y) = centroid(&img, bg, 5).expect("a lit mask has a centroid");
        // Mean of the inclusive spans 4..=7 and 6..=9.
        assert!((center_x - 5.5).abs() < 1e-6, "centroid x was {center_x}, expected 5.5");
        assert!((center_y - 7.5).abs() < 1e-6, "centroid y was {center_y}, expected 7.5");
    }

    #[test]
    fn bounding_box_recovers_rect_corners() {
        let bg = [69, 79, 105];
        let rect = Rect { min_x: 4, min_y: 6, max_x: 7, max_y: 9 };
        let img = solid_with_rect(16, 16, [bg[0], bg[1], bg[2], 255], [200, 32, 32, 255], rect);
        assert_eq!(bounding_box(&img, bg, 5), Some(rect));
    }

    #[test]
    fn region_scoped_coverage_divides_by_clamped_region_area_not_frame_area() {
        let bg = [69, 79, 105];
        // 4×4 lit rect (4..=7 × 6..=9) on a 16×16 frame.
        let rect = Rect { min_x: 4, min_y: 6, max_x: 7, max_y: 9 };
        let img = solid_with_rect(16, 16, [bg[0], bg[1], bg[2], 255], [200, 32, 32, 255], rect);
        // A region that fully contains the lit rect plus surrounding
        // background: 8×8 region (2..=9 × 4..=11), of which the same
        // 16 pixels are lit. Coverage is relative to the 64-pixel
        // region, not the 256-pixel frame.
        let region = Rect { min_x: 2, min_y: 4, max_x: 9, max_y: 11 };
        let fraction = coverage_in_region(&img, Some(region), bg, 5);
        assert!((fraction - 16.0 / 64.0).abs() < 1e-6, "region coverage was {fraction}, expected 16/64");
        // A region that excludes half the lit rect (only rows 6..=7 of
        // the lit rect's 6..=9) scores just the included lit pixels
        // over the smaller region's area.
        let half_region = Rect { min_x: 4, min_y: 6, max_x: 7, max_y: 7 };
        let half_fraction = coverage_in_region(&img, Some(half_region), bg, 5);
        assert!(
            (half_fraction - 8.0 / 8.0).abs() < 1e-6,
            "half-region coverage was {half_fraction}, expected 8/8 (fully lit sub-rect)",
        );
    }

    #[test]
    fn region_scoped_centroid_and_bounding_box_report_frame_coordinates() {
        let bg = [69, 79, 105];
        // Two separate lit rects: one the region will include, one it
        // will exclude, so a region-scoped centroid/bbox that leaked
        // the excluded rect's pixels would fail these asserts.
        let included_rect = Rect { min_x: 4, min_y: 6, max_x: 7, max_y: 9 };
        let excluded_rect = Rect { min_x: 12, min_y: 12, max_x: 14, max_y: 14 };
        let mut img = solid_with_rect(16, 16, [bg[0], bg[1], bg[2], 255], [200, 32, 32, 255], included_rect);
        for y in excluded_rect.min_y..=excluded_rect.max_y {
            for x in excluded_rect.min_x..=excluded_rect.max_x {
                let start = ((y * 16 + x) * 4) as usize;
                img.rgba[start..start + 4].copy_from_slice(&[200, 32, 32, 255]);
            }
        }
        // Region covers only `included_rect` (with some background
        // padding), excluding `excluded_rect` entirely.
        let region = Rect { min_x: 0, min_y: 0, max_x: 9, max_y: 11 };
        let (center_x, center_y) = centroid_in_region(&img, Some(region), bg, 5).expect("region has a lit sub-rect");
        assert!(
            (center_x - 5.5).abs() < 1e-6 && (center_y - 7.5).abs() < 1e-6,
            "region centroid ({center_x}, {center_y}) should be the included rect's frame-\
             coordinate center (5.5, 7.5), not blended with the excluded rect",
        );
        let bbox = bounding_box_in_region(&img, Some(region), bg, 5).expect("region has a lit sub-rect");
        assert_eq!(bbox, included_rect, "region bounding box should be the included rect's own frame coordinates");
    }

    #[test]
    fn region_scoped_reductions_clamp_an_overhanging_region_to_the_frame() {
        let bg = [69, 79, 105];
        // Lit rect touching the bottom-right corner of a 16×16 frame.
        let rect = Rect { min_x: 12, min_y: 12, max_x: 15, max_y: 15 };
        let img = solid_with_rect(16, 16, [bg[0], bg[1], bg[2], 255], [200, 32, 32, 255], rect);
        // Region overhangs the frame on both far edges; clamping should
        // restrict scoring to the in-frame intersection (12..=15 ×
        // 12..=15 — exactly the lit rect), an 16-pixel area.
        let overhanging_region = Rect { min_x: 12, min_y: 12, max_x: 100, max_y: 100 };
        let fraction = coverage_in_region(&img, Some(overhanging_region), bg, 5);
        assert!(
            (fraction - 16.0 / 16.0).abs() < 1e-6,
            "clamped-region coverage was {fraction}, expected 16/16 (fully lit clamped region)",
        );
        let bbox =
            bounding_box_in_region(&img, Some(overhanging_region), bg, 5).expect("clamped region has a lit sub-rect");
        assert_eq!(bbox, rect);
    }

    #[test]
    fn region_scoped_reductions_empty_mask_on_degenerate_or_out_of_bounds_region() {
        let bg = [69, 79, 105];
        let rect = Rect { min_x: 4, min_y: 6, max_x: 7, max_y: 9 };
        let img = solid_with_rect(16, 16, [bg[0], bg[1], bg[2], 255], [200, 32, 32, 255], rect);

        // Fully out-of-bounds region (frame is 16×16).
        let out_of_bounds = Rect { min_x: 20, min_y: 20, max_x: 25, max_y: 25 };
        assert_eq!(coverage_in_region(&img, Some(out_of_bounds), bg, 5), 0.0);
        assert!(centroid_in_region(&img, Some(out_of_bounds), bg, 5).is_none());
        assert!(bounding_box_in_region(&img, Some(out_of_bounds), bg, 5).is_none());

        // Degenerate region: min > max on both axes.
        let degenerate = Rect { min_x: 10, min_y: 10, max_x: 2, max_y: 2 };
        assert_eq!(coverage_in_region(&img, Some(degenerate), bg, 5), 0.0);
        assert!(centroid_in_region(&img, Some(degenerate), bg, 5).is_none());
        assert!(bounding_box_in_region(&img, Some(degenerate), bg, 5).is_none());
    }

    #[test]
    fn target_color_stats_matches_within_inclusive_tolerance_boundary() {
        // A pixel exactly `tolerance` away on every channel must match
        // (inclusive bound); one channel a single unit further must not.
        let target = [200, 32, 32];
        let tolerance = 5;
        let on_boundary = solid(2, 2, [205, 37, 27, 255]);
        let stats = target_color_stats(&on_boundary, target, tolerance, None);
        assert_eq!(stats.matching, 4, "a pixel exactly at the tolerance boundary must match");

        let past_boundary = solid(2, 2, [206, 32, 32, 255]);
        let stats = target_color_stats(&past_boundary, target, tolerance, None);
        assert_eq!(stats.matching, 0, "a pixel one unit past the tolerance boundary must not match");
        assert_eq!(stats.sampled, 4);
        assert_eq!(stats.fraction, 0.0);
        assert_eq!(stats.centroid, None);
        assert_eq!(stats.bounding_box, None);
    }

    #[test]
    fn target_color_stats_ignores_alpha() {
        // Same RGB, wildly different alpha — the match predicate must
        // ignore alpha entirely, matching every other reduction's
        // convention.
        let target = [10, 20, 30];
        let opaque = solid(2, 2, [10, 20, 30, 255]);
        let transparent = solid(2, 2, [10, 20, 30, 0]);
        let opaque_stats = target_color_stats(&opaque, target, 0, None);
        let transparent_stats = target_color_stats(&transparent, target, 0, None);
        assert_eq!(opaque_stats.matching, 4);
        assert_eq!(transparent_stats.matching, 4, "alpha must not affect the target-color match");
    }

    #[test]
    fn target_color_stats_excludes_matches_outside_requested_region() {
        let target = [200, 32, 32];
        let bg = [69, 79, 105];
        // Two separate target-colored rects: one the requested region
        // will include, one it will exclude entirely.
        let included_rect = Rect { min_x: 4, min_y: 6, max_x: 7, max_y: 9 };
        let excluded_rect = Rect { min_x: 12, min_y: 12, max_x: 14, max_y: 14 };
        let mut img =
            solid_with_rect(16, 16, [bg[0], bg[1], bg[2], 255], [target[0], target[1], target[2], 255], included_rect);
        for y in excluded_rect.min_y..=excluded_rect.max_y {
            for x in excluded_rect.min_x..=excluded_rect.max_x {
                let start = ((y * 16 + x) * 4) as usize;
                img.rgba[start..start + 4].copy_from_slice(&[target[0], target[1], target[2], 255]);
            }
        }
        // Region covers only `included_rect` (with background padding).
        let region = Rect { min_x: 0, min_y: 0, max_x: 9, max_y: 11 };
        let stats = target_color_stats(&img, target, 5, Some(region));
        assert_eq!(stats.matching, 16, "only the 4x4 included rect should count, not the excluded rect too");
    }

    #[test]
    fn target_color_stats_reports_frame_coordinate_centroid_and_bounds() {
        let target = [200, 32, 32];
        let bg = [69, 79, 105];
        let rect = Rect { min_x: 4, min_y: 6, max_x: 7, max_y: 9 };
        let img = solid_with_rect(16, 16, [bg[0], bg[1], bg[2], 255], [target[0], target[1], target[2], 255], rect);
        let stats = target_color_stats(&img, target, 5, None);
        let center = stats.centroid.expect("a matched region has a centroid");
        assert!(
            (center.x - 5.5).abs() < 1e-6 && (center.y - 7.5).abs() < 1e-6,
            "centroid ({}, {}) should be the rect's frame-coordinate \
             center (5.5, 7.5)",
            center.x,
            center.y,
        );
        assert_eq!(
            stats.bounding_box,
            Some(rect),
            "bounding box should recover the exact rect corners in frame coordinates",
        );
    }

    #[test]
    fn target_color_stats_counts_sampled_and_matching_with_fraction() {
        let target = [200, 32, 32];
        let bg = [69, 79, 105];
        // 4x4 target-colored rect on a 16x16 frame: 16 of 256 pixels.
        let rect = Rect { min_x: 4, min_y: 6, max_x: 7, max_y: 9 };
        let img = solid_with_rect(16, 16, [bg[0], bg[1], bg[2], 255], [target[0], target[1], target[2], 255], rect);
        let stats = target_color_stats(&img, target, 5, None);
        assert_eq!(stats.sampled, 256);
        assert_eq!(stats.matching, 16);
        assert!((stats.fraction - 16.0 / 256.0).abs() < 1e-6, "fraction was {}, expected 16/256", stats.fraction);
    }

    #[test]
    fn target_color_stats_clamps_overhanging_region_to_the_frame() {
        let target = [200, 32, 32];
        let bg = [69, 79, 105];
        // Target-colored rect touching the bottom-right corner of a
        // 16x16 frame.
        let rect = Rect { min_x: 12, min_y: 12, max_x: 15, max_y: 15 };
        let img = solid_with_rect(16, 16, [bg[0], bg[1], bg[2], 255], [target[0], target[1], target[2], 255], rect);
        // Region overhangs the frame on both far edges; clamping should
        // restrict sampling to the in-frame intersection (12..=15 x
        // 12..=15 — exactly the target rect), a 16-pixel area.
        let overhanging_region = Rect { min_x: 12, min_y: 12, max_x: 100, max_y: 100 };
        let stats = target_color_stats(&img, target, 5, Some(overhanging_region));
        assert_eq!(stats.sampled, 16);
        assert_eq!(stats.matching, 16);
        assert_eq!(stats.bounding_box, Some(rect));
    }

    #[test]
    fn target_color_stats_empty_on_degenerate_or_out_of_bounds_region() {
        let target = [200, 32, 32];
        let bg = [69, 79, 105];
        let rect = Rect { min_x: 4, min_y: 6, max_x: 7, max_y: 9 };
        let img = solid_with_rect(16, 16, [bg[0], bg[1], bg[2], 255], [target[0], target[1], target[2], 255], rect);

        // Fully out-of-bounds region (frame is 16x16).
        let out_of_bounds = Rect { min_x: 20, min_y: 20, max_x: 25, max_y: 25 };
        let stats = target_color_stats(&img, target, 5, Some(out_of_bounds));
        assert_eq!(stats.sampled, 0);
        assert_eq!(stats.matching, 0);
        assert_eq!(stats.fraction, 0.0);
        assert!(stats.centroid.is_none());
        assert!(stats.bounding_box.is_none());

        // Degenerate region: min > max on both axes.
        let degenerate = Rect { min_x: 10, min_y: 10, max_x: 2, max_y: 2 };
        let stats = target_color_stats(&img, target, 5, Some(degenerate));
        assert_eq!(stats.sampled, 0);
        assert_eq!(stats.matching, 0);
        assert_eq!(stats.fraction, 0.0);
        assert!(stats.centroid.is_none());
        assert!(stats.bounding_box.is_none());
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
        let empty = Image { width: 0, height: 0, rgba: Vec::new() };
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
    fn mae_identical_images_score_zero() {
        let img = solid(4, 4, [100, 150, 200, 255]);
        let score = mean_absolute_error(&img, &img).expect("test setup: same dims");
        assert_eq!(score, 0.0, "identical images must score exactly 0");
    }

    #[test]
    fn mae_known_delta_images_score_expected() {
        // a = solid red [255, 0, 0, 255], b = solid black [0, 0, 0, 255].
        // Per-pixel RGB diff: 255 + 0 + 0 = 255. Normalized: 255 / (3 * 255) = 1/3.
        let a = solid(2, 2, [255, 0, 0, 255]);
        let b = solid(2, 2, [0, 0, 0, 255]);
        let score = mean_absolute_error(&a, &b).expect("test setup: same dims");
        let expected = 1.0_f32 / 3.0;
        assert!((score - expected).abs() < 1e-6, "red vs black MAE was {score}, expected {expected}");
    }

    #[test]
    fn mae_dimension_mismatch_returns_err() {
        let a = solid(4, 4, [0, 0, 0, 255]);
        let b = solid(8, 8, [0, 0, 0, 255]);
        let err = mean_absolute_error(&a, &b).expect_err("test setup: different dims must err");
        assert!(err.contains("dimension mismatch"), "error message should describe the mismatch: {err}");
    }

    #[test]
    fn mae_ignores_alpha_channel() {
        // Differ only in alpha; RGB is identical. Score should be 0.
        let a = solid(2, 2, [50, 100, 150, 0]);
        let b = solid(2, 2, [50, 100, 150, 255]);
        let score = mean_absolute_error(&a, &b).expect("test setup: same dims");
        assert_eq!(score, 0.0, "alpha difference must not affect the MAE score");
    }

    #[test]
    fn differs_from_background_fails_on_uniform_color() {
        let img = solid(8, 8, [69, 79, 105, 255]);
        let err = differs_from_background(&img, 5).expect_err("test setup: uniform background must fail");
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
        let img = Image { width: 0, height: 0, rgba: Vec::new() };
        let err = differs_from_background(&img, 5).expect_err("test setup: empty image must fail with \"too small\"");
        assert!(err.contains("too small"));
    }

    fn pixel_at(rgba: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
        let start = ((y * width + x) * 4) as usize;
        [rgba[start], rgba[start + 1], rgba[start + 2], rgba[start + 3]]
    }

    #[test]
    fn diagnostic_mask_marks_lit_white_and_unlit_black() {
        let bg = [69, 79, 105];
        let rect = Rect { min_x: 4, min_y: 6, max_x: 7, max_y: 9 };
        let img = solid_with_rect(16, 16, [bg[0], bg[1], bg[2], 255], [200, 32, 32, 255], rect);
        let check =
            FrameCheck { reduction: FrameReduction::Coverage, tolerance: 5, background: Some(bg), region: None };
        let mask = diagnostic_mask(&img, &check);
        assert_eq!(pixel_at(&mask, 16, 5, 7), [255, 255, 255, 255], "a lit pixel should render opaque white");
        assert_eq!(pixel_at(&mask, 16, 0, 0), [0, 0, 0, 255], "an in-frame unlit pixel should render opaque black");
    }

    #[test]
    fn diagnostic_mask_not_all_black_ignores_background_and_tolerance() {
        let mut img = solid(2, 1, [0, 0, 0, 255]);
        img.rgba[4] = 1;
        let check = FrameCheck {
            reduction: FrameReduction::NotAllBlack,
            tolerance: u8::MAX,
            background: Some([1, 0, 0]),
            region: None,
        };
        let mask = diagnostic_mask(&img, &check);
        assert_eq!(
            pixel_at(&mask, 2, 0, 0),
            [0, 0, 0, 255],
            "a black pixel must remain unlit regardless of the check background",
        );
        assert_eq!(
            pixel_at(&mask, 2, 1, 0),
            [255, 255, 255, 255],
            "a non-zero RGB component must remain lit regardless of check tolerance",
        );

        let verdict = run_checks(img.rgba, img.width, img.height, &[check]);
        assert!(matches!(verdict.results.as_slice(), [FrameCheckResult::NotAllBlack { passed: true, .. }],));
    }

    #[test]
    fn diagnostic_mask_clips_out_of_region_pixels_to_transparent() {
        let bg = [69, 79, 105];
        let rect = Rect { min_x: 4, min_y: 6, max_x: 7, max_y: 9 };
        let img = solid_with_rect(16, 16, [bg[0], bg[1], bg[2], 255], [200, 32, 32, 255], rect);
        let region = FrameRect { min_x: 4, min_y: 6, max_x: 7, max_y: 9 };
        let check = FrameCheck {
            reduction: FrameReduction::Coverage,
            tolerance: 5,
            background: Some(bg),
            region: Some(region),
        };
        let mask = diagnostic_mask(&img, &check);
        assert_eq!(
            pixel_at(&mask, 16, 0, 0),
            [0, 0, 0, 0],
            "a pixel outside the requested region should be fully transparent, \
             not just background-colored",
        );
        assert_eq!(
            pixel_at(&mask, 16, 5, 7),
            [255, 255, 255, 255],
            "a lit pixel inside the requested region should still render opaque white",
        );
    }

    #[test]
    fn diagnostic_mask_is_fully_transparent_on_empty_region() {
        let bg = [69, 79, 105];
        let img = solid(8, 8, [bg[0], bg[1], bg[2], 255]);
        let out_of_bounds = FrameRect { min_x: 20, min_y: 20, max_x: 25, max_y: 25 };
        let check = FrameCheck {
            reduction: FrameReduction::Coverage,
            tolerance: 5,
            background: Some(bg),
            region: Some(out_of_bounds),
        };
        let mask = diagnostic_mask(&img, &check);
        assert!(
            mask.iter().all(|&byte| byte == 0),
            "a region that clamps to zero pixels should produce an all-transparent mask",
        );
    }

    #[test]
    fn diagnostic_mask_lit_count_agrees_with_run_checks_coverage() {
        let bg = [69, 79, 105];
        let rect = Rect { min_x: 4, min_y: 6, max_x: 7, max_y: 9 };
        let img = solid_with_rect(16, 16, [bg[0], bg[1], bg[2], 255], [200, 32, 32, 255], rect);
        let check =
            FrameCheck { reduction: FrameReduction::Coverage, tolerance: 5, background: Some(bg), region: None };
        let mask = diagnostic_mask(&img, &check);
        let lit_count = mask.chunks_exact(4).filter(|pixel| *pixel == [255, 255, 255, 255]).count();
        let verdict = run_checks(img.rgba.clone(), img.width, img.height, &[check]);
        let FrameCheckResult::Coverage { fraction, .. } = &verdict.results[0] else {
            panic!("expected a Coverage result");
        };
        #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let expected_lit = (fraction * (16 * 16) as f32).round() as usize;
        assert_eq!(
            lit_count, expected_lit,
            "the mask's lit-pixel count should agree with run_checks' scored coverage fraction \
             so the artifact can never visualize a different partition than the verdict",
        );
    }

    #[test]
    fn diagnostic_mask_differs_from_background_ignores_explicit_background_like_run_checks() {
        // `run_checks` always partitions `DiffersFromBackground` against
        // the frame's actual top-left pixel, never `check.background`
        // (a known contract mismatch tracked separately, issue 2914 side
        // findings). The diagnostic mask must reproduce that exact
        // behavior rather than the documented-but-unimplemented
        // "explicit background is respected" contract, or the artifact
        // would show a different partition than the verdict it explains.
        let top_left = [10, 10, 10];
        let pinned_background = [200, 200, 200];
        let mut img = solid(4, 4, [top_left[0], top_left[1], top_left[2], 255]);
        // Pixel index 1's R channel moves far from top_left (lit against
        // top_left) but happens to land near pinned_background — if the
        // mask wrongly partitioned against pinned_background this pixel
        // would read unlit.
        img.rgba[4] = pinned_background[0];
        let check = FrameCheck {
            reduction: FrameReduction::DiffersFromBackground,
            tolerance: 5,
            background: Some(pinned_background),
            region: None,
        };
        let mask = diagnostic_mask(&img, &check);
        assert_eq!(
            pixel_at(&mask, 4, 1, 0),
            [255, 255, 255, 255],
            "mask should read lit against the top-left reference, not the ignored explicit \
             background — matching run_checks",
        );
    }
}
