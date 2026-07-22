//! Shared desktop-surface GPU helpers for chassis / runtimes that own a
//! wgpu `Surface` (ADR-0161). The wireframe-overlay pipeline builder and
//! the swapchain-texture acquisition were duplicated between the desktop
//! chassis's `Gpu` body and the pumped render runtime's owned-field body;
//! extracting them keeps that overlap in one place (both the desktop
//! chassis and the pumped runtime call these). wgpu-only — no winit — so
//! they ride the `runtime` feature and a headless consumer that never owns
//! a surface simply never calls them.

use std::slice;
use std::sync::Arc;

use aether_substrate::render::{DEPTH_FORMAT, vertex_buffer_layout};

/// Resolved wgpu surface + device the desktop chassis `Gpu` boot and the
/// pumped render runtime both stand up from a window (ADR-0161). Extracted
/// so the instance → surface → adapter → device → swapchain-config boot
/// lives once; the two callers differ only in what they build on top (the
/// pooled runtime installs a `RenderGpu` into `RenderHandles`; the pumped
/// runtime owns the `RenderGpu` outright). winit-free — the caller passes
/// the surface target and its pixel size — so it rides the `runtime`
/// feature.
pub struct BootedSurface {
    pub device: Arc<wgpu::Device>,
    pub queue: Arc<wgpu::Queue>,
    pub surface: wgpu::Surface<'static>,
    pub config: wgpu::SurfaceConfiguration,
    /// Chosen swapchain color format (sRGB-preferred).
    pub format: wgpu::TextureFormat,
    /// `Line` when `AETHER_WIREFRAME=line` and the adapter supports
    /// `POLYGON_MODE_LINE`; `Fill` otherwise. The main pipeline is built
    /// with this.
    pub polygon_mode: wgpu::PolygonMode,
    /// `true` when `AETHER_WIREFRAME=overlay` (or `1`) and the adapter
    /// supports `POLYGON_MODE_LINE` — the caller builds the wireframe
    /// overlay pipeline via [`build_wireframe_overlay_pipeline`].
    pub build_overlay: bool,
    /// Adapter identity chosen at boot, retained for `platform_info`.
    pub adapter_info: wgpu::AdapterInfo,
    /// Resolved device limits.
    pub limits: wgpu::Limits,
}

/// Map the resolved `AETHER_WIREFRAME` value to `(wants_line, wants_overlay)`:
///   unset / `"" | "0" | "off"` → filled (default), neither flag
///   `"line"` → the main pipeline draws in `PolygonMode::Line`
///   anything else (`"1"`, `"overlay"`, …) → filled + a wireframe overlay
fn wireframe_flags(wireframe: Option<&str>) -> (bool, bool) {
    match wireframe {
        None | Some("" | "0" | "off") => (false, false),
        Some("line") => (true, false),
        Some(_) => (false, true),
    }
}

/// Boot the wgpu instance, surface, adapter, device, and swapchain
/// configuration for a windowed chassis. `target` is the window surface
/// target (`Arc<winit::window::Window>` on desktop); `size` is its current
/// inner pixel size; `wireframe` is the resolved `AETHER_WIREFRAME` value
/// (argv > env > default) — `None` / `"" | "0" | "off"` is filled faces,
/// `"line"` draws the main pipeline in `PolygonMode::Line`, anything else
/// is filled faces plus a wireframe overlay.
///
/// # Panics
/// Panics if surface creation, adapter selection, or device acquisition
/// fail — fail-fast per ADR-0063: a windowed chassis can't proceed without
/// a usable GPU pipeline.
#[must_use]
pub fn boot_surface(
    target: impl Into<wgpu::SurfaceTarget<'static>>,
    size: (u32, u32),
    wireframe: Option<&str>,
) -> BootedSurface {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
    let surface = instance.create_surface(target).expect("create_surface");
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::default(),
        compatible_surface: Some(&surface),
        force_fallback_adapter: false,
    }))
    .expect("no compatible wgpu adapter");
    let adapter_info = adapter.get_info();
    let limits = wgpu::Limits::default();

    // Wireframe rendering is opt-in via `AETHER_WIREFRAME`; the line modes
    // need the adapter's `POLYGON_MODE_LINE` feature, so if unsupported we
    // fall back to filled with a warning rather than failing device creation.
    let (wants_line, wants_overlay) = wireframe_flags(wireframe);
    let mut polygon_mode = if wants_line {
        wgpu::PolygonMode::Line
    } else {
        wgpu::PolygonMode::Fill
    };
    let mut build_overlay = wants_overlay;
    let supports_line = adapter.features().contains(wgpu::Features::POLYGON_MODE_LINE);
    if (wants_line || wants_overlay) && !supports_line {
        tracing::warn!(
            adapter = %adapter_info.name,
            "AETHER_WIREFRAME requested but adapter lacks POLYGON_MODE_LINE; falling back to filled"
        );
        polygon_mode = wgpu::PolygonMode::Fill;
        build_overlay = false;
    }
    let required_features = if (wants_line || wants_overlay) && supports_line {
        wgpu::Features::POLYGON_MODE_LINE
    } else {
        wgpu::Features::empty()
    };

    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("aether-substrate device"),
        required_features,
        required_limits: limits.clone(),
        experimental_features: wgpu::ExperimentalFeatures::default(),
        memory_hints: wgpu::MemoryHints::default(),
        trace: wgpu::Trace::default(),
    }))
    .expect("request_device");

    let device = Arc::new(device);
    let queue = Arc::new(queue);

    let caps = surface.get_capabilities(&adapter);
    // Prefer sRGB so the clear color matches intuition.
    let format = caps.formats.iter().copied().find(wgpu::TextureFormat::is_srgb).unwrap_or(caps.formats[0]);
    let config = wgpu::SurfaceConfiguration {
        // COPY_DST: the swapchain receives a texture-to-texture copy from
        // the offscreen each frame. No draw pass writes to it directly.
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_DST,
        format,
        width: size.0.max(1),
        height: size.1.max(1),
        present_mode: wgpu::PresentMode::Fifo,
        alpha_mode: caps.alpha_modes[0],
        view_formats: vec![],
        desired_maximum_frame_latency: 2,
    };
    surface.configure(&device, &config);

    BootedSurface { device, queue, surface, config, format, polygon_mode, build_overlay, adapter_info, limits }
}

