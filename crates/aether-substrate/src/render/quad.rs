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

use super::targets::Targets;
use std::iter;
use std::slice;

/// Bytes per expanded quad vertex: `anchor vec3<f32>` (12) +
/// `offset_px vec2<f32>` (8) + `uv vec2<f32>` (8) + `tint vec4<f32>`
/// (16) + `k f32` (4) + `is_screen u32` (4) = 52.
/// [`push_screen_quad_vertices`] and [`push_world_quad_vertices`] both
/// write exactly this stride per vertex.
pub const QUAD_VERTEX_STRIDE: u64 = 52;

/// Vertices one quad expands to: two triangles, six vertices.
pub const QUAD_VERTICES_PER_QUAD: usize = 6;

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

/// Owned GPU state for the quad overlay pipeline: the render pipeline,
/// the per-frame vertex buffer, and the viewport uniform + its bind
/// group (group 0). Texture group-1 state is supplied by
/// [`TextureBindings`].
#[allow(clippy::struct_field_names)]
pub struct QuadPipeline {
    straight: wgpu::RenderPipeline,
    premultiplied: wgpu::RenderPipeline,
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

/// One draw inside the overlay pass: the group-1 bind group for the
/// batch's texture, and the vertex sub-range (in vertices, not bytes)
/// the batch's expanded quads occupy in the shared vertex buffer.
pub struct OverlayDraw<'a> {
    pub bind_group: &'a wgpu::BindGroup,
    pub first_vertex: u32,
    pub vertex_count: u32,
    /// Optional framebuffer-pixel scissor: `[x, y, width, height]`.
    pub clip: Option<[f32; 4]>,
    pub blend: CompositeBlend,
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
#[allow(clippy::too_many_lines)]
#[must_use]
pub fn build_quad_pipeline(
    device: &wgpu::Device,
    color_format: wgpu::TextureFormat,
    texture_bindings: &TextureBindings,
) -> QuadPipeline {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("aether quad shader"),
        source: wgpu::ShaderSource::Wgsl(QUAD_SHADER_WGSL.into()),
    });

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
    // pipeline object and nothing at record time but a rebind.
    let build = |label, blend| {
        let fragment_targets = [Some(super::color_target_state(color_format, blend))];
        device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some(label),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: slice::from_ref(&vertex_layout),
            },
            fragment: Some(super::fragment_state(&shader, "fs_main", &fragment_targets)),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                // Quads are authored as two triangles in a fixed winding;
                // overlay UI shouldn't be culled by face orientation.
                cull_mode: None,
                polygon_mode: wgpu::PolygonMode::Fill,
                unclipped_depth: false,
                conservative: false,
            },
            // Overlay quads draw on top of the world pass with no depth
            // interaction at all — the main pass already resolved depth.
            depth_stencil: None,
            multisample: wgpu::MultisampleState {
                count: super::MSAA_SAMPLE_COUNT,
                ..wgpu::MultisampleState::default()
            },
            multiview_mask: None,
            cache: None,
        })
    };
    let straight = build("aether quad pipeline", wgpu::BlendState::ALPHA_BLENDING);
    let premultiplied = build("aether quad premultiplied pipeline", wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING);

    let vertex_buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("aether quad vertex buffer"),
        size: QUAD_VERTEX_BUFFER_BYTES as u64,
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });

    QuadPipeline { straight, premultiplied, vertex_buffer, viewport_buffer, viewport_bind_group }
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
        // anchor (0,0,0) + offset_px (pixel pos) + uv + tint + k=0
        let floats: [f32; 12] = [
            0.0, 0.0, 0.0, // anchor (unused on screen path)
            px, py, // offset_px: absolute pixel position
            u, v, // uv
            tint[0], tint[1], tint[2], tint[3], // tint
            0.0,     // k (unused on screen path)
        ];
        out.extend_from_slice(bytemuck::cast_slice(&floats));
        let is_screen: u32 = 1;
        out.extend_from_slice(bytemuck::cast_slice(&[is_screen]));
    }
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
        let floats: [f32; 12] = [
            anchor[0], anchor[1], anchor[2], // anchor: world-space point
            ox, oy, // offset_px: pixel offset (y-down)
            u, v, // uv
            tint[0], tint[1], tint[2], tint[3], // tint
            k,       // scale factor
        ];
        out.extend_from_slice(bytemuck::cast_slice(&floats));
        let is_screen: u32 = 0;
        out.extend_from_slice(bytemuck::cast_slice(&[is_screen]));
    }
}

/// Record the overlay pass: upload `vertex_bytes` + the `view_proj` /
/// `viewport` uniform, then draw each `OverlayDraw` range with its
/// texture bind group into the offscreen color target. The pass loads
/// (does not clear) the existing color so the world pass beneath shows
/// through, and binds no depth target. Empty `draws` is a no-op;
/// `vertex_bytes` exceeding [`QUAD_VERTEX_BUFFER_BYTES`] drops the pass
/// with a warn. `view_proj` is column-major — the World quad path
/// transforms anchors through it in the vertex shader.
// Eight arguments mirror the same all-in-one pattern `record_main_pass`
// uses; bundling into a struct here for one call site adds no clarity.
#[allow(clippy::too_many_arguments)]
pub fn record_quad_overlay_pass(
    queue: &wgpu::Queue,
    encoder: &mut wgpu::CommandEncoder,
    pipeline: &QuadPipeline,
    targets: &Targets,
    vertex_bytes: &[u8],
    draws: &[OverlayDraw<'_>],
    viewport: [f32; 2],
    view_proj: [f32; 16],
) {
    if draws.is_empty() || vertex_bytes.is_empty() {
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
    queue.write_buffer(&pipeline.vertex_buffer, 0, vertex_bytes);
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
    pass.set_vertex_buffer(0, pipeline.vertex_buffer.slice(..vertex_bytes.len() as u64));
    for draw in draws {
        if draw.vertex_count == 0 {
            continue;
        }
        let Some(scissor) = clamped_scissor(draw.clip, targets.width(), targets.height()) else {
            continue;
        };
        pass.set_pipeline(match draw.blend {
            CompositeBlend::Straight => &pipeline.straight,
            CompositeBlend::Premultiplied => &pipeline.premultiplied,
        });
        pass.set_scissor_rect(scissor[0], scissor[1], scissor[2], scissor[3]);
        pass.set_bind_group(1, draw.bind_group, &[]);
        pass.draw(draw.first_vertex..draw.first_vertex + draw.vertex_count, 0..1);
    }
}

#[allow(clippy::cast_precision_loss)]
fn clamped_scissor(clip: Option<[f32; 4]>, target_width: u32, target_height: u32) -> Option<[u32; 4]> {
    let Some([x, y, width, height]) = clip else {
        return Some([0, 0, target_width, target_height]);
    };
    if !x.is_finite() || !y.is_finite() || !width.is_finite() || !height.is_finite() {
        return None;
    }
    let min_x = x.max(0.0).min(target_width as f32).floor();
    let min_y = y.max(0.0).min(target_height as f32).floor();
    let max_x = (x + width).max(0.0).min(target_width as f32).ceil();
    let max_y = (y + height).max(0.0).min(target_height as f32).ceil();
    if max_x <= min_x || max_y <= min_y {
        return None;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Some([min_x as u32, min_y as u32, (max_x - min_x) as u32, (max_y - min_y) as u32])
}
