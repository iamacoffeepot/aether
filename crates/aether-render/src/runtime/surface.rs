//! Desktop-surface GPU helpers for the pumped render runtime when it owns a
//! wgpu `Surface` (ADR-0161): the wireframe-overlay pipeline builder, the
//! swapchain-texture acquisition, and the surface / offscreen device boot.
//! wgpu-only — no winit — so they ride the `runtime` feature, and a headless
//! consumer that never owns a surface simply never calls them.

use std::slice;
use std::sync::Arc;

use aether_substrate::render::{DEPTH_FORMAT, MSAA_SAMPLE_COUNT, vertex_buffer_layout};

/// Resolved first wgpu surface + shared device context. The render runtime
/// retains the instance and adapter so later windows can attach compatible
/// surfaces to the same device and pipelines.
#[cfg(feature = "desktop")]
pub struct BootedSurface {
    pub instance: wgpu::Instance,
    pub adapter: wgpu::Adapter,
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
}

/// One additional surface attached to an already-booted render device.
#[cfg(feature = "desktop")]
pub struct AttachedSurface {
    pub surface: wgpu::Surface<'static>,
    pub config: wgpu::SurfaceConfiguration,
}

/// Resolved surfaceless wgpu device the offscreen pumped render runtime
/// stands up (ADR-0161 slice R4). The substrate harness owns no window, so
/// its pumped runtime boots the GPU with no surface — the offscreen color +
/// depth targets [`crate::runtime::RenderGpu::new`] allocates are the only
/// render targets, and capture reads back from them directly. This winit-free
/// boot lets the lazy [`crate::RenderCapability`] offscreen path match the
/// windowed one minus the swapchain.
pub struct BootedOffscreen {
    pub device: Arc<wgpu::Device>,
    pub queue: Arc<wgpu::Queue>,
    /// Fixed offscreen color format (sRGB RGBA); with no surface to query
    /// the runtime commits to it, keeping the readback path swizzle-free.
    pub format: wgpu::TextureFormat,
    /// `Line` when `AETHER_WIREFRAME=line` and the adapter supports
    /// `POLYGON_MODE_LINE`; `Fill` otherwise.
    pub polygon_mode: wgpu::PolygonMode,
    /// `true` when `AETHER_WIREFRAME=overlay` and the adapter supports
    /// `POLYGON_MODE_LINE` — the caller builds the wireframe overlay
    /// pipeline against the offscreen color format.
    pub build_overlay: bool,
}

/// Offscreen color format (sRGB RGBA): with no surface to query the runtime
/// commits to RGBA at boot so the capture readback stays swizzle-free.
const OFFSCREEN_COLOR_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

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

/// Resolve the wireframe polygon mode + overlay flag + required device
/// features against an adapter's `POLYGON_MODE_LINE` support, warning and
/// falling back to filled when the mode is requested but unsupported.
/// Shared by [`boot_surface`] and [`boot_offscreen`] so the tri-state
/// resolution lives once.
fn resolve_wireframe(
    adapter: &wgpu::Adapter,
    adapter_name: &str,
    wireframe: Option<&str>,
) -> (wgpu::PolygonMode, bool, wgpu::Features) {
    let (wants_line, wants_overlay) = wireframe_flags(wireframe);
    let supports_line = adapter.features().contains(wgpu::Features::POLYGON_MODE_LINE);
    if (wants_line || wants_overlay) && !supports_line {
        tracing::warn!(
            adapter = %adapter_name,
            "AETHER_WIREFRAME requested but adapter lacks POLYGON_MODE_LINE; falling back to filled"
        );
        return (wgpu::PolygonMode::Fill, false, wgpu::Features::empty());
    }
    let polygon_mode = if wants_line {
        wgpu::PolygonMode::Line
    } else {
        wgpu::PolygonMode::Fill
    };
    let required_features = if wants_line || wants_overlay {
        wgpu::Features::POLYGON_MODE_LINE
    } else {
        wgpu::Features::empty()
    };
    (polygon_mode, wants_overlay, required_features)
}

/// Boot a surfaceless wgpu device for the offscreen pumped render runtime
/// (ADR-0161 slice R4). No surface, no swapchain — the substrate harness
/// owns no window, so the runtime records into the offscreen targets
/// [`crate::runtime::RenderGpu::new`] allocates and reads back from them.
/// `size` is retained by the caller for `RenderGpu::new`; `wireframe` is
/// the resolved `AETHER_WIREFRAME` value, honored the same way
/// [`boot_surface`] honors it.
///
/// # Panics
/// Panics if adapter selection or device acquisition fail — fail-fast per
/// ADR-0063: the harness can't proceed without a usable offscreen pipeline,
/// and driverless dev boxes are expected to skip the scenario upstream.
#[must_use]
pub fn boot_offscreen(wireframe: Option<&str>) -> BootedOffscreen {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::default(),
        compatible_surface: None,
        force_fallback_adapter: false,
    }))
    .expect("no compatible wgpu adapter");
    let adapter_info = adapter.get_info();
    let (polygon_mode, build_overlay, required_features) = resolve_wireframe(&adapter, &adapter_info.name, wireframe);

    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("aether-render offscreen device"),
        required_features,
        required_limits: wgpu::Limits::default(),
        experimental_features: wgpu::ExperimentalFeatures::default(),
        memory_hints: wgpu::MemoryHints::default(),
        trace: wgpu::Trace::default(),
    }))
    .expect("request_device");

    BootedOffscreen {
        device: Arc::new(device),
        queue: Arc::new(queue),
        format: OFFSCREEN_COLOR_FORMAT,
        polygon_mode,
        build_overlay,
    }
}

