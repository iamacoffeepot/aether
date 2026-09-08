//! Per-frame overlay accumulator state for the `aether.render` cap
//! (ADR-0105). `on_draw_textured_quads` / `on_draw_screen_triangles` /
//! `on_draw_shapes` push an [`OverlayBatch`] into the accumulator; the
//! driver's `record_overlay_batches` consumes them at record time.

use aether_kinds::{ClipRect, QuadSpace};

use super::super::kinds::{
    DrawScreenTriangles, DrawShapes, DrawTexturedQuads, QuadBlend, ScreenTriangle, Shape, TexturedQuad,
};
use super::texture::TextureRegistry;

/// One accumulated overlay batch (ADR-0105): what it draws, the projection
/// `space` reads its coordinates in, and the scissor it draws inside. Every
/// arm records in the one overlay pass in submission order, so painter order
/// is the order the cap received the mail. Cloned out of the accumulator at
/// record time so the cap dispatcher thread can keep appending the next
/// frame's batches while the driver thread expands these.
///
/// The arms carry their own fields rather than sharing a header: only the
/// textured arm composites a caller-supplied image, so only it names a
/// `texture_id` and a `blend`, and the record path's match is exhaustive over
/// what each arm actually holds.
#[derive(Clone)]
pub enum OverlayBatch {
    /// Axis-aligned rects, each cornered out into two triangles under the
    /// projection `space` selects (ADR-0105). The one arm whose colour is a
    /// caller-supplied image, so the one arm that carries a `blend`.
    Textured { texture_id: u32, clip: Option<ClipRect>, space: QuadSpace, blend: QuadBlend, quads: Vec<TexturedQuad> },
    /// Caller-supplied triangles under the projection `space` selects
    /// (iamacoffeepot/aether#5504). The corners carry flat colours the
    /// substrate rasterizes over the reserved white texture, so the composite
    /// is always straight and the batch names no texture of its own.
    ScreenTriangles { clip: Option<ClipRect>, space: QuadSpace, triangles: Vec<ScreenTriangle> },
    /// Rounded, stroked, shadowed boxes evaluated as a distance field under
    /// the projection `space` selects (ADR-0213). A shape names the texture
    /// it samples inside its own fill, if any, so the batch names none.
    Shapes { clip: Option<ClipRect>, space: QuadSpace, shapes: Vec<Shape> },
}

impl OverlayBatch {
    /// The batch a `draw_textured_quads` submission accumulates to — a direct
    /// carry of the mail's fields.
    pub fn textured(mail: DrawTexturedQuads) -> Self {
        Self::Textured {
            texture_id: mail.texture_id,
            clip: mail.clip,
            space: mail.space,
            blend: mail.blend,
            quads: mail.quads,
        }
    }

    /// The batch a `draw_screen_triangles` submission accumulates to
    /// (iamacoffeepot/aether#5504). The corners carry their own colours, so
    /// the record path samples the reserved white texture and lets the
    /// per-vertex tint state the colour — registered here on first use.
    pub fn screen_triangles(mail: DrawScreenTriangles, textures: &mut TextureRegistry) -> Self {
        textures.ensure_white();
        Self::ScreenTriangles { clip: mail.clip, space: mail.space, triangles: mail.triangles }
    }

    /// The batch a `draw_shapes` submission accumulates to (ADR-0213).
    pub fn shapes(mail: DrawShapes) -> Self {
        Self::Shapes { clip: mail.clip, space: mail.space, shapes: mail.shapes }
    }

    /// The scissor this batch draws inside — the one field every overlay verb
    /// carries, so the record path reads it without matching first.
    pub fn clip(&self) -> Option<&ClipRect> {
        match self {
            Self::Textured { clip, .. } | Self::ScreenTriangles { clip, .. } | Self::Shapes { clip, .. } => {
                clip.as_ref()
            }
        }
    }
}
