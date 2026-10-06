//! The `aether.render` cap's drawing + texture mail kinds (ADR-0121).
//!
//! These ride the always-on (marker-only `render`) region of the render
//! module, so a wasm guest on the `render` feature sees the kind types
//! for typed `ctx.send::<RenderCapability>(&kind)` addressing
//! without the `render-runtime` GPU stack. The capture-request kinds
//! (`CaptureFrame` / `CaptureFrameResult` / `SimilarityCheck`) and the
//! `FrameCheck` verification family stay in `aether-kinds`: the former
//! are consumed by `aether-mcp` and the latter by the substrate core, so
//! moving them here would close a dependency cycle (ADR-0121). The
//! `QuadSpace` / `QuadScale` projection types also stay central — the
//! `aether.text.draw` kind in `aether-kinds` consumes them — so the quad
//! draw kinds below import them from there.

use aether_actor::HeldReply;
use aether_data::{Blob, ErasedActorPath, MailId};
use aether_kinds::{ClipRect, QuadSpace};
use aether_math::{Rgb, Rgba};
use bytemuck::{Pod, Zeroable};
use serde::{Deserialize, Serialize};

/// Chassis-internal frame-request kind (ADR-0161 §Decision 1). A pumping
/// driver mails one each frame after the advance chain settles;
/// `RenderCapability::on_frame` records the frame and resolves any pending
/// capture. `replay_cache_when_idle` carries the issue 847 semantic —
/// harness captures replay the last committed accumulators when the producer
/// was idle; desktop always commits current. Not addressed by wasm guests —
/// the pumping chassis driver is its sole sender. Engine-only mail (ADR-0233):
/// the driver pushes it from host code through the mailer.
#[aether_data::kind(name = "aether.render.frame", default, eq, engine_only)]
pub struct Frame {
    pub replay_cache_when_idle: bool,
    /// Engine window targets dirtied by this application turn, each the
    /// window's canonical actor path. The render actor deduplicates the list
    /// before presenting; an empty list is the explicit surfaceless harness
    /// path.
    pub windows: Vec<ErasedActorPath>,
}

/// Chassis-internal pre-mail-settlement notice (ADR-0161 §Decision 4). One
/// arrives per capture pre-mail whose causal chain has settled;
/// `RenderCapability::on_pre_settled` decrements the pending capture's
/// `pre_remaining`. Wire-identical to `aether.trace.settled` (a single
/// `MailId` field) so the settlement registry's notice-mail bridge
/// (`ctx.subscribe_settlement`) delivers it directly. Chassis-internal —
/// the settlement bridge is its sole sender. Engine-only mail (ADR-0233): the
/// settlement registry pushes it from host code through the mailer.
#[aether_data::kind(name = "aether.render.pre_settled", copy, eq, engine_only)]
pub struct PreSettled {
    pub mail_id: MailId,
}

/// Chassis-internal window-occlusion signal (ADR-0161 §Decision 4). A
/// pumping driver forwards `WindowEvent::Occluded`;
/// `RenderCapability::on_occluded` fail-fasts a pending capture when the
/// window becomes occluded (relocating `fail_capture_if_occluded` into the
/// actor, issue 1317). Chassis-internal — the driver is its sole sender.
/// Engine-only mail (ADR-0233): the desktop driver pushes it from host code
/// through the mailer.
#[aether_data::kind(name = "aether.render.occluded", eq, engine_only)]
pub struct Occluded {
    pub window: ErasedActorPath,
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
#[aether_data::kind(name = "aether.draw_triangle", pod, default, partial_eq)]
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
#[aether_data::kind(name = "aether.view_projection", pod, default, partial_eq)]
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
    /// Two bytes per pixel, one little-endian `f16` — the data-plane
    /// format for a quantity a filtering sampler has to read. Core
    /// WebGPU filters 16-bit floats, so a program pass may take a
    /// fractional coordinate through one and be handed the interpolation
    /// rather than computing it from four point fetches; what it gives
    /// up against [`Self::R32Float`] is mantissa, about eleven bits of
    /// it, which is finer than any eight-bit target the plane resolves
    /// into. A label or a texel index does not belong in it — that is
    /// what the 32-bit plane is for.
    R16Float,
    /// Eight bytes per pixel, four little-endian `f16` channels — the
    /// filterable data-plane format for several quantities that travel
    /// through the same authored-program operation. Each channel keeps
    /// the precision and filtering semantics of [`Self::R16Float`]; the
    /// wider texel lets one pass carry independent planes without
    /// quantizing them into color.
    Rgba16Float,
}

impl TextureFormat {
    #[must_use]
    pub const fn bytes_per_pixel(self) -> usize {
        match self {
            Self::Rgba8 | Self::R32Float => 4,
            Self::R16Float => 2,
            Self::Rgba16Float => 8,
            Self::R8 => 1,
        }
    }

    /// Whether core WebGPU can linear-filter this format — equivalently,
    /// whether its realized texture binds through the shared filtering
    /// layout the color material / overlay pipelines are built against.
    #[must_use]
    pub const fn filterable(self) -> bool {
        match self {
            Self::Rgba8 | Self::R8 | Self::R16Float | Self::Rgba16Float => true,
            Self::R32Float => false,
        }
    }

