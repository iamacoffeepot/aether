//! Per-frame overlay accumulator state for the `aether.render` cap
//! (ADR-0105). `on_draw_textured_quads` / `on_draw_solid_quads` /
//! `on_draw_screen_triangles` push a [`QuadBatch`] into the accumulator;
//! the driver's `record_overlay_pass` consumes them at record time.

use aether_kinds::{ClipRect, QuadSpace};

use super::super::kinds::{
    DrawScreenTriangles, DrawSolidQuads, DrawTexturedQuads, QuadBlend, ScreenTriangle, SolidQuad, TexturedQuad,
};
use super::texture::TextureRegistry;

/// What an accumulated overlay batch draws. Both arms record in the one
/// overlay pass in submission order through the one pipeline; they
/// differ only in how the batch expands to vertices.
#[derive(Clone)]
pub enum OverlayGeometry {
    /// Axis-aligned rects, each cornered out into two triangles under
    /// the projection `space` selects (ADR-0105).
    Quads { space: QuadSpace, quads: Vec<TexturedQuad> },
    /// Caller-supplied triangles in window pixels, drawn on the overlay
    /// pass's screen path (iamacoffeepot/aether#5504). No `space`: the
    /// point of the kind is that pixel coordinates are absolute, so
    /// there is no projection to choose.
    ScreenTriangles(Vec<ScreenTriangle>),
}

/// One accumulated overlay batch (ADR-0105): the texture it samples, the
/// scissor and blend it draws under, the layer it draws on, and its
/// geometry. Cloned out of the accumulator at record time so the cap
/// dispatcher thread can keep appending the next frame's batches while
/// the driver thread expands these.
#[derive(Clone)]
pub struct QuadBatch {
    pub texture_id: u32,
    pub clip: Option<ClipRect>,
    pub blend: QuadBlend,
    /// Ascending draw order across batches; `0` is the ordinary layer.
    /// [`sort_by_layer`] orders the frame by it.
    pub layer: u8,
    pub geometry: OverlayGeometry,
}

/// Order a frame's accumulated batches for the overlay pass: ascending
/// `layer`, submission order preserved inside each layer.
///
/// The stability is the contract, not an implementation detail — an
/// all-layer-`0` frame must record in exactly the order it was submitted
/// in, which is what every drawing that predates layers depends on.
pub fn sort_by_layer(batches: &mut [QuadBatch]) {
    batches.sort_by_key(|batch| batch.layer);
}

impl QuadBatch {
    /// The batch a `draw_textured_quads` submission accumulates to — a direct
    /// carry of the mail's fields.
    pub fn textured(mail: DrawTexturedQuads) -> Self {
        Self {
            texture_id: mail.texture_id,
            clip: mail.clip,
            blend: mail.blend,
            layer: mail.layer,
            geometry: OverlayGeometry::Quads { space: mail.space, quads: mail.quads },
        }
    }

    /// The batch a `draw_solid_quads` submission accumulates to (ADR-0107 §4).
    /// Solid quads have no texture of their own, so each expands to a
    /// full-extent sample of the reserved white texture tinted by its `color`
    /// — which is why this needs the registry: the white texture is registered
    /// on first use.
    pub fn solid(mail: DrawSolidQuads, textures: &mut TextureRegistry) -> Self {
        textures.ensure_white();
        let quads = mail
            .quads
            .into_iter()
            .map(|SolidQuad { x, y, width, height, color }| TexturedQuad {
                x,
                y,
                width,
                height,
                u0: 0.0,
                v0: 0.0,
                u1: 1.0,
                v1: 1.0,
                tint: color,
            })
            .collect();
        // A solid quad is a flat colour, so its colour was never
        // scaled by a coverage it does not have.
        Self {
            texture_id: super::texture::WHITE_TEXTURE_ID,
            clip: mail.clip,
            blend: QuadBlend::Straight,
            layer: mail.layer,
            geometry: OverlayGeometry::Quads { space: mail.space, quads },
        }
    }

    /// The batch a `draw_screen_triangles` submission accumulates to
    /// (iamacoffeepot/aether#5504). The corners carry their own colours,
    /// so like a solid quad the batch samples the reserved white texture
    /// and lets the per-vertex tint state the colour — registered here on
    /// first use for the same reason.
    pub fn screen_triangles(mail: DrawScreenTriangles, textures: &mut TextureRegistry) -> Self {
        textures.ensure_white();
        Self {
            texture_id: super::texture::WHITE_TEXTURE_ID,
            clip: mail.clip,
            blend: QuadBlend::Straight,
            layer: mail.layer,
            geometry: OverlayGeometry::ScreenTriangles(mail.triangles),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{OverlayGeometry, QuadBatch, sort_by_layer};
    use crate::QuadBlend;

    /// The ordering `record_overlay_batches` consumes, pinned as an
    /// explicit sequence: ascending layer, submission order preserved
    /// inside each layer. `texture_id` stands in for submission order.
    ///
    /// The named bugs are the choices this function makes rather than
    /// std's sort: keying on a field other than `layer`, ordering
    /// descending so a raised batch draws *under* the ordinary one, and
    /// the function no-oping. (Stability at scale is not provable here —
    /// std's unstable sort agrees with the stable one on inputs this
    /// small, so the assertion this test can honestly make is the key and
    /// the direction.)
    #[test]
    fn sort_by_layer_orders_ascending_and_keeps_same_layer_order() {
        let batch = |texture_id: u32, layer: u8| QuadBatch {
            texture_id,
            clip: None,
            blend: QuadBlend::Straight,
            layer,
            geometry: OverlayGeometry::ScreenTriangles(Vec::new()),
        };

        let mut batches = vec![batch(0, 1), batch(1, 0), batch(2, 1), batch(3, 0), batch(4, 2)];
        sort_by_layer(&mut batches);

        let order: Vec<(u32, u8)> = batches.iter().map(|batch| (batch.texture_id, batch.layer)).collect();
        assert_eq!(order, vec![(1, 0), (3, 0), (0, 1), (2, 1), (4, 2)]);
    }
}
