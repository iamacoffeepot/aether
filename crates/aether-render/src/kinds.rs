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
    /// Four bytes per pixel, one little-endian `f32` — the data-plane
    /// format (ADR-0170) whose texel values are quantities or labels
    /// rather than colors. Core WebGPU cannot linear-filter 32-bit
    /// floats, so a create with `TextureSampling::Linear` is rejected
    /// and the realized texture binds through a non-filtering nearest
    /// binding. The color material / overlay passes sample through the
    /// filtering binding only, so they warn-drop batches over this
    /// format; its consumers are the authored render programs.
    R32Float,
}

impl TextureFormat {
    #[must_use]
    pub const fn bytes_per_pixel(self) -> usize {
        match self {
            Self::Rgba8 | Self::R32Float => 4,
            Self::R8 => 1,
        }
    }

    /// Whether core WebGPU can linear-filter this format — equivalently,
    /// whether its realized texture binds through the shared filtering
    /// layout the color material / overlay pipelines are built against.
    #[must_use]
    pub const fn filterable(self) -> bool {
        match self {
            Self::Rgba8 | Self::R8 => true,
            Self::R32Float => false,
        }
    }
}

/// How a registered texture's texels are filtered when sampled.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq)]
pub enum TextureSampling {
    /// Bilinear filtering — texels are colors, and blending adjacent
    /// texels produces an in-between color. The right choice for
    /// images, glyph atlases, and coverage fields.
    Linear,
    /// Nearest texel — texel values are identities (region labels,
    /// cell states), and interpolating between neighbors would
    /// manufacture values no texel holds (ADR-0170). Required for
    /// `TextureFormat::R32Float`.
    Nearest,
}

/// Which GPU role a registered texture is realized for.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq)]
pub enum TextureUsage {
    /// CPU-staged pixels sampled by draws — wgpu
    /// `TEXTURE_BINDING | COPY_DST`. `CreateTexture.pixels` stages the
    /// initial content and `UpdateTexture` overwrites sub-rects.
    Sampled,
    /// A GPU render target sampled by draws — wgpu
    /// `RENDER_ATTACHMENT | TEXTURE_BINDING` (ADR-0170). Created
    /// without staged pixels (`CreateTexture.pixels` must be empty) and
    /// cleared to transparent black at realization; authored render
    /// programs draw into it, so there is no CPU staging and
    /// `UpdateTexture` warn-drops.
    Writable,
}

/// `aether.render.create_texture` — register a texture in the render
/// cap's session-scoped texture registry. For a `Sampled` texture,
/// `pixels` is exactly `width * height * format.bytes_per_pixel()`
/// bytes, row-major and top-down; for a `Writable` texture, `pixels`
/// must be empty — the texture is a GPU render target cleared to
/// transparent black at realization (ADR-0170). The cap validates the
/// dimensions, assigns the next `texture_id` past any previously
/// created texture (the same id-assignment shape ADR-0103 uses for
/// instrument ids), stages any pixels CPU-side, and replies as soon as
/// the id is assigned — the wgpu texture is realized lazily at the
/// next frame record. Reply: `CreateTextureResult`. Desktop-only — the
/// headless chassis replies `Err` (fail-fast, ADR-0105).
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.render.create_texture")]
pub struct CreateTexture {
    pub width: u32,
    pub height: u32,
    pub format: TextureFormat,
    pub sampling: TextureSampling,
    pub usage: TextureUsage,
    #[serde(with = "aether_data::bytes")]
    pub pixels: Vec<u8>,
}

/// Reply to `CreateTexture`. `Ok` carries the assigned `texture_id` —
/// thread it into `DrawTexturedQuads.texture_id` and
/// `UpdateTexture.texture_id`. `Err` carries a human-readable reason —
/// a zero dimension, a dimension past the device's
/// `max_texture_dimension_2d` (named against the limit, since the
/// texture is realized lazily and an unchecked one would fault the
/// frame that first drew with it rather than this reply), a `pixels`
/// length that doesn't match the texture format's byte count (or isn't
/// empty for a `Writable` texture), or `Linear` sampling on the
/// non-filterable `R32Float` format.
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
/// `texture_id`, an out-of-bounds rect, or a `Writable` texture (a GPU
/// render target with no CPU staging) logs and drops. The staged
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
    #[serde(with = "aether_data::bytes")]
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

