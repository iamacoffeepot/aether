//! Textured-quad overlay pipeline (ADR-0105). A second, alpha-blended
//! pipeline beside the main triangle pipeline: it draws textured quads
//! into the same offscreen color target in an overlay pass recorded
//! after [`super::record_main_pass`], with no depth test or write so
//! the quads always land on top of the world geometry.
//!
//! The pipeline can't ride `record_main_pass`'s `extra_pipelines` hook
//! (those re-draw the *same* `(pos, color)` vertex buffer + layout): a
//! quad has its own `(pos, uv, tint)` vertex layout, its own shader, a
//! texture + sampler bind group, and alpha blending. So it is a sibling
//! pass with its own vertex buffer ([`record_quad_overlay_pass`]).
//!
//! Texture realization is lazy: the render capability stages pixels
//! CPU-side at `create_texture` time and calls [`realize_texture`]
//! / [`upload_texture_full`] at record time, when a device + queue are
//! available. The realized [`RealizedTexture`] carries the wgpu texture
//! plus the group-1 bind group built against shared texture bindings.

use super::shape::{SHAPE_VERTEX_BUFFER_BYTES, ShapePipeline, build_shape_pipeline};
use super::targets::Targets;
use aether_math::Rect2;
use std::iter;

/// Bytes per expanded quad vertex: `anchor vec3<f32>` (12) +
/// `offset_px vec2<f32>` (8) + `uv vec2<f32>` (8) + `tint vec4<f32>`
/// (16) + `k f32` (4) + `is_screen u32` (4) = 52.
/// [`push_screen_quad_vertices`], [`push_world_quad_vertices`], and
/// [`push_screen_triangle_vertices`] all write exactly this stride per
/// vertex.
pub const QUAD_VERTEX_STRIDE: u64 = 52;

/// Vertices one quad expands to: two triangles, six vertices.
pub const QUAD_VERTICES_PER_QUAD: usize = 6;

/// Vertices one caller-supplied overlay triangle expands to. A triangle
/// is already the rasterizer's primitive, so it expands one-to-one —
/// unlike a quad, which is a rect the expansion has to corner out.
pub const QUAD_VERTICES_PER_TRIANGLE: usize = 3;

/// Maximum size of the per-frame quad vertex buffer. The render cap's
/// overlay encode drops the pass with a warn rather than overflow the
/// GPU buffer if a frame's expanded quad bytes exceed this.
pub const QUAD_VERTEX_BUFFER_BYTES: usize = 4 * 1024 * 1024;

/// Quad overlay uniform buffer size: `mat4x4<f32>` `view_proj` (64) +
/// `vec2<f32>` viewport size (8) + `vec2<f32>` pad (8) = 80 bytes (the
/// WGSL `Viewport` struct).
pub const QUAD_UNIFORM_BYTES: u64 = 80;

/// Source for the quad overlay shader.
pub const QUAD_SHADER_WGSL: &str = include_str!("quad.wgsl");

/// Shared GPU texture binding state for every pipeline that samples a
/// registered render texture.
pub struct TextureBindings {
    /// Filtering layout for texture view at binding 0 plus sampler at
    /// binding 1 — the layout the color material / overlay pipelines
    /// are built against. Filterable formats only.
    pub layout: wgpu::BindGroupLayout,
    /// Non-filtering companion of `layout` — a `filterable: false`
    /// texture entry plus a `NonFiltering` sampler entry — for
    /// data-plane formats core WebGPU cannot linear-filter (`R32Float`,
    /// ADR-0170). Bind groups built against it are not compatible with
    /// pipelines built on `layout`.
    pub data_layout: wgpu::BindGroupLayout,
    pub sampler: wgpu::Sampler,
    /// Nearest-neighbor sampler for label planes whose texel values are
    /// identities rather than colors (ADR-0170). Non-filtering, so it
    /// binds under both layouts.
    pub nearest_sampler: wgpu::Sampler,
}

