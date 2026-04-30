//! Shared offscreen render pipeline + the main draw pass body.
//!
//! Both desktop and test-bench use this pipeline for the bulk of
//! their rendering: a single `(pos, color)` vertex layout, camera
//! uniform bound at group 0, drawing into a paired offscreen color
//! target + `Depth32Float` depth target with `LessEqual` testing.
//! Desktop additionally builds its own wireframe-overlay pipeline
//! (using [`super::vertex_buffer_layout`] + [`super::MAIN_SHADER_WGSL`])
//! and runs it as an extra draw inside [`record_main_pass`].

use super::targets::Targets;
use super::{
    CAMERA_UNIFORM_BYTES, DEPTH_FORMAT, IDENTITY_VIEW_PROJ, MAIN_SHADER_WGSL, VERTEX_BUFFER_BYTES,
    VERTEX_STRIDE, vertex_buffer_layout,
};

/// Surfaceable failures from `record_main_pass`. Today there's only
/// the buffer-overflow case (frame dropped); reified as a `Result`
/// so callers can decide whether to log + continue or escalate.
#[derive(Debug)]
pub enum RenderError {
    /// Frame's vertex bytes exceed [`VERTEX_BUFFER_BYTES`]. The
    /// pass is skipped — no draw, no encoder writes. Render sinks
    /// already truncate before forwarding, so this is a belt-and-
    /// suspenders check; if a future caller bypasses the sink-side
    /// clamp this surfaces here instead of overflowing the GPU buffer.
    VertexBufferOverflow { vertex_bytes: usize, cap: usize },
}

/// Owned GPU pipeline state shared across chassis: the render
/// pipeline, the fixed vertex buffer, and the camera uniform + bind
/// group. Built once at chassis boot via [`build_main_pipeline`];
/// each frame's vertex blob and view-projection matrix are uploaded
/// via [`record_main_pass`].
pub struct Pipeline {
    pub(super) pipeline: wgpu::RenderPipeline,
    pub(super) vertex_buffer: wgpu::Buffer,
    pub(super) camera_buffer: wgpu::Buffer,
    pub(super) camera_bind_group: wgpu::BindGroup,
    /// Layout retained so chassis-side helpers (desktop's wireframe
    /// overlay) can build extra pipelines that share the camera bind
    /// group without re-deriving the layout.
    pub camera_bind_group_layout: wgpu::BindGroupLayout,
    /// Pipeline layout retained for the same reason — the wireframe
    /// overlay reuses it via `Some(&layout)` in its descriptor.
    pub pipeline_layout: wgpu::PipelineLayout,
}

/// Build the shared offscreen render pipeline. `color_format` matches
/// the [`Targets`] colour target the pass attaches to. `polygon_mode`
/// controls fill vs line at construction — desktop sets `Line` when
/// `AETHER_WIREFRAME=line`; everything else passes `Fill`.
pub fn build_main_pipeline(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    color_format: wgpu::TextureFormat,
    polygon_mode: wgpu::PolygonMode,
) -> Pipeline {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("aether main shader"),
        source: wgpu::ShaderSource::Wgsl(MAIN_SHADER_WGSL.into()),
    });

    let camera_bind_group_layout =
        device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("camera bind group layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: wgpu::BufferSize::new(CAMERA_UNIFORM_BYTES),
                },
                count: None,
            }],
        });

    let camera_buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("camera uniform"),
        size: CAMERA_UNIFORM_BYTES,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    queue.write_buffer(&camera_buffer, 0, bytemuck::cast_slice(&IDENTITY_VIEW_PROJ));

    let camera_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("camera bind group"),
        layout: &camera_bind_group_layout,
        entries: &[wgpu::BindGroupEntry {
            binding: 0,
            resource: camera_buffer.as_entire_binding(),
        }],
    });

    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("aether main pipeline layout"),
        bind_group_layouts: &[Some(&camera_bind_group_layout)],
        immediate_size: 0,
    });

    let vertex_layout = vertex_buffer_layout();
    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("aether main pipeline"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs_main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            buffers: std::slice::from_ref(&vertex_layout),
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs_main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format: color_format,
                blend: Some(wgpu::BlendState::REPLACE),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            strip_index_format: None,
            front_face: wgpu::FrontFace::Ccw,
            cull_mode: None,
            polygon_mode,
            unclipped_depth: false,
            conservative: false,
        },
        depth_stencil: Some(wgpu::DepthStencilState {
            format: DEPTH_FORMAT,
            depth_write_enabled: Some(true),
            depth_compare: Some(wgpu::CompareFunction::LessEqual),
            stencil: wgpu::StencilState::default(),
            bias: wgpu::DepthBiasState::default(),
        }),
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    });

    let vertex_buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("aether vertex buffer"),
        size: VERTEX_BUFFER_BYTES as u64,
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });

    Pipeline {
        pipeline,
        vertex_buffer,
        camera_buffer,
        camera_bind_group,
        camera_bind_group_layout,
        pipeline_layout,
    }
}