/// Storage format of one vertex attribute in a geometry layout
/// (ADR-0171). A closed set: the scalar and small-vector forms the
/// authored vertex stages consume, including the integer and normalized
/// shapes skinning needs, so a rigged mesh's layout is expressible
/// without reopening the enum. Variant names match the
/// `wgpu::VertexFormat` they realize as.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq)]
pub enum VertexFormat {
    /// Three 32-bit floats — positions and normals. 12 bytes.
    Float32x3,
    /// Two 32-bit floats — texture coordinates. 8 bytes.
    Float32x2,
    /// One 32-bit float — a scalar attribute (a class label, a weight).
    /// 4 bytes.
    Float32,
    /// Four 8-bit unsigned integers — skinning joint indices. 4 bytes.
    Uint8x4,
    /// Four 8-bit unsigned normalized values sampled as `0.0..=1.0` —
    /// skinning weights, packed colors. 4 bytes.
    Unorm8x4,
}

impl VertexFormat {
    /// Byte width of one attribute in this format. Every variant is a
    /// multiple of four bytes, so a layout stride always satisfies
    /// wgpu's four-byte buffer alignment.
    #[must_use]
    pub const fn bytes(self) -> usize {
        match self {
            Self::Float32x3 => 12,
            Self::Float32x2 => 8,
            Self::Float32 | Self::Uint8x4 | Self::Unorm8x4 => 4,
        }
    }
}

/// One declared vertex attribute (ADR-0171): the WGSL `@location` index
/// the authored vertex stage binds it at, plus its storage format.
/// Attributes pack in declaration order with no padding — the layout's
/// stride is the sum of its formats' bytes ([`vertex_stride_bytes`]).
/// Not a kind on its own — only addressable inside
/// `CreateGeometry.layout`.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq)]
pub struct VertexAttribute {
    pub location: u32,
    pub format: VertexFormat,
}

/// Byte stride of one vertex under `layout`: the sum of its attribute
/// formats' bytes, in declaration order with no padding. The single
/// source of the stride rule — create/update validation divides the
/// staged vertex bytes by it, and the draw-pass stage (ADR-0171) builds
/// its `wgpu::VertexBufferLayout` from it.
#[must_use]
pub fn vertex_stride_bytes(layout: &[VertexAttribute]) -> usize {
    layout.iter().map(|attribute| attribute.format.bytes()).sum()
}

/// `aether.render.create_geometry` — register a geometry in the render
/// cap's session-scoped geometry registry (ADR-0171). `layout` declares
/// the vertex attributes; `vertices` is the packed attribute bytes
/// (length a multiple of the layout stride) and `indices` the 32-bit
/// little-endian triangle-list indices (length a multiple of four, each
/// within the vertex count). The cap validates at create — an empty
/// layout, a vertex length off the stride, an index length off four,
/// or an out-of-range index each reject with a distinguishable
/// reason — assigns the next `geometry_id` past any previously created
/// geometry (the same id-assignment shape as texture ids), stages the
/// bytes CPU-side, and replies as soon as the id is assigned; the wgpu
/// vertex/index buffers are realized lazily at first GPU use. Geometry
/// uploads happen at subject-load cadence — deformation is program
/// content riding the uniform blob, never per-frame re-creation. Reply:
/// `CreateGeometryResult`. Desktop-only — the headless chassis replies
/// `Err` (fail-fast, ADR-0105).
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.render.create_geometry")]
pub struct CreateGeometry {
    pub layout: Vec<VertexAttribute>,
    #[serde(with = "aether_data::bytes")]
    pub vertices: Vec<u8>,
    #[serde(with = "aether_data::bytes")]
    pub indices: Vec<u8>,
}