/// Owned GPU state for the overlay pass: the two textured-quad render
/// pipelines (one per blend), the shape pipeline (ADR-0213), the
/// per-frame vertex buffer, and the viewport uniform + its bind group
/// (group 0), which every overlay pipeline is built against. Texture
/// group-1 state is supplied by [`TextureBindings`].
#[allow(clippy::struct_field_names)]
pub struct QuadPipeline {
    straight: wgpu::RenderPipeline,
    premultiplied: wgpu::RenderPipeline,
    shape: ShapePipeline,
    vertex_buffer: wgpu::Buffer,
    viewport_buffer: wgpu::Buffer,
    viewport_bind_group: wgpu::BindGroup,
}

/// A texture realized on the GPU plus its group-1 bind group, built
/// against shared [`TextureBindings`].
/// The render cap caches one of these per registered texture and
/// re-uploads its pixels via [`upload_texture_full`] when the staged
/// CPU pixels change.
pub struct RealizedTexture {
    texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    width: u32,
    height: u32,
    format: wgpu::TextureFormat,
}

impl RealizedTexture {
    /// The group-1 bind group to set before drawing quads that sample
    /// this texture.
    #[must_use]
    pub fn bind_group(&self) -> &wgpu::BindGroup {
        &self.bind_group
    }

    /// The realized wgpu texture. The authored-program executor
    /// (ADR-0170) views it directly — as a render attachment for a
    /// writable output binding, and as a sampled entry in a pass's
    /// combined input bind group (which pairs each input with the
    /// sampler its format and sampling mode select, so the cached
    /// per-texture `bind_group` shape doesn't fit).
    #[must_use]
    pub fn texture(&self) -> &wgpu::Texture {
        &self.texture
    }
}

/// How a textured composite lays its source over the target.
///
/// `Straight` weights the source colour by the alpha it is handed;
/// `Premultiplied` adds it as it stands, for a source whose colour was
/// already scaled by its own coverage — which is what any texture a
/// render program wrote necessarily is. The two differ only in the
/// source factor, so a pass switches between them by pipeline and
/// nothing else.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum CompositeBlend {
    #[default]
    Straight,
    Premultiplied,
}

/// What one overlay draw samples and which pipeline it runs through. The
/// two sources index different vertex buffers — textured draws the quad
/// buffer, shape draws the shape buffer (ADR-0213) — so an
/// [`OverlayDraw`]'s vertex range is read against the buffer its source
/// names.
pub enum OverlaySource<'a> {
    /// Textured quads or screen triangles: the group-1 bind group for the
    /// batch's texture, drawn through the pipeline `blend` selects.
    Textured { bind_group: &'a wgpu::BindGroup, blend: CompositeBlend },
    /// Distance-field shapes, drawn through a shape pipeline. `texture` is
    /// the group-1 bind group the run's shapes sample inside their fill
    /// coverage, or `None` for shapes that sample nothing — the two select
    /// the textured and the plain shape pipeline respectively.
    Shapes { texture: Option<&'a wgpu::BindGroup> },
}

/// One draw inside the overlay pass: its source, and the vertex
/// sub-range (in vertices, not bytes) the batch's expanded geometry
/// occupies in that source's vertex buffer.
pub struct OverlayDraw<'a> {
    pub source: OverlaySource<'a>,
    pub first_vertex: u32,
    pub vertex_count: u32,
    /// Optional framebuffer-pixel scissor: `[x, y, width, height]`.
    pub clip: Option<[f32; 4]>,
}

/// Build the shared texture + sampler bindings used by texture-sampling
/// pipelines.
#[must_use]
pub fn build_texture_bindings(device: &wgpu::Device) -> TextureBindings {
    let layout = sampled_texture_layout(device, "shared texture bind group layout", true);
    let data_layout = sampled_texture_layout(device, "shared data texture bind group layout", false);
    let sampler = build_sampler(device, "shared texture sampler", wgpu::FilterMode::Linear);
    let nearest_sampler = build_sampler(device, "shared nearest texture sampler", wgpu::FilterMode::Nearest);
    TextureBindings { layout, data_layout, sampler, nearest_sampler }
}

/// Texture-view + sampler bind group layout in the shared shape.
/// `filterable` selects the filtering pair (`Float { filterable: true }`
/// texture + `Filtering` sampler) or the non-filtering pair data-plane
/// formats require.
fn sampled_texture_layout(device: &wgpu::Device, label: &'static str, filterable: bool) -> wgpu::BindGroupLayout {
    let sampler_type = if filterable {
        wgpu::SamplerBindingType::Filtering
    } else {
        wgpu::SamplerBindingType::NonFiltering
    };
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some(label),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(sampler_type),
                count: None,
            },
        ],
    })
}

