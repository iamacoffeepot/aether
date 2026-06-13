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
//! Texture realization is lazy: the render capability stages RGBA8
//! pixels CPU-side at `create_texture` time and calls [`realize_texture`]
//! / [`upload_texture_full`] at record time, when a device + queue are
//! available. The realized [`RealizedTexture`] carries the wgpu texture
//! plus the group-1 bind group built against this pipeline's layout.

use super::targets::Targets;
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

/// Owned GPU state for the quad overlay pipeline: the render pipeline,
/// the per-frame vertex buffer, the viewport uniform + its bind group
/// (group 0), the shared sampler, and the group-1 (texture + sampler)
/// bind group layout that [`realize_texture`] builds per-texture bind
/// groups against. Built once at chassis boot via
/// [`build_quad_pipeline`].
#[allow(clippy::struct_field_names)]
pub struct QuadPipeline {
    pipeline: wgpu::RenderPipeline,
    vertex_buffer: wgpu::Buffer,
    viewport_buffer: wgpu::Buffer,
    viewport_bind_group: wgpu::BindGroup,
    sampler: wgpu::Sampler,
    /// Layout for group 1 (texture view at binding 0, sampler at
    /// binding 1). Retained so the render cap can build a bind group per
    /// realized texture without re-deriving the layout.
    texture_bind_group_layout: wgpu::BindGroupLayout,
}

/// A texture realized on the GPU plus its group-1 bind group, built
/// against a [`QuadPipeline`]'s `texture_bind_group_layout` + sampler.
/// The render cap caches one of these per registered texture and
/// re-uploads its pixels via [`upload_texture_full`] when the staged
/// CPU pixels change.
pub struct RealizedTexture {
    texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    width: u32,
    height: u32,
}

impl RealizedTexture {
    /// The group-1 bind group to set before drawing quads that sample
    /// this texture.
    #[must_use]
    pub fn bind_group(&self) -> &wgpu::BindGroup {
        &self.bind_group
    }
}

/// One draw inside the overlay pass: the group-1 bind group for the
/// batch's texture, and the vertex sub-range (in vertices, not bytes)
/// the batch's expanded quads occupy in the shared vertex buffer.
pub struct OverlayDraw<'a> {
    pub bind_group: &'a wgpu::BindGroup,
    pub first_vertex: u32,
    pub vertex_count: u32,
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
) -> QuadPipeline {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("aether quad shader"),
        source: wgpu::ShaderSource::Wgsl(QUAD_SHADER_WGSL.into()),
    });

    let viewport_bind_group_layout =
        device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("quad viewport bind group layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: wgpu::BufferSize::new(QUAD_UNIFORM_BYTES),
                },
                count: None,
            }],
        });

    let texture_bind_group_layout =
        device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("quad texture bind group layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

    let viewport_buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("quad viewport uniform"),
        size: QUAD_UNIFORM_BYTES,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });

    let viewport_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("quad viewport bind group"),
        layout: &viewport_bind_group_layout,
        entries: &[wgpu::BindGroupEntry {
            binding: 0,
            resource: viewport_buffer.as_entire_binding(),
        }],
    });

    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("quad sampler"),
        address_mode_u: wgpu::AddressMode::ClampToEdge,
        address_mode_v: wgpu::AddressMode::ClampToEdge,
        address_mode_w: wgpu::AddressMode::ClampToEdge,
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        mipmap_filter: wgpu::MipmapFilterMode::Nearest,
        ..Default::default()
    });

    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("aether quad pipeline layout"),
        bind_group_layouts: &[
            Some(&viewport_bind_group_layout),
            Some(&texture_bind_group_layout),
        ],
        immediate_size: 0,
    });

    let vertex_layout = wgpu::VertexBufferLayout {
        array_stride: QUAD_VERTEX_STRIDE,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes: &[
            // anchor: vec3<f32> at offset 0
            wgpu::VertexAttribute {
                offset: 0,
                shader_location: 0,
                format: wgpu::VertexFormat::Float32x3,
            },
            // offset_px: vec2<f32> at offset 12
            wgpu::VertexAttribute {
                offset: 12,
                shader_location: 1,
                format: wgpu::VertexFormat::Float32x2,
            },
            // uv: vec2<f32> at offset 20
            wgpu::VertexAttribute {
                offset: 20,
                shader_location: 2,
                format: wgpu::VertexFormat::Float32x2,
            },
            // tint: vec4<f32> at offset 28
            wgpu::VertexAttribute {
                offset: 28,
                shader_location: 3,
                format: wgpu::VertexFormat::Float32x4,
            },
            // k: f32 at offset 44
            wgpu::VertexAttribute {
                offset: 44,
                shader_location: 4,
                format: wgpu::VertexFormat::Float32,
            },
            // is_screen: u32 at offset 48
            wgpu::VertexAttribute {
                offset: 48,
                shader_location: 5,
                format: wgpu::VertexFormat::Uint32,
            },
        ],
    };

    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("aether quad pipeline"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs_main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            buffers: slice::from_ref(&vertex_layout),
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs_main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format: color_format,
                blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
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
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    });

    let vertex_buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("aether quad vertex buffer"),
        size: QUAD_VERTEX_BUFFER_BYTES as u64,
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });

    QuadPipeline {
        pipeline,
        vertex_buffer,
        viewport_buffer,
        viewport_bind_group,
        sampler,
        texture_bind_group_layout,
    }
}

