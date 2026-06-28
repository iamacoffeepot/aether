//! Pure glyph-rasterization + immediate-mode layout helpers for the
//! `aether.text` cap (ADR-0105). Native-only — they run fontdue's
//! horizontal metrics and shape the textured-quad batch the cap emits to
//! `aether.render` — so they ride the same `text-runtime` gate as the
//! rest of the cap's runtime half (`super`, this module's parent).

use std::path::Path;

use aether_kinds::{FontMetrics, GlyphAdvance, QuadSpace};
use aether_substrate::actor::native::NativeCtx;

use crate::render::{DrawTexturedQuads, RenderCapability, TexturedQuad};

use super::atlas::AtlasEntry;

/// Emit the accumulated quad batch to `aether.render`.
pub(super) fn emit_draw(
    ctx: &mut NativeCtx<'_>,
    texture_id: u32,
    space: QuadSpace,
    quads: Vec<TexturedQuad>,
) {
    let draw = DrawTexturedQuads {
        texture_id,
        space,
        quads,
    };
    ctx.actor::<RenderCapability>().send(&draw);
}

/// A glyph bitmap's pixel dimensions. fontdue bounds these well below
/// `u32::MAX`, so the `usize → u32` narrowing is exact.
#[allow(clippy::cast_possible_truncation)]
pub(super) fn glyph_dimensions(metrics: &fontdue::Metrics) -> (u32, u32) {
    (metrics.width as u32, metrics.height as u32)
}

/// Place a glyph's quad in screen pixels. fontdue uses +y up with
/// `ymin` the glyph's bottom above the baseline; screen space is y-down
/// with the baseline at `baseline`, so the top row sits at
/// `baseline - (ymin + height)` and the left edge at `pen_x + xmin`.
/// Glyph extents are small integers, exact in `f32`.
#[allow(clippy::cast_precision_loss)]
pub(super) fn glyph_quad(
    metrics: &fontdue::Metrics,
    pen_x: f32,
    baseline: f32,
    entry: &AtlasEntry,
    tint: [f32; 4],
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
pub(super) fn quantize_size(size_pixels: f32) -> u32 {
    // Caller already checked `size_pixels` is finite and positive.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let rounded = size_pixels.round().max(1.0) as u32;
    rounded
}

/// The font's display name — the file stem of its path (e.g.
/// `fonts/RobotoMono.ttf` → `RobotoMono`), or the whole path when it
/// has no stem.
pub(super) fn font_name_from_path(path: &str) -> String {
    Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .map_or_else(|| path.to_owned(), ToOwned::to_owned)
}

/// Walk a parsed font into its size-independent [`FontMetrics`] table
/// — `units_per_em`, the horizontal line metrics, and every cmap
/// glyph's advance, all in font units.
///
/// Evaluating fontdue at `px = units_per_em` makes its scale factor
/// exactly `1.0`, so each `metrics(..).advance_width` is the raw
/// font-unit advance with no rounding — the value a consumer scales
/// back up with `aether_kinds::scale_units` to reproduce this cap's
/// draw-path advance (`metrics(ch, size).advance_width`) bit-for-bit.
pub(super) fn build_font_metrics(font: &fontdue::Font) -> FontMetrics {
    let units_per_em = font.units_per_em();
    let (ascent, descent, line_gap) = font
        .horizontal_line_metrics(units_per_em)
        .map_or((0.0, 0.0, 0.0), |line| {
            (line.ascent, line.descent, line.line_gap)
        });
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
    FontMetrics {
        units_per_em,
        ascent,
        descent,
        line_gap,
        default_advance,
        advances,
    }
}