fn build_sampler(device: &wgpu::Device, label: &'static str, filter: wgpu::FilterMode) -> wgpu::Sampler {
    device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some(label),
        address_mode_u: wgpu::AddressMode::ClampToEdge,
        address_mode_v: wgpu::AddressMode::ClampToEdge,
        address_mode_w: wgpu::AddressMode::ClampToEdge,
        mag_filter: filter,
        min_filter: filter,
        mipmap_filter: wgpu::MipmapFilterMode::Nearest,
        ..Default::default()
    })
}

/// Build the quad overlay pipeline. `color_format` matches the
/// [`Targets`] color target the overlay pass attaches to (the same
/// format the main pipeline draws into).
// Single boot path: layouts, sampler, uniform, pipeline, vertex buffer
// all tied together, mirroring `build_main_pipeline`. Splitting would
// thread the same handles around without saving readability.
#[must_use]
pub fn build_quad_pipeline(
    device: &wgpu::Device,
    color_format: wgpu::TextureFormat,
    texture_bindings: &TextureBindings,
) -> QuadPipeline {
    let shader = super::overlay_shader_module(device, "aether quad shader", QUAD_SHADER_WGSL);

    let viewport_bind_group_layout =
        super::uniform_bind_group_layout(device, "quad viewport bind group layout", QUAD_UNIFORM_BYTES);

    let viewport_buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("quad viewport uniform"),
        size: QUAD_UNIFORM_BYTES,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });

    let viewport_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("quad viewport bind group"),
        layout: &viewport_bind_group_layout,
        entries: &[wgpu::BindGroupEntry { binding: 0, resource: viewport_buffer.as_entire_binding() }],
    });

    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("aether quad pipeline layout"),
        bind_group_layouts: &[Some(&viewport_bind_group_layout), Some(&texture_bindings.layout)],
        immediate_size: 0,
    });

    let vertex_layout = wgpu::VertexBufferLayout {
        array_stride: QUAD_VERTEX_STRIDE,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes: &[
            // anchor: vec3<f32> at offset 0
            wgpu::VertexAttribute { offset: 0, shader_location: 0, format: wgpu::VertexFormat::Float32x3 },
            // offset_px: vec2<f32> at offset 12
            wgpu::VertexAttribute { offset: 12, shader_location: 1, format: wgpu::VertexFormat::Float32x2 },
            // uv: vec2<f32> at offset 20
            wgpu::VertexAttribute { offset: 20, shader_location: 2, format: wgpu::VertexFormat::Float32x2 },
            // tint: vec4<f32> at offset 28
            wgpu::VertexAttribute { offset: 28, shader_location: 3, format: wgpu::VertexFormat::Float32x4 },
            // k: f32 at offset 44
            wgpu::VertexAttribute { offset: 44, shader_location: 4, format: wgpu::VertexFormat::Float32 },
            // is_screen: u32 at offset 48
            wgpu::VertexAttribute { offset: 48, shader_location: 5, format: wgpu::VertexFormat::Uint32 },
        ],
    };

    // One pipeline per blend. Everything else — layout, shader, vertex
    // layout, depth, multisample — is shared, so the pair costs a second
    // pipeline object and nothing at record time but a rebind. Overlay quads
    // draw on top of the world pass with no depth interaction at all (the
    // main pass already resolved depth), so neither takes a depth state.
    let build = |label, blend| {
        super::render_pipeline(
            device,
            super::RenderPipelineSpec {
                label,
                layout: &pipeline_layout,
                shader: &shader,
                fragment_entry: "fs_main",
                vertex_layout: &vertex_layout,
                color_format,
                blend,
                depth: None,
            },
        )
    };
    let straight = build("aether quad pipeline", wgpu::BlendState::ALPHA_BLENDING);
    let premultiplied = build("aether quad premultiplied pipeline", wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING);
    // The shape pipelines (ADR-0213) share the viewport layout so the
    // overlay pass binds group 0 once for every pipeline, and the textured
    // one shares the group-1 texture layout with the quad pipelines.
    let shape = build_shape_pipeline(device, color_format, &viewport_bind_group_layout, &texture_bindings.layout);

    let vertex_buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("aether quad vertex buffer"),
        size: QUAD_VERTEX_BUFFER_BYTES as u64,
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });

    QuadPipeline { straight, premultiplied, shape, vertex_buffer, viewport_buffer, viewport_bind_group }
}