    /// Whether a multisampled texture of this format can be resolved to
    /// a single-sample one, which is what a pass reading a
    /// [`Samples::Four`] transient reads. Core WebGPU resolves every
    /// format here but `R32Float`, which it can multisample and cannot
    /// resolve.
    #[must_use]
    pub const fn resolvable(self) -> bool {
        match self {
            Self::Rgba8 | Self::R8 | Self::R16Float | Self::Rgba16Float => true,
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
/// next frame record. Reply: `CreateTextureResult`. The headless chassis
/// composes no render actor. `pixels` arrives as a `Blob` and is staged as
/// received, without a copy.
#[aether_data::kind(name = "aether.render.create_texture")]
pub struct CreateTexture {
    pub width: u32,
    pub height: u32,
    pub format: TextureFormat,
    pub sampling: TextureSampling,
    pub usage: TextureUsage,
    pub pixels: Blob,
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
#[aether_data::kind(name = "aether.render.create_texture_result")]
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
#[aether_data::kind(name = "aether.render.update_texture")]
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
/// texture, texture array or volume texture from the render cap's
/// session-scoped texture registry; the three share one id space, so this
/// is the destroy path of each. Fire-and-forget; an unknown `texture_id` or the reserved
/// internal white-texture id logs and drops. Dropping the registry entry
/// releases staged pixels and any realized GPU resources.
#[aether_data::kind(name = "aether.render.destroy_texture")]
pub struct DestroyTexture {
    pub texture_id: u32,
}

/// `aether.render.create_texture_array` — register an array texture
/// (ADR-0246 decision 6): `layers` square layers of `side` texels in
/// `format`, the resource a `SlotShape::TextureArray` program binding
/// takes. Side and layer count are fixed at creation; growing either
/// means creating a new array.
///
/// `mips` says which levels the array has. `Mips::Base` is the base
/// level alone. `Mips::Chain` is `floor(log2(side)) + 1` levels, level
/// `n` having side `max(1, side >> n)`. The engine generates no level:
/// every one is supplied by `WriteTextureLayer`.
///
/// The array is created with no pixels. A layer that was never written
/// reads as zero in every channel. It is read linear when `format` can
/// be filtered and nearest when it cannot (`R32Float`).
///
/// The id comes from the sequence `CreateTexture` draws from, so one
/// `texture_id` names a texture or an array and never both, and
/// `DestroyTexture` releases either. The layer ceiling is the render
/// device's, so creation needs a device. A create sent before the
/// render device exists (desktop: before the first window attaches) is
/// answered once the device is up. Its own chain settles first, so
/// nothing waits on a window: a `send_mail` over MCP returns with no
/// reply for it, and one sent after a window is listed is answered
/// inside the call. Reply: `CreateTextureArrayResult`.
#[aether_data::kind(name = "aether.render.create_texture_array")]
pub struct CreateTextureArray {
    pub format: TextureFormat,
    pub side: u32,
    pub layers: u32,
    pub mips: Mips,
}

/// Reply to `CreateTextureArray`. `Ok` carries the assigned
/// `texture_id` — thread it into `WriteTextureLayer.texture_id` and a
/// `TextureArray` entry of `ProgramDispatch.bindings`. `Err` carries a
/// human-readable reason, one per class: a zero `side`, zero `layers`, a
/// `side` past the device's `max_texture_dimension_2d`, `layers` past
/// the device's `max_texture_array_layers`, or a layer whose byte size
/// overflows. A refused create consumes no id.
#[aether_data::kind(name = "aether.render.create_texture_array_result")]
pub enum CreateTextureArrayResult {
    Ok { texture_id: u32 },
    Err { error: String },
}

impl HeldReply for CreateTextureArrayResult {
    fn unanswered() -> Self {
        Self::Err { error: "render capability closed before the texture array request was answered".into() }
    }
}

/// `aether.render.write_texture_layer` — replace the contents of one
/// layer of a texture array, in place (ADR-0246 decision 6).
///
/// `pixels` carries every level the array has for that layer, base
/// level first, each level row-major and top-down, with nothing between
/// levels. Its length is the sum over the levels of
/// `level_side * level_side * format.bytes_per_pixel()`: one level for a
/// `Mips::Base` array, the whole chain for a `Mips::Chain` one. A write
/// is all of a layer's levels or none of them.
///
/// Fire-and-forget. An unknown `texture_id`, an id that names a plain
/// texture or a volume, a `layer` at or past the array's layer count, pixel bytes
/// that are not resident in this process, or a wrong `pixels` length
/// logs a warning and leaves the layer as it was. The engine keeps the
/// pixels it is given, so a written layer survives a render device
/// replacement under the same id.
#[aether_data::kind(name = "aether.render.write_texture_layer")]
pub struct WriteTextureLayer {
    pub texture_id: u32,
    pub layer: u32,
    pub pixels: Blob,
}

/// `aether.render.create_texture_volume` — register a volume texture
/// (ADR-0246 decision 6): `width` by `height` by `depth` texels in
/// `format`, the resource a `SlotShape::TextureVolume` program binding
/// takes.
///
/// `pixels` is the whole volume: `depth` slices, slice 0 first, each
/// slice row-major and top-down, with nothing between slices. Its length
/// is exactly `width * height * depth * format.bytes_per_pixel()` bytes.
/// Texture coordinate `w = 0` is the near face of slice 0, so slice `k`
/// is centred at `w = (k + 0.5) / depth`.
///
/// A volume is immutable: no kind writes it after creation, and new
/// contents are a new volume. It has one mip level. It is read linear
/// when `format` can be filtered and nearest when it cannot (`R32Float`),
/// and a linear read interpolates on all three axes, between slices as
/// between texels.
///
/// The id comes from the sequence `CreateTexture` draws from, so one
/// `texture_id` names a texture, an array or a volume and never two of
/// them, and `DestroyTexture` releases any of the three. Each dimension
/// is checked against the three-dimensional limit every render device is
/// requested at, so the create needs no device: it is answered inside the
/// call, before the render device exists as after, and the wgpu texture
/// is realized at the first dispatch that binds the volume. The engine
/// keeps the pixels it is given, so a volume survives a render device
/// replacement under the same id. Reply: `CreateTextureVolumeResult`.
#[aether_data::kind(name = "aether.render.create_texture_volume")]
pub struct CreateTextureVolume {
    pub format: TextureFormat,
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    pub pixels: Blob,
}

/// Reply to `CreateTextureVolume`. `Ok` carries the assigned
/// `texture_id` — thread it into a `TextureVolume` entry of
/// `ProgramDispatch.bindings`. `Err` carries a human-readable reason, one
/// per class: pixel bytes that are not resident in this process, a zero
/// dimension, a dimension past `max_texture_dimension_3d` (named against
/// the limit), a volume whose byte size overflows, or a `pixels` length
/// that is not the volume's byte count. A refused create consumes no id.
#[aether_data::kind(name = "aether.render.create_texture_volume_result")]
pub enum CreateTextureVolumeResult {
    Ok { texture_id: u32 },
    Err { error: String },
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
/// `CreateGeometryResult`. The headless chassis composes no render actor.
/// `vertices` and `indices` arrive as `Blob`s and are staged as received,
/// without a copy.
#[aether_data::kind(name = "aether.render.create_geometry")]
pub struct CreateGeometry {
    pub layout: Vec<VertexAttribute>,
    pub vertices: Blob,
    pub indices: Blob,
}

/// Reply to `CreateGeometry`. `Ok` carries the assigned `geometry_id` —
/// thread it into `UpdateGeometry.geometry_id` and
/// `DestroyGeometry.geometry_id` (and the draw-pass geometry binding,
/// ADR-0171). `Err` carries a human-readable reason naming its
/// validation class: an empty layout, a vertex byte length that does
/// not divide by the layout stride, an index byte length that does not
/// divide by four, or an index outside the vertex count.
#[aether_data::kind(name = "aether.render.create_geometry_result")]
pub enum CreateGeometryResult {
    Ok { geometry_id: u32 },
    Err { error: String },
}

/// `aether.render.update_geometry` — replace a previously-created
/// geometry's vertex and index bytes in place (ADR-0171). The layout is
/// fixed at create; the replacement is validated against it under the
/// same rules as `CreateGeometry` and swaps wholesale (the byte lengths
/// may change). Fire-and-forget; an unknown `geometry_id` or an invalid
/// replacement logs and drops, leaving the previous content staged. The
/// staged bytes update immediately; the GPU buffers re-realize at the
/// next GPU use. While a draw set names the geometry (ADR-0246 decision
/// 2) the replacement may change the vertex contents only: one with a
/// different vertex count, or with indices that are not byte-for-byte
/// the ones staged, logs and drops the same way, because the set's
/// index ranges were checked against the sizes it was made with. Once
/// no set names the geometry it may be resized again. Nothing is
/// replied either way; a refusal is a warning in the render actor's
/// log. Per-frame updates are for view-dependent geometry that
/// is small by nature (the ink ribbons) — a deforming mesh poses
/// through the uniform blob instead. `vertices` and `indices` arrive as
/// `Blob`s and are staged as received, without a copy.
#[aether_data::kind(name = "aether.render.update_geometry")]
pub struct UpdateGeometry {
    pub geometry_id: u32,
    pub vertices: Blob,
    pub indices: Blob,
}

/// `aether.render.destroy_geometry` — release a previously-created
/// geometry from the render cap's session-scoped geometry registry,
/// mirroring `destroy_texture`. Fire-and-forget; an unknown
/// `geometry_id` logs and drops. The id stops answering at once: a
/// later update, dispatch, or draw naming it finds nothing. The staged
/// bytes and any realized GPU buffers are released with it, unless a
/// draw set names the geometry (ADR-0246 decision 2), in which case they
/// live until the last such set is destroyed or patched off it, and the
/// set goes on drawing them.
#[aether_data::kind(name = "aether.render.destroy_geometry")]
pub struct DestroyGeometry {
    pub geometry_id: u32,
}

/// `aether.render.create_instances` — register a buffer of instance
/// records in the render cap's session-scoped instance registry
/// (ADR-0246 decision 3). A record is one instance's attributes, packed
/// as `layout` declares with the stride [`vertex_stride_bytes`] gives;
/// the engine does not interpret it. `capacity` counts records, never
/// bytes, and is fixed for the buffer's life. `records` is the initial
/// contents from record 0: it may hold fewer records than the capacity,
/// and the rest start zeroed. The cap validates before it assigns an id
/// — an empty layout, a zero capacity, a capacity whose byte size
/// exceeds the device's buffer limit, record bytes that are not resident
/// in this process, a record length off the layout stride, or more
/// initial records than the capacity each refuse with their own reason —
/// and copies the bytes into a buffer it owns, so the records survive a
/// render device replacement. Reply: `CreateInstancesResult`.
#[aether_data::kind(name = "aether.render.create_instances")]
pub struct CreateInstances {
    pub layout: Vec<VertexAttribute>,
    pub capacity: u32,
    pub records: Blob,
}

/// Reply to `CreateInstances`. `Ok` carries the assigned `instances_id`
/// — thread it into `UpdateInstances.instances_id` and
/// `DestroyInstances.instances_id`. `Err` carries a human-readable
/// reason naming its validation class, and a refused create consumes no
/// id.
#[aether_data::kind(name = "aether.render.create_instances_result")]
pub enum CreateInstancesResult {
    Ok { instances_id: u32 },
    Err { error: String },
}

/// `aether.render.update_instances` — overwrite a run of records in a
/// previously-created instance buffer, in place (ADR-0246 decision 3).
/// `first` is the index of the first record written, counted in
/// records; `records` holds whole records under the layout fixed at
/// create, and the run must end inside the capacity. The buffer's
/// capacity and identity never change. Fire-and-forget: an unknown
/// `instances_id`, record bytes that are not resident in this process, a
/// length off the layout stride, or a run past the capacity logs and
/// drops, leaving every record as it was. An empty `records` is accepted
/// and writes nothing.
#[aether_data::kind(name = "aether.render.update_instances")]
pub struct UpdateInstances {
    pub instances_id: u32,
    pub first: u32,
    pub records: Blob,
}

/// `aether.render.destroy_instances` — release a previously-created
/// instance buffer, mirroring `destroy_geometry`. Fire-and-forget; an
/// unknown `instances_id` logs and drops. The id stops answering at
/// once, so a later update or draw naming it finds nothing, and it is
/// never handed out again. The records and the GPU buffer are released
/// with it, unless a draw set names the buffer (ADR-0246 decision 2), in
/// which case they live, with the contents they had, until the last such
/// set is destroyed or patched off it.
#[aether_data::kind(name = "aether.render.destroy_instances")]
pub struct DestroyInstances {
    pub instances_id: u32,
}

/// A run of a geometry's indices: `count` indices starting at index
/// `first`. Both count indices, never bytes and never triangles. A
/// `count` of zero is inside every geometry and draws nothing.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq)]
pub struct IndexRange {
    pub first: u32,
    pub count: u32,
}

/// A run of an instance buffer's records: `count` records starting at
/// record `first`. Both count records, never bytes. A `count` of zero is
/// inside every buffer and draws nothing.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq)]
pub struct InstanceRange {
    pub first: u32,
    pub count: u32,
}

/// One draw of a draw set (ADR-0246 decision 1): the `indices` run of
/// geometry `geometry_id`, drawn once per record of the `instances` run
/// of instance buffer `instances_id`. A draw carries no texture and no
/// per-draw constant; what varies between draws rides in the instance
/// records. A draw whose index or instance count is zero is valid and
/// draws nothing, which blanks one entry of a set without moving the
/// others.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq)]
pub struct DrawSpec {
    pub geometry_id: u32,
    pub indices: IndexRange,
    pub instances_id: u32,
    pub instances: InstanceRange,
}

/// `aether.render.create_draw_set` — make a retained list of draws in
/// the render cap's session-scoped draw-set registry (ADR-0246 decisions
/// 1 and 2). `vertex_layout` and `instance_layout` are the layouts every
/// draw's geometry and instance buffer must have been created with; a
/// set is checked against layouts, not against a program, so any pass
/// with the same two layouts may draw it. `draws` may be empty.
///
/// Every check runs here, before an id is assigned, and the first
/// failure refuses the whole mail: an empty vertex layout or an empty
/// instance layout, and then per draw, in order, an unknown
/// `geometry_id`, a geometry whose layout is not `vertex_layout`, an
/// index range that ends past the geometry's index count, an unknown
/// `instances_id`, an instance buffer whose layout is not
/// `instance_layout`, or an instance range that ends past the buffer's
/// capacity. A range's end is `first + count`, summed without wrapping.
/// A per-draw reason starts `draw N:`, with `N` the draw's position in
/// `draws`.
///
/// An accepted set holds the geometries and instance buffers it names:
/// destroying one retires its id while its bytes and GPU buffers live
/// until no set names it, and `UpdateGeometry` may not change the vertex
/// count or the indices of a geometry a set names. Reply:
/// `CreateDrawSetResult`.
#[aether_data::kind(name = "aether.render.create_draw_set")]
pub struct CreateDrawSet {
    pub vertex_layout: Vec<VertexAttribute>,
    pub instance_layout: Vec<VertexAttribute>,
    pub draws: Vec<DrawSpec>,
}

/// Reply to `CreateDrawSet`. `Ok` carries the assigned `draw_set_id` —
/// thread it into `UpdateDrawSet.draw_set_id` and
/// `DestroyDrawSet.draw_set_id`. `Err` carries a human-readable reason
/// naming its class and, for a per-draw class, the draw. A refused
/// create consumes no id and holds no buffer.
#[aether_data::kind(name = "aether.render.create_draw_set_result")]
pub enum CreateDrawSetResult {
    Ok { draw_set_id: u32 },
    Err { error: String },
}

/// `aether.render.update_draw_set` — patch a draw set in place (ADR-0246
/// decision 1). `draws` is written over the set's entries from position
/// `first`, counted in draws. `first` may be at most the set's length,
/// and a run that passes the end extends the set, so appending is a
/// patch with `first` equal to the length. An empty `draws` truncates
/// the set to its first `first` entries. No entry moves unless the
/// sender moves it.
///
/// Every new draw is checked as `CreateDrawSet` checks it, against the
/// layouts fixed at create, and the first failure refuses the whole
/// mail, as do an unknown `draw_set_id` and a `first` past the set's
/// length. A per-draw reason starts `draw N:`, with `N` the draw's
/// position in this mail's `draws`. A refused patch leaves the set and
/// what it holds exactly as they were. An accepted patch lets go of each
/// buffer no draw of the set names any more. Reply:
/// `UpdateDrawSetResult`.
#[aether_data::kind(name = "aether.render.update_draw_set")]
pub struct UpdateDrawSet {
    pub draw_set_id: u32,
    pub first: u32,
    pub draws: Vec<DrawSpec>,
}

/// Reply to `UpdateDrawSet`. `Ok` means the whole patch was applied;
/// `Err` carries a human-readable reason naming its class and means none
/// of it was.
#[aether_data::kind(name = "aether.render.update_draw_set_result")]
pub enum UpdateDrawSetResult {
    Ok,
    Err { error: String },
}

/// `aether.render.destroy_draw_set` — release a draw set and let go of
/// every geometry and instance buffer it holds. Fire-and-forget; an
/// unknown `draw_set_id` logs and drops. A buffer that was destroyed
/// while the set named it is released for good when its last set goes.
/// The released id is never handed out again.
#[aether_data::kind(name = "aether.render.destroy_draw_set")]
pub struct DestroyDrawSet {
    pub draw_set_id: u32,
}

/// How a textured composite lays its source over what is already in
/// the target.
///
/// The distinction is what the source's colour channels already carry.
/// `Straight` is an ordinary image: colour and coverage are
/// independent, so the blend weights the colour by the alpha it is
/// handed — `src.rgb * src.a + dst.rgb * (1 - src.a)`. `Premultiplied`
/// is an image whose colour has already been scaled by its own
/// coverage, so the blend adds it as it stands — `src.rgb + dst.rgb *
/// (1 - src.a)`.
///
/// The second exists because render-to-texture produces it whether or
/// not anyone asked. A fragment pass writing an `Rgba8` target
/// alpha-blends onto a transparent clear, so writing `(colour, a)`
/// stores `(colour * a, a)` — every partially covered texel of a
/// program's output is premultiplied by construction. Compositing that
/// as `Straight` weights it by its coverage a second time and squares
/// it: a half-covered texel lays down a quarter of its colour. On the
/// drawing that found this (ADR-0172's ink, one- to two-pixel strokes,
/// so almost every inked pixel is a partial one) it cost about 39% of
/// the ink's weight.
///
/// Straight alpha cannot be recovered after the fact, either — the
/// un-premultiply wants a colour in the texels coverage did not reach,
/// and writing one there multiplies it by its own zero alpha on the way
/// in. So the choice belongs at the composite, which is here.
///
/// Which verbs carry it follows from that: a `blend` field appears
/// exactly where the caller composites an *image of its own* —
/// [`DrawTexturedQuads`] and [`DrawMaterialTextured`] — because only the
/// caller that produced those texels knows whether they were already
/// scaled by their coverage. A verb whose colours the substrate itself
/// rasterizes ([`DrawShapes`], [`DrawScreenTriangles`],
/// [`DrawMaterialCoverage`]) carries no `blend`: the fragment stage
/// knows what it wrote, so there is nothing for the caller to declare.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum QuadBlend {
    /// Colour and coverage independent. Every sender before ADR-0172
    /// meant this, and it stays the default.
    #[default]
    Straight,
    /// Colour already scaled by its own coverage — what a program's
    /// `Rgba8` output always is.
    Premultiplied,
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
/// selects and under the blend `blend` selects. Accumulated per frame
/// with the same immediate-mode
/// contract as `aether.draw_triangle`: send it every frame the quads
/// should appear, or they vanish next frame. `texture_id` is a
/// registry id from a prior `CreateTexture`; an unknown id warn-drops
/// the batch. Fire-and-forget; no reply.
#[aether_data::kind(name = "aether.render.draw_textured_quads")]
pub struct DrawTexturedQuads {
    pub texture_id: u32,
    pub space: QuadSpace,
    /// Optional framebuffer-pixel scissor applied to this batch. `None`
    /// leaves the draw unclipped.
    pub clip: Option<ClipRect>,
    /// How the sampled texel lays over the target. `Straight` for an
    /// ordinary uploaded image, `Premultiplied` for a texture a render
    /// program wrote.
    pub blend: QuadBlend,
    pub quads: Vec<TexturedQuad>,
}

/// The stroke a [`Shape`] draws just inside its edge: `width_pixels`
/// wide, in the unit the batch's `space` selects, in linear RGBA `color`.
/// It lies over the fill, so a translucent stroke shows the fill through
/// it. Not a kind on its own — only addressable inside `Shape.stroke`.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct ShapeStroke {
    pub width_pixels: f32,
    pub color: Rgba,
}

/// The shadow a [`Shape`] casts: the same rounded box moved by `offset`
/// (`[x, y]`, y down, in the batch's unit) with its edge feathered over
/// `blur_pixels` each side, in linear RGBA `color`. It lies under the
/// fill and the stroke, so with an opaque fill only the part that
/// escapes the box is seen; with no fill it is a soft halo. Not a kind on
/// its own — only addressable inside `Shape.shadow`.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct ShapeShadow {
    pub blur_pixels: f32,
    pub offset: [f32; 2],
    pub color: Rgba,
}

/// The image a [`Shape`] draws inside its fill: the sub-rect
/// `(u0, v0)`–`(u1, v1)` of the registered texture `texture_id`
/// (`0,0` top-left to `1,1` bottom-right), stretched across the shape's
/// box and sampled only where the fill covers — so the corner radius, the
/// circle, and the anti-aliased edge apply to the image exactly as they
/// apply to a flat colour. A rounded avatar, a thumbnail at the panel's
/// radius, and a circular icon are this and nothing else.
///
/// The shape's `fill` multiplies the sampled texel the way
/// [`TexturedQuad`]'s `tint` does — `Rgba::WHITE` draws the image
/// unmodified, and a `Shape` with a `texture` but no `fill` draws no image
/// at all, because there is no fill coverage to sample into. `blend` says
/// whether the texel's colour was already scaled by its own coverage,
/// exactly as it does on [`DrawTexturedQuads`]. An unknown, unrealized, or
/// non-filterable `texture_id` warn-drops the shapes that name it.
///
/// Not a kind on its own — only addressable inside `Shape.texture`.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct ShapeTexture {
    pub texture_id: u32,
    pub u0: f32,
    pub v0: f32,
    pub u1: f32,
    pub v1: f32,
    pub blend: QuadBlend,
}

/// One shape in a `DrawShapes` batch (ADR-0213): an axis-aligned box —
/// `(x, y)` the top-left corner, `(width, height)` the size, in the unit
/// the batch's `space` selects — with its corners rounded by
/// `corner_radius`, filled with `fill` when given, stroked inside its edge
/// by `stroke` when given, and shadowed by `shadow` when given. The three
/// parts compose shadow under fill under stroke, every edge anti-aliased.
/// A radius at or above half the shorter side is a circle (or a stadium);
/// a stroke with no fill is a ring; a shadow with neither is a soft halo;
/// a `corner_radius` of `0.0` with a `fill` alone is the flat rect the
/// retired `draw_solid_quads` drew. A `texture` draws an image inside the
/// fill's coverage instead of a flat colour, so the same box is also a
/// rounded avatar or a circular icon.
/// Not a kind on its own — only addressable inside `DrawShapes.shapes`.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Shape {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub corner_radius: f32,
    pub fill: Option<Rgba>,
    pub stroke: Option<ShapeStroke>,
    pub shadow: Option<ShapeShadow>,
    pub texture: Option<ShapeTexture>,
}

/// `aether.render.draw_shapes` — draw a batch of rounded, stroked,
/// shadowed boxes (ADR-0213) in the projection `space` selects, evaluated
/// as a signed distance field on the GPU. Accumulated per frame with the
/// same immediate-mode contract as `aether.draw_triangle`: send it every
/// frame the shapes should appear, or they vanish next frame. Rides the
/// overlay pass at the same painter position and under the same scissor
/// as the quad batches, through its own pipeline — one more overlay
/// draw, not a pass and not a layer. A batch may mix untextured shapes
/// with shapes naming different textures; the record path splits it into
/// draws at each texture transition, so painter order inside the batch is
/// the order the shapes were listed in. The vocabulary is fixed and
/// substrate-owned: callers supply parameters, never WGSL.
/// Fire-and-forget; no reply.
#[aether_data::kind(name = "aether.render.draw_shapes")]
pub struct DrawShapes {
    pub space: QuadSpace,
    /// Optional framebuffer-pixel scissor applied to this batch. `None`
    /// leaves the draw unclipped.
    pub clip: Option<ClipRect>,
    pub shapes: Vec<Shape>,
}

/// One corner of a [`ScreenTriangle`]. `(x, y)` is a position in the
/// unit the batch's `space` selects — window pixels with the top-left
/// origin and y pointing down for `Screen`, pixel offsets from the
/// anchor for `World` — the same convention a quad's corners address
/// in. `color` is a linear
/// RGBA value whose alpha scales the blend; the three corner colors
/// interpolate across the face, so a single flat fill repeats one color
/// and a gradient states three. Not a kind on its own — only addressable
/// inside `ScreenTriangle`.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct ScreenVertex {
    pub x: f32,
    pub y: f32,
    pub color: Rgba,
}