/// Reply to `CreateGeometry`. `Ok` carries the assigned `geometry_id` —
/// thread it into `UpdateGeometry.geometry_id` and
/// `DestroyGeometry.geometry_id` (and the draw-pass geometry binding,
/// ADR-0171). `Err` carries a human-readable reason naming its
/// validation class: an empty layout, a vertex byte length that does
/// not divide by the layout stride, an index byte length that does not
/// divide by four, or an index outside the vertex count.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.render.create_geometry_result")]
pub enum CreateGeometryResult {
    Ok { geometry_id: u32 },
    Err { reason: String },
}

/// `aether.render.update_geometry` — replace a previously-created
/// geometry's vertex and index bytes in place (ADR-0171). The layout is
/// fixed at create; the replacement is validated against it under the
/// same rules as `CreateGeometry` and swaps wholesale (the byte lengths
/// may change). Fire-and-forget; an unknown `geometry_id` or an invalid
/// replacement logs and drops, leaving the previous content staged. The
/// staged bytes update immediately; the GPU buffers re-realize at the
/// next GPU use. Per-frame updates are for view-dependent geometry that
/// is small by nature (the ink ribbons) — a deforming mesh poses
/// through the uniform blob instead.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.render.update_geometry")]
pub struct UpdateGeometry {
    pub geometry_id: u32,
    #[serde(with = "aether_data::bytes")]
    pub vertices: Vec<u8>,
    #[serde(with = "aether_data::bytes")]
    pub indices: Vec<u8>,
}

/// `aether.render.destroy_geometry` — release a previously-created
/// geometry from the render cap's session-scoped geometry registry,
/// mirroring `destroy_texture`. Fire-and-forget; an unknown
/// `geometry_id` logs and drops. Dropping the registry entry releases
/// the staged bytes and any realized GPU buffers.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.render.destroy_geometry")]
pub struct DestroyGeometry {
    pub geometry_id: u32,
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

/// Shared world rect for material draws. `(x, y, z)` is the rect's
/// origin corner and `right` / `up` are the world directions its
/// `width` and `height` extend along: a corner at fractional `(u, v)`
/// sits at `origin + right * width * u + up * height * v`. A draped
/// planar caller passes the world axes (`right = [1, 0, 0]`,
/// `up = [0, 1, 0]`); an oriented caller — a camera-facing
/// underpainting standing behind its subject — hands the basis it
/// already knows. Depth-tests like any world geometry. Not a kind on
/// its own — embedded in the typed material draw payloads below.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct MaterialRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub z: f32,
    pub right: [f32; 3],
    pub up: [f32; 3],
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

/// How a program texture slot's size derives from the program's
/// reference extent — the size of the dispatch binding the final pass
/// writes (ADR-0170).
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq)]
pub enum SlotExtent {
    /// The reference extent itself.
    Full,
    /// The reference extent divided by `divisor` on both axes — floor
    /// division, clamped to at least one texel — for pyramid and
    /// reduced-resolution work. `divisor` must be at least 1; a zero
    /// divisor rejects at register.
    Divided { divisor: u32 },
}

/// The declared shape of one program texture slot — an entry in
/// `ProgramRegister.bindings` (a registry texture supplied at dispatch)
/// or `ProgramRegister.transients` (an executor-owned intermediate).
/// `format` fixes the slot's pixel format at register time, which is
/// what lets every pass pipeline build (and fail) inside the register
/// reply rather than at first dispatch; a dispatch binding whose
/// registry texture disagrees with the declared format or resolved
/// extent warn-drops the dispatch.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq)]
pub struct SlotSpec {
    pub format: TextureFormat,
    pub extent: SlotExtent,
}

