// Test-bench wgpu shim. ADR-0071 phase C3: pipeline + targets +
// device + queue ownership all live inside core's `RenderCapability`
// (via `RenderGpu` + `install_gpu`). What's left in this file is the
// thinnest reasonable wrapper: device acquisition (offscreen, no
// surface), and per-frame helpers that wrap encoder lifecycle around
// `RenderCapability`'s encoder-level methods.

use std::sync::Arc;

use aether_capabilities::{RenderGpu, RenderHandles};
use aether_kinds::{FrameCheck, FrameVerdict};
use aether_substrate::capture::ReferenceCapture;
use aether_substrate::render::{RenderError, encode_png};

use crate::visual;
pub use aether_substrate::render::VERTEX_BUFFER_BYTES;
use std::iter;

/// PNG bytes, optional [`FrameVerdict`], optional similarity score, and
/// optional similarity pass that `render_and_capture` produces. The
/// verdict is `Some` iff the request carried `checks`
/// (iamacoffeepot/aether#1777); the similarity score / pass are `Some`
/// iff the request carried a `reference` (iamacoffeepot/aether#1780).
type CaptureOutcome = Result<(Vec<u8>, Option<FrameVerdict>, Option<f32>, Option<bool>), String>;

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
    render_handles: RenderHandles,
}

impl Gpu {
    /// Initialise wgpu with no presentation surface, build the shared
    /// pipeline + targets via [`RenderGpu::new`], install them on
    /// `render_running` so encoder methods on the running can read
    /// them. `width` and `height` size the offscreen color + depth
    /// targets — the dimensions every captured frame will report.
    ///
    /// # Panics
    /// Panics if adapter selection or device acquisition fail —
    /// fail-fast per ADR-0063: the test bench can't proceed without a
    /// usable offscreen wgpu pipeline, and driverless dev boxes are
    /// expected to skip the test entirely (handled at the scenario
    /// runner layer per ADR-0067).
    #[must_use]
    pub fn new(width: u32, height: u32, render_handles: RenderHandles) -> Self {
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

        render_handles.install_gpu(RenderGpu::new(
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
            render_handles,
        }
    }

    /// Resize the offscreen target. Test-bench has no surface, so a
    /// resize just reallocates the offscreen color + depth textures
    /// and invalidates the readback buffer.
    #[allow(dead_code)] // wired in PR2 alongside test_bench.advance kinds
    pub fn resize(&mut self, width: u32, height: u32) {
        self.render_handles.resize(width, height);
    }

    /// Draw the current accumulator's vertices into the offscreen
    /// target with the latest camera view-proj. No presentation step
    /// — desktop's swapchain blit is omitted because there's no
    /// surface. Drives the test-bench's advance path; commits the
    /// current frame to the render cap's `last_submitted` cache so
    /// any subsequent `capture` observes the freshly-rendered state
    /// (or an empty cache, if the producer chose not to emit).
    pub fn render(&mut self) {
        let device = self.render_handles.device();
        let queue = self.render_handles.queue();
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("frame encoder"),
        });
        // Advance path: commit-current (false). Empty live clears
        // the cache so a producer that stopped emitting flushes
        // cleanly to the next capture.
        match self.render_handles.record_frame(&mut encoder, &[], false) {
            Ok(()) => {}
            Err(RenderError::VertexBufferOverflow { .. }) => return,
        }
        // ADR-0105 textured-quad overlay, recorded after the world pass.
        self.render_handles.record_overlay_pass(&mut encoder, false);
        queue.submit(iter::once(encoder.finish()));
    }

    /// Variant of `render` that also copies the offscreen texture
    /// into a readback buffer, maps it, and returns an encoded PNG
    /// plus an optional [`FrameVerdict`] scored on the same raw RGBA
    /// (present iff `checks` is non-empty; iamacoffeepot/aether#1777).
    /// On any capture-path failure, returns `Err(reason)`; the frame
    /// still rendered to the offscreen — capture is a side channel.
    ///
    /// Drives the test-bench's `TestBench::capture` path with
    /// `dispatch_tick=false`. The render cap's `replay_cache_when_idle`
    /// flag is set so an empty live accumulator (no producer
    /// emitted, because no `Tick` was dispatched for this frame)
    /// replays the cache from the last committed advance —
    /// iamacoffeepot/aether#847 retired the historical `nudge_tick`
    /// boilerplate that worked around the prior consume-and-discard
    /// behaviour.
    pub fn render_and_capture(
        &mut self,
        checks: &[FrameCheck],
        reference: Option<&ReferenceCapture>,
    ) -> CaptureOutcome {
        let device = self.render_handles.device();
        let queue = self.render_handles.queue();
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("frame encoder"),
        });
        // Capture path: replay-cache (true). Empty live → render
        // whatever the last advance committed.
        match self.render_handles.record_frame(&mut encoder, &[], true) {
            Ok(()) => {}
            Err(RenderError::VertexBufferOverflow { .. }) => {
                return Err("vertex buffer overflow — capture skipped".to_owned());
            }
        }
        // ADR-0105 textured-quad overlay, same replay-cache semantics so
        // an idle capture replays the last committed quads.
        self.render_handles.record_overlay_pass(&mut encoder, true);
        let meta = self.render_handles.record_capture_copy(&mut encoder);
        queue.submit(iter::once(encoder.finish()));
        // Map the readback once; encode the PNG and score the verdict
        // from the same de-padded RGBA so a verdict scores the exact
        // bytes the PNG carries (iamacoffeepot/aether#1777).
        let rgba = self.render_handles.map_capture_rgba(&meta)?;
        let png = encode_png(&rgba, meta.width, meta.height)?;
        // Score the similarity check before `run_checks` consumes `rgba`
        // (iamacoffeepot/aether#1780). `score_similarity` clones internally.
        let (similarity_score, similarity_pass) =
            visual::score_similarity(&rgba, meta.width, meta.height, reference)?;
        let verdict =
            (!checks.is_empty()).then(|| visual::run_checks(rgba, meta.width, meta.height, checks));
        Ok((png, verdict, similarity_score, similarity_pass))
    }
}
