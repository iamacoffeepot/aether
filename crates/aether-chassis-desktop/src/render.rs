// Desktop wgpu plumbing. ADR-0071 phase C2: pipeline + targets moved
// into core's `RenderCapability` (via `RenderGpu` + `install_gpu`); this
// file now owns only the desktop-specific surface + swapchain config
// + optional wireframe overlay pipeline. Each frame creates an
// encoder, asks `render_handles.record_frame(...)` to record the
// shared offscreen pass, optionally records a capture copy, copies
// offscreen → swapchain, submits, and presents.
//
// Wireframe (`AETHER_WIREFRAME=line|overlay`) is desktop-only — a
// dev-affordance for inspecting triangulation on the windowed
// chassis. `Line` builds RenderGpu with `PolygonMode::Line` so the
// main pipeline draws as wires; `Overlay` keeps Fill and adds a
// second pipeline as an extra in `record_frame`.

use std::sync::Arc;

use aether_kinds::{FrameCheck, FrameVerdict};
use aether_render::{
    RenderGpu, RenderHandles, acquire_surface_texture, boot_surface, build_wireframe_overlay_pipeline,
};
use aether_substrate::capture::ReferenceCapture;
use aether_substrate::render::{self, RenderError, encode_png};
use winit::dpi::PhysicalSize;
use winit::window::Window;

use aether_harness_substrate_capture::visual;

pub use render::VERTEX_BUFFER_BYTES;
use std::iter;

/// PNG bytes, optional [`FrameVerdict`], optional similarity score, and
/// optional similarity pass that `render_and_capture` produces
/// (iamacoffeepot/aether#1777, #1780). `verdict` is `Some` iff the
/// request carried `checks`; `similarity_score` / `similarity_pass` are
/// `Some` iff the request carried `similarity`.
type CaptureOutcome = Result<(Vec<u8>, Option<FrameVerdict>, Option<f32>, Option<bool>), String>;

pub struct Gpu {
    pub surface: wgpu::Surface<'static>,
    pub config: wgpu::SurfaceConfiguration,
    /// Snapshot of the adapter chosen at `new()` — `AdapterInfo` plus
    /// the resolved `Limits`. Retained so `platform_info` can report
    /// what the substrate is running on without a second adapter
    /// request (which would be expensive and is a one-time fact
    /// anyway).
    pub adapter_info: wgpu::AdapterInfo,
    pub limits: wgpu::Limits,
    /// Wireframe overlay pipeline. `Some` only when `AETHER_WIREFRAME`
    /// is `1` / `overlay`. `record_frame` draws this after the main
    /// pipeline as an extra inside the same render pass.
    wire_pipeline: Option<wgpu::RenderPipeline>,
    render_handles: RenderHandles,
    /// Submission index of the previous frame's `queue.submit`, drained
    /// at the top of the next `render_impl` to bound the window present
    /// loop to one frame in flight (iamacoffeepot/aether#1312).
    ///
    /// Without this bound the loop submits + presents as fast as
    /// `nextDrawable` backpressure allows (up to `maximumDrawableCount`
    /// = 3 frames of command buffers overlapping). Under sustained
    /// rendering that exposes a use-after-free in Metal/IOGPU's command-
    /// buffer completion path (`IOGPUMetalCommandBufferStorageReset` on
    /// `com.Metal.CompletionQueueDispatch`): the completion handler for
    /// an earlier frame tears its command buffer down while the main
    /// thread is acquiring/submitting a later one. Draining the prior
    /// submission first means that teardown runs while this thread is
    /// parked in `device.poll`, never racing the next submit.
    last_submission: Option<wgpu::SubmissionIndex>,
}

