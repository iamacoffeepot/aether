//! Depth-tested world-space material pass (ADR-0140). A sibling pass
//! between the main triangle pass and the screen overlay: it samples
//! registered render textures, tests against the main pass depth buffer,
//! leaves depth writes off, and alpha-blends into the shared offscreen
//! color target.

use super::targets::Targets;
use super::{DEPTH_FORMAT, Pipeline};
use crate::render::TextureBindings;
use std::slice;

pub const MATERIAL_VERTEX_STRIDE: u64 = 20;
pub const MATERIAL_VERTICES_PER_RECT: usize = 6;
pub const MATERIAL_VERTEX_BUFFER_BYTES: usize = 4 * 1024 * 1024;
const MATERIAL_PARAMS_BUFFER_BYTES: u64 = 1024 * 1024;
const PARAMS_ALIGN: usize = 256;
const MATERIAL_PARAMS_BYTES: usize = 64;

pub const MATERIAL_SHADER_WGSL: &str = include_str!("material.wgsl");

#[allow(clippy::struct_field_names)]
pub struct MaterialPipelines {
    textured: wgpu::RenderPipeline,
    coverage: wgpu::RenderPipeline,
    vertex_buffer: wgpu::Buffer,
    textured_params_buffer: wgpu::Buffer,
    textured_params_bind_group: wgpu::BindGroup,
    coverage_params_buffer: wgpu::Buffer,
    coverage_params_bind_group: wgpu::BindGroup,
}

pub struct MaterialDraw<'a> {
    pub bind_group: &'a wgpu::BindGroup,
    pub first_vertex: u32,
    pub vertex_count: u32,
    pub params_offset: u32,
}

pub enum MaterialPassDraw<'a> {
    Textured(MaterialDraw<'a>),
    Coverage(MaterialDraw<'a>),
}

#[must_use]
pub fn build_material_pipelines(
    device: &wgpu::Device,
    color_format: wgpu::TextureFormat,
    camera_layout: &wgpu::BindGroupLayout,
    texture_bindings: &TextureBindings,
) -> MaterialPipelines {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("aether material shader"),
        source: wgpu::ShaderSource::Wgsl(MATERIAL_SHADER_WGSL.into()),
    });

    let textured_params_layout =
        material_params_layout(device, "material textured params bind group layout", MATERIAL_PARAMS_BYTES as u64);
    let coverage_params_layout =
        material_params_layout(device, "material coverage params bind group layout", MATERIAL_PARAMS_BYTES as u64);

    let vertex_layout = material_vertex_layout();
    let textured_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("aether textured material pipeline layout"),
        bind_group_layouts: &[Some(camera_layout), Some(&texture_bindings.layout), Some(&textured_params_layout)],
        immediate_size: 0,
    });
    let coverage_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("aether coverage material pipeline layout"),
        bind_group_layouts: &[Some(camera_layout), Some(&texture_bindings.layout), Some(&coverage_params_layout)],
        immediate_size: 0,
    });

    let textured = material_pipeline(
        device,
        &shader,
        &textured_layout,
        "aether textured material pipeline",
        color_format,
        "fs_textured",
        &vertex_layout,
    );
    let coverage = material_pipeline(
        device,
        &shader,
        &coverage_layout,
        "aether coverage material pipeline",
        color_format,
        "fs_coverage",
        &vertex_layout,
    );

    let vertex_buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("aether material vertex buffer"),
        size: MATERIAL_VERTEX_BUFFER_BYTES as u64,
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let textured_params_buffer = material_params_buffer(device, "aether textured params buffer");
    let coverage_params_buffer = material_params_buffer(device, "aether coverage params buffer");
    let textured_params_bind_group = material_params_bind_group(
        device,
        "aether textured params bind group",
        &textured_params_layout,
        &textured_params_buffer,
        MATERIAL_PARAMS_BYTES as u64,
    );
    let coverage_params_bind_group = material_params_bind_group(
        device,
        "aether coverage params bind group",
        &coverage_params_layout,
        &coverage_params_buffer,
        MATERIAL_PARAMS_BYTES as u64,
    );

    MaterialPipelines {
        textured,
        coverage,
        vertex_buffer,
        textured_params_buffer,
        textured_params_bind_group,
        coverage_params_buffer,
        coverage_params_bind_group,
    }
}

