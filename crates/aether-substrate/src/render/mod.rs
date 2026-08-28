//! Shared offscreen render path for chassis that draw (issue 421).
//!
//! `aether-substrate-desktop` and `aether-harness-substrate` both
//! own a wgpu pipeline that draws `(pos, color)` triangles into an
//! offscreen color + depth target and (optionally) reads back a PNG
//! capture. ADR-0067 acknowledged the duplication; this module is
//! that extraction.
//!
//! The split is composable rather than monolithic: chassis still own
//! their own `Gpu` struct, but compose `Pipeline` + `Targets` from
//! here and call `record_main_pass` / `prepare_capture_copy` /
//! `finish_capture` as primitives. Surface acquisition + present +
//! desktop's wireframe overlay stay desktop-side; the offscreen
//! pipeline + capture readback live here.
//!
//! Gated by the `render` feature on `aether-substrate`. That gate does
//! not currently keep wgpu out of the chassis that don't draw:
//! `aether-chassis` enables the feature unconditionally and reaches
//! wgpu a second way through `aether-text` -> `aether-render`, so hub
//! and headless link it too. Decoupling them is future work.

mod capture;
mod material;
mod pipeline;
mod program;
mod quad;
mod targets;
// ADR-0161 §Decision 4: the `FrameCheck` verdict + similarity scorer, rehomed
// here from `aether-harness-substrate-capture` so the pumped render runtime in
// `aether-render` (which depends on this crate) can score captures without a
// dependency cycle. `pub` so the capture harness re-exports it unchanged.
pub mod visual;

pub use capture::{CaptureMeta, encode_png, finish_capture, map_capture_rgba, prepare_capture_copy};
pub use material::{
    MATERIAL_VERTEX_BUFFER_BYTES, MATERIAL_VERTEX_STRIDE, MATERIAL_VERTICES_PER_RECT, MaterialDraw, MaterialPassDraw,
    MaterialPassRecord, MaterialPipelines, build_material_pipelines, push_coverage_params, push_material_rect_vertices,
    push_textured_params, record_material_pass,
};
pub use pipeline::{Pipeline, RenderError, build_main_pipeline, record_main_pass};
pub use program::{
    PROGRAM_DEPTH_FORMAT, PROGRAM_FULLSCREEN_ENTRY, PROGRAM_FULLSCREEN_WGSL, PassTimestamps, ProgramComputePass,
    ProgramComputePipelineSpec, ProgramDepthAttachment, ProgramDrawCommand, ProgramDrawPass, ProgramDrawPipelineSpec,
    ProgramPassDraw, build_fullscreen_vertex_module, build_program_compute_pipeline, build_program_draw_pipeline,
    build_program_pipeline, create_program_depth_transient, create_program_transient, program_inputs_layout,
    program_storage_layout, program_uniform_layout, record_program_compute_pass, record_program_draw_pass,
    record_program_pass,
};
pub use quad::{
    CompositeBlend, OverlayDraw, QUAD_UNIFORM_BYTES, QUAD_VERTEX_BUFFER_BYTES, QUAD_VERTEX_STRIDE,
    QUAD_VERTICES_PER_QUAD, QuadPipeline, RealizedTexture, TextureBindings, build_quad_pipeline,
    build_texture_bindings, push_screen_quad_vertices, push_world_quad_vertices, realize_texture,
    realize_writable_texture, record_quad_overlay_pass, upload_texture_full,
};
pub use targets::{Targets, record_resolve_pass};

/// Bytes per vertex on the wire: `vec3<f32> position + vec3<f32>
/// color` = 24. Both chassis upload exactly this stride; the vertex
/// shader reads `position` from offset 0 and `color` from offset 12.
pub const VERTEX_STRIDE: u64 = 24;

/// Default maximum size of the per-frame vertex buffer: 64 MiB
/// (~2.8M vertices at [`VERTEX_STRIDE`]). The render capability's
/// `vertex_buffer_bytes` boot knob (`AETHER_RENDER_VERTEX_BUFFER_BYTES`,
/// ADR-0090) overrides it per chassis; [`build_main_pipeline`] sizes
/// the GPU buffer from the resolved value. Render sinks truncate to
/// the resolved cap before forwarding bytes; if a future caller
/// bypasses the sink-side clamp, `record_main_pass` drops the frame
/// with a warn rather than overflow the GPU buffer.
pub const VERTEX_BUFFER_BYTES: usize = 64 * 1024 * 1024;

/// Camera uniform buffer size: a single 4×4 column-major `f32` view-
/// projection matrix. The vertex shader applies `camera.view_proj *
/// vec4(position, 1.0)` to every vertex; until the first
/// `aether.view_projection` mail arrives the buffer holds [`IDENTITY_VIEW_PROJ`].
pub const CAMERA_UNIFORM_BYTES: u64 = 64;