/// One triangle in a `DrawScreenTriangles` batch — three corners at any
/// orientation, in the unit the batch's `space` selects. Either winding
/// draws (the overlay pipeline does not cull). Not a kind on its own —
/// only addressable inside `DrawScreenTriangles.triangles`.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct ScreenTriangle {
    pub a: ScreenVertex,
    pub b: ScreenVertex,
    pub c: ScreenVertex,
}

/// `aether.render.draw_screen_triangles` — draw a batch of
/// alpha-blended triangles whose corners are pixels. The aspect-correct
/// 2D counterpart to `aether.draw_triangle`: coordinates are pixels
/// rather than world units, so a rotated shape keeps its proportions on
/// any window. Where `draw_shapes` fills rounded axis-aligned boxes,
/// this fills arbitrary geometry — ribbons at an angle, gauges, graph
/// edges, a caret.
///
/// `space` selects the projection exactly as it does on
/// [`DrawTexturedQuads`] and [`DrawShapes`]: `Screen` puts the corners
/// at absolute window pixels, so flat content needs no camera at all;
/// `World` reads them as pixel offsets from a projected anchor, so a
/// gauge or a graph edge can hang off a point in the world the way a
/// label already can.
///
/// Rides the same overlay pass and pipeline as the quad batches
/// (ADR-0105), in the second pass after the world pass: alpha-blended,
/// painter's order within the frame, no depth test. Accumulated per
/// frame with the same immediate-mode contract as `aether.draw_triangle`
/// — resend every frame the triangles should appear, or they vanish next
/// frame. Fire-and-forget; no reply.
#[aether_data::kind(name = "aether.render.draw_screen_triangles")]
pub struct DrawScreenTriangles {
    pub space: QuadSpace,
    /// Optional framebuffer-pixel scissor applied to this batch. `None`
    /// leaves the draw unclipped.
    pub clip: Option<ClipRect>,
    pub triangles: Vec<ScreenTriangle>,
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
#[aether_data::kind(name = "aether.render.material.textured")]
pub struct DrawMaterialTextured {
    pub texture_id: u32,
    /// How the sampled texel lays over the target, exactly as it means
    /// on [`DrawTexturedQuads`].
    pub blend: QuadBlend,
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
#[aether_data::kind(name = "aether.render.material.coverage")]
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

/// How many samples each texel of a transient or a depth transient
/// holds (ADR-0246 decision 7).
///
/// A pass that writes a `Four` transient rasterizes at four samples per
/// texel, so the edge of a triangle covers a texel by quarters. A pass
/// that reads it reads the resolved image: one value per texel, the
/// average of its four samples. The executor keeps the four-sample
/// texture between passes and resolves it once after the last pass to
/// write it before each pass that reads it, so a `Four` transient some
/// pass reads costs a four-sample texture and a single-sample one, and
/// one that no pass reads costs the four-sample texture alone.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq)]
pub enum Samples {
    /// One sample per texel.
    One,
    /// Four samples per texel.
    Four,
}

impl Samples {
    /// The sample count a texture or a pipeline is created with.
    #[must_use]
    pub const fn count(self) -> u32 {
        match self {
            Self::One => 1,
            Self::Four => 4,
        }
    }
}

/// What one program binding takes (ADR-0246 decision 5): a texture
/// sized from the program's output, or one of a size of its own.
///
/// Only a `Target` has an extent, so only a `Target` can be a pass
/// output: the executor has to know the size of what it attaches. The
/// other three shapes are read-only, take a texture of any size, and are
/// sampled by the fragment stage of any pass, by the authored vertex
/// stage of a draw pass, and by a compute pass.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq)]
pub enum SlotShape {
    /// A texture the size of the program's reference extent scaled by
    /// the [`SlotExtent`]. A pass may write it or read it. The bound
    /// texture must be exactly that size, and the shader declares it
    /// `texture_2d<f32>`.
    Target(SlotExtent),
    /// A texture of any size, read only: a lookup table, a tile sheet,
    /// a table of per-instance data. The shader declares it
    /// `texture_2d<f32>`; a pass naming it as its output is refused at
    /// register.
    Texture,
    /// An array texture of any size and layer count, read only. The
    /// shader declares it `texture_2d_array<f32>`; a pass naming it as
    /// its output is refused at register, and a dispatch that binds a
    /// texture that is not an array there is dropped.
    TextureArray,
    /// A volume texture of any width, height and depth, read only. The
    /// shader declares it `texture_3d<f32>` and reads it with a
    /// three-component coordinate; a pass naming it as its output is
    /// refused at register, and a dispatch that binds a texture that is
    /// not a volume there is dropped.
    TextureVolume,
}

/// How a `Filtered` binding addresses a coordinate outside `0..1`, on
/// every axis the bound texture has: the third axis of a volume wraps as
/// the first two do.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum Wrap {
    /// The edge texel extends outward.
    Clamp,
    /// The texture tiles.
    Repeat,
}

/// Mip levels, in its two uses: on a `Filtered` binding it says which
/// levels are read, and on `CreateTextureArray` it says which levels
/// exist.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum Mips {
    /// The base level only. A binding reads it whatever the bound
    /// texture holds; an array has that one level.
    Base,
    /// The whole chain. A binding blends between levels when the
    /// texture is linear-filtered, and a texture with no chain reads as
    /// `Base` does; an array has every level down to one texel.
    Chain,
}

