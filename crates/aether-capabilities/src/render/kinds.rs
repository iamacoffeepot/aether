//! The `aether.render` cap's drawing + texture mail kinds (ADR-0121).
//!
//! These ride the always-on (marker-only `render`) region of the render
//! module, so a wasm guest on the `render` feature sees the kind types
//! for typed `ctx.actor::<RenderCapability>().send(&kind)` addressing
//! without the `render-native` GPU stack. The capture-request kinds
//! (`CaptureFrame` / `CaptureFrameResult` / `SimilarityCheck`) and the
//! `FrameCheck` verification family stay in `aether-kinds`: the former
//! are consumed by `aether-mcp` and the latter by the substrate core, so
//! moving them here would close a dependency cycle (ADR-0121). The
//! `QuadSpace` / `QuadScale` projection types also stay central — the
//! `aether.text.draw` kind in `aether-kinds` consumes them — so the quad
//! draw kinds below import them from there.

use aether_kinds::QuadSpace;
use bytemuck::{Pod, Zeroable};
use serde::{Deserialize, Serialize};

/// A single world-space vertex with per-vertex color. Matches the
/// substrate's `VertexBufferLayout`: `(pos: vec3<f32>, color: vec3<f32>)`,
/// 24 bytes on the wire. Positions are world-space; the shader
/// multiplies by the camera's `view_proj` uniform to produce clip
/// space. Not a kind on its own — only addressable as the element
/// type inside `DrawTriangle.verts`.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Pod, Zeroable, aether_data::Schema)]
pub struct Vertex {
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub r: f32,
    pub g: f32,
    pub b: f32,
}

/// A draw-triangle item. One `DrawTriangle` is three vertices; the mail
/// `count` field is the number of triangles in the payload when
/// sent as a slice.
#[repr(C)]
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Pod, Zeroable, aether_data::Kind, aether_data::Schema,
)]
#[kind(name = "aether.draw_triangle")]
pub struct DrawTriangle {
    pub verts: [Vertex; 3],
}

/// Wire size of one `aether.draw_triangle` item: three `Vertex`es.
/// Property of the wire shape, lives next to `DrawTriangle` so any
/// chassis / sink that needs to clamp at whole-triangle boundaries
/// has one canonical source. `repr(C)` + `Pod` + `[Vertex; 3]` packs
/// without padding, so `size_of::<DrawTriangle>()` is exactly the
/// per-triangle wire footprint.
pub const DRAW_TRIANGLE_BYTES: usize = size_of::<DrawTriangle>();

/// Camera state: column-major `view_proj` matrix (world → clip). The
/// desktop chassis's `camera` sink writes the latest payload into the
/// GPU uniform every frame; the WGSL vertex shader multiplies each
/// vertex position by this matrix. Column-major layout matches wgpu's
/// uniform upload — 64 bytes uploaded verbatim, no transpose. Camera
/// components emit this on every `Tick`; the substrate reads only the
/// most recent value before issuing the next draw. Before the first
/// `Camera` arrives, the uniform holds identity and vertices render
/// in clip-space 1:1 (the pre-camera behaviour).
#[repr(C)]
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Pod, Zeroable, aether_data::Kind, aether_data::Schema,
)]
#[kind(name = "aether.camera")]
pub struct Camera {
    pub view_proj: [f32; 16],
}

