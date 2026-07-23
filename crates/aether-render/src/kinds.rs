//! The `aether.render` cap's drawing + texture mail kinds (ADR-0121).
//!
//! These ride the always-on (marker-only `render`) region of the render
//! module, so a wasm guest on the `render` feature sees the kind types
//! for typed `ctx.actor::<RenderCapability>().send(&kind)` addressing
//! without the `render-runtime` GPU stack. The capture-request kinds
//! (`CaptureFrame` / `CaptureFrameResult` / `SimilarityCheck`) and the
//! `FrameCheck` verification family stay in `aether-kinds`: the former
//! are consumed by `aether-mcp` and the latter by the substrate core, so
//! moving them here would close a dependency cycle (ADR-0121). The
//! `QuadSpace` / `QuadScale` projection types also stay central — the
//! `aether.text.draw` kind in `aether-kinds` consumes them — so the quad
//! draw kinds below import them from there.

use aether_data::MailId;
use aether_kinds::{ClipRect, QuadSpace, WindowId};
use aether_math::{Rgb, Rgba};
use bytemuck::{Pod, Zeroable};
use serde::{Deserialize, Serialize};

/// Chassis-internal frame-request kind (ADR-0161 §Decision 1). A pumping
/// driver mails one each frame after the advance chain settles;
/// `RenderCapability::on_frame` records the frame and resolves any pending
/// capture. `replay_cache_when_idle` carries the issue 847 semantic —
/// harness captures replay the last committed accumulators when the producer
/// was idle; desktop always commits current. Not addressed by wasm guests —
/// the pumping chassis driver is its sole sender.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, aether_data::Kind, aether_data::Schema)]
#[kind(name = "aether.render.frame")]
pub struct Frame {
    pub replay_cache_when_idle: bool,
    /// Engine window targets dirtied by this application turn. The render
    /// actor deduplicates the list before presenting; an empty list is the
    /// explicit surfaceless harness path.
    pub windows: Vec<WindowId>,
}

/// Chassis-internal pre-mail-settlement notice (ADR-0161 §Decision 4). One
/// arrives per capture pre-mail whose causal chain has settled;
/// `RenderCapability::on_pre_settled` decrements the pending capture's
/// `pre_remaining`. Wire-identical to `aether.trace.settled` (a single
/// `MailId` field) so the settlement registry's notice-mail bridge
/// (`subscribe_settlement_mail`) delivers it directly. Chassis-internal —
/// the settlement bridge is its sole sender.
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, aether_data::Kind, aether_data::Schema,
)]
#[kind(name = "aether.render.pre_settled")]
pub struct PreSettled {
    pub mail_id: MailId,
}

/// Chassis-internal window-occlusion signal (ADR-0161 §Decision 4). A
/// pumping driver forwards `WindowEvent::Occluded`;
/// `RenderCapability::on_occluded` fail-fasts a pending capture when the
/// window becomes occluded (relocating `fail_capture_if_occluded` into the
/// actor, issue 1317). Chassis-internal — the driver is its sole sender.
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, aether_data::Kind, aether_data::Schema,
)]
#[kind(name = "aether.render.occluded")]
pub struct Occluded {
    pub window: WindowId,
    pub occluded: bool,
}

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
    pub color: Rgb,
}

/// A draw-triangle item. One `DrawTriangle` is three vertices; the mail
/// `count` field is the number of triangles in the payload when
/// sent as a slice.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Pod, Zeroable, aether_data::Kind, aether_data::Schema)]
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

/// View-projection state: column-major `view_proj` matrix (world → clip).
/// The desktop chassis's `aether.view_projection` sink writes the latest
/// payload into the GPU uniform every frame; the WGSL vertex shader
/// multiplies each vertex position by this matrix. Column-major layout
/// matches wgpu's uniform upload — 64 bytes uploaded verbatim, no transpose.
/// Camera components emit this on every `Tick`; the substrate reads only
/// the most recent value before issuing the next draw. Before the first
/// `ViewProjection` arrives, the uniform holds identity and vertices render
/// in clip-space 1:1 (the pre-camera behaviour).
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Pod, Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "aether.view_projection")]
pub struct ViewProjection {
    pub view_proj: [f32; 16],
}

/// Pixel storage format for a registered render texture.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq)]
pub enum TextureFormat {
    /// Four bytes per pixel, row-major RGBA, top-down.
    Rgba8,
    /// One unsigned normalized byte per pixel. Sampling in WGSL yields
    /// `vec4(r, 0.0, 0.0, 1.0)`.
    R8,
}

impl TextureFormat {
    #[must_use]
    pub const fn bytes_per_pixel(self) -> usize {
        match self {
            Self::Rgba8 => 4,
            Self::R8 => 1,
        }
    }
}

/// `aether.render.create_texture` — register a texture in the render
/// cap's session-scoped texture registry. `pixels` is exactly
/// `width * height * format.bytes_per_pixel()` bytes, row-major and
/// top-down. The cap validates the dimensions, assigns the next
/// `texture_id` past any previously created texture (the same
/// id-assignment shape ADR-0103 uses for instrument ids), stages the
/// pixels CPU-side, and replies as soon as the id is assigned — the
/// wgpu texture is realized lazily at the next frame record. Reply:
/// `CreateTextureResult`. Desktop-only — the headless chassis replies
/// `Err` (fail-fast, ADR-0105).
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.render.create_texture")]
pub struct CreateTexture {
    pub width: u32,
    pub height: u32,
    pub format: TextureFormat,
    pub pixels: Vec<u8>,
}

