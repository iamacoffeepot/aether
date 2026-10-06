//! Desktop-surface GPU helpers for the pumped render runtime when it owns a
//! wgpu `Surface` (ADR-0161): the wireframe-overlay pipeline builder, the
//! swapchain-texture acquisition, and the surface / offscreen device boot.
//! wgpu-only — no winit — so they ride the `runtime` feature, and a headless
//! consumer that never owns a surface simply never calls them.

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

/// The floor under every aether render device's limits: what mail-time
/// validation may assume before any device exists. Both boot paths
/// request [`device_limits`], which is never below these, so a ceiling
/// checked here (`max_texture_dimension_2d`) and the alignment the
/// program executor stages uniform windows at
/// (`min_uniform_buffer_offset_alignment`) hold on whatever device is
/// granted — which is what lets `create_texture` reject an oversized
/// request at mail time even though the GPU boots lazily.
///
/// Two limits are raised past this floor, `max_texture_array_layers` and
/// `max_buffer_size`. A check that wants the raised value reads it from
/// the live device (`create_texture_array` does); one that reads it here
/// stays sound and admits only the floor.
#[must_use]
pub fn render_limits() -> wgpu::Limits {
    wgpu::Limits::default()
}

/// The limits a render device is requested with: [`render_limits`], with
/// the array-layer and buffer-size ceilings taken from the adapter
/// (ADR-0246 decision 6), whose defaults of 256 layers and 256 MiB a
/// scene reaches first. Neither is requested below the floor, so an
/// adapter that offers less fails device creation as it did before.
fn device_limits(adapter: &wgpu::Adapter) -> wgpu::Limits {
    let floor = render_limits();
    let offered = adapter.limits();
    wgpu::Limits {
        max_texture_array_layers: offered.max_texture_array_layers.max(floor.max_texture_array_layers),
        max_buffer_size: offered.max_buffer_size.max(floor.max_buffer_size),
        ..floor
    }
}

/// Route wgpu errors that escape every error scope into the render log
/// instead of wgpu's default handler, which panics the thread it lands on
/// (`default_error_handler` in wgpu's backend). An actor's invalid graph
/// or oversized request must not take the renderer — and with it the
/// fleet's picture — down, so the frame's work is dropped loudly rather
/// than fatally. Validation this crate performs up front is still the
/// primary contract; this is the backstop for what slips past it, and an
/// error logged here is a bug in that validation, not a normal outcome.
fn install_uncaptured_error_handler(device: &wgpu::Device) {
    device.on_uncaptured_error(Arc::new(|error| {
        tracing::error!(
            target: "aether_render",
            %error,
            "uncaptured wgpu error; dropping the frame's work rather than panicking the renderer",
        );
    }));
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

/// The optional features the render device takes whenever the adapter
/// offers them (iamacoffeepot/aether#4423). `TIMESTAMP_QUERY` is what
/// the per-pass timing instrument brackets passes with; requesting it
/// opportunistically rather than requiring it is what keeps a device
/// that lacks it booting exactly as before, reporting the instrument
/// absent-with-reason instead of failing. Nothing else in the runtime
/// consults it, so a device that grants it and an operator who leaves
/// the instrument off costs the same as one that never had it.
///
/// `FLOAT32_BLENDABLE` is what lets a program pass declare
/// `Blend::Alpha` or `Blend::Additive` onto an `R32Float` output
/// (ADR-0246 decision 7); core WebGPU blends every other format a
/// program writes. On a device without it such a pass fails pipeline
/// creation inside the register reply, naming the format, and a pass
/// that replaces an `R32Float` output registers as it does everywhere.
fn opportunistic_features(adapter: &wgpu::Adapter) -> wgpu::Features {
    adapter.features() & (wgpu::Features::TIMESTAMP_QUERY | wgpu::Features::FLOAT32_BLENDABLE)
}

/// The adapter a surfaceless device is requested from.
fn request_offscreen_adapter() -> Result<wgpu::Adapter, String> {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
    pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::default(),
        compatible_surface: None,
        force_fallback_adapter: false,
        apply_limit_buckets: false,
    }))
    .map_err(|error| format!("request offscreen adapter: {error}"))
}

