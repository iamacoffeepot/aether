//! Per-frame textured-quad accumulator state for the `aether.render`
//! cap (ADR-0105). `on_draw_textured_quads` / `on_draw_solid_quads`
//! push a [`QuadBatch`] into the accumulator; the driver's
//! `record_overlay_pass` consumes them at record time.

use aether_kinds::{ClipRect, QuadSpace};

use super::super::kinds::{DrawSolidQuads, DrawTexturedQuads, SolidQuad, TexturedQuad};
use super::texture::TextureRegistry;

/// One accumulated `draw_textured_quads` batch (ADR-0105): the
/// texture it samples, the projection it draws under, and the quad
/// list. Cloned out of the accumulator at record time so the cap
/// dispatcher thread can keep appending the next frame's batches
/// while the driver thread expands these.
#[derive(Clone)]
pub struct QuadBatch {
    pub texture_id: u32,
    pub space: QuadSpace,
    pub clip: Option<ClipRect>,
    pub quads: Vec<TexturedQuad>,
}

impl QuadBatch {
    /// The batch a `draw_textured_quads` submission accumulates to — a direct
    /// carry of the mail's fields.
    pub fn textured(mail: DrawTexturedQuads) -> Self {
        Self { texture_id: mail.texture_id, space: mail.space, clip: mail.clip, quads: mail.quads }
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
        Self { texture_id: super::texture::WHITE_TEXTURE_ID, space: mail.space, clip: mail.clip, quads }
    }
}
