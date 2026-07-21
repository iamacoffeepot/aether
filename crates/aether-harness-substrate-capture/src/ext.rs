//! Seam implementations plugging the wgpu pipeline into the core harness
//! (issue #3765): [`GpuRenderExt`] chains the render cap into the
//! chassis builder from the boot wiring, [`GpuFrameHook`] drives the
//! per-frame draw + capture readback in the pump, and the two extension
//! traits give the builder and the harness their render-typed surface
//! back (`with_render`, `committed_overlay_snapshot`).

use std::any::Any;
use std::sync::Arc;

use aether_actor::Addressable;
use aether_harness_substrate::{
    BenchWiring, CaptureOutcome, FrameHook, RenderExt, SubstrateHarness, SubstrateHarnessBuilder,
    SubstrateHarnessChassis,
};
use aether_kinds::FrameCheck;
use aether_render::{
    CaptureBackend, DrawTexturedQuads, RenderCapability, RenderHandles, RenderParams, RenderTuningConfig,
};
use aether_substrate::capture::ReferenceCapture;
use aether_substrate::chassis::builder::{Builder, PassiveChassis};
use aether_substrate::mail::MailboxId;
use aether_substrate::render::VERTEX_BUFFER_BYTES;

use crate::gpu::Gpu;

/// [`RenderExt`] implementation: chains `RenderCapability` into the builder
/// with its `RenderTuningConfig` knobs + `RenderParams` wiring (ADR-0155 §4 /
/// ADR-0156 §3 keep the config pure — the observability hook and capture
/// backend are not config fields), and installs the Start-stage capture
/// backend after boot.
pub struct GpuRenderExt;

impl RenderExt for GpuRenderExt {
    fn compose(
        &self,
        wiring: &BenchWiring,
        builder: Builder<SubstrateHarnessChassis>,
    ) -> Builder<SubstrateHarnessChassis> {
        builder
            .with_config(RenderTuningConfig { vertex_buffer_bytes: VERTEX_BUFFER_BYTES })
            .with_actor::<RenderCapability>(RenderParams {
                observed_kinds: wiring.observed_kinds.clone(),
                assets_dir: wiring.assets_dir.clone(),
            })
    }

    fn install_capture_backend(&self, wiring: &BenchWiring, passive: &PassiveChassis<SubstrateHarnessChassis>) {
        // Issue 629 / Phase A: the render cap published its handles during
        // `init`; ADR-0155 §4 makes the capture backend a Start-stage handoff
        // installed into that shared bundle rather than a `RenderParams`
        // field. The desktop driver does the same in its `boot`.
        let handles: RenderHandles = passive.handle::<RenderHandles>().expect(
            "RenderHandles must be published before installing the capture backend — \
             RenderCapability boots via GpuRenderExt::compose",
        );
        handles.install_capture_backend(CaptureBackend {
            queue: wiring.capture_queue.clone(),
            wake: Arc::clone(&wiring.capture_wake),
            outbound: Arc::clone(&wiring.outbound),
        });
    }
}

/// [`FrameHook`] implementation wrapping the offscreen [`Gpu`]: the
/// advance path's per-frame draw, the capture readback, and the render
/// mailbox capture requests route to.
pub struct GpuFrameHook {
    gpu: Gpu,
}

impl GpuFrameHook {
    /// Snapshot the committed overlay batches from the latest rendered
    /// frame — the concrete accessor `RenderHarnessExt` reaches through
    /// [`FrameHook::as_any`].
    #[must_use]
    pub fn committed_overlay_snapshot(&self) -> Vec<DrawTexturedQuads> {
        self.gpu.committed_overlay_snapshot()
    }
}

impl FrameHook for GpuFrameHook {
    fn render_frame(&mut self) {
        self.gpu.render();
    }

    fn render_and_capture(&mut self, checks: &[FrameCheck], reference: Option<&ReferenceCapture>) -> CaptureOutcome {
        self.gpu.render_and_capture(checks, reference)
    }

    fn capture_mailbox(&self) -> MailboxId {
        // Harness route to the render cap's own id (its NAMESPACE) —
        // ctx-less driver-side push, no resolver in scope.
        #[allow(clippy::disallowed_methods)]
        aether_data::mailbox_id_from_name(RenderCapability::NAMESPACE)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Builder extension composing render support: the cap (via
/// [`GpuRenderExt`]) plus the frame hook built against the booted
/// chassis's published [`RenderHandles`] at the builder's offscreen
/// size.
pub trait RenderHarnessBuilderExt {
    #[must_use]
    fn with_render(self) -> Self;
}

impl RenderHarnessBuilderExt for SubstrateHarnessBuilder {
    fn with_render(self) -> Self {
        self.render_ext(
            Box::new(GpuRenderExt),
            Box::new(|passive, width, height| {
                // Issue 629 / Phase A: render publishes its handles on the
                // chassis's `ExportedHandles` map during `init`; no
                // `Arc<RenderCapability>` ever escapes the dispatcher.
                let handles: RenderHandles = passive.handle::<RenderHandles>().ok_or_else(|| {
                    anyhow::anyhow!(
                        "RenderHandles not published — RenderCapability must boot via the render ext before the hook builds",
                    )
                })?;
                Ok(Box::new(GpuFrameHook { gpu: Gpu::new(width, height, handles) }))
            }),
        )
    }
}

/// Harness extension restoring the render-typed overlay accessor the
/// core no longer owns.
pub trait RenderHarnessExt {
    /// Snapshot the ordered, typed overlay submissions from the latest
    /// frame committed by an `advance` or `capture` op. Solid
    /// submissions appear normalized as [`DrawTexturedQuads`] over the
    /// renderer's reserved white texture; batches rejected while
    /// recording (missing texture, invalid/empty clip, past the vertex
    /// budget) are absent. Capture uses replay-cache semantics: with no
    /// new overlay mail the snapshot remains the latest committed
    /// frame, and an advance committing an empty overlay frame clears
    /// it. Returned values own their data.
    ///
    /// # Panics
    /// Panics if the harness was built without
    /// [`RenderHarnessBuilderExt::with_render`] — there is no overlay
    /// pipeline to snapshot.
    #[must_use]
    fn committed_overlay_snapshot(&self) -> Vec<DrawTexturedQuads>;
}

impl RenderHarnessExt for SubstrateHarness {
    fn committed_overlay_snapshot(&self) -> Vec<DrawTexturedQuads> {
        self.frame_hook()
            .and_then(|hook| hook.as_any().downcast_ref::<GpuFrameHook>())
            .expect("committed_overlay_snapshot requires a harness built with .with_render() (issue #3764)")
            .committed_overlay_snapshot()
    }
}