/// Depth target format. `LessEqual` comparison with this paired
/// against the offscreen color target gives the "larger world-z draws
/// in front" convention components use (floors at z=0, foreground at
/// z=0.1+).
pub const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;

/// Sample count of the shared world color + depth targets, and so of
/// every pipeline that draws into them: the world pass, the material
/// pass, the quad overlay, and the chassis-side wireframe overlay.
/// WebGPU guarantees 4x on every backend, so this needs no adapter
/// negotiation. The passes draw into the multisampled pair and
/// [`record_resolve_pass`] resolves once into the single-sample
/// offscreen texture the swapchain blit and the capture readback both
/// consume.
///
/// Program passes (ADR-0170 / ADR-0171) are deliberately *not* covered:
/// they render into their own transients, which later passes sample as
/// ordinary textures, so they stay single-sample.
pub const MSAA_SAMPLE_COUNT: u32 = 4;

/// Row-byte alignment wgpu's `copy_texture_to_buffer` requires for
/// `bytes_per_row`. Capture readback pads each row up to this
/// boundary, then strips the padding when assembling RGBA bytes.
pub const COPY_ROW_ALIGN: u32 = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;

/// 4×4 identity matrix in column-major order — what the camera
/// uniform holds before the first `aether.view_projection` mail arrives.
pub const IDENTITY_VIEW_PROJ: [f32; 16] = [
    1.0, 0.0, 0.0, 0.0, //
    0.0, 1.0, 0.0, 0.0, //
    0.0, 0.0, 1.0, 0.0, //
    0.0, 0.0, 0.0, 1.0, //
];

fn vertex_layout(array_stride: u64, attributes: &'static [wgpu::VertexAttribute]) -> wgpu::VertexBufferLayout<'static> {
    wgpu::VertexBufferLayout { array_stride, step_mode: wgpu::VertexStepMode::Vertex, attributes }
}

fn color_target_state(format: wgpu::TextureFormat, blend: wgpu::BlendState) -> wgpu::ColorTargetState {
    wgpu::ColorTargetState { format, blend: Some(blend), write_mask: wgpu::ColorWrites::ALL }
}

fn fragment_state<'a>(
    shader: &'a wgpu::ShaderModule,
    entry_point: &'a str,
    targets: &'a [Option<wgpu::ColorTargetState>],
) -> wgpu::FragmentState<'a> {
    wgpu::FragmentState {
        module: shader,
        entry_point: Some(entry_point),
        compilation_options: wgpu::PipelineCompilationOptions::default(),
        targets,
    }
}

/// `pos vec3 + color vec3` interleaved vertex layout the shared
/// pipeline expects. Exposed so chassis-side helpers building extra
/// pipelines (e.g. desktop's wireframe overlay) can match the layout
/// without re-deriving offsets.
//noinspection DuplicatedCode -- this color layout is distinct from the material pipeline's UV layout.
#[must_use]
pub fn vertex_buffer_layout() -> wgpu::VertexBufferLayout<'static> {
    const ATTRIBUTES: &[wgpu::VertexAttribute] = &[
        wgpu::VertexAttribute { offset: 0, shader_location: 0, format: wgpu::VertexFormat::Float32x3 },
        wgpu::VertexAttribute { offset: 12, shader_location: 1, format: wgpu::VertexFormat::Float32x3 },
    ];
    vertex_layout(VERTEX_STRIDE, ATTRIBUTES)
}

/// Single-entry vertex-stage uniform-buffer bind group layout — the
/// shape the camera and quad-viewport uniforms share.
fn uniform_bind_group_layout(device: &wgpu::Device, label: &str, bytes: u64) -> wgpu::BindGroupLayout {
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some(label),
        entries: &[wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgpu::ShaderStages::VERTEX,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: false,
                min_binding_size: wgpu::BufferSize::new(bytes),
            },
            count: None,
        }],
    })
}

/// Load-preserving color attachment over the frame's multisampled color
/// target — how the material and quad-overlay passes draw over the main
/// pass output without clearing it. No `resolve_target`: the samples
/// stay multisampled for whatever pass comes next, and the chain
/// resolves once at the end via [`record_resolve_pass`].
fn load_color_attachment(view: &wgpu::TextureView) -> wgpu::RenderPassColorAttachment<'_> {
    wgpu::RenderPassColorAttachment {
        view,
        resolve_target: None,
        depth_slice: None,
        ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
    }
}

/// Source for the shared `(pos, color)` shader. Chassis-side
/// pipelines that share the vertex layout (wireframe overlay, etc.)
/// can reach for this directly.
pub const MAIN_SHADER_WGSL: &str = include_str!("shader.wgsl");