/// Create a GPU texture from staged `pixels` and build its group-1 bind
/// group against shared texture bindings. `pixels` must be exactly
/// `width * height * bytes_per_pixel(format)` bytes (the render cap
/// validates this at `create_texture` time). `nearest` selects the
/// nearest sampler for label planes; a non-filterable `format` binds
/// through the non-filtering data layout regardless. Pair with
/// [`upload_texture_full`] to refresh the pixels later without rebuilding
/// the bind group.
// Eight arguments mirror the same all-in-one shape `record_quad_overlay_pass`
// uses; bundling into a struct for the one render-cap call site adds no
// clarity.
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn realize_texture(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    texture_bindings: &TextureBindings,
    width: u32,
    height: u32,
    format: wgpu::TextureFormat,
    nearest: bool,
    pixels: &[u8],
) -> RealizedTexture {
    let texture = create_registry_texture(
        device,
        "aether quad texture",
        width,
        height,
        format,
        wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
    );
    let bind_group = texture_bind_group(device, texture_bindings, &texture, format, nearest);
    let realized = RealizedTexture { texture, bind_group, width, height, format };
    upload_texture_full(queue, &realized, pixels);
    realized
}

/// Create a writable registry texture (ADR-0170): a GPU render target
/// draws paint into and the sampling passes read — wgpu
/// `RENDER_ATTACHMENT | TEXTURE_BINDING`, no CPU staging. The initial
/// content is defined by an explicit clear pass to transparent black
/// recorded and submitted here, which also puts the render-attachment
/// usage under wgpu validation at realization time.
#[must_use]
pub fn realize_writable_texture(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    texture_bindings: &TextureBindings,
    width: u32,
    height: u32,
    format: wgpu::TextureFormat,
    nearest: bool,
) -> RealizedTexture {
    let texture = create_registry_texture(
        device,
        "aether writable texture",
        width,
        height,
        format,
        wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
    );
    let bind_group = texture_bind_group(device, texture_bindings, &texture, format, nearest);

    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let mut encoder =
        device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("aether writable texture clear") });
    // Beginning and immediately ending the pass performs the clear.
    drop(encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("aether writable texture clear pass"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view: &view,
            resolve_target: None,
            depth_slice: None,
            ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT), store: wgpu::StoreOp::Store },
        })],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
        multiview_mask: None,
    }));
    queue.submit(iter::once(encoder.finish()));

    RealizedTexture { texture, bind_group, width, height, format }
}

fn create_registry_texture(
    device: &wgpu::Device,
    label: &'static str,
    width: u32,
    height: u32,
    format: wgpu::TextureFormat,
    usage: wgpu::TextureUsages,
) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d { width: width.max(1), height: height.max(1), depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage,
        view_formats: &[],
    })
}

/// Group-1 bind group for a registry texture: a filterable `format`
/// binds against the filtering layout with the linear or nearest
/// sampler as `nearest` selects; a non-filterable one (`R32Float`)
/// binds against the non-filtering data layout with the nearest
/// sampler — core WebGPU refuses to linear-filter it, so the layout
/// choice is forced, and the resulting bind group is incompatible with
/// pipelines built on the filtering layout.
fn texture_bind_group(
    device: &wgpu::Device,
    texture_bindings: &TextureBindings,
    texture: &wgpu::Texture,
    format: wgpu::TextureFormat,
    nearest: bool,
) -> wgpu::BindGroup {
    let filterable = !matches!(format, wgpu::TextureFormat::R32Float);
    let layout = if filterable {
        &texture_bindings.layout
    } else {
        &texture_bindings.data_layout
    };
    let sampler = if nearest || !filterable {
        &texture_bindings.nearest_sampler
    } else {
        &texture_bindings.sampler
    };
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("aether texture bind group"),
        layout,
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) },
            wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(sampler) },
        ],
    })
}

