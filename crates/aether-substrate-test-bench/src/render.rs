// Test-bench wgpu shim. ADR-0071 phase C3: pipeline + targets +
// device + queue ownership all live inside core's `RenderRunning`
// (via `RenderGpu` + `install_gpu`). What's left in this file is the
// thinnest reasonable wrapper: device acquisition (offscreen, no
// surface), and per-frame helpers that wrap encoder lifecycle around
// `RenderRunning`'s encoder-level methods.

use std::sync::Arc;

use aether_substrate_core::capabilities::{RenderGpu, RenderRunning};
use aether_substrate_core::render::RenderError;

pub use aether_substrate_core::render::VERTEX_BUFFER_BYTES;

/// Render target format. Test-bench commits to RGBA at init since
/// there's no surface to query, which keeps the readback path swizzle-
/// free.
const COLOR_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

pub struct Gpu {
    pub adapter_info: wgpu::AdapterInfo,
    /// Resolved adapter limits. Kept for diagnostics; desktop uses
    /// the equivalent for `platform_info` which test-bench replies
    /// `Err` to.
    #[allow(dead_code)]
    pub limits: wgpu::Limits,
    render_running: Arc<RenderRunning>,
}

impl Gpu {
    /// Initialise wgpu with no presentation surface, build the shared
    /// pipeline + targets via [`RenderGpu::new`], install them on
    /// `render_running` so encoder methods on the running can read
    /// them. `width` and `height` size the offscreen color + depth
    /// targets — the dimensions every captured frame will report.
    pub fn new(width: u32, height: u32, render_running: Arc<RenderRunning>) -> Self {
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

        render_running.install_gpu(RenderGpu::new(
            Arc::new(device),
            Arc::new(queue),
            COLOR_FORMAT,
            width,
            height,
            wgpu::PolygonMode::Fill,
        ));

        Self {
            adapter_info,
            limits,
            render_running,
        }
    }

    /// Resize the offscreen target. Test-bench has no surface, so a
    /// resize just reallocates the offscreen color + depth textures
    /// and invalidates the readback buffer.
    #[allow(dead_code)] // wired in PR2 alongside test_bench.advance kinds
    pub fn resize(&mut self, width: u32, height: u32) {
        self.render_running.resize(width, height);
    }

    /// Draw the current accumulator's vertices into the offscreen
    /// target with the latest camera view-proj. No presentation step
    /// — desktop's swapchain blit is omitted because there's no
    /// surface.
    pub fn render(&mut self) {
        let device = self.render_running.device();
        let queue = self.render_running.queue();
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("frame encoder"),
        });
        match self.render_running.record_frame(&mut encoder, &[]) {
            Ok(()) => {}
            Err(RenderError::VertexBufferOverflow { .. }) => return,
        }
        queue.submit(std::iter::once(encoder.finish()));
    }

    /// Variant of `render` that also copies the offscreen texture
    /// into a readback buffer, maps it, and returns an encoded PNG.
    /// On any capture-path failure, returns `Err(reason)`; the frame
    /// still rendered to the offscreen — capture is a side channel.
    pub fn render_and_capture(&mut self) -> Result<Vec<u8>, String> {
        let device = self.render_running.device();
        let queue = self.render_running.queue();
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("frame encoder"),
        });
        match self.render_running.record_frame(&mut encoder, &[]) {
            Ok(()) => {}
            Err(RenderError::VertexBufferOverflow { .. }) => {
                return Err("vertex buffer overflow — capture skipped".to_owned());
            }
        }
        let meta = self.render_running.record_capture_copy(&mut encoder);
        queue.submit(std::iter::once(encoder.finish()));
        self.render_running.finish_capture(&meta)
    }
}