#[cfg(feature = "desktop")]
fn surface_configuration(
    surface: &wgpu::Surface<'_>,
    adapter: &wgpu::Adapter,
    size: (u32, u32),
    required_format: Option<wgpu::TextureFormat>,
) -> Result<(wgpu::SurfaceConfiguration, wgpu::TextureFormat), String> {
    let caps = surface.get_capabilities(adapter);
    if !caps.usages.contains(wgpu::TextureUsages::COPY_DST) {
        return Err("surface does not support COPY_DST presentation from the shared offscreen target".to_owned());
    }
    let format = match required_format {
        Some(format) if caps.formats.contains(&format) => format,
        Some(format) => {
            return Err(format!(
                "surface is incompatible with the shared render format {format:?}; supported formats: {:?}",
                caps.formats
            ));
        }
        None => caps
            .formats
            .iter()
            .copied()
            .find(wgpu::TextureFormat::is_srgb)
            .or_else(|| caps.formats.first().copied())
            .ok_or_else(|| "surface reports no compatible formats".to_owned())?,
    };
    let present_mode = caps
        .present_modes
        .iter()
        .copied()
        .find(|mode| *mode == wgpu::PresentMode::Fifo)
        .or_else(|| caps.present_modes.first().copied())
        .ok_or_else(|| "surface reports no presentation modes".to_owned())?;
    let alpha_mode = caps.alpha_modes.first().copied().ok_or_else(|| "surface reports no alpha modes".to_owned())?;
    let config = wgpu::SurfaceConfiguration {
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_DST,
        format,
        width: size.0.max(1),
        height: size.1.max(1),
        present_mode,
        alpha_mode,
        view_formats: vec![],
        desired_maximum_frame_latency: 2,
    };
    Ok((config, format))
}

/// Boot the first window surface and the shared wgpu device. Every failure is
/// returned before the caller mutates its target map, so window creation can
/// roll back transactionally.
#[cfg(feature = "desktop")]
pub fn boot_surface(
    target: impl Into<wgpu::SurfaceTarget<'static>>,
    size: (u32, u32),
    wireframe: Option<&str>,
) -> Result<BootedSurface, String> {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
    let surface = instance.create_surface(target).map_err(|error| format!("create render surface: {error}"))?;
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::default(),
        compatible_surface: Some(&surface),
        force_fallback_adapter: false,
    }))
    .map_err(|error| format!("request compatible render adapter: {error}"))?;
    let adapter_info = adapter.get_info();
    let limits = wgpu::Limits::default();
    let (config, format) = surface_configuration(&surface, &adapter, size, None)?;

    // Wireframe rendering is opt-in via `AETHER_WIREFRAME`; the line modes
    // need the adapter's `POLYGON_MODE_LINE` feature, so if unsupported we
    // fall back to filled with a warning rather than failing device creation.
    let (polygon_mode, build_overlay, required_features) = resolve_wireframe(&adapter, &adapter_info.name, wireframe);

    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("aether-substrate device"),
        required_features,
        required_limits: limits,
        experimental_features: wgpu::ExperimentalFeatures::default(),
        memory_hints: wgpu::MemoryHints::default(),
        trace: wgpu::Trace::default(),
    }))
    .map_err(|error| format!("request render device: {error}"))?;

    let device = Arc::new(device);
    let queue = Arc::new(queue);
    surface.configure(&device, &config);

    Ok(BootedSurface { instance, adapter, device, queue, surface, config, format, polygon_mode, build_overlay })
}

/// Attach one later window to the already-selected adapter/device. The
/// surface must support the exact shared color format and `COPY_DST`; failure
/// leaves the caller's map untouched.
#[cfg(feature = "desktop")]
pub fn attach_surface(
    instance: &wgpu::Instance,
    adapter: &wgpu::Adapter,
    device: &wgpu::Device,
    target: impl Into<wgpu::SurfaceTarget<'static>>,
    size: (u32, u32),
    format: wgpu::TextureFormat,
) -> Result<AttachedSurface, String> {
    let surface = instance.create_surface(target).map_err(|error| format!("create render surface: {error}"))?;
    let (config, _) = surface_configuration(&surface, adapter, size, Some(format))?;
    surface.configure(device, &config);
    Ok(AttachedSurface { surface, config })
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
        // Drawn as an extra inside the world pass, so it shares that
        // pass's multisampled attachments.
        multisample: wgpu::MultisampleState { count: MSAA_SAMPLE_COUNT, ..wgpu::MultisampleState::default() },
        multiview_mask: None,
        cache: None,
    })
}

/// Try to get the current swapchain texture. Reconfigures the surface on
/// `Suboptimal` / `Lost` / `Outdated` so the next frame recovers; on
/// `Occluded` / `Timeout` / an unexpected status returns `None` and the
/// caller skips the present step for this frame. Offscreen is the source
/// of truth for capture, so a skipped present never blocks a readback.
///
/// Desktop-only, like every other surface entry point in this module: a
/// swapchain exists only where a window does, and its one caller is
/// `target::RenderTarget::prepare_frame`.
#[cfg(feature = "desktop")]
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