/// Fallibly acquire a surfaceless wgpu device for an offscreen replacement
/// transaction (ADR-0173). No surface, no swapchain — the substrate harness
/// owns no window, so the runtime records into the offscreen targets
/// [`crate::runtime::RenderGpu::new`] allocates and reads back from them.
/// `wireframe` is the resolved `AETHER_WIREFRAME` value, honored the same way
/// [`boot_surface`] honors it.
pub fn try_boot_offscreen(wireframe: Option<&str>) -> Result<BootedOffscreen, String> {
    let adapter = request_offscreen_adapter()?;
    let adapter_info = adapter.get_info();
    let (polygon_mode, build_overlay, required_features) = resolve_wireframe(&adapter, &adapter_info.name, wireframe);

    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("aether-render offscreen device"),
        required_features: required_features | opportunistic_features(&adapter),
        required_limits: device_limits(&adapter),
        experimental_features: wgpu::ExperimentalFeatures::default(),
        memory_hints: wgpu::MemoryHints::default(),
        trace: wgpu::Trace::default(),
    }))
    .map_err(|error| format!("request offscreen device: {error}"))?;
    install_uncaptured_error_handler(&device);

    Ok(BootedOffscreen {
        device: Arc::new(device),
        queue: Arc::new(queue),
        format: OFFSCREEN_COLOR_FORMAT,
        polygon_mode,
        build_overlay,
    })
}

/// Boot the first surfaceless device. Initial offscreen boot remains
/// fail-fast; only an ADR-0173 replacement uses [`try_boot_offscreen`]'s
/// returned error and enters terminal `Unusable` on failure.
///
/// # Panics
/// Panics if adapter selection or device acquisition fail — fail-fast per
/// ADR-0063: the harness can't proceed without a usable offscreen pipeline,
/// and driverless dev boxes are expected to skip the scenario upstream.
#[must_use]
pub fn boot_offscreen(wireframe: Option<&str>) -> BootedOffscreen {
    try_boot_offscreen(wireframe).expect("initial offscreen render device acquisition failed")
}

/// How a window surface presents a finished frame. From the surface's side
/// only two behaviours exist, so this is all render is told: who paces a
/// window that does not wait is its owner's business.
#[cfg(feature = "desktop")]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SurfacePresent {
    /// The present waits for the display's refresh.
    InStep,
    /// The present returns at once, whatever the display is doing.
    Unsynced,
}

/// The wgpu present mode that serves `present` on a surface offering
/// `offered`, or an error naming what the surface does offer.
///
/// `InStep` is `Fifo`. `Unsynced` is `Immediate`, else `Mailbox`: both
/// return without waiting, and `Immediate` is the one more surfaces offer.
/// Nothing falls back to a mode with the other behaviour, so a caller that
/// asked not to wait is never handed a display-paced surface in silence.
/// wgpu's `AutoVsync` and `AutoNoVsync` are such fallback chains (the second
/// ends in `Fifo`) and `FifoRelaxed` tears when a frame is late, so none of
/// the three is chosen.
#[cfg(feature = "desktop")]
pub fn present_mode(present: SurfacePresent, offered: &[wgpu::PresentMode]) -> Result<wgpu::PresentMode, String> {
    let preferred: &[wgpu::PresentMode] = match present {
        SurfacePresent::InStep => &[wgpu::PresentMode::Fifo],
        SurfacePresent::Unsynced => &[wgpu::PresentMode::Immediate, wgpu::PresentMode::Mailbox],
    };
    preferred.iter().copied().find(|mode| offered.contains(mode)).ok_or_else(|| {
        format!(
            "the surface cannot present {present:?}: it offers {offered:?}, and {present:?} needs one of {preferred:?}"
        )
    })
}

