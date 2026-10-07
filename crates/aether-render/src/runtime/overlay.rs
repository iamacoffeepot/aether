//! Per-frame overlay accumulator state for the `aether.render` cap
//! (ADR-0105). `on_draw_textured_quads` / `on_draw_text` /
//! `on_draw_screen_triangles` / `on_draw_shapes` file an [`OverlayBatch`]
//! in the [`OverlayFrame`] at the [`Placement`] of the actor that sent it; the frame commit
//! sorts the batches into painter order (ADR-0248 §4), and the driver's
//! `record_overlay_batches` consumes the sorted list at record time.

use aether_kinds::{ClipRect, QuadSpace};
use aether_substrate::mail::registry::LineageOrder;

use super::super::kinds::{
    DrawScreenTriangles, DrawShapes, DrawTexturedQuads, QuadBlend, ScreenTriangle, Shape, TexturedQuad,
};
use super::texture::{GLYPH_ATLAS_TEXTURE_ID, TextureRegistry};

/// One accumulated overlay batch (ADR-0105): what it draws, the projection
/// `space` reads its coordinates in, and the scissor it draws inside. Every
/// arm records in the one overlay pass in the order the frame commit left
/// the batches in: between two actors that is lineage order, a child over
/// its parent and a later sibling over an earlier one, and inside one actor
/// it is the order that actor sent them ([`OverlayFrame::commit`]). Cloned
/// out of the committed list at record time so the cap dispatcher thread can
/// keep filing the next frame's batches while the driver thread expands
/// these.
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

    /// The batch a `draw_text` submission accumulates to (ADR-0248 §10): its
    /// laid-out glyph quads over the reserved glyph-atlas texture. A
    /// rasterized glyph is an ordinary image, coverage in alpha beside a
    /// colour never scaled by it, so the composite is straight.
    pub fn glyphs(clip: Option<ClipRect>, space: QuadSpace, quads: Vec<TexturedQuad>) -> Self {
        Self::Textured { texture_id: GLYPH_ATLAS_TEXTURE_ID, clip, space, blend: QuadBlend::Straight, quads }
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

/// Where one overlay batch lies in the frame's painter order (ADR-0248 §4),
/// decided once, when the batch is filed. The cases are declared back to
/// front, so the derived ordering is the painter order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Placement {
    /// A draw pushed from outside the actor tree, so no actor's place in
    /// the tree applies to it. It lies under every actor's draws.
    Unplaced,
    /// A draw an actor sent, at that actor's place in the tree: a parent's
    /// draws lie under its children's, and an earlier sibling's under a
    /// later one's.
    At(LineageOrder),
}

/// One frame's overlay batches, each filed at the placement of the mail it
/// arrived as (ADR-0248 §4).
///
/// Mail from two actors reaches the renderer in an order that can differ
/// from frame to frame, so arrival order between actors means nothing. The
/// accumulator keeps where each batch lies, and [`Self::commit`] orders the
/// frame once, after the frame's draws have settled. There is no way to add
/// a batch without saying where it lies.
#[derive(Default)]
pub(super) struct OverlayFrame {
    /// In arrival order, which is the order each single sender sent its own.
    filed: Vec<(Placement, OverlayBatch)>,
}

impl OverlayFrame {
    /// File `batch` at `placement`, where the mail it arrived as lies.
    pub(super) fn file(&mut self, placement: Placement, batch: OverlayBatch) {
        self.filed.push((placement, batch));
    }

    /// Take the frame's batches in painter order, back to front, leaving the
    /// accumulator empty for the next frame.
    ///
    /// Batches are ordered by [`Placement`]. The sort is stable, so one
    /// sender's batches keep the order it sent them in. No draw states an
    /// order and none is refused for want of one.
    pub(super) fn commit(&mut self) -> Vec<OverlayBatch> {
        self.filed.sort_by_key(|(placement, _)| *placement);

        self.filed.drain(..).map(|(_, batch)| batch).collect()
    }

    /// The batches filed so far, in arrival order.
    #[cfg(test)]
    pub(super) fn filed(&self) -> impl ExactSizeIterator<Item = &OverlayBatch> + DoubleEndedIterator {
        self.filed.iter().map(|(_, batch)| batch)
    }
}