/// Create a GPU texture from staged RGBA8 `pixels` and build its group-1
/// bind group against `pipeline`'s texture layout + sampler. `pixels`
/// must be exactly `width * height * 4` bytes (the render cap validates
/// this at `create_texture` time). Pair with [`upload_texture_full`] to
/// refresh the pixels later without rebuilding the bind group.
#[must_use]
pub fn realize_texture(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    pipeline: &QuadPipeline,
    width: u32,
    height: u32,
    pixels: &[u8],
) -> RealizedTexture {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("aether quad texture"),
        size: wgpu::Extent3d {
            width: width.max(1),
            height: height.max(1),
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("aether quad texture bind group"),
        layout: &pipeline.texture_bind_group_layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(&pipeline.sampler),
            },
        ],
    });
    let realized = RealizedTexture {
        texture,
        bind_group,
        width,
        height,
    };
    upload_texture_full(queue, &realized, pixels);
    realized
}

/// Re-upload the full staged `pixels` into an already-realized texture.
/// Used when an `update_texture` mail changed the staged CPU pixels: the
/// render cap re-uploads the whole texture at the next record rather
/// than tracking dirty sub-rects on the GPU. `pixels` must be
/// `width * height * 4` bytes.
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
            bytes_per_row: Some(realized.width.max(1) * 4),
            rows_per_image: Some(realized.height.max(1)),
        },
        wgpu::Extent3d {
            width: realized.width.max(1),
            height: realized.height.max(1),
            depth_or_array_layers: 1,
        },
    );
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
    let corners = [
        (x0, y0, u0, v0),
        (x0, y1, u0, v1),
        (x1, y1, u1, v1),
        (x0, y0, u0, v0),
        (x1, y1, u1, v1),
        (x1, y0, u1, v0),
    ];
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
    let corners = [
        (x0, y0, u0, v0),
        (x0, y1, u0, v1),
        (x1, y1, u1, v1),
        (x0, y0, u0, v0),
        (x1, y1, u1, v1),
        (x1, y0, u1, v0),
    ];
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
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view: targets.color_view(),
            resolve_target: None,
            depth_slice: None,
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Load,
                store: wgpu::StoreOp::Store,
            },
        })],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
        multiview_mask: None,
    });
    pass.set_pipeline(&pipeline.pipeline);
    pass.set_bind_group(0, &pipeline.viewport_bind_group, &[]);
    pass.set_vertex_buffer(0, pipeline.vertex_buffer.slice(..vertex_bytes.len() as u64));
    for draw in draws {
        if draw.vertex_count == 0 {
            continue;
        }
        pass.set_bind_group(1, draw.bind_group, &[]);
        pass.draw(
            draw.first_vertex..draw.first_vertex + draw.vertex_count,
            0..1,
        );
    }
}