/// How a program reads one binding (ADR-0246 decision 5).
///
/// The binding decides whether there is a sampler, how it addresses a
/// coordinate outside the texture and which mip levels it reads. The
/// bound texture decides linear or nearest: nearest when it was created
/// `TextureSampling::Nearest` or its format cannot be filtered
/// (`R32Float`), linear otherwise.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq)]
pub enum Sampling {
    /// A texture and a sampler. Input `n` of a pass is the texture at
    /// `@group(1) @binding(2 * n)` and the sampler at
    /// `@group(1) @binding(2 * n + 1)`.
    Filtered { wrap: Wrap, mips: Mips },
    /// A texture and no sampler, read texel by texel with `textureLoad`.
    /// Input `n` is still the texture at `@group(1) @binding(2 * n)`;
    /// `@binding(2 * n + 1)` is left out of the pass's layout, so the
    /// inputs after it keep their numbers. A pass whose entry point
    /// uses a sampler there is refused at register.
    Texel,
}

/// One program binding: an entry in `ProgramRegister.bindings`, a
/// registry texture supplied at dispatch.
///
/// `format` fixes the binding's pixel format at register time, which is
/// what lets every pass pipeline build (and fail) inside the register
/// reply rather than at first dispatch. `shape` says what the binding
/// takes and whether a pass may write it; `sampling` says how a pass
/// that reads it does so. A binding is always single-sample.
///
/// At dispatch a binding's registry texture must have the declared
/// format, and for a `Target` the resolved size; a `Texture` takes any
/// size. A texture that disagrees drops the dispatch with a warning
/// naming the binding.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq)]
pub struct SlotSpec {
    pub format: TextureFormat,
    pub shape: SlotShape,
    pub sampling: Sampling,
}