/// One input slot a program pass samples (ADR-0170). Every variant
/// resolves to a texture the pass binds in its group-1 input pairs, in
/// declaration order.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq)]
pub enum InputSlot {
    /// The dispatch binding at `index` into `ProgramDispatch.bindings`,
    /// declared at the same `index` in `ProgramRegister.bindings`.
    Binding { index: u32 },
    /// Whatever slot the pass at sequence index `pass` wrote its output
    /// into — an alias resolved at register time, so a ping-pong chain
    /// reads "the previous pass's result" without naming the transient
    /// twice. `pass` must be earlier in the sequence.
    PassOutput { pass: u32 },
    /// The transient intermediate at `index` into
    /// `ProgramRegister.transients`. Must be written by an earlier pass
    /// before it is read — the register-time sequence-index check.
    Transient { index: u32 },
}

/// The slot a program pass writes (ADR-0170): a dispatch binding (a
/// writable registry texture) or a transient intermediate. Passes never
/// write another pass's output alias, so that variant does not exist
/// here.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq)]
pub enum OutputSlot {
    /// The dispatch binding at `index` into `ProgramDispatch.bindings`.
    /// The bound registry texture must be `TextureUsage::Writable`;
    /// a `Sampled` texture there warn-drops the dispatch.
    Binding { index: u32 },
    /// The transient intermediate at `index` into
    /// `ProgramRegister.transients`.
    Transient { index: u32 },
}

/// One geometry slot a program declares (ADR-0171) — an entry in
/// `ProgramRegister.geometries` that a `ProgramDispatch.geometries` id
/// fills, the same supply shape texture bindings use. `layout` is the
/// vertex layout the slot's geometry must have been created with: the
/// register builds each draw pass's vertex buffer layout from it and
/// checks the authored vertex stage's interface against it, and a
/// dispatch whose geometry disagrees warn-drops.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct GeometrySlotSpec {
    pub layout: Vec<VertexAttribute>,
}

/// What a draw pass does to its color output before drawing (ADR-0171).
/// Unlike a fragment pass — whose first write in a dispatch always
/// clears and whose later writes load — a draw pass declares this
/// outright, so a layered bake states its own composition.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq)]
pub enum PassLoad {
    /// Clear the output to transparent black, then draw.
    Clear,
    /// Load whatever the output already holds and draw over it — the
    /// retained pixels of a writable binding across dispatches, or an
    /// earlier pass's work within one.
    Load,
}

/// The `PassStage::Draw` declaration (ADR-0171): what a rasterizing pass
/// needs beyond what every pass declares. The pass's fragment entry
/// point, input slots, color output, and uniform window stay on
/// [`ProgramPass`]; this carries the vertex half.
///
/// `depth` names an index into `ProgramRegister.depth_transients`. The
/// declaration *is* the depth rule: a pass depth-tests exactly when it
/// names a depth slot (`Depth32Float`, `LessEqual`, depth-write on), and
/// a pass naming none rasterizes in draw order with no depth at all.
/// The first pass of a dispatch to name a given slot clears it to the
/// far plane and later passes naming the same slot load it, so
/// consecutive draw passes agree on occlusion by naming one slot.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct DrawPass {
    /// Vertex entry point in the program's WGSL module. It consumes the
    /// geometry slot's declared attributes at their `@location` indices
    /// and may read the pass's uniform window, which binds at
    /// `@group(0) @binding(0)` for the vertex stage as well as the
    /// fragment stage.
    pub vertex_entry_point: String,
    /// Index into `ProgramRegister.geometries` — the slot whose id the
    /// dispatch supplies.
    pub geometry: u32,
    /// Index into `ProgramRegister.depth_transients`, or `None` for a
    /// pass that does not depth-test.
    pub depth: Option<u32>,
    pub load: PassLoad,
}

/// Which GPU stage a program pass runs (ADR-0170, ADR-0171). A `Compute`
/// arm arrives as an addition when a consumer needs shared-memory tiles,
/// reductions, or scatter writes — the slot / extent / uniform-window
/// vocabulary is stage-agnostic.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum PassStage {
    /// A fullscreen-triangle fragment pipeline over a render attachment.
    Fragment,
    /// An indexed triangle-list draw of a bound geometry through an
    /// authored vertex stage, optionally depth-tested (ADR-0171).
    Draw(DrawPass),
}