/// Re-upload the full staged `pixels` into an already-realized texture.
/// Used when an `update_texture` mail changed the staged CPU pixels: the
/// render cap re-uploads the whole texture at the next record rather
/// than tracking dirty sub-rects on the GPU. `pixels` must match the
/// realized texture format's byte count.
pub fn upload_texture_full(queue: &wgpu::Queue, realized: &RealizedTexture, pixels: &[u8]) {
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &realized.texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        pixels,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(realized.width.max(1) * texture_bytes_per_pixel(realized.format)),
            rows_per_image: Some(realized.height.max(1)),
        },
        wgpu::Extent3d { width: realized.width.max(1), height: realized.height.max(1), depth_or_array_layers: 1 },
    );
}

fn texture_bytes_per_pixel(format: wgpu::TextureFormat) -> u32 {
    match format {
        wgpu::TextureFormat::Rgba8Unorm | wgpu::TextureFormat::R32Float => 4,
        wgpu::TextureFormat::R16Float => 2,
        wgpu::TextureFormat::Rgba16Float => 8,
        wgpu::TextureFormat::R8Unorm => 1,
        _ => panic!("unsupported render texture format: {format:?}"),
    }
}

/// Push the six vertices (two triangles) for one screen-space quad into
/// `out` as raw bytes — each vertex is 52 bytes
/// ([`QUAD_VERTEX_STRIDE`]) in the unified world-aware layout: `anchor
/// vec3` (zeroed), `offset_px vec2` (absolute pixel position), `uv
/// vec2`, `tint vec4`, `k f32` (zeroed), `is_screen u32` (1). `rect`
/// is `[x, y, width, height]` (top-left + size in window pixels); `uv`
/// is `[u0, v0, u1, v1]`; `tint` is the per-vertex RGBA multiplier.
pub fn push_screen_quad_vertices(out: &mut Vec<u8>, rect: [f32; 4], uv: [f32; 4], tint: [f32; 4]) {
    let [x, y, width, height] = rect;
    let [u0, v0, u1, v1] = uv;
    let x0 = x;
    let y0 = y;
    let x1 = x + width;
    let y1 = y + height;
    // Two triangles, CCW in pixel space (top-left, bottom-left,
    // bottom-right) + (top-left, bottom-right, top-right). Cull mode is
    // off so winding doesn't gate visibility regardless.
    let corners =
        [(x0, y0, u0, v0), (x0, y1, u0, v1), (x1, y1, u1, v1), (x0, y0, u0, v0), (x1, y1, u1, v1), (x1, y0, u1, v0)];
    for (px, py, u, v) in corners {
        push_overlay_vertex(out, [0.0; 3], [px, py], [u, v], tint, 0.0, true);
    }
}

/// Push the three vertices of one screen-space triangle into `out` as
/// raw bytes, in the same 52-byte layout ([`QUAD_VERTEX_STRIDE`]) and on
/// the same `is_screen` path [`push_screen_quad_vertices`] writes — the
/// only difference is that the caller supplies the corners instead of
/// the expansion cornering out a rect, so the triangle can sit at any
/// orientation. `positions` are absolute window pixels (top-left origin,
/// y down) and `tints` the matching per-vertex RGBA, interpolated across
/// the face. The uv is pinned to the texture centre: this path draws the
/// reserved flat white texture, so the sample carries no detail the
/// tint does not already state. Cull mode is off, so either winding
/// draws.
pub fn push_screen_triangle_vertices(out: &mut Vec<u8>, positions: [[f32; 2]; 3], tints: [[f32; 4]; 3]) {
    for (position, tint) in positions.into_iter().zip(tints) {
        push_overlay_vertex(out, [0.0; 3], position, [0.5, 0.5], tint, 0.0, true);
    }
}

/// Push the three vertices of one world-anchored triangle into `out` —
/// the same expansion as [`push_screen_triangle_vertices`] with the
/// corners read as pixel offsets from `anchor`, and `k` the world scale
/// factor the quad overlay uses (`k < 0` Pixels mode, `k > 0` the
/// Distance-mode reference distance). The world counterpart a gauge or a
/// graph edge hanging off a point in the world is drawn through.
pub fn push_world_triangle_vertices(
    out: &mut Vec<u8>,
    anchor: [f32; 3],
    positions: [[f32; 2]; 3],
    tints: [[f32; 4]; 3],
    k: f32,
) {
    for (position, tint) in positions.into_iter().zip(tints) {
        push_overlay_vertex(out, anchor, position, [0.5, 0.5], tint, k, false);
    }
}