fn material_params_layout(device: &wgpu::Device, label: &'static str, min_binding_size: u64) -> wgpu::BindGroupLayout {
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some(label),
        entries: &[wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: true,
                min_binding_size: wgpu::BufferSize::new(min_binding_size),
            },
            count: None,
        }],
    })
}

fn material_params_buffer(device: &wgpu::Device, label: &'static str) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size: MATERIAL_PARAMS_BUFFER_BYTES,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

fn material_params_bind_group(
    device: &wgpu::Device,
    label: &'static str,
    layout: &wgpu::BindGroupLayout,
    buffer: &wgpu::Buffer,
    size: u64,
) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some(label),
        layout,
        entries: &[wgpu::BindGroupEntry {
            binding: 0,
            resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                buffer,
                offset: 0,
                size: wgpu::BufferSize::new(size),
            }),
        }],
    })
}

//noinspection DuplicatedCode -- material UVs and the main pipeline's colors require distinct layouts.
fn material_vertex_layout() -> wgpu::VertexBufferLayout<'static> {
    const ATTRIBUTES: &[wgpu::VertexAttribute] = &[
        wgpu::VertexAttribute { offset: 0, shader_location: 0, format: wgpu::VertexFormat::Float32x3 },
        wgpu::VertexAttribute { offset: 12, shader_location: 1, format: wgpu::VertexFormat::Float32x2 },
    ];
    super::vertex_layout(MATERIAL_VERTEX_STRIDE, ATTRIBUTES)
}

fn material_pipeline(
    device: &wgpu::Device,
    shader: &wgpu::ShaderModule,
    layout: &wgpu::PipelineLayout,
    label: &'static str,
    color_format: wgpu::TextureFormat,
    fragment_entry: &'static str,
    vertex_layout: &wgpu::VertexBufferLayout<'_>,
) -> wgpu::RenderPipeline {
    let fragment_targets = [Some(super::color_target_state(color_format, wgpu::BlendState::ALPHA_BLENDING))];
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(label),
        layout: Some(layout),
        vertex: wgpu::VertexState {
            module: shader,
            entry_point: Some("vs_main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            buffers: slice::from_ref(vertex_layout),
        },
        fragment: Some(super::fragment_state(shader, fragment_entry, &fragment_targets)),
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            strip_index_format: None,
            front_face: wgpu::FrontFace::Ccw,
            cull_mode: None,
            polygon_mode: wgpu::PolygonMode::Fill,
            unclipped_depth: false,
            conservative: false,
        },
        depth_stencil: Some(wgpu::DepthStencilState {
            format: DEPTH_FORMAT,
            depth_write_enabled: Some(false),
            depth_compare: Some(wgpu::CompareFunction::LessEqual),
            stencil: wgpu::StencilState::default(),
            bias: wgpu::DepthBiasState::default(),
        }),
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    })
}

pub fn material_params_offset(index: usize) -> Option<u32> {
    index.checked_mul(PARAMS_ALIGN).and_then(|offset| u32::try_from(offset).ok())
}

pub fn push_material_rect_vertices(
    out: &mut Vec<u8>,
    origin: [f32; 3],
    right: [f32; 3],
    up: [f32; 3],
    size: [f32; 2],
    uv: [f32; 4],
) {
    let [u0, v0, u1, v1] = uv;
    let corner = |u: f32, v: f32| {
        [
            origin[0] + right[0] * size[0] * u + up[0] * size[1] * v,
            origin[1] + right[1] * size[0] * u + up[1] * size[1] * v,
            origin[2] + right[2] * size[0] * u + up[2] * size[1] * v,
        ]
    };
    let corners = [
        (corner(0.0, 0.0), u0, v0),
        (corner(0.0, 1.0), u0, v1),
        (corner(1.0, 1.0), u1, v1),
        (corner(0.0, 0.0), u0, v0),
        (corner(1.0, 1.0), u1, v1),
        (corner(1.0, 0.0), u1, v0),
    ];
    for (position, u, v) in corners {
        let floats: [f32; 5] = [position[0], position[1], position[2], u, v];
        out.extend_from_slice(bytemuck::cast_slice(&floats));
    }
}

