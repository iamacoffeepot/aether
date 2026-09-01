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
/// scissor and blend it draws under, and its geometry. Cloned out of the
/// accumulator at record time so the cap dispatcher thread can keep
/// appending the next frame's batches while the driver thread expands
/// these.
#[derive(Clone)]
pub struct QuadBatch {
    pub texture_id: u32,
    pub clip: Option<ClipRect>,
    pub blend: QuadBlend,
    pub geometry: OverlayGeometry,
}

impl QuadBatch {
    /// The batch a `draw_textured_quads` submission accumulates to — a direct
    /// carry of the mail's fields.
    pub fn textured(mail: DrawTexturedQuads) -> Self {
        Self {
            texture_id: mail.texture_id,
            clip: mail.clip,
            blend: mail.blend,
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
            geometry: OverlayGeometry::ScreenTriangles(mail.triangles),
        }
    }
}
