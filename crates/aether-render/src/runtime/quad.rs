//! Per-frame overlay accumulator state for the `aether.render` cap
//! (ADR-0105). `on_draw_textured_quads` / `on_draw_screen_triangles` /
//! `on_draw_shapes` push a [`QuadBatch`] into the accumulator; the
//! driver's `record_overlay_pass` consumes them at record time.

use aether_kinds::{ClipRect, QuadSpace};

use super::super::kinds::{
    DrawScreenTriangles, DrawShapes, DrawTexturedQuads, QuadBlend, ScreenTriangle, Shape, TexturedQuad,
};
use super::texture::TextureRegistry;

/// What an accumulated overlay batch draws. Every arm records in the one
/// overlay pass in submission order; the textured arms share one
/// pipeline and differ only in how the batch expands to vertices, while
/// shapes run through their own pipeline (ADR-0213).
#[derive(Clone)]
pub enum OverlayGeometry {
    /// Axis-aligned rects, each cornered out into two triangles under
    /// the projection `space` selects (ADR-0105). The one arm whose
    /// colour is a caller-supplied image, so the one arm that carries a
    /// `blend`.
    Quads { space: QuadSpace, blend: QuadBlend, quads: Vec<TexturedQuad> },
    /// Caller-supplied triangles under the projection `space` selects
    /// (iamacoffeepot/aether#5504). The corners carry flat colours the
    /// substrate rasterizes itself, so the composite is always straight.
    ScreenTriangles { space: QuadSpace, triangles: Vec<ScreenTriangle> },
    /// Rounded, stroked, shadowed boxes evaluated as a distance field
    /// under the projection `space` selects (ADR-0213). Samples no
    /// texture: the batch's `texture_id` is unused.
    Shapes { space: QuadSpace, shapes: Vec<Shape> },
}

/// One accumulated overlay batch (ADR-0105): the texture it samples, the
/// scissor it draws under, and its geometry. Cloned out of the
/// accumulator at record time so the cap dispatcher thread can keep
/// appending the next frame's batches while the driver thread expands
/// these.
#[derive(Clone)]
pub struct QuadBatch {
    pub texture_id: u32,
    pub clip: Option<ClipRect>,
    pub geometry: OverlayGeometry,
}

impl QuadBatch {
    /// The batch a `draw_textured_quads` submission accumulates to — a direct
    /// carry of the mail's fields.
    pub fn textured(mail: DrawTexturedQuads) -> Self {
        Self {
            texture_id: mail.texture_id,
            clip: mail.clip,
            geometry: OverlayGeometry::Quads { space: mail.space, blend: mail.blend, quads: mail.quads },
        }
    }

    /// The batch a `draw_screen_triangles` submission accumulates to
    /// (iamacoffeepot/aether#5504). The corners carry their own colours,
    /// so the batch samples the reserved white texture and lets the
    /// per-vertex tint state the colour — registered here on first use.
    pub fn screen_triangles(mail: DrawScreenTriangles, textures: &mut TextureRegistry) -> Self {
        textures.ensure_white();
        Self {
            texture_id: super::texture::WHITE_TEXTURE_ID,
            clip: mail.clip,
            geometry: OverlayGeometry::ScreenTriangles { space: mail.space, triangles: mail.triangles },
        }
    }

    /// The batch a `draw_shapes` submission accumulates to (ADR-0213). A
    /// shape carries its own colours and samples no texture, so the batch
    /// names the reserved white id only to fill the field; the record path
    /// never looks it up.
    pub fn shapes(mail: DrawShapes) -> Self {
        Self {
            texture_id: super::texture::WHITE_TEXTURE_ID,
            clip: mail.clip,
            geometry: OverlayGeometry::Shapes { space: mail.space, shapes: mail.shapes },
        }
    }
}