/// One transient: an entry in `ProgramRegister.transients`, an
/// intermediate texture the executor owns and pools (ADR-0246
/// decision 7).
///
/// A transient is sized from the program's output by `extent` and holds
/// `samples` samples per texel. A pass that reads it declares a
/// `texture_2d<f32>` at `@binding(2 * n)` and a `sampler` at
/// `@binding(2 * n + 1)`: the sampler clamps, reads the base level, and
/// is linear, or nearest for a format that cannot be filtered
/// (`R32Float`). A shader may read it with `textureLoad` and leave the
/// sampler unused.
///
/// A [`Samples::Four`] transient is read resolved, so its format must
/// be one that can be resolved: a `Four` `R32Float` transient is
/// refused at register.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq)]
pub struct TransientSpec {
    pub format: TextureFormat,
    pub extent: SlotExtent,
    pub samples: Samples,
}

/// How a depth transient is sized (ADR-0246 decision 9).
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq)]
pub enum DepthExtent {
    /// Sized from the program's output, as a transient is: the
    /// reference extent scaled by the [`SlotExtent`].
    Output(SlotExtent),
    /// A square of its own size, `side` texels on each axis, whatever
    /// the reference extent is: a shadow map. `side` must be at least 1
    /// and at most the device's `max_texture_dimension_2d`; a side
    /// outside that rejects at register. A `Fixed` slot keeps its size,
    /// and its pooled texture, when the output is resized.
    Fixed { side: u32 },
}

/// One depth transient: an entry in `ProgramRegister.depth_transients`,
/// a pooled `Depth32Float` target rasterizing passes clear and test
/// against (ADR-0171, ADR-0246 decisions 7 and 9).
///
/// A pass with a color output attaches its depth slot beside that
/// output, so the slot declares [`DepthExtent::Output`] of the output's
/// [`SlotExtent`] and the output's `samples`: a pass writing a
/// [`Samples::Four`] transient names a `Four` depth slot, and a pass
/// writing a binding or a `One` transient names a `One` depth slot. A
/// [`DepthExtent::Fixed`] slot under a color output is refused at
/// register whatever its side, because the output's size is the size of
/// the texture a dispatch binds and is not known when the program
/// registers.
///
/// A depth-only pass ([`OutputSlot::None`] on a rasterizing stage) has
/// no color output to match and attaches a slot of either extent and
/// either sample count. A depth slot is never resolved.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq)]
pub struct DepthSpec {
    pub extent: DepthExtent,
    pub samples: Samples,
}

/// How a pass reads a depth slot it names as an input (ADR-0246
/// decision 9). The read is declared on the input, so one pass may
/// compare a slot that another loads.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq)]
pub enum DepthRead {
    /// A `texture_depth_2d` and no sampler, read with `textureLoad`,
    /// which returns the stored depth. `@binding(2 * n + 1)` is left
    /// out of the pass's layout, as it is for [`Sampling::Texel`].
    Texel,
    /// A `texture_depth_2d` and a `sampler_comparison` at
    /// `@binding(2 * n + 1)`, read with `textureSampleCompare` in a
    /// fragment stage and `textureSampleCompareLevel` in any stage. The
    /// comparison is `LessEqual`, and the sampler clamps and is linear.
    Compare,
}