#[cfg(feature = "desktop")]
fn surface_configuration(
    surface: &wgpu::Surface<'_>,
    adapter: &wgpu::Adapter,
    size: (u32, u32),
    required_format: Option<wgpu::TextureFormat>,
    present: SurfacePresent,
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
    let present_mode = present_mode(present, &caps.present_modes)?;
    let alpha_mode = caps.alpha_modes.first().copied().ok_or_else(|| "surface reports no alpha modes".to_owned())?;
    let config = wgpu::SurfaceConfiguration {
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_DST,
        format,
        width: size.0.max(1),
        height: size.1.max(1),
        present_mode,
        alpha_mode,
        color_space: wgpu::SurfaceColorSpace::Auto,
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
    present: SurfacePresent,
) -> Result<BootedSurface, String> {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
    let surface = instance.create_surface(target).map_err(|error| format!("create render surface: {error}"))?;
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::default(),
        compatible_surface: Some(&surface),
        force_fallback_adapter: false,
        apply_limit_buckets: false,
    }))
    .map_err(|error| format!("request compatible render adapter: {error}"))?;
    let adapter_info = adapter.get_info();
    let (config, format) = surface_configuration(&surface, &adapter, size, None, present)?;

    // Wireframe rendering is opt-in via `AETHER_WIREFRAME`; the line modes
    // need the adapter's `POLYGON_MODE_LINE` feature, so if unsupported we
    // fall back to filled with a warning rather than failing device creation.
    let (polygon_mode, build_overlay, required_features) = resolve_wireframe(&adapter, &adapter_info.name, wireframe);

    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("aether-substrate device"),
        required_features: required_features | opportunistic_features(&adapter),
        required_limits: device_limits(&adapter),
        experimental_features: wgpu::ExperimentalFeatures::default(),
        memory_hints: wgpu::MemoryHints::default(),
        trace: wgpu::Trace::default(),
    }))
    .map_err(|error| format!("request render device: {error}"))?;
    install_uncaptured_error_handler(&device);

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
    present: SurfacePresent,
) -> Result<AttachedSurface, String> {
    let surface = instance.create_surface(target).map_err(|error| format!("create render surface: {error}"))?;
    let (config, _) = surface_configuration(&surface, adapter, size, Some(format), present)?;
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
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("wireframe overlay pipeline"),
        layout: Some(pipeline_layout),
        vertex: wgpu::VertexState {
            module: &wire_shader,
            entry_point: Some("vs_main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            buffers: &[Some(vertex_buffer_layout())],
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
    use aether_harness_substrate_capture::test_helpers::has_wgpu_adapter;

    use super::{boot_offscreen, render_limits, request_offscreen_adapter, wireframe_flags};

    /// The offscreen device is granted the adapter's array-layer and
    /// buffer-size limits (ADR-0246 decision 6). The named bug: a device
    /// descriptor still passing the defaults, which caps an array at 256
    /// layers whatever the adapter offers.
    #[test]
    fn the_offscreen_device_takes_the_adapters_layer_and_buffer_limits() {
        if !has_wgpu_adapter() {
            return;
        }
        let offered = request_offscreen_adapter().expect("the adapter the gate found").limits();
        let granted = boot_offscreen(None).device.limits();

        assert_eq!(granted.max_texture_array_layers, offered.max_texture_array_layers);
        assert_eq!(granted.max_buffer_size, offered.max_buffer_size);
        assert_eq!(
            granted.max_texture_dimension_2d,
            render_limits().max_texture_dimension_2d,
            "a limit mail-time validation reads stays at the floor",
        );
    }

    /// The present-mode table, over lists a surface might offer. Fails if a
    /// silent fallback returns: `Unsynced` handed `Fifo` on a surface that
    /// offers nothing else, which turns an uncapped window into a
    /// display-paced one with no word to its owner, or `InStep` handed
    /// whichever mode a surface lists first.
    #[cfg(feature = "desktop")]
    #[test]
    fn a_present_mode_the_surface_lacks_is_an_error_naming_what_it_offers() {
        use wgpu::PresentMode::{Fifo, FifoRelaxed, Immediate, Mailbox};

        use super::{SurfacePresent, present_mode};

        let refused = present_mode(SurfacePresent::Unsynced, &[Fifo]).expect_err("Fifo waits for the display");
        assert!(refused.contains("Fifo"), "the refusal names the offered modes: {refused}");
        assert_eq!(present_mode(SurfacePresent::Unsynced, &[Fifo, Mailbox]), Ok(Mailbox));
        assert_eq!(present_mode(SurfacePresent::Unsynced, &[Fifo, Mailbox, Immediate]), Ok(Immediate));
        assert_eq!(present_mode(SurfacePresent::Unsynced, &[Fifo, FifoRelaxed]).ok(), None);

        assert_eq!(present_mode(SurfacePresent::InStep, &[Immediate, Fifo]), Ok(Fifo));
        let refused = present_mode(SurfacePresent::InStep, &[Immediate, Mailbox]).expect_err("neither waits");
        assert!(refused.contains("Immediate") && refused.contains("Mailbox"), "{refused}");
    }

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