/// `aether.render.create_texture` — register an RGBA8 texture in the
/// render cap's session-scoped texture registry. `pixels` is exactly
/// `width * height * 4` bytes (RGBA8, row-major, top-down). The cap
/// validates the dimensions, assigns the next `texture_id` past any
/// previously created texture (the same id-assignment shape ADR-0103
/// uses for instrument ids), stages the pixels CPU-side, and replies
/// as soon as the id is assigned — the wgpu texture is realized lazily
/// at the next frame record. Reply: `CreateTextureResult`. Desktop-
/// only — the headless chassis replies `Err` (fail-fast, ADR-0105).
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.render.create_texture")]
pub struct CreateTexture {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

/// Reply to `CreateTexture`. `Ok` carries the assigned `texture_id` —
/// thread it into `DrawTexturedQuads.texture_id` and
/// `UpdateTexture.texture_id`. `Err` carries a human-readable reason —
/// a zero dimension, or a `pixels` length that doesn't match
/// `width * height * 4`.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.render.create_texture_result")]
pub enum CreateTextureResult {
    Ok { texture_id: u32 },
    Err { error: String },
}

/// `aether.render.update_texture` — overwrite a sub-rectangle of a
/// previously-created texture's pixels (atlas growth — e.g. the text
/// cap rasterizing a new glyph into its atlas). `pixels` is exactly
/// `width * height * 4` bytes covering the `(x, y, width, height)`
/// sub-rect. Fire-and-forget; a bad `texture_id` or an out-of-bounds
/// rect logs and drops. The staged pixels update immediately; the GPU
/// texture re-uploads at the next frame record.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.render.update_texture")]
pub struct UpdateTexture {
    pub texture_id: u32,
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

/// One textured quad in a `DrawTexturedQuads` batch. `(x, y)` is the
/// top-left corner and `(width, height)` the size, both in the unit
/// the batch's `space` selects — window pixels for `Screen`, pixel
/// offsets from the anchor for `World`. `(u0, v0)`–`(u1, v1)` is the
/// uv sub-rect sampled from the batch's texture (`0,0` top-left to
/// `1,1` bottom-right). `tint` is a linear RGBA multiplier applied to
/// the sampled texel — `[1.0; 4]` draws the texture unmodified; the
/// alpha channel scales the blend. Not a kind on its own — only
/// addressable inside `DrawTexturedQuads.quads`.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct TexturedQuad {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub u0: f32,
    pub v0: f32,
    pub u1: f32,
    pub v1: f32,
    pub tint: [f32; 4],
}

/// `aether.render.draw_textured_quads` — draw a batch of textured,
/// alpha-blended quads sampling one texture, in the projection `space`
/// selects. Accumulated per frame with the same immediate-mode
/// contract as `aether.draw_triangle`: send it every frame the quads
/// should appear, or they vanish next frame. `texture_id` is a
/// registry id from a prior `CreateTexture`; an unknown id warn-drops
/// the batch. Fire-and-forget; no reply.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.render.draw_textured_quads")]
pub struct DrawTexturedQuads {
    pub texture_id: u32,
    pub space: QuadSpace,
    pub quads: Vec<TexturedQuad>,
}

/// One flat-colored quad in a `DrawSolidQuads` batch. `(x, y)` is the
/// top-left corner and `(width, height)` the size, both in the unit
/// the batch's `space` selects — window pixels for `Screen`, pixel
/// offsets from the anchor for `World`. `color` is a linear RGBA value;
/// the alpha channel scales the blend. Not a kind on its own — only
/// addressable inside `DrawSolidQuads.quads`.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct SolidQuad {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub color: [f32; 4],
}

/// `aether.render.draw_solid_quads` — draw a batch of flat-colored,
/// alpha-blended quads in the projection `space` selects. Accumulated
/// per frame with the same immediate-mode contract as
/// `aether.draw_triangle`: send it every frame the quads should appear,
/// or they vanish next frame. Reuses the textured-quad overlay pipeline
/// with a reserved internal 1×1 white texture tinted by `color` — no
/// new GPU pipeline. Fire-and-forget; no reply.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.render.draw_solid_quads")]
pub struct DrawSolidQuads {
    pub space: QuadSpace,
    pub quads: Vec<SolidQuad>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_data::{decode_slice, encode_slice};

    #[test]
    fn draw_triangle_slice_size() {
        let v = Vertex {
            x: 0.0,
            y: 0.5,
            z: 0.0,
            r: 1.0,
            g: 0.0,
            b: 0.0,
        };
        let tris = [
            DrawTriangle { verts: [v, v, v] },
            DrawTriangle { verts: [v, v, v] },
        ];
        let bytes = encode_slice(&tris);
        assert_eq!(bytes.len(), 2 * 72);
        let back: &[DrawTriangle] =
            decode_slice(&bytes).expect("test setup: DrawTriangle slice decodes zero-copy");
        assert_eq!(back, &tris);
    }
}