/// Repetition of one program pass (ADR-0170): the pass records `count`
/// times, iteration `i` binding its uniform window at
/// `uniform_offset + i * uniform_stride`, so a chain of pours is one
/// pass entry over a strided parameter table rather than many entries.
/// The first iteration clears the output slot (if nothing wrote it
/// earlier in the dispatch); later iterations load it, so iterations
/// accumulate through the pass's blend. `count` must be at least 1 and
/// at most 4096; `uniform_stride` may be 0 to rebind the same window
/// every iteration.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq)]
pub struct PassRepeat {
    pub count: u32,
    pub uniform_stride: u32,
}

/// One pass in a program's declared graph (ADR-0170). The graph is a
/// sequence: a pass may read only slots already written, which makes
/// the DAG check a single index comparison at register time.
/// `entry_point` names a fragment entry in the program's WGSL module —
/// paired with the substrate's fullscreen vertex stage for a
/// `PassStage::Fragment` pass, or with the authored vertex stage the
/// `PassStage::Draw` declaration names (ADR-0171). `inputs` bind in
/// order as the pass's group-1 texture / sampler
/// pairs; `output` is the render attachment. `uniform_offset` /
/// `uniform_length` window the dispatch's uniform blob in bytes — the
/// window binds at `@group(0) @binding(0)` and must cover the uniform
/// block the entry point declares (checked at register from naga's
/// layout; a shorter window rejects). A pass whose entry point declares
/// no uniform block passes a zero-length window.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ProgramPass {
    pub stage: PassStage,
    pub entry_point: String,
    pub inputs: Vec<InputSlot>,
    pub output: OutputSlot,
    pub uniform_offset: u32,
    pub uniform_length: u32,
    pub repeat: Option<PassRepeat>,
}

/// `aether.render.program.register` — register an authored render
/// program (ADR-0170): one WGSL module plus a declared pass graph the
/// substrate compiles, validates, and executes without knowing what it
/// paints. Validation happens here, once, each failure class with a
/// distinguishable `Err` reason: the WGSL through naga (`invalid
/// wgsl`), then the graph — every declared extent divisor is nonzero,
/// every pass's entry point exists as a fragment entry, every slot is
/// written before it is read (the sequence-index check), no pass reads
/// its own output, every uniform window covers the uniform block its
/// entry point declares, the graph's per-dispatch cost stays inside the
/// executor's budget (the render passes it encodes and the uniform bytes
/// it stages, both summed over the whole pass list — a per-pass repeat
/// ceiling alone leaves their product unbounded), the final pass writes
/// a dispatch binding
/// declared `Full` extent (the program's result texture, whose size is
/// the reference every other extent scales from) — and finally wgpu
/// shader-module + pipeline creation under a validation error scope
/// (`pipeline creation failed`), so a bad-but-parseable program replies
/// `Err` instead of crashing the substrate. A rejected register
/// consumes no id.
///
/// The shader contract: the substrate owns the vertex stage of a
/// fragment pass (a fullscreen triangle), so such a pass's entry point
/// may take `@location(0) uv: vec2<f32>` — `(0, 0)`
/// top-left to `(1, 1)` bottom-right, texture convention — and returns
/// `@location(0) vec4<f32>`. Its uniform window binds at
/// `@group(0) @binding(0) var<uniform>`; its input slots bind in
/// declaration order at group 1 as texture / sampler pairs — input `n`
/// is `@binding(2 * n)` (`texture_2d<f32>`) plus `@binding(2 * n + 1)`
/// (`sampler`). Blendable-format outputs (`Rgba8`, `R8`) alpha-blend
/// onto the target; `R32Float` outputs replace it (core WebGPU cannot
/// blend 32-bit floats). The first write a dispatch makes to each
/// output slot clears it to transparent black; later writes — a
/// repeat's iterations, a second pass onto the same slot — load the
/// existing content.
///
/// A `PassStage::Draw` pass (ADR-0171) replaces the fullscreen vertex
/// stage with an authored one over a bound geometry and states its own
/// color load semantic instead of following the clear-on-first-write
/// rule. `geometries` declares the geometry slots a dispatch fills by
/// id, and `depth_transients` the pooled `Depth32Float` targets draw
/// passes clear and test against — declared as extents alone, since
/// their format is fixed. Both lists are empty for a fragment-only
/// program, which registers exactly as it did before this arm existed.
///
/// Reply: `ProgramRegisterResult`; `program_id` is session-scoped,
/// assigned like texture and instrument ids. Desktop-only — the
/// headless chassis replies `Err` (fail-fast, ADR-0105), and a register
/// before the render GPU boots (desktop: before the first window
/// attaches) replies `Err` rather than parking.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.render.program.register")]
pub struct ProgramRegister {
    pub wgsl: String,
    pub bindings: Vec<SlotSpec>,
    pub transients: Vec<SlotSpec>,
    /// Geometry slots draw passes bind, filled by id per dispatch
    /// (ADR-0171). Empty for a fragment-only program.
    pub geometries: Vec<GeometrySlotSpec>,
    /// Pooled `Depth32Float` targets draw passes clear and test
    /// against, declared as extents against the reference (ADR-0171).
    /// Empty for a fragment-only program.
    pub depth_transients: Vec<SlotExtent>,
    pub passes: Vec<ProgramPass>,
}