/// Wireframe-overlay shader: same vertex layout as the main shader so the
/// pipeline shares the existing vertex buffer. The fragment stage emits a
/// flat dark color so wires read against any filled color underneath.
const WIREFRAME_WGSL: &str = r"
struct Camera {
    view_proj: mat4x4<f32>,
}

@group(0) @binding(0)
var<uniform> camera: Camera;

struct VertexInput {
    @location(0) position: vec3<f32>,
    @location(1) color: vec3<f32>,
}

@vertex
fn vs_main(in: VertexInput) -> @builtin(position) vec4<f32> {
    return camera.view_proj * vec4<f32>(in.position, 1.0);
}

@fragment
fn fs_main() -> @location(0) vec4<f32> {
    return vec4<f32>(0.05, 0.07, 0.12, 1.0);
}
";

/// Build the wireframe overlay pipeline (`AETHER_WIREFRAME=overlay`): the
/// main vertex/uniform layout drawn in `PolygonMode::Line` with a flat
/// dark fragment color, so the wires read against any filled color
/// underneath. `pipeline_layout` borrows the installed main pipeline's
/// layout (same camera bind group). `target_format` is the color target
/// the overlay draws into. The pipeline is drawn after the main pipeline
/// as an `extra` in the world pass, inside the same render pass.
///
/// The caller resolves whether `AETHER_WIREFRAME` asked for overlay and
/// whether the adapter supports `POLYGON_MODE_LINE` before building this.
#[must_use]
pub fn build_wireframe_overlay_pipeline(
    device: &wgpu::Device,
    target_format: wgpu::TextureFormat,
    pipeline_layout: &wgpu::PipelineLayout,
) -> wgpu::RenderPipeline {
    let wire_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("wireframe shader"),
        source: wgpu::ShaderSource::Wgsl(WIREFRAME_WGSL.into()),
    });
    let vertex_layout = vertex_buffer_layout();
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("wireframe overlay pipeline"),
        layout: Some(pipeline_layout),
        vertex: wgpu::VertexState {
            module: &wire_shader,
            entry_point: Some("vs_main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            buffers: slice::from_ref(&vertex_layout),
        },
        fragment: Some(wgpu::FragmentState {
            module: &wire_shader,
            entry_point: Some("fs_main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format: target_format,
                blend: Some(wgpu::BlendState::REPLACE),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            strip_index_format: None,
            front_face: wgpu::FrontFace::Ccw,
            cull_mode: None,
            polygon_mode: wgpu::PolygonMode::Line,
            unclipped_depth: false,
            conservative: false,
        },
        depth_stencil: Some(wgpu::DepthStencilState {
            format: DEPTH_FORMAT,
            depth_write_enabled: Some(false),
            depth_compare: Some(wgpu::CompareFunction::LessEqual),
            stencil: wgpu::StencilState::default(),
            bias: wgpu::DepthBiasState { constant: -1, slope_scale: -1.0, clamp: 0.0 },
        }),
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    })
}

/// Try to get the current swapchain texture. Reconfigures the surface on
/// `Suboptimal` / `Lost` / `Outdated` so the next frame recovers; on
/// `Occluded` / `Timeout` / an unexpected status returns `None` and the
/// caller skips the present step for this frame. Offscreen is the source
/// of truth for capture, so a skipped present never blocks a readback.
#[must_use]
pub fn acquire_surface_texture(
    surface: &wgpu::Surface<'_>,
    device: &wgpu::Device,
    config: &wgpu::SurfaceConfiguration,
) -> Option<wgpu::SurfaceTexture> {
    match surface.get_current_texture() {
        wgpu::CurrentSurfaceTexture::Success(t) => Some(t),
        wgpu::CurrentSurfaceTexture::Suboptimal(t) => {
            surface.configure(device, config);
            Some(t)
        }
        wgpu::CurrentSurfaceTexture::Lost | wgpu::CurrentSurfaceTexture::Outdated => {
            surface.configure(device, config);
            None
        }
        wgpu::CurrentSurfaceTexture::Occluded | wgpu::CurrentSurfaceTexture::Timeout => None,
        other @ wgpu::CurrentSurfaceTexture::Validation => {
            tracing::warn!(
                target: "aether_substrate::render",
                status = ?other,
                "surface.get_current_texture returned unexpected status",
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::wireframe_flags;

    // Tripwire: pins the `AETHER_WIREFRAME` tri-state parse (threaded from
    // `WindowConfig::wireframe`) that `boot_surface` keys the main polygon
    // mode + overlay build off — drifts if an arm changes. Relocated here
    // from the desktop chassis when the surface boot was extracted.
    #[test]
    fn wireframe_flags_maps_the_tri_state() {
        assert_eq!(wireframe_flags(None), (false, false));
        assert_eq!(wireframe_flags(Some("off")), (false, false));
        assert_eq!(wireframe_flags(Some("line")), (true, false));
        assert_eq!(wireframe_flags(Some("overlay")), (false, true));
        assert_eq!(wireframe_flags(Some("garbage")), (false, true));
    }
}