/// Reply to `CreateTexture`. `Ok` carries the assigned `texture_id` —
/// thread it into `DrawTexturedQuads.texture_id` and
/// `UpdateTexture.texture_id`. `Err` carries a human-readable reason —
/// a zero dimension, or a `pixels` length that doesn't match the
/// texture format's byte count.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.render.create_texture_result")]
pub enum CreateTextureResult {
    Ok { texture_id: u32 },
    Err { error: String },
}

/// `aether.render.update_texture` — overwrite a sub-rectangle of a
/// previously-created texture's pixels (atlas growth — e.g. the text
/// cap rasterizing a new glyph into its atlas). `pixels` is exactly
/// `width * height * texture_format.bytes_per_pixel()` bytes covering
/// the `(x, y, width, height)` sub-rect. Fire-and-forget; a bad
/// `texture_id` or an out-of-bounds rect logs and drops. The staged
/// pixels update immediately; the GPU texture re-uploads at the next
/// frame record.
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

/// `aether.render.destroy_texture` — release a previously-created
/// texture from the render cap's session-scoped texture registry.
/// Fire-and-forget; an unknown `texture_id` or the reserved internal
/// white-texture id logs and drops. Dropping the registry entry releases
/// staged pixels and any realized GPU resources.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.render.destroy_texture")]
pub struct DestroyTexture {
    pub texture_id: u32,
}

/// One textured quad in a `DrawTexturedQuads` batch. `(x, y)` is the
/// top-left corner and `(width, height)` the size, both in the unit
/// the batch's `space` selects — window pixels for `Screen`, pixel
/// offsets from the anchor for `World`. `(u0, v0)`–`(u1, v1)` is the
/// uv sub-rect sampled from the batch's texture (`0,0` top-left to
/// `1,1` bottom-right). `tint` is a linear RGBA multiplier applied to
/// the sampled texel — `Rgba::WHITE` draws the texture unmodified; the
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
    pub tint: Rgba,
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
    /// Optional framebuffer-pixel scissor applied to this batch. `None`
    /// leaves the draw unclipped.
    pub clip: Option<ClipRect>,
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
    pub color: Rgba,
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
    /// Optional framebuffer-pixel scissor applied to this batch. `None`
    /// leaves the draw unclipped.
    pub clip: Option<ClipRect>,
    pub quads: Vec<SolidQuad>,
}

/// Shared world-plane rect for material draws. `(x, y, width, height)`
/// are world units and `z` is the depth-tested layer the material quad
/// sits on. Not a kind on its own — embedded in the typed material
/// draw payloads below.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct MaterialRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub z: f32,
}

/// One textured material rect. The rect expands to a world-space quad;
/// `(u0, v0)`–`(u1, v1)` selects the sampled texture region and `tint`
/// multiplies the sampled RGBA texel.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct MaterialTexturedRect {
    pub rect: MaterialRect,
    pub u0: f32,
    pub v0: f32,
    pub u1: f32,
    pub v1: f32,
    pub tint: Rgba,
}

/// `aether.render.material.textured` — draw depth-tested, alpha-blended
/// world-space image rects sampling one registered texture. This is the
/// general substrate-authored image-in-world material for sprites,
/// decals, and splats. `texture_id` comes from `CreateTexture`; an
/// unknown texture warn-drops the batch at record time. Fire-and-forget,
/// immediate-mode: resend every frame the material should be visible.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.render.material.textured")]
pub struct DrawMaterialTextured {
    pub texture_id: u32,
    pub rects: Vec<MaterialTexturedRect>,
}

/// One coverage material rect. The shader samples an R8 texture,
/// thresholds at the fixed iso value 127.5, antialiases the edge with
/// `fwidth`, fills inside with `body_color`, and colors an inner band of
/// `rim_width` coverage-fraction units with `rim_color`.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct MaterialCoverageRect {
    pub rect: MaterialRect,
    pub body_color: Rgba,
    pub rim_color: Rgba,
    pub rim_width: f32,
}

/// `aether.render.material.coverage` — draw depth-tested coverage bands
/// from an R8 texture. The material is substrate-authored and closed:
/// callers provide data (texture id + rect parameters), not WGSL. A
/// non-R8 texture or unknown texture warn-drops the batch at record time.
/// Fire-and-forget, immediate-mode.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.render.material.coverage")]
pub struct DrawMaterialCoverage {
    pub texture_id: u32,
    pub rects: Vec<MaterialCoverageRect>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_data::{decode_slice, encode_slice};

    #[test]
    fn draw_triangle_slice_size() {
        let v = Vertex { x: 0.0, y: 0.5, z: 0.0, color: Rgb::new(1.0, 0.0, 0.0) };
        let tris = [DrawTriangle { verts: [v, v, v] }, DrawTriangle { verts: [v, v, v] }];
        let bytes = encode_slice(&tris);
        assert_eq!(bytes.len(), 2 * 72);
        let back: &[DrawTriangle] = decode_slice(&bytes).expect("test setup: DrawTriangle slice decodes zero-copy");
        assert_eq!(back, &tris);
    }
}