pub fn push_textured_params(out: &mut Vec<u8>, tint: [f32; 4]) -> Option<u32> {
    let offset = material_params_offset(out.len() / PARAMS_ALIGN)?;
    out.extend_from_slice(bytemuck::cast_slice(&tint));
    out.extend_from_slice(bytemuck::cast_slice(&[0.0_f32; 12]));
    pad_params(out);
    Some(offset)
}

pub fn push_coverage_params(
    out: &mut Vec<u8>,
    body_color: [f32; 4],
    rim_color: [f32; 4],
    rim_width: f32,
) -> Option<u32> {
    let offset = material_params_offset(out.len() / PARAMS_ALIGN)?;
    out.extend_from_slice(bytemuck::cast_slice(&body_color));
    out.extend_from_slice(bytemuck::cast_slice(&rim_color));
    out.extend_from_slice(bytemuck::cast_slice(&[rim_width, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]));
    pad_params(out);
    Some(offset)
}

fn pad_params(out: &mut Vec<u8>) {
    let padding = (PARAMS_ALIGN - (out.len() % PARAMS_ALIGN)) % PARAMS_ALIGN;
    out.resize(out.len() + padding, 0);
}

#[derive(Clone, Copy)]
pub struct MaterialPassRecord<'a> {
    pub queue: &'a wgpu::Queue,
    pub pipeline: &'a MaterialPipelines,
    pub main_pipeline: &'a Pipeline,
    pub targets: &'a Targets,
    pub vertex_bytes: &'a [u8],
    pub draws: &'a [MaterialPassDraw<'a>],
    pub textured_params: &'a [u8],
    pub coverage_params: &'a [u8],
}