/// One input slot a program pass samples (ADR-0170, ADR-0246
/// decision 9). A `Binding`, `PassOutput` or `Transient` input resolves
/// to a texture the pass binds at group 1 in declaration order: a
/// binding with the sampler its [`Sampling`] declares, and a transient
/// with the sampler every transient has. A `Depth` input names a depth
/// slot under the same numbering.
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
    /// The depth transient at `index` into
    /// `ProgramRegister.depth_transients`, read as `read` declares.
    /// Input `n` keeps the numbering every input has: its texture is
    /// `@binding(2 * n)`, declared `texture_depth_2d`.
    ///
    /// Four things are refused at register, each with its own reason:
    /// an `index` past `depth_transients`; a [`Samples::Four`] slot,
    /// which can be neither compared nor sampled; a slot the same pass
    /// attaches; and a slot no earlier pass attaches, since a depth
    /// slot gets its contents only from the passes that attach it.
    ///
    /// The slot holds what the passes before this one drew into it in
    /// the same dispatch, and the far plane where they drew nothing:
    /// the first pass of a dispatch to attach a slot clears it.
    Depth { index: u32, read: DepthRead },
}

/// The color slot a program pass writes (ADR-0170): a dispatch binding
/// (a writable registry texture), a transient intermediate, or none.
/// Passes never write another pass's output alias, so that variant does
/// not exist here.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq)]
pub enum OutputSlot {
    /// The dispatch binding at `index` into `ProgramDispatch.bindings`.
    /// The binding must be declared [`SlotShape::Target`], or the
    /// register is refused. The bound registry texture must be
    /// `TextureUsage::Writable`; a `Sampled` texture there warn-drops
    /// the dispatch.
    Binding { index: u32 },
    /// The transient intermediate at `index` into
    /// `ProgramRegister.transients`.
    Transient { index: u32 },
    /// No color output. On a compute pass it is the only valid output:
    /// the pass's writes land in the resident geometry buffers declared
    /// by its stage. On a `Draw`, `DrawIndexedIndirect` or `DrawSets`
    /// pass it declares a depth-only pass (ADR-0246 decision 9), which
    /// attaches its depth slot and no color target. A `Fragment` pass
    /// attaches no depth slot, so `None` there is refused.
    ///
    /// A depth-only pass names a depth slot and writes it, declares
    /// [`Blend::Replace`] and [`PassLoad::Load`], and names a fragment
    /// entry point that returns nothing or `@builtin(frag_depth)` alone
    /// (it may `discard`); anything else is refused at register. It
    /// rasterizes at its depth slot's sample count.
    ///
    /// A later pass cannot name a pass with no color output through
    /// `PassOutput`, and the final pass of a program must still write a
    /// dispatch binding.
    None,
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
/// outright, so a layered bake states its own composition. What the
/// pass then draws composes with the loaded content through the pass's
/// declared [`Blend`].
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq)]
pub enum PassLoad {
    /// Clear the output to transparent black, then draw.
    Clear,
    /// Load whatever the output already holds and draw over it — the
    /// retained pixels of a writable binding across dispatches, or an
    /// earlier pass's work within one.
    Load,
}

/// How a pass composes what its fragment stage returns with what its
/// color output already holds (ADR-0246 decision 7). The pass declares
/// it and it applies whatever the output's format.
///
/// A compute pass has no color output and declares `Replace`; any other
/// value there is refused at register. An `Alpha` or `Additive` pass
/// onto an `R32Float` output needs a device that can blend 32-bit
/// floats (`float32-blendable`); on a device without it the register
/// replies `Err` with `pipeline creation failed`, naming the format.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq)]
pub enum Blend {
    /// The returned value overwrites the output on all four channels,
    /// alpha included.
    Replace,
    /// Straight-alpha source over: color is
    /// `source * source.a + destination * (1 - source.a)` and alpha is
    /// `source.a + destination.a * (1 - source.a)`.
    Alpha,
    /// The returned value is added to the output on all four channels:
    /// `source + destination`.
    Additive,
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
/// consecutive draw passes agree on occlusion by naming one slot. Under
/// a color output the slot is [`DepthExtent::Output`] of that output's
/// extent and has its sample count ([`DepthSpec`]); one that differs in
/// either, or is [`DepthExtent::Fixed`], is refused at register.
///
/// A pass whose [`ProgramPass`] declares [`OutputSlot::None`] is
/// depth-only (ADR-0246 decision 9): it must name a depth slot, of
/// either extent and either sample count, and declare `load` as
/// [`PassLoad::Load`].
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

/// Which triangles a `PassStage::DrawSets` pass discards before
/// rasterizing (ADR-0246). The front face is counter-clockwise.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq)]
pub enum Cull {
    /// Draw every triangle, whichever way it winds.
    None,
    /// Discard clockwise triangles.
    Back,
}

/// Whether a depth-testing `PassStage::DrawSets` pass also writes the
/// depth it passes (ADR-0246).
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq)]
pub enum DepthWrite {
    /// Test against the slot and write each fragment that passes.
    Write,
    /// Test against the slot and leave it as it was.
    TestOnly,
}

/// The depth slot a `PassStage::DrawSets` pass tests against and what
/// it does to it (ADR-0246). `slot` is an index into
/// `ProgramRegister.depth_transients`; the test is `LessEqual`.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq)]
pub struct DepthUse {
    pub slot: u32,
    pub write: DepthWrite,
}

/// The `PassStage::DrawSets` declaration (ADR-0246 decision 4): one
/// render pass that draws every draw of every draw set the dispatch
/// lists for it, in order. The pass's fragment entry point, input
/// slots, color output and uniform window stay on [`ProgramPass`].
///
/// The pass binds two vertex buffers. Buffer 0 is a draw's geometry,
/// laid out by `vertex_layout` and stepped per vertex; buffer 1 is the
/// draw's instance records, laid out by `instance_layout` and stepped
/// per instance. Each attribute binds at the `@location` its layout
/// declares, so the two layouts share no location and neither declares
/// one twice. A draw set is drawn by a pass whose two layouts equal the
/// set's.
///
/// `depth` attaches a `ProgramRegister.depth_transients` slot under a
/// `LessEqual` test. The first pass of a dispatch to name a slot clears
/// it to the far plane and later passes load it, whatever their stage
/// and whatever `write` says. Under a color output the slot is
/// [`DepthExtent::Output`] of that output's extent and has its sample
/// count ([`DepthSpec`]); one that differs in either, or is
/// [`DepthExtent::Fixed`], is refused at register.
///
/// A pass whose [`ProgramPass`] declares [`OutputSlot::None`] is
/// depth-only (ADR-0246 decision 9): it must name a depth slot, of
/// either extent and either sample count, under [`DepthWrite::Write`]
/// (a `TestOnly` depth-only pass would write nothing and is refused),
/// and declare `load` as [`PassLoad::Load`].
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct DrawSetsPass {
    /// Vertex entry point in the program's WGSL module. It may read the
    /// pass's uniform window and its group-1 inputs, as a draw pass's
    /// vertex stage does.
    pub vertex_entry_point: String,
    /// The layout of every geometry the pass draws, bound per vertex.
    pub vertex_layout: Vec<VertexAttribute>,
    /// The layout of every instance buffer the pass draws, bound per
    /// instance.
    pub instance_layout: Vec<VertexAttribute>,
    /// Index into `ProgramDispatch.draw_sets`: the list of draw sets
    /// this pass draws. Two passes may name one list.
    pub draw_sets: u32,
    pub cull: Cull,
    /// The depth slot the pass tests against, or `None` for a pass that
    /// does not depth-test.
    pub depth: Option<DepthUse>,
    pub load: PassLoad,
}

/// Which resident buffer of a declared geometry slot a compute pass
/// binds at group 2. Vertex and index buffers preserve the created
/// geometry's capacity; the indirect buffer is the substrate-owned
/// 32-byte control block consumed by `DrawIndexedIndirect`.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum GeometryBuffer {
    /// Packed interleaved vertex bytes, storage-visible as raw u32
    /// words. The authored shader decodes the declared vertex layout;
    /// natural WGSL struct alignment is not the wire stride.
    Vertices,
    /// The geometry's u32 index capacity.
    Indices,
    /// Eight u32 words. Words 0 through 4 are WebGPU's indexed-indirect
    /// arguments (`index_count`, `instance_count`, `first_index`,
    /// signed `base_vertex` bits, `first_instance`); words 5 and 6 are
    /// vertex and index capacities, and word 7 is an authored overflow
    /// flag. Reconstruction seeds a zero index count, one instance,
    /// zero offsets, the CPU-staged capacities, and zero overflow.
    /// `first_instance` must remain zero because the render device does
    /// not request WebGPU's optional indirect-first-instance feature.
    DrawIndexedIndirect,
}

/// Access a compute entry point declares for one group-2 resident
/// geometry buffer binding.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum StorageAccess {
    Read,
    ReadWrite,
}

