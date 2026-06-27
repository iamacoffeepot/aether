//! Per-frame textured-quad accumulator state for the `aether.render`
//! cap (ADR-0105). `on_draw_textured_quads` / `on_draw_solid_quads`
//! push a [`QuadBatch`] into the accumulator; the driver's
//! `record_overlay_pass` consumes them at record time.

use aether_kinds::QuadSpace;

use super::super::kinds::TexturedQuad;

/// One accumulated `draw_textured_quads` batch (ADR-0105): the
/// texture it samples, the projection it draws under, and the quad
/// list. Cloned out of the accumulator at record time so the cap
/// dispatcher thread can keep appending the next frame's batches
/// while the driver thread expands these.
#[derive(Clone)]
pub(in crate::render) struct QuadBatch {
    pub(in crate::render) texture_id: u32,
    pub(in crate::render) space: QuadSpace,
    pub(in crate::render) quads: Vec<TexturedQuad>,
}