impl Gpu {
    /// Construct the desktop chassis's wgpu state and install the shared
    /// `RenderGpu` into `render_handles`. Called once during desktop boot
    /// from inside winit's `resumed` handler. The instance / surface /
    /// adapter / device / swapchain boot is the shared
    /// [`boot_surface`](aether_render::boot_surface); this owns the desktop
    /// surface + the optional wireframe overlay pipeline on top.
    ///
    /// # Panics
    /// Panics if surface creation, adapter selection, or device
    /// acquisition fail — fail-fast per ADR-0063: the desktop chassis
    /// can't proceed without a usable GPU pipeline.
    //
    // `window` is owned because the boot path is a one-shot handoff: the
    // driver builds the `Arc<Window>` once and `boot_surface` takes a clone
    // for the surface; the owning form mirrors the `RenderHandles` argument.
    #[must_use]
    #[allow(clippy::needless_pass_by_value)]
    pub fn new(window: Arc<Window>, render_handles: RenderHandles, wireframe: Option<&str>) -> Self {
        let size = window.inner_size();
        let booted = boot_surface(Arc::clone(&window), (size.width, size.height), wireframe);
        render_handles.install_gpu(RenderGpu::new(
            Arc::clone(&booted.device),
            Arc::clone(&booted.queue),
            booted.format,
            booted.config.width,
            booted.config.height,
            booted.polygon_mode,
            render_handles.vertex_buffer_bytes,
        ));

        // Wireframe overlay pipeline, built post-install so it can borrow
        // the installed pipeline's layout (same camera bind group).
        let wire_pipeline = booted.build_overlay.then(|| {
            let installed = render_handles.gpu().expect("install_gpu just succeeded");
            build_wireframe_overlay_pipeline(&booted.device, booted.config.format, &installed.pipeline.pipeline_layout)
        });

        Self {
            surface: booted.surface,
            config: booted.config,
            adapter_info: booted.adapter_info,
            limits: booted.limits,
            wire_pipeline,
            render_handles,
            last_submission: None,
        }
    }

    pub fn resize(&mut self, size: PhysicalSize<u32>) {
        if size.width == 0 || size.height == 0 {
            return;
        }
        self.config.width = size.width;
        self.config.height = size.height;
        self.surface.configure(&self.render_handles.device(), &self.config);
        self.render_handles.resize(self.config.width, self.config.height);
    }

    pub fn render(&mut self) {
        let _ = self.render_impl(None, None);
    }

    /// Variant of `render` that also copies the offscreen texture into
    /// a readback buffer, maps it, and returns an encoded PNG plus an
    /// optional [`FrameVerdict`] scored on the same raw RGBA (present
    /// iff `checks` is non-empty, iamacoffeepot/aether#1777) and an
    /// optional similarity score (present iff `reference` is `Some`,
    /// iamacoffeepot/aether#1780). On any capture-path failure, returns
    /// `Err(reason)`; the frame itself still renders and (if the surface
    /// is available) presents, since capture is a side channel.
    pub fn render_and_capture(
        &mut self,
        checks: &[FrameCheck],
        reference: Option<&ReferenceCapture>,
    ) -> CaptureOutcome {
        self.render_impl(Some(checks), reference).ok_or_else(|| "capture did not produce a result".to_owned())?
    }

