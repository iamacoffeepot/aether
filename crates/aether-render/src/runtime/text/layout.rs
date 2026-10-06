//! Pure glyph-rasterization and immediate-mode layout helpers for the
//! renderer's text draws (ADR-0105): fontdue's horizontal metrics in, the
//! textured quads of one run out, each new glyph written into the atlas
//! texture on the way.

use aether_kinds::{FontMetrics, GlyphAdvance};
use aether_math::Rgba;

use super::atlas::{Atlas, AtlasEntry, GlyphKey, GlyphSlot};
use crate::kinds::{TextRun, TexturedQuad};
use crate::runtime::texture::StagedTexture;

/// A glyph bitmap's pixel dimensions. fontdue bounds these well below
/// `u32::MAX`, so the `usize → u32` narrowing is exact.
#[allow(clippy::cast_possible_truncation)]
pub fn glyph_dimensions(metrics: &fontdue::Metrics) -> (u32, u32) {
    (metrics.width as u32, metrics.height as u32)
}

/// Place a glyph's quad in screen pixels. fontdue uses +y up with
/// `ymin` the glyph's bottom above the baseline; screen space is y-down
/// with the baseline at `baseline`, so the top row sits at
/// `baseline - (ymin + height)` and the left edge at `pen_x + xmin`.
/// Glyph extents are small integers, exact in `f32`.
#[allow(clippy::cast_precision_loss)]
pub fn glyph_quad(
    metrics: &fontdue::Metrics,
    pen_x: f32,
    baseline: f32,
    entry: &AtlasEntry,
    tint: Rgba,
) -> TexturedQuad {
    let top = baseline - (metrics.ymin as f32 + metrics.height as f32);
    let left = pen_x + metrics.xmin as f32;
    TexturedQuad {
        x: left,
        y: top,
        width: metrics.width as f32,
        height: metrics.height as f32,
        u0: entry.u0,
        v0: entry.v0,
        u1: entry.u1,
        v1: entry.v1,
        tint,
    }
}

/// Round a pixel size to its nearest integer for the glyph cache key,
/// clamped to at least 1.
pub fn quantize_size(size_pixels: f32) -> u32 {
    // Caller already checked `size_pixels` is finite and positive.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let rounded = size_pixels.round().max(1.0) as u32;
    rounded
}

/// The atlas rect a character's glyph samples, or `None` when it draws
/// nothing: a glyph with no coverage, or one the full atlas could not
/// place. A glyph seen for the first time is rasterized here and its
/// pixels written into `texture`, the reserved atlas texture's staged
/// pixels, so the quad that samples it this frame finds it there.
fn glyph_entry(
    font: &fontdue::Font,
    atlas: &mut Atlas,
    texture: &mut StagedTexture,
    key: GlyphKey,
    ch: char,
    size: f32,
) -> Option<AtlasEntry> {
    // Rasterize only on a cache miss.
    let slot = atlas.cached(&key).unwrap_or_else(|| {
        let (metrics, coverage) = font.rasterize(ch, size);
        let (width, height) = glyph_dimensions(&metrics);
        atlas.get_or_insert(key, width, height, &coverage)
    });

    match slot {
        GlyphSlot::Cached(entry) => Some(entry),
        GlyphSlot::Placed { entry, rgba } => {
            let written = texture.apply_subrect(entry.x, entry.y, entry.width, entry.height, &rgba);
            if !written {
                tracing::error!(
                    target: "aether_render",
                    x = entry.x,
                    y = entry.y,
                    width = entry.width,
                    height = entry.height,
                    "a packed glyph does not fit the atlas texture; it draws blank",
                );
            }
            Some(entry)
        }
        // Empty: no pixels, just advance the pen. Full: the atlas
        // saturated during this draw; it is reset at the top of the next
        // text draw, which places the glyph then.
        GlyphSlot::Empty | GlyphSlot::Full => None,
    }
}

/// Lay one run out and append its glyph quads to `quads`. `world` says the
/// batch draws under `QuadSpace::World`, where the quads are pixel offsets
/// from the anchor and the run's `origin` is ignored.
pub fn lay_out_run(
    font: &fontdue::Font,
    atlas: &mut Atlas,
    texture: &mut StagedTexture,
    run: &TextRun,
    world: bool,
    quads: &mut Vec<TexturedQuad>,
) {
    let size = run.size_pixels;
    // Quantize the size for the glyph cache key — two draws at the same
    // nominal size share one raster.
    let size_key = quantize_size(size);
    let baseline = font.horizontal_line_metrics(size).map_or(size, |line| line.ascent);
    let first = quads.len();

    let mut pen_x = 0.0f32;
    for ch in run.text.chars() {
        let metrics = font.metrics(ch, size);
        let key = GlyphKey { font_id: run.font_id, glyph_index: font.lookup_glyph_index(ch), size_pixels: size_key };
        if let Some(entry) = glyph_entry(font, atlas, texture, key, ch, size) {
            quads.push(glyph_quad(&metrics, pen_x, baseline, &entry, run.color));
        }
        pen_x += metrics.advance_width;
    }

    // World quads carry pixel offsets relative to the anchor, not absolute
    // screen positions: centre the string horizontally and put the
    // baseline at y=0, so the anchor is the baseline point and the text
    // sits above it. Screen quads flow from the run's origin.
    let [offset_x, offset_y] = if world {
        [-pen_x / 2.0, -baseline]
    } else {
        run.origin
    };
    for quad in &mut quads[first..] {
        quad.x += offset_x;
        quad.y += offset_y;
    }
}

/// Walk a parsed font into its size-independent [`FontMetrics`] table
/// — `units_per_em`, the horizontal line metrics, and every cmap
/// glyph's advance, all in font units.
///
/// Evaluating fontdue at `px = units_per_em` makes its scale factor
/// exactly `1.0`, so each `metrics(..).advance_width` is the raw
/// font-unit advance with no rounding — the value a consumer scales
/// back up with `aether_kinds::scale_units` to reproduce the draw path's
/// advance (`metrics(ch, size).advance_width`) bit-for-bit.
pub fn build_font_metrics(font: &fontdue::Font) -> FontMetrics {
    let units_per_em = font.units_per_em();
    let (ascent, descent, line_gap) = font
        .horizontal_line_metrics(units_per_em)
        .map_or((0.0, 0.0, 0.0), |line| (line.ascent, line.descent, line.line_gap));
    // Glyph 0 is `.notdef` — the advance the draw path uses for a
    // codepoint the font has no glyph for.
    let default_advance = font.metrics_indexed(0, units_per_em).advance_width;
    let mut advances: Vec<GlyphAdvance> = font
        .chars()
        .keys()
        .map(|&ch| GlyphAdvance {
            codepoint: u32::from(ch),
            advance_units: font.metrics(ch, units_per_em).advance_width,
        })
        .collect();
    advances.sort_unstable_by_key(|glyph| glyph.codepoint);
    FontMetrics { units_per_em, ascent, descent, line_gap, default_advance, advances }
}
