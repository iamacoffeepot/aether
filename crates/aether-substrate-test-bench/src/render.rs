// wgpu plumbing for the test-bench chassis. Owns the device/queue
// plus the shared `core::render::Pipeline` + `Targets` (issue 421);
// no surface, no presentation. Each frame the main thread hands in
// a byte blob drained from the render sink plus the latest camera
// view_proj; render() forwards both to `core::render::record_main_pass`
// which uploads them and emits one draw call into the offscreen.

use aether_substrate_core::render::{
    self, CaptureMeta, Pipeline, RenderError, Targets, build_main_pipeline, finish_capture,
    prepare_capture_copy, record_main_pass,
};

pub use render::{IDENTITY_VIEW_PROJ, VERTEX_BUFFER_BYTES};

/// Render target format. Test-bench commits to RGBA at init since
/// there's no surface to query, which keeps the readback path swizzle-
/// free.
const COLOR_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

pub struct Gpu {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    /// Snapshot of the adapter chosen at `new()`. Kept for
    /// diagnostics — desktop also exposes this for `platform_info`,
    /// which test-bench replies `Err` to (window-only operation), but
    /// the field is cheap and lets the boot log report adapter info.
    pub adapter_info: wgpu::AdapterInfo,
    /// Resolved adapter limits. Kept for diagnostics; desktop uses
    /// the equivalent for `platform_info` which test-bench replies
    /// `Err` to.
    #[allow(dead_code)]
    pub limits: wgpu::Limits,
    pipeline: Pipeline,
    targets: Targets,
}

impl Gpu {
    /// Initialise wgpu with no presentation surface. `width` and
    /// `height` size the offscreen color + depth targets — they're
    /// the dimensions every captured frame will report. wgpu picks
    /// an adapter via `request_adapter` with `compatible_surface:
    /// None`; on CI runners this resolves to whatever Vulkan/Metal/
    /// DX12 driver is available (ADR-0067 driver fallback policy).
    pub fn new(width: u32, height: u32) -> Self {
        let instance =
            wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::default(),
            compatible_surface: None,
            force_fallback_adapter: false,
        }))
        .expect("no compatible wgpu adapter");
        let adapter_info = adapter.get_info();
        let limits = wgpu::Limits::default();

        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("aether-test-bench device"),
            required_features: wgpu::Features::empty(),
            required_limits: limits.clone(),
            experimental_features: wgpu::ExperimentalFeatures::default(),
            memory_hints: wgpu::MemoryHints::default(),
            trace: wgpu::Trace::default(),
        }))
        .expect("request_device");

        let targets = Targets::new(&device, COLOR_FORMAT, width, height);
        let pipeline = build_main_pipeline(&device, &queue, COLOR_FORMAT, wgpu::PolygonMode::Fill);

        Self {
            device,
            queue,
            adapter_info,
            limits,
            pipeline,
            targets,
        }
    }

    /// Resize the offscreen target. Test-bench has no surface, so a
    /// resize just reallocates the offscreen color + depth textures
    /// and invalidates the readback buffer. Scenario scripts that
    /// need a different aspect ratio can mail an advance-style
    /// control to trigger this — for v1 the size is fixed at boot.
    #[allow(dead_code)] // wired in PR2 alongside test_bench.advance kinds
    pub fn resize(&mut self, width: u32, height: u32) {
        self.targets.resize(&self.device, width, height);
    }

    pub fn render(&mut self, vertices: &[u8], view_proj: &[f32; 16]) {
        let _ = self.render_impl(vertices, view_proj, false);
    }

    /// Variant of `render` that also copies the offscreen texture
    /// into a readback buffer, maps it, and returns an encoded PNG.
    /// On any capture-path failure, returns `Err(reason)`; the frame
    /// still rendered to the offscreen — capture is a side channel.
    pub fn render_and_capture(
        &mut self,
        vertices: &[u8],
        view_proj: &[f32; 16],
    ) -> Result<Vec<u8>, String> {
        self.render_impl(vertices, view_proj, true)
            .ok_or_else(|| "capture did not produce a result".to_owned())?
    }

    /// Draw `vertices` into the offscreen target with `view_proj` as
    /// the camera uniform, optionally encode a capture copy. Returns
    /// `Some(Ok(png))` / `Some(Err(reason))` when `capture` is set;
    /// `None` when `capture` is false or the capture path couldn't
    /// allocate. There is no presentation step — desktop's swapchain
    /// blit is omitted because there's no surface.
    fn render_impl(
        &mut self,
        vertices: &[u8],
        view_proj: &[f32; 16],
        capture: bool,
    ) -> Option<Result<Vec<u8>, String>> {
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("frame encoder"),
            });

        match record_main_pass(
            &self.queue,
            &mut encoder,
            &self.pipeline,
            &self.targets,
            vertices,
            view_proj,
            &[],
        ) {
            Ok(()) => {}
            Err(RenderError::VertexBufferOverflow { .. }) => return None,
        }

        let capture_meta: Option<CaptureMeta> = if capture {
            Some(prepare_capture_copy(
                &self.device,
                &mut self.targets,
                &mut encoder,
            ))
        } else {
            None
        };

        self.queue.submit(std::iter::once(encoder.finish()));

        capture_meta.map(|meta| finish_capture(&self.device, &self.targets, &meta))
    }
}