pub fn record_material_pass(encoder: &mut wgpu::CommandEncoder, record: MaterialPassRecord<'_>) {
    let MaterialPassRecord {
        queue,
        pipeline,
        main_pipeline,
        targets,
        vertex_bytes,
        draws,
        textured_params,
        coverage_params,
    } = record;
    if vertex_bytes.is_empty() || draws.is_empty() {
        return;
    }
    if vertex_bytes.len() > MATERIAL_VERTEX_BUFFER_BYTES {
        tracing::warn!(
            target: "aether_substrate::render",
            vertex_bytes = vertex_bytes.len(),
            cap = MATERIAL_VERTEX_BUFFER_BYTES,
            "dropping material pass: vertex bytes exceed fixed buffer",
        );
        return;
    }
    if textured_params.len() as u64 > MATERIAL_PARAMS_BUFFER_BYTES
        || coverage_params.len() as u64 > MATERIAL_PARAMS_BUFFER_BYTES
    {
        tracing::warn!(
            target: "aether_substrate::render",
            textured_params_bytes = textured_params.len(),
            coverage_params_bytes = coverage_params.len(),
            cap = MATERIAL_PARAMS_BUFFER_BYTES,
            "dropping material pass: parameter bytes exceed fixed buffers",
        );
        return;
    }

    queue.write_buffer(&pipeline.vertex_buffer, 0, vertex_bytes);
    if !textured_params.is_empty() {
        queue.write_buffer(&pipeline.textured_params_buffer, 0, textured_params);
    }
    if !coverage_params.is_empty() {
        queue.write_buffer(&pipeline.coverage_params_buffer, 0, coverage_params);
    }

    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("aether material pass"),
        color_attachments: &[Some(super::load_color_attachment(targets.color_view()))],
        depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
            view: &targets.depth.view,
            depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store }),
            stencil_ops: None,
        }),
        timestamp_writes: None,
        occlusion_query_set: None,
        multiview_mask: None,
    });
    pass.set_vertex_buffer(0, pipeline.vertex_buffer.slice(..vertex_bytes.len() as u64));
    pass.set_bind_group(0, &main_pipeline.camera_bind_group, &[]);
    for draw in draws {
        match draw {
            MaterialPassDraw::Textured(draw) => {
                pass.set_pipeline(&pipeline.textured);
                pass.set_bind_group(1, draw.bind_group, &[]);
                pass.set_bind_group(2, &pipeline.textured_params_bind_group, &[draw.params_offset]);
                pass.draw(draw.first_vertex..draw.first_vertex + draw.vertex_count, 0..1);
            }
            MaterialPassDraw::Coverage(draw) => {
                pass.set_pipeline(&pipeline.coverage);
                pass.set_bind_group(1, draw.bind_group, &[]);
                pass.set_bind_group(2, &pipeline.coverage_params_bind_group, &[draw.params_offset]);
                pass.draw(draw.first_vertex..draw.first_vertex + draw.vertex_count, 0..1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn material_rect_vertices_have_expected_stride_and_corners() {
        // Tripwire: the world-axis basis must reproduce the flat XY-plane
        // expansion byte for byte — every draped caller depends on it.
        let mut bytes = Vec::new();
        push_material_rect_vertices(
            &mut bytes,
            [1.0, 2.0, 0.5],
            [1.0, 0.0, 0.0],
            [0.0, 1.0, 0.0],
            [3.0, 4.0],
            [0.1, 0.2, 0.8, 0.9],
        );
        let vertex_stride = usize::try_from(MATERIAL_VERTEX_STRIDE).expect("material vertex stride fits usize");
        assert_eq!(bytes.len(), MATERIAL_VERTICES_PER_RECT * vertex_stride);
        let floats: &[f32] = bytemuck::cast_slice(&bytes);
        assert_eq!(&floats[0..5], &[1.0, 2.0, 0.5, 0.1, 0.2]);
        assert_eq!(&floats[5..10], &[1.0, 6.0, 0.5, 0.1, 0.9]);
        assert_eq!(&floats[10..15], &[4.0, 6.0, 0.5, 0.8, 0.9]);
        assert_eq!(&floats[25..30], &[4.0, 2.0, 0.5, 0.8, 0.2]);
    }

    #[test]
    fn material_rect_vertices_extend_along_the_given_basis() {
        let mut bytes = Vec::new();
        push_material_rect_vertices(
            &mut bytes,
            [1.0, 2.0, 0.5],
            [0.0, 0.0, 1.0],
            [0.0, 1.0, 0.0],
            [3.0, 4.0],
            [0.0, 0.0, 1.0, 1.0],
        );
        let floats: &[f32] = bytemuck::cast_slice(&bytes);
        assert_eq!(&floats[0..3], &[1.0, 2.0, 0.5]);
        assert_eq!(&floats[5..8], &[1.0, 6.0, 0.5]);
        assert_eq!(&floats[10..13], &[1.0, 6.0, 3.5]);
        assert_eq!(&floats[25..28], &[1.0, 2.0, 3.5]);
    }

    #[test]
    fn material_params_are_aligned_for_dynamic_uniform_offsets() {
        let mut bytes = Vec::new();
        let first = push_textured_params(&mut bytes, [1.0, 0.0, 0.0, 1.0]).expect("first textured params offset");
        let second = push_textured_params(&mut bytes, [0.0, 1.0, 0.0, 1.0]).expect("second textured params offset");
        let params_align = u32::try_from(PARAMS_ALIGN).expect("material params alignment fits u32");
        assert_eq!(first, 0);
        assert_eq!(second, params_align);
        assert_eq!(bytes.len(), PARAMS_ALIGN * 2);
    }
}