/// One group-2 storage-buffer binding of an authored compute pass.
/// Bindings are assigned in list order: entry `n` is
/// `@group(2) @binding(n)` in WGSL.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct ComputeBufferBinding {
    /// Index into `ProgramRegister.geometries`.
    pub geometry: u32,
    pub buffer: GeometryBuffer,
    pub access: StorageAccess,
}

/// The `PassStage::Compute` declaration: resident geometry buffers and
/// the fixed workgroup grid dispatched for each pass iteration. Fixed
/// dimensions keep structure at register time; per-run values continue
/// to ride the uniform window.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ComputePass {
    pub buffers: Vec<ComputeBufferBinding>,
    pub workgroups: [u32; 3],
}

/// Which GPU stage a program pass runs (ADR-0170, ADR-0171, ADR-0246).
/// Compute adds shared-memory, reductions, and scatter writes over
/// resident geometry; indexed-indirect draw consumes its derived control
/// block; a draw-sets pass draws retained lists of instanced draws.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum PassStage {
    /// A fullscreen-triangle fragment pipeline over a render attachment.
    Fragment,
    /// An indexed triangle-list draw of a bound geometry through an
    /// authored vertex stage, optionally depth-tested (ADR-0171).
    Draw(DrawPass),
    /// An indexed triangle-list draw whose arguments come from the
    /// bound geometry's resident indirect/control buffer. A preceding
    /// compute pass in the same graph must write that buffer.
    DrawIndexedIndirect(DrawPass),
    /// A compute dispatch over resident geometry buffers. Compute has
    /// no texture render attachment, so its `ProgramPass.output` must
    /// be `OutputSlot::None`.
    Compute(ComputePass),
    /// One render pass over the draw sets the dispatch lists for it
    /// (ADR-0246): every draw of every listed set, in order, each an
    /// indexed draw of one geometry once per record of a run of one
    /// instance buffer.
    DrawSets(DrawSetsPass),
}

/// Repetition of one program pass (ADR-0170): the pass records `count`
/// times, iteration `i` binding its uniform window at
/// `uniform_offset + i * uniform_stride`, so a chain of pours is one
/// pass entry over a strided parameter table rather than many entries.
/// The first iteration clears the output slot (if nothing wrote it
/// earlier in the dispatch); later iterations load it, so iterations
/// accumulate through the [`Blend`] the pass declares: under `Alpha`
/// or `Additive` each iteration composes with the ones before it, and
/// under `Replace` the last iteration is what remains. A multisampled
/// output is resolved after the last iteration, never between
/// iterations. `count` must be at least 1 and
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
/// `blend` is how a render stage composes what its fragment entry
/// returns with what `output` already holds; a compute pass and a
/// depth-only pass, which have no color output, declare
/// [`Blend::Replace`]. `entry_point` names a fragment entry for
/// fragment, draw and draw-sets stages, or a compute entry for
/// `PassStage::Compute`. `inputs` bind in order at group 1, input `n`
/// at `@binding(2 * n)` with its sampler at `@binding(2 * n + 1)` — a
/// transient always has one, and a binding has one unless it declares
/// `Sampling::Texel`; render stages attach `output`, rasterizing at its
/// sample count, while compute declares `OutputSlot::None` and writes
/// its group-2 resident buffers. A rasterizing stage that declares
/// `OutputSlot::None` is depth-only: it attaches its depth slot alone
/// and rasterizes at that slot's sample count. `uniform_offset` /
/// `uniform_length` window the dispatch's uniform blob in bytes — the
/// window binds at `@group(0) @binding(0)` and must cover the uniform
/// block the entry point declares (checked at register from naga's
/// layout; a shorter window rejects). A pass whose entry point declares
/// no uniform block passes a zero-length window.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ProgramPass {
    pub stage: PassStage,
    pub blend: Blend,
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
/// every [`Samples::Four`] transient has a format that can be resolved,
/// every binding a pass writes is a [`SlotShape::Target`],
/// every pass's entry point exists in its declared stage, every texture
/// slot is written before it is read (the sequence-index check), no pass reads
/// its own output, every [`DepthExtent::Fixed`] side is at least 1 and
/// within the device's texture limit, every depth slot a pass names
/// beside a color output is [`DepthExtent::Output`] of that output's
/// extent and has its sample count, every rasterizing pass with no
/// color output is a well-formed depth-only pass (it names a depth slot
/// and writes it, declares [`Blend::Replace`] and [`PassLoad::Load`],
/// and its fragment entry point returns no color), every
/// [`InputSlot::Depth`] names a declared [`Samples::One`] depth slot
/// that an earlier pass attaches and its own pass does not, every
/// compute pass declares
/// [`Blend::Replace`], every uniform window covers the uniform block its
/// entry point declares, the graph's per-dispatch cost stays inside the
/// executor's budget (the render passes it encodes and the uniform bytes
/// it stages, both summed over the whole pass list — a per-pass repeat
/// ceiling alone leaves their product unbounded), the final pass writes
/// a dispatch binding
/// declared `Target(SlotExtent::Full)` (the program's result texture,
/// whose size is the reference every other extent scales from) — and
/// finally wgpu
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
/// declaration order at group 1 — input `n` is the texture at
/// `@binding(2 * n)`, `texture_2d<f32>` for a transient or a `Target`
/// or `Texture` binding, `texture_2d_array<f32>` for a `TextureArray`
/// and `texture_3d<f32>` for a `TextureVolume`, plus the `sampler` at `@binding(2 * n + 1)` for a
/// transient or a binding whose sampling is `Filtered`. A `Texel`
/// binding has no sampler and leaves
/// `@binding(2 * n + 1)` unused, so the numbering of the inputs after
/// it does not move; the shader reads it with `textureLoad`. A module
/// whose entry point disagrees with the slots — it reads an array or a
/// volume where a plain texture is declared, or uses a sampler on a
/// `Texel` input —
/// fails pipeline creation and replies `Err`. Group 1 is visible to the fragment stage
/// of every pass, to the authored vertex stage of a draw pass and to a
/// compute pass. What a pass's fragment entry returns composes with its
/// output through the [`Blend`] the pass declares, whatever the
/// output's format. The first write a dispatch makes to each
/// output slot clears it to transparent black; later writes — a
/// repeat's iterations, a second pass onto the same slot — load the
/// existing content.
///
/// A pass rasterizes at the sample count of its output: one for a
/// binding, and what a transient declares ([`Samples`]). A pass that
/// reads a `Four` transient reads it resolved, the executor resolving
/// it once after the last pass to write it before each pass that reads
/// it. Every pass writes a binding or a transient and none writes the
/// frame: a program's output reaches the frame as a texture the quad
/// and material paths draw. The one pass that writes neither is a
/// depth-only pass, which writes its depth slot alone.
///
/// A `PassStage::Draw` pass (ADR-0171) replaces the fullscreen vertex
/// stage with an authored one over a bound geometry and states its own
/// color load semantic instead of following the clear-on-first-write
/// rule. `geometries` declares the geometry slots a dispatch fills by
/// id, and `depth_transients` the pooled `Depth32Float` targets draw
/// passes clear and test against — declared by [`DepthExtent`] and
/// sample count, since their format is fixed. Both lists are empty for a fragment-only
/// program, which registers exactly as it did before this arm existed.
/// A `PassStage::Compute` pass instead binds its uniform at group 0,
/// sampled inputs at group 1, and the declared resident geometry
/// buffers at group 2 in list order. It writes no texture attachment;
/// a later indexed-indirect draw consumes the derived buffers.
///
/// A `PassStage::DrawSets` pass (ADR-0246) names no geometry slot: it
/// carries its own vertex and instance layouts and draws the draw sets
/// a dispatch lists for it. Each attribute binds at the `@location` its
/// layout declares, the vertex layout in vertex buffer 0 and the
/// instance layout in vertex buffer 1, so a register is refused when
/// either layout is empty, when one declares a location twice, when the
/// two share a location, or when the vertex stage reads a location
/// neither declares or reads one as the wrong type. The list slots a
/// program's passes name are dense: a dispatch supplies one more list
/// than the highest `DrawSetsPass.draw_sets` index, and a graph that
/// leaves a lower index unnamed is refused, so every list a dispatch
/// supplies is drawn by at least one pass.
///
/// Reply: `ProgramRegisterResult`; `program_id` is session-scoped,
/// assigned like texture and instrument ids. The headless chassis
/// composes no render actor. A register sent before the render device
/// exists (desktop: before the first window attaches) is answered once
/// the device is up. Its own chain settles first, so nothing waits on a
/// window: a `send_mail` over MCP returns with no reply for it, and one
/// sent after a window is listed is answered inside the call.
#[aether_data::kind(name = "aether.render.program.register")]
pub struct ProgramRegister {
    pub wgsl: String,
    pub bindings: Vec<SlotSpec>,
    /// Intermediate textures the executor owns and pools, each sized
    /// from the program's output and holding one or four samples per
    /// texel (ADR-0246 decision 7).
    pub transients: Vec<TransientSpec>,
    /// Geometry slots draw passes bind, filled by id per dispatch
    /// (ADR-0171). Empty for a fragment-only program.
    pub geometries: Vec<GeometrySlotSpec>,
    /// Pooled `Depth32Float` targets draw passes clear and test
    /// against, each declared by its extent — against the reference, or
    /// a fixed square — and its sample count (ADR-0171, ADR-0246
    /// decisions 7 and 9). Empty for a fragment-only program.
    pub depth_transients: Vec<DepthSpec>,
    pub passes: Vec<ProgramPass>,
}