/// Write one overlay vertex into `out` in the unified world-aware
/// layout: `anchor vec3`, `offset_px vec2`, `uv vec2`, `tint vec4`,
/// `k f32`, `is_screen u32` — [`QUAD_VERTEX_STRIDE`] bytes. The single
/// writer for every overlay path, so the byte layout the pipeline's
/// `VertexBufferLayout` describes is stated once.
fn push_overlay_vertex(
    out: &mut Vec<u8>,
    anchor: [f32; 3],
    offset_px: [f32; 2],
    uv: [f32; 2],
    tint: [f32; 4],
    k: f32,
    is_screen: bool,
) {
    let floats: [f32; 12] = [
        anchor[0],
        anchor[1],
        anchor[2],
        offset_px[0],
        offset_px[1],
        uv[0],
        uv[1],
        tint[0],
        tint[1],
        tint[2],
        tint[3],
        k,
    ];
    out.extend_from_slice(bytemuck::cast_slice(&floats));
    out.extend_from_slice(bytemuck::cast_slice(&[u32::from(is_screen)]));
}

/// Push the six vertices (two triangles) for one world-anchored quad
/// into `out` as raw bytes — each vertex is 52 bytes
/// ([`QUAD_VERTEX_STRIDE`]) in the unified world-aware layout: `anchor
/// vec3` (world-space anchor, same for all six vertices), `offset_px
/// vec2` (pixel offset from the projected anchor in screen y-down
/// convention), `uv vec2`, `tint vec4`, `k f32` (scale factor), and
/// `is_screen u32` (0). `rect` is `[x, y, width, height]` (top-left
/// pixel offset from anchor + pixel size); `uv` is `[u0, v0, u1, v1]`;
/// `tint` is the per-vertex RGBA multiplier. `k < 0` selects Pixels
/// mode (shader uses `clip.w`, constant on-screen size); `k > 0` is the
/// reference distance for Distance mode (label holds its size at that
/// depth).
pub fn push_world_quad_vertices(
    out: &mut Vec<u8>,
    anchor: [f32; 3],
    rect: [f32; 4],
    uv: [f32; 4],
    tint: [f32; 4],
    k: f32,
) {
    let [x, y, width, height] = rect;
    let [u0, v0, u1, v1] = uv;
    let x0 = x;
    let y0 = y;
    let x1 = x + width;
    let y1 = y + height;
    let corners =
        [(x0, y0, u0, v0), (x0, y1, u0, v1), (x1, y1, u1, v1), (x0, y0, u0, v0), (x1, y1, u1, v1), (x1, y0, u1, v0)];
    for (ox, oy, u, v) in corners {
        push_overlay_vertex(out, anchor, [ox, oy], [u, v], tint, k, false);
    }
}

