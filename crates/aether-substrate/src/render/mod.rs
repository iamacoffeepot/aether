//! Shared offscreen render path for chassis that draw (issue 421).
//!
//! `aether-substrate-desktop` and `aether-substrate-test-bench` both
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
//! Gated by the `render` feature on `aether-substrate`. Headless and
//! hub don't enable the feature so wgpu stays out of their build.

mod capture;
mod pipeline;
mod targets;

pub use capture::{CaptureMeta, finish_capture, prepare_capture_copy};
pub use pipeline::{Pipeline, RenderError, build_main_pipeline, record_main_pass};
pub use targets::Targets;

/// Bytes per vertex on the wire: `vec3<f32> position + vec3<f32>
/// color` = 24. Both chassis upload exactly this stride; the vertex
/// shader reads `position` from offset 0 and `color` from offset 12.
pub const VERTEX_STRIDE: u64 = 24;

/// Maximum size of the per-frame vertex buffer. Render sinks truncate
/// to this cap before forwarding bytes; if a future caller bypasses
/// the sink-side clamp, `record_main_pass` drops the frame with a
/// warn rather than overflow the GPU buffer.
pub const VERTEX_BUFFER_BYTES: usize = 4 * 1024 * 1024;

/// Camera uniform buffer size: a single 4×4 column-major `f32` view-
/// projection matrix. The vertex shader applies `camera.view_proj *
/// vec4(position, 1.0)` to every vertex; until the first
/// `aether.camera` mail arrives the buffer holds [`IDENTITY_VIEW_PROJ`].
pub const CAMERA_UNIFORM_BYTES: u64 = 64;

/// Depth target format. `LessEqual` comparison with this paired
/// against the offscreen color target gives the "larger world-z draws
/// in front" convention components use (floors at z=0, foreground at
/// z=0.1+).
pub const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;

/// Row-byte alignment wgpu's `copy_texture_to_buffer` requires for
/// `bytes_per_row`. Capture readback pads each row up to this
/// boundary, then strips the padding when assembling RGBA bytes.
pub const COPY_ROW_ALIGN: u32 = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;

/// 4×4 identity matrix in column-major order — what the camera
/// uniform holds before the first `aether.camera` mail arrives.
pub const IDENTITY_VIEW_PROJ: [f32; 16] = [
    1.0, 0.0, 0.0, 0.0, //
    0.0, 1.0, 0.0, 0.0, //
    0.0, 0.0, 1.0, 0.0, //
    0.0, 0.0, 0.0, 1.0, //
];

/// `pos vec3 + color vec3` interleaved vertex layout the shared
/// pipeline expects. Exposed so chassis-side helpers building extra
/// pipelines (e.g. desktop's wireframe overlay) can match the layout
/// without re-deriving offsets.
pub fn vertex_buffer_layout() -> wgpu::VertexBufferLayout<'static> {
    wgpu::VertexBufferLayout {
        array_stride: VERTEX_STRIDE,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes: &[
            wgpu::VertexAttribute {
                offset: 0,
                shader_location: 0,
                format: wgpu::VertexFormat::Float32x3,
            },
            wgpu::VertexAttribute {
                offset: 12,
                shader_location: 1,
                format: wgpu::VertexFormat::Float32x3,
            },
        ],
    }
}

/// Source for the shared `(pos, color)` shader. Chassis-side
/// pipelines that share the vertex layout (wireframe overlay, etc.)
/// can reach for this directly.
pub const MAIN_SHADER_WGSL: &str = include_str!("shader.wgsl");