/// Reply to `ProgramRegister`. `Ok` carries the assigned `program_id`
/// — thread it into `ProgramDispatch.program_id` and
/// `ProgramDestroy.program_id`. `Err` carries a human-readable reason
/// prefixed by its validation class: `invalid wgsl` (naga parse or
/// validation), a graph-check message naming the offending pass and
/// slot, or `pipeline creation failed` (a wgpu validation error caught
/// by the register's error scope).
#[aether_data::kind(name = "aether.render.program.register_result")]
pub enum ProgramRegisterResult {
    Ok { program_id: u32 },
    Err { error: String },
}

impl HeldReply for ProgramRegisterResult {
    fn unanswered() -> Self {
        Self::Err { error: "render capability closed before the program register request was answered".into() }
    }
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
/// (ADR-0171), also in order; `draw_sets` gives one list of draw-set
/// ids per list slot the program's `PassStage::DrawSets` passes name
/// (ADR-0246), each list the sets that slot's passes draw this frame,
/// in order; `uniforms` is one byte
/// blob the passes window into (each window is copied into an aligned
/// staging arrangement, so windows need no alignment of their own —
/// pack them tight). Runtime mismatches — an unknown `program_id`, a
/// wrong binding or geometry count, an unknown texture or geometry id,
/// a binding whose format disagrees with its declared slot, a `Target`
/// binding whose size is not the reference extent scaled by its
/// declared extent (a `Texture` binding takes any size), a binding
/// whose texture is not the kind its shape takes (a plain texture for
/// `Target` and `Texture`, an array for `TextureArray`, a volume for
/// `TextureVolume`), a written
/// binding whose texture is not writable, a geometry
/// whose layout disagrees with its declared slot, a wrong number of
/// draw-set lists, an unknown draw-set id, a draw set whose layouts are
/// not the layouts of a pass that draws it, a uniform
/// window past the blob's end, or a pass whose input and output resolve
/// to the same texture — warn-drop the dispatch naming the program,
/// pass, and binding in the render actor's log ring, the same
/// convention as an unknown texture id in `draw_textured_quads`.
#[aether_data::kind(name = "aether.render.program.dispatch")]
pub struct ProgramDispatch {
    pub program_id: u32,
    pub bindings: Vec<u32>,
    /// One geometry id per declared `ProgramRegister.geometries` slot,
    /// in order. Empty for a fragment-only program.
    pub geometries: Vec<u32>,
    /// One list of draw-set ids per list slot the program's
    /// `PassStage::DrawSets` passes name, in slot order (ADR-0246).
    /// Empty for a program with no such pass; a list may be empty, and
    /// its passes then draw nothing.
    pub draw_sets: Vec<Vec<u32>>,
    #[serde(with = "aether_data::bytes")]
    pub uniforms: Vec<u8>,
}

/// `aether.render.program.destroy` — release a registered program from
/// the render cap's session-scoped program registry, mirroring
/// `destroy_texture`. Fire-and-forget; an unknown `program_id` logs and
/// drops. Dropping the entry releases the program's compiled pipelines;
/// pooled transient textures stay in the shared pool for other
/// programs.
#[aether_data::kind(name = "aether.render.program.destroy")]
pub struct ProgramDestroy {
    pub program_id: u32,
}

/// Which pipeline shape a timed pass ran, flattened from
/// [`PassStage`] — draw and compute declarations are register-time
/// authoring detail a timing reader has no use for, and carrying it
/// would make the reply grow with the graph's vertex layouts.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq)]
pub enum PassStageKind {
    Fragment,
    Draw,
    Compute,
}

/// One pass's measured GPU duration, in the shape `actor_cost` reports a
/// handler's execution cost: an exponentially-weighted mean, the
/// mean-absolute deviation around it, and the folded-sample count, all
/// integer nanos. `samples: 0` is the neutral seed — a pass the graph
/// declares that has not yet been measured — which is what distinguishes
/// "known but unrun" from "absent", so a zero mean is never mistaken for
/// a free pass.
///
/// **`mean_nanos` is marginal, and rows add up.** A pass is charged the
/// interval between the pass before it retiring and itself retiring, not
/// its own begin-to-end span. A GPU that keeps many passes in flight
/// gives every pass a span covering its predecessors' work as well as
/// its own, so spans overlap and summing them overcounts the frame
/// severalfold; the marginal chain sums to the frame's GPU envelope
/// instead. Summing a program's rows therefore gives that program's
/// share of the frame's GPU time, and comparing two rows compares what
/// removing one or the other would actually save.
///
/// The identity fields answer *which* pass and *at what size*, because
/// that is what a pass-merging or extent decision keys on: `pass` is the
/// index into the registered graph's pass list, `label` its WGSL entry
/// point, `width` / `height` the extent its output slot resolved to on
/// the most recent dispatch (`0 / 0` for compute), and `divisor` the
/// declared [`SlotExtent`] that extent came from (`1` for `Full`). A
/// depth-only pass has stage `Draw` and reports the size its depth slot
/// resolved to, with the divisor of a [`DepthExtent::Output`] extent
/// and `1` for a [`DepthExtent::Fixed`] one. `iterations`
/// is the pass's repeat count — one row covers all of a repeated pass's
/// iterations, so a large mean over a large `iterations` is a chain, not
/// a single expensive pass.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct PassTimingRow {
    pub pass: u32,
    pub label: String,
    pub stage: PassStageKind,
    pub width: u32,
    pub height: u32,
    pub divisor: u32,
    pub iterations: u32,
    pub mean_nanos: u64,
    pub mad_nanos: u64,
    pub samples: u64,
}

/// `aether.render.program.timings` — read the per-pass GPU duration
/// table a registered program has accumulated. Durations only: the
/// instrument brackets each recorded pass with wgpu timestamp queries,
/// resolves them a frame later off the frame's critical path, and folds
/// the deltas into per-pass EWMAs. Nothing of the program's pixels is
/// read back.
///
/// Reply: [`ProgramTimingsResult`]. The instrument needs wgpu's
/// `TIMESTAMP_QUERY` feature, which the render device requests whenever
/// the selected adapter offers it; where the adapter does not, or where
/// the operator turned the instrument off, the reply is `Absent` with
/// the reason rather than a table of zeros.
#[aether_data::kind(name = "aether.render.program.timings")]
pub struct ProgramTimings {
    pub program_id: u32,
}

/// Reply to [`ProgramTimings`]. `Ok` carries one [`PassTimingRow`] per
/// declared pass, in graph order — including passes never measured
/// (`samples: 0`), so the reply always describes the whole graph.
/// `Absent` means the instrument is not running and says why (no
/// adapter support, or disabled by configuration); it is not an error
/// and a caller should read it as "this device cannot answer", not "this
/// program is free" — hence its payload is `reason`, not the `error`
/// every failure arm carries. `Err` is a genuine failure — an unknown
/// `program_id`, or no booted render GPU.
#[aether_data::kind(name = "aether.render.program.timings_result")]
pub enum ProgramTimingsResult {
    Ok { program_id: u32, rows: Vec<PassTimingRow> },
    Absent { reason: String },
    Err { error: String },
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