/// Upload the frame's `vertices` + `view_proj`, then draw them into
/// the offscreen target with `LessEqual` depth testing and the same
/// background clear color both chassis use.
///
/// `extra_pipelines`: optional pipelines drawn after the main one
/// inside the same render pass, sharing the same vertex range and
/// camera bind group. Desktop passes a wireframe overlay pipeline
/// here when `AETHER_WIREFRAME=overlay`; test-bench passes `&[]`.
///
/// Returns `Err(RenderError::VertexBufferOverflow)` if the frame's
/// bytes exceed [`VERTEX_BUFFER_BYTES`] — the pass is skipped, no
/// encoder writes happen, the caller decides whether to log and
/// continue (skipping submit) or short-circuit. Empty `vertices` is
/// fine: the clear still runs, no draw is issued.
pub fn record_main_pass(
    queue: &wgpu::Queue,
    encoder: &mut wgpu::CommandEncoder,
    pipeline: &Pipeline,
    targets: &Targets,
    vertices: &[u8],
    view_proj: &[f32; 16],
    extra_pipelines: &[&wgpu::RenderPipeline],
) -> Result<(), RenderError> {
    let vertex_bytes = vertices.len();
    if vertex_bytes > VERTEX_BUFFER_BYTES {
        tracing::warn!(
            target: "aether_substrate::render",
            vertex_bytes,
            cap = VERTEX_BUFFER_BYTES,
            "dropping frame: vertex bytes exceed fixed buffer",
        );
        return Err(RenderError::VertexBufferOverflow {
            vertex_bytes,
            cap: VERTEX_BUFFER_BYTES,
        });
    }
    if !vertices.is_empty() {
        queue.write_buffer(&pipeline.vertex_buffer, 0, vertices);
    }
    queue.write_buffer(&pipeline.camera_buffer, 0, bytemuck::cast_slice(view_proj));
    let vertex_count = (vertex_bytes as u64 / VERTEX_STRIDE) as u32;

    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("aether triangle pass"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view: targets.color_view(),
            resolve_target: None,
            depth_slice: None,
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Clear(wgpu::Color {
                    r: 0.05,
                    g: 0.07,
                    b: 0.12,
                    a: 1.0,
                }),
                store: wgpu::StoreOp::Store,
            },
        })],
        depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
            view: &targets.depth.view,
            depth_ops: Some(wgpu::Operations {
                load: wgpu::LoadOp::Clear(1.0),
                store: wgpu::StoreOp::Discard,
            }),
            stencil_ops: None,
        }),
        timestamp_writes: None,
        occlusion_query_set: None,
        multiview_mask: None,
    });
    if vertex_count > 0 {
        pass.set_pipeline(&pipeline.pipeline);
        pass.set_bind_group(0, &pipeline.camera_bind_group, &[]);
        pass.set_vertex_buffer(0, pipeline.vertex_buffer.slice(..vertex_bytes as u64));
        pass.draw(0..vertex_count, 0..1);
        for extra in extra_pipelines {
            pass.set_pipeline(extra);
            pass.draw(0..vertex_count, 0..1);
        }
    }
    Ok(())
}