/// Record the overlay pass: upload `vertex_bytes` (quads and triangles)
/// and `shape_vertex_bytes` (ADR-0213 shapes) + the `view_proj` /
/// `viewport` uniform, then draw each `OverlayDraw` range through the
/// pipeline its source selects into the offscreen color target. The pass
/// loads (does not clear) the existing color so the world pass beneath
/// shows through, and binds no depth target. Empty `draws` is a no-op;
/// either byte buffer exceeding its cap ([`QUAD_VERTEX_BUFFER_BYTES`] /
/// [`SHAPE_VERTEX_BUFFER_BYTES`]) drops the pass with a warn. `view_proj`
/// is column-major — the World paths transform anchors through it in
/// the vertex shader.
// Nine arguments mirror the same all-in-one pattern `record_main_pass`
// uses; bundling into a struct here for one call site adds no clarity.
#[allow(clippy::too_many_arguments)]
pub fn record_quad_overlay_pass(
    queue: &wgpu::Queue,
    encoder: &mut wgpu::CommandEncoder,
    pipeline: &QuadPipeline,
    targets: &Targets,
    vertex_bytes: &[u8],
    shape_vertex_bytes: &[u8],
    draws: &[OverlayDraw<'_>],
    viewport: [f32; 2],
    view_proj: [f32; 16],
) {
    if draws.is_empty() || (vertex_bytes.is_empty() && shape_vertex_bytes.is_empty()) {
        return;
    }
    if vertex_bytes.len() > QUAD_VERTEX_BUFFER_BYTES {
        tracing::warn!(
            target: "aether_substrate::render",
            vertex_bytes = vertex_bytes.len(),
            cap = QUAD_VERTEX_BUFFER_BYTES,
            "dropping overlay pass: quad vertex bytes exceed fixed buffer",
        );
        return;
    }
    if shape_vertex_bytes.len() > SHAPE_VERTEX_BUFFER_BYTES {
        tracing::warn!(
            target: "aether_substrate::render",
            shape_vertex_bytes = shape_vertex_bytes.len(),
            cap = SHAPE_VERTEX_BUFFER_BYTES,
            "dropping overlay pass: shape vertex bytes exceed fixed buffer",
        );
        return;
    }
    if !vertex_bytes.is_empty() {
        queue.write_buffer(&pipeline.vertex_buffer, 0, vertex_bytes);
    }
    if !shape_vertex_bytes.is_empty() {
        queue.write_buffer(&pipeline.shape.vertex_buffer, 0, shape_vertex_bytes);
    }
    // Viewport uniform: view_proj (16 f32 = 64 bytes) + size (2 f32 =
    // 8 bytes) + pad (2 f32 = 8 bytes) = 80 bytes total.
    let mut uniform = [0f32; 20];
    uniform[..16].copy_from_slice(&view_proj);
    uniform[16] = viewport[0];
    uniform[17] = viewport[1];
    queue.write_buffer(&pipeline.viewport_buffer, 0, bytemuck::cast_slice(&uniform));

    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("aether quad overlay pass"),
        color_attachments: &[Some(super::load_color_attachment(targets.msaa_view()))],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
        multiview_mask: None,
    });
    pass.set_bind_group(0, &pipeline.viewport_bind_group, &[]);
    for draw in draws {
        if draw.vertex_count == 0 {
            continue;
        }
        let Some(scissor) = clamped_scissor(draw.clip, targets.width(), targets.height()) else {
            continue;
        };
        // Each source reads its own vertex buffer; the bind is re-stated
        // per draw so painter order can interleave the two freely.
        match draw.source {
            OverlaySource::Textured { bind_group, blend } => {
                pass.set_pipeline(match blend {
                    CompositeBlend::Straight => &pipeline.straight,
                    CompositeBlend::Premultiplied => &pipeline.premultiplied,
                });
                pass.set_vertex_buffer(0, pipeline.vertex_buffer.slice(..vertex_bytes.len() as u64));
                pass.set_bind_group(1, bind_group, &[]);
            }
            OverlaySource::Shapes { texture } => {
                pass.set_vertex_buffer(0, pipeline.shape.vertex_buffer.slice(..shape_vertex_bytes.len() as u64));
                if let Some(bind_group) = texture {
                    pass.set_pipeline(&pipeline.shape.textured);
                    pass.set_bind_group(1, bind_group, &[]);
                } else {
                    pass.set_pipeline(&pipeline.shape.plain);
                }
            }
        }
        pass.set_scissor_rect(scissor[0], scissor[1], scissor[2], scissor[3]);
        pass.draw(draw.first_vertex..draw.first_vertex + draw.vertex_count, 0..1);
    }
}

/// The scissor rect a draw's optional clip names, or `None` when the
/// clip covers no pixel of the target and the draw should be skipped.
/// An absent clip is the whole target.
///
/// `aether-render`'s observation sink applies the same contract through
/// the same [`Rect2::clamp_to_pixels`], so a harness-reported batch and
/// a GPU-recorded one cannot disagree about what survives.
fn clamped_scissor(clip: Option<[f32; 4]>, target_width: u32, target_height: u32) -> Option<[u32; 4]> {
    let Some([x, y, width, height]) = clip else {
        return Some([0, 0, target_width, target_height]);
    };
    Rect2::from_xywh(x, y, width, height).clamp_to_pixels(target_width, target_height)
}