    /// Draw the current accumulator vertices into the offscreen target
    /// with the latest camera view-proj, optionally encode a capture
    /// copy, then best-effort blit to the swapchain and present.
    /// Returns `Some(Ok((png, verdict, score, pass)))` /
    /// `Some(Err(reason))` when `capture` is `Some(checks)`; `None`
    /// when capture wasn't requested or the capture path couldn't
    /// allocate. Surface unavailability does *not* prevent capture —
    /// offscreen is the source of truth.
    fn render_impl(
        &mut self,
        capture: Option<&[FrameCheck]>,
        reference: Option<&ReferenceCapture>,
    ) -> Option<CaptureOutcome> {
        let device = self.render_handles.device();
        let queue = self.render_handles.queue();

        // Bound the loop to one frame in flight (iamacoffeepot/aether#1312):
        // block until the previous frame's submission has fully completed
        // before recording the next. This drains the prior frame's command-
        // buffer completion (and its Metal/IOGPU teardown) while this thread
        // is parked here, so it can't race the acquire/submit below — and it
        // serialises writes to the shared persistent vertex/camera buffers
        // against the prior frame's reads. `poll` errors (a lost device)
        // fall through to `acquire_surface_texture`, which reconfigures.
        if let Some(index) = self.last_submission.take()
            && let Err(error) = device.poll(wgpu::PollType::Wait { submission_index: Some(index), timeout: None })
        {
            tracing::warn!(
                target: "aether_substrate::render",
                ?error,
                "device.poll for previous frame failed; continuing",
            );
        }

        let mut encoder =
            device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame encoder") });

        let wire_ref = self.wire_pipeline.as_ref();
        let extras_storage: [&wgpu::RenderPipeline; 1];
        let extra_pipelines: &[&wgpu::RenderPipeline] = match wire_ref {
            Some(p) => {
                extras_storage = [p];
                &extras_storage
            }
            None => &[],
        };
        // Desktop renders every frame from current producer state —
        // commit-current semantic (false). The replay-cache mode is
        // reserved for `SubstrateHarness::capture` (iamacoffeepot/aether#847).
        match self.render_handles.record_frame(&mut encoder, extra_pipelines, false) {
            Ok(()) => {}
            Err(RenderError::VertexBufferOverflow { .. }) => return None,
        }

        // ADR-0140 material pass, recorded after the world pass and
        // before the screen overlay.
        self.render_handles.record_material_pass(&mut encoder, false);

        // ADR-0105 textured-quad overlay, recorded after world/material —
        // commit-current semantic to match `record_frame` above.
        self.render_handles.record_overlay_pass(&mut encoder, false);

        // Capture path: the copy runs against the offscreen texture,
        // which is unaffected by whether a swapchain image is available
        // this frame. That decouples capture from window visibility.
        let capture_meta = if capture.is_some() {
            Some(self.render_handles.record_capture_copy(&mut encoder))
        } else {
            None
        };

        // Try to obtain a swapchain texture for presentation. If the
        // surface is occluded/lost/outdated we just skip the blit +
        // present step — the offscreen is already fresh and captures
        // still resolve.
        let surface_tex = acquire_surface_texture(&self.surface, &self.render_handles.device(), &self.config);
        if let Some(tex) = surface_tex.as_ref() {
            let (w, h) = self.render_handles.color_size();
            self.render_handles.with_color_texture(|src| {
                encoder.copy_texture_to_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: src,
                        mip_level: 0,
                        origin: wgpu::Origin3d::ZERO,
                        aspect: wgpu::TextureAspect::All,
                    },
                    wgpu::TexelCopyTextureInfo {
                        texture: &tex.texture,
                        mip_level: 0,
                        origin: wgpu::Origin3d::ZERO,
                        aspect: wgpu::TextureAspect::All,
                    },
                    wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                );
            });
        }

        // Retain the submission index so the next frame can wait on it
        // (see `last_submission`). The present command buffer wgpu creates
        // internally is committed after this submit and bounded by
        // `nextDrawable`, so it needs no separate tracking.
        self.last_submission = Some(queue.submit(iter::once(encoder.finish())));
        if let Some(tex) = surface_tex {
            tex.present();
        }

        // Map the readback once; encode the PNG, (when checks were
        // requested) score the verdict, and (when a reference was
        // provided) score the MAE similarity — all from the same
        // de-padded RGBA so every check sees the exact bytes the PNG
        // carries (iamacoffeepot/aether#1777, #1780).
        // `capture_meta` is `Some` iff `capture` is `Some`, so
        // `unwrap_or(&[])` only papers over an unreachable case.
        capture_meta.map(|meta| {
            let rgba = self.render_handles.map_capture_rgba(&meta)?;
            let png = encode_png(&rgba, meta.width, meta.height)?;

            // Score the similarity check before `run_checks` consumes
            // `rgba` (#1780). `score_similarity` clones the slice it needs.
            let (similarity_score, similarity_pass) =
                visual::score_similarity(&rgba, meta.width, meta.height, reference)?;

            let checks = capture.unwrap_or(&[]);
            let verdict = (!checks.is_empty()).then(|| visual::run_checks(rgba, meta.width, meta.height, checks));
            Ok((png, verdict, similarity_score, similarity_pass))
        })
    }
}