/// Reply to `ProgramRegister`. `Ok` carries the assigned `program_id`
/// — thread it into `ProgramDispatch.program_id` and
/// `ProgramDestroy.program_id`. `Err` carries a human-readable reason
/// prefixed by its validation class: `invalid wgsl` (naga parse or
/// validation), a graph-check message naming the offending pass and
/// slot, or `pipeline creation failed` (a wgpu validation error caught
/// by the register's error scope).
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.render.program.register_result")]
pub enum ProgramRegisterResult {
    Ok { program_id: u32 },
    Err { reason: String },
}

/// `aether.render.program.dispatch` — execute a registered program once
/// at the next frame record (ADR-0170). Fire-and-forget, immediate-mode
/// like every draw kind: register once, dispatch per repaint or per
/// frame with fresh uniforms. The program's passes record into the
/// frame's command encoder *before* the world / material / overlay
/// passes, so those passes sample the program's freshly written outputs
/// in the same frame. The written outputs persist in their writable
/// registry textures between dispatches — a program is re-executed only
/// when dispatched again.
///
/// `bindings` names one registry texture id per declared
/// `ProgramRegister.bindings` slot, in order; `geometries` names one
/// registry geometry id per declared `ProgramRegister.geometries` slot
/// (ADR-0171), also in order; `uniforms` is one byte
/// blob the passes window into (each window is copied into an aligned
/// staging arrangement, so windows need no alignment of their own —
/// pack them tight). Runtime mismatches — an unknown `program_id`, a
/// wrong binding or geometry count, an unknown texture or geometry id,
/// a binding whose format,
/// size, or writability disagrees with the declared graph, a geometry
/// whose layout disagrees with its declared slot, a uniform
/// window past the blob's end, or a pass whose input and output resolve
/// to the same texture — warn-drop the dispatch naming the program,
/// pass, and binding in the render actor's log ring, the same
/// convention as an unknown texture id in `draw_textured_quads`. The
/// headless chassis absorbs it (no-op).
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.render.program.dispatch")]
pub struct ProgramDispatch {
    pub program_id: u32,
    pub bindings: Vec<u32>,
    /// One geometry id per declared `ProgramRegister.geometries` slot,
    /// in order. Empty for a fragment-only program.
    pub geometries: Vec<u32>,
    #[serde(with = "aether_data::bytes")]
    pub uniforms: Vec<u8>,
}

/// `aether.render.program.destroy` — release a registered program from
/// the render cap's session-scoped program registry, mirroring
/// `destroy_texture`. Fire-and-forget; an unknown `program_id` logs and
/// drops. Dropping the entry releases the program's compiled pipelines;
/// pooled transient textures stay in the shared pool for other
/// programs. The headless chassis absorbs it (no-op).
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.render.program.destroy")]
pub struct ProgramDestroy {
    pub program_id: u32,
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
