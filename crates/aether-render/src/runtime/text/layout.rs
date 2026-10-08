//! Pure glyph-rasterization and immediate-mode layout helpers for the
//! renderer's text draws (ADR-0105): fontdue's horizontal metrics in, the
//! textured quads of one run out, each new glyph written into the atlas
//! texture on the way.

use aether_kinds::{FontMetrics, GlyphAdvance};
use aether_math::Rgba;

use super::atlas::{Atlas, AtlasEntry, GlyphKey, GlyphPlacement, GlyphSlot, LaidGlyph};
use crate::kinds::{TextRun, TexturedQuad};
use crate::runtime::texture::StagedTexture;

/// A glyph bitmap's pixel dimensions. fontdue bounds these well below
/// `u32::MAX`, so the `usize → u32` narrowing is exact.
#[allow(clippy::cast_possible_truncation)]
pub fn glyph_dimensions(metrics: &fontdue::Metrics) -> (u32, u32) {
    (metrics.width as u32, metrics.height as u32)
}

/// Where a glyph's quad sits against the pen and the baseline. fontdue
/// uses +y up with `ymin` the glyph's bottom above the baseline; screen
/// space is y-down, so the top row sits `ymin + height` above the baseline
/// and the left edge `xmin` right of the pen. Glyph extents are small
/// integers, exact in `f32`.
#[allow(clippy::cast_precision_loss)]
fn glyph_placement(metrics: &fontdue::Metrics, entry: AtlasEntry) -> GlyphPlacement {
    GlyphPlacement {
        left: metrics.xmin as f32,
        rise: metrics.ymin as f32 + metrics.height as f32,
        width: metrics.width as f32,
        height: metrics.height as f32,
        entry,
    }
}

/// The quad of a placed glyph with the pen at `pen_x` and the baseline at
/// `baseline`.
fn glyph_quad(placement: &GlyphPlacement, pen_x: f32, baseline: f32, tint: Rgba) -> TexturedQuad {
    TexturedQuad {
        x: pen_x + placement.left,
        y: baseline - placement.rise,
        width: placement.width,
        height: placement.height,
        u0: placement.entry.u0,
        v0: placement.entry.v0,
        u1: placement.entry.u1,
        v1: placement.entry.v1,
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

/// A character laid out for the first time since the atlas was reset, and
/// whether the result may be kept: a glyph the full atlas could not place
/// is laid out again by the next draw, after the reset.
struct FirstLayout {
    glyph: LaidGlyph,
    keep: bool,
}

/// Lay one character out from the font: its advance, and its quad if the
/// glyph has pixels. A glyph the atlas has not placed is rasterized here
/// and its pixels written into `texture`, the reserved atlas texture's
/// staged pixels, so the quad that samples it this frame finds it there.
fn lay_out_glyph(
    font: &fontdue::Font,
    atlas: &mut Atlas,
    texture: &mut StagedTexture,
    font_id: u32,
    ch: char,
    size: f32,
) -> FirstLayout {
    let metrics = font.metrics(ch, size);
    // Quantize the size for the raster cache key — two draws at the same
    // nominal size share one raster.
    let key = GlyphKey { font_id, glyph_index: font.lookup_glyph_index(ch), size_pixels: quantize_size(size) };
    // Rasterize only on a cache miss.
    let slot = atlas.cached(&key).unwrap_or_else(|| {
        let (raster, coverage) = font.rasterize(ch, size);
        let (width, height) = glyph_dimensions(&raster);
        atlas.get_or_insert(key, width, height, &coverage)
    });

    let (entry, keep) = match slot {
        GlyphSlot::Cached(entry) => (Some(entry), true),
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
            (Some(entry), true)
        }
        // No pixels: the pen still advances.
        GlyphSlot::Empty => (None, true),
        // The atlas saturated during this draw; it is reset at the top of
        // the next text draw, which places the glyph then.
        GlyphSlot::Full => (None, false),
    };
    let placement = entry.map(|entry| glyph_placement(&metrics, entry));
    FirstLayout { glyph: LaidGlyph { advance: metrics.advance_width, placement }, keep }
}

/// Lay one run out and append its glyph quads to `quads`. `world` says the
/// batch draws under `QuadSpace::World`, where the quads are pixel offsets
/// from the anchor and the run's `origin` is ignored.
///
/// A character this font has drawn at this size since the last atlas reset
/// costs one table read and one quad: its advance and placement are kept
/// with the atlas, and the font is consulted only for a character seen for
/// the first time.
pub fn lay_out_run(
    font: &fontdue::Font,
    atlas: &mut Atlas,
    texture: &mut StagedTexture,
    run: &TextRun,
    world: bool,
    quads: &mut Vec<TexturedQuad>,
) {
    let size = run.size_pixels;
    let baseline = font.horizontal_line_metrics(size).map_or(size, |line| line.ascent);
    let first = quads.len();
    let mut glyphs = atlas.take_run_glyphs(run.font_id, size);

    let mut pen_x = 0.0f32;
    for ch in run.text.chars() {
        let glyph = glyphs.get(ch).unwrap_or_else(|| {
            let laid = lay_out_glyph(font, atlas, texture, run.font_id, ch, size);
            if laid.keep {
                glyphs.insert(ch, laid.glyph);
            }
            laid.glyph
        });
        if let Some(placement) = &glyph.placement {
            quads.push(glyph_quad(placement, pen_x, baseline, run.color));
        }
        pen_x += glyph.advance;
    }
    atlas.put_run_glyphs(run.font_id, size, glyphs);

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
