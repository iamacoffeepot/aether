//! Seam implementations plugging the render pipeline into the core harness.
//!
//! ADR-0161 slice R4 moves the in-process `SubstrateHarness` onto the
//! **pumped** `aether.render` runtime: [`PumpedGpuRenderExt`] composes no
//! build-time render cap (the pumped slot is claimed post-boot), and
//! [`GpuFrameHook`] owns the [`PumpedSlot`] for the pumped render actor,
//! draining it at the harness's step / capture pump points so draw dispatch,
//! capture readback, and present all run on the harness thread that owns the
//! offscreen GPU. The `with_render` builder extension boots that slot.
//!
//! The pooled [`GpuRenderExt`] + [`Gpu`](crate::Gpu) survive for the
//! standalone `substrate-harness` binary (a separate MCP-driven driver that
//! runs its own event loop over the pooled `RenderHandles`); the pumped
//! deletion of the pooled path is R5.

use std::any::Any;
use std::sync::Arc;

use aether_actor::Addressable;
use aether_data::{Kind, MailId};
use aether_harness_substrate::{
    BenchWiring, FrameHook, RenderExt, RenderHookWiring, SubstrateHarness, SubstrateHarnessBuilder,
    SubstrateHarnessChassis,
};
use aether_render::{
    CaptureBackend, CaptureScorer, DrawTexturedQuads, Frame, PumpedRenderCapability, PumpedRenderParams,
    RenderCapability, RenderHandles, RenderParams, RenderTuningConfig,
};
use aether_substrate::PumpedSlot;
use aether_substrate::chassis::builder::{Builder, PassiveChassis};
use aether_substrate::mail::mailer::Mailer;
use aether_substrate::mail::{Mail, MailboxId};
use aether_substrate::render::VERTEX_BUFFER_BYTES;

// Partition note (ADR-0161 R4 PR): the `FrameCheck` / similarity scoring
// symbols live in the `visual` module today; a parallel change is relocating
// that module out of this crate. Per the R4 partition rule this import stays
// pointed at the current home — whichever PR lands second reconciles the path.
use crate::visual;

/// Pooled [`RenderExt`] implementation for the standalone `substrate-harness`
/// binary: chains the pooled `RenderCapability` into the builder with its
/// `RenderTuningConfig` knobs + `RenderParams` wiring and installs the
/// Start-stage capture backend after boot. The in-process `SubstrateHarness`
/// uses the pumped path ([`PumpedGpuRenderExt`]) instead.
pub struct GpuRenderExt;

impl RenderExt for GpuRenderExt {
    fn compose(
        &self,
        wiring: &BenchWiring,
        builder: Builder<SubstrateHarnessChassis>,
    ) -> Builder<SubstrateHarnessChassis> {
        // ADR-0156 §5: compose + stage the render tuning in one paired call.
        builder.with_actor_configured::<RenderCapability>(
            RenderParams { observed_kinds: wiring.observed_kinds.clone(), assets_dir: wiring.assets_dir.clone() },
            RenderTuningConfig { vertex_buffer_bytes: VERTEX_BUFFER_BYTES },
        )
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

/// Pumped [`RenderExt`] for the in-process `SubstrateHarness` (ADR-0161 slice
/// R4). The pumped `aether.render` slot is claimed post-`build_passive` by
/// the hook factory via `PassiveChassis::boot_pumped_actor`, so there is no
/// build-time render cap to compose and no capture backend to install — both
/// hooks are no-ops. The wiring the pooled path threaded through
/// `RenderParams` rides [`RenderHookWiring`] to the hook factory instead.
pub struct PumpedGpuRenderExt;

impl RenderExt for PumpedGpuRenderExt {
    fn compose(
        &self,
        _wiring: &BenchWiring,
        builder: Builder<SubstrateHarnessChassis>,
    ) -> Builder<SubstrateHarnessChassis> {
        // No build-time render cap: the pumped slot claims `aether.render`
        // after boot (the hook factory), so nothing is composed here.
        builder
    }

    fn install_capture_backend(&self, _wiring: &BenchWiring, _passive: &PassiveChassis<SubstrateHarnessChassis>) {
        // The pumped `on_capture_frame` owns the capture machine outright —
        // there is no cross-thread `CaptureBackend` to install (R5 deletes
        // the pooled backend entirely).
    }
}

/// [`FrameHook`] owning the [`PumpedSlot`] for the pumped `aether.render`
/// actor (ADR-0161 slice R4). The harness drains the slot at its step /
/// capture pump points and mails `aether.render.frame` to record; capture is
/// mail-driven inside the actor, so this hook never touches wgpu directly —
/// the surfaceless GPU boots lazily inside the pumped runtime on the first
/// frame from the `offscreen_size` params.
pub struct GpuFrameHook {
    slot: PumpedSlot<PumpedRenderCapability>,
    /// The chassis mailer, so the hook can mail `frame` to the pumped slot.
    mailer: Arc<Mailer>,
    /// The pumped render actor's mailbox — where `frame` mail routes.
    render_mailbox: MailboxId,
}

impl GpuFrameHook {
    /// Snapshot the committed overlay batches from the latest rendered frame
    /// — the concrete accessor `RenderHarnessExt` reaches through
    /// [`FrameHook::as_any`], read off the pumped actor's state.
    #[must_use]
    // The pumped state type is `pub` inside a private module of aether-render,
    // so it is unnameable here — the closure form is required (the suggested
    // method reference would not compile).
    #[allow(clippy::redundant_closure_for_method_calls)]
    pub fn committed_overlay_snapshot(&self) -> Vec<DrawTexturedQuads> {
        self.slot.read_state(|state| state.committed_overlay_snapshot()).unwrap_or_default()
    }
}

impl FrameHook for GpuFrameHook {
    fn send_frame(&mut self, replay_cache_when_idle: bool) {
        // Fire-and-forget internal frame request (disarmed lineage — no
        // settlement obligation): the harness awaits the capture reply / the
        // advance's `LifecycleAdvanceComplete`, not the frame itself.
        let payload = Frame { replay_cache_when_idle }.encode_into_bytes();
        self.mailer.push(Mail::new(self.render_mailbox, <Frame as Kind>::ID, payload, 1).with_lineage(
            MailId::NONE,
            MailId::NONE,
            None,
        ));
        self.slot.drain_available();
    }

    fn pump(&mut self) {
        self.slot.drain_available();
    }

    // See `committed_overlay_snapshot`: the pumped state type is unnameable
    // here, so the closure form is required over a method reference.
    #[allow(clippy::redundant_closure_for_method_calls)]
    fn has_pending_capture(&self) -> bool {
        self.slot.read_state(|state| state.has_pending_capture()).unwrap_or(false)
    }

    fn render_mailbox(&self) -> MailboxId {
        self.render_mailbox
    }

    fn shutdown(&mut self) {
        self.slot.shutdown();
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Builder extension composing render support via the pumped runtime
/// (ADR-0161 slice R4): [`PumpedGpuRenderExt`] (a no-op compose) plus the
/// hook factory that boots the pumped `aether.render` slot against the booted
/// chassis at the builder's offscreen size, injecting the offscreen GPU
/// config + the capture scorer through `PumpedRenderParams`.
pub trait RenderHarnessBuilderExt {
    #[must_use]
    fn with_render(self) -> Self;
}

impl RenderHarnessBuilderExt for SubstrateHarnessBuilder {
    fn with_render(self) -> Self {
        self.render_ext(
            Box::new(PumpedGpuRenderExt),
            Box::new(|passive, wiring, width, height| {
                let RenderHookWiring { mailer, observed_kinds, assets_dir } = wiring;
                // The capture scorer the pumped runtime injects on a ready
                // readback (ADR-0161 R4): similarity first (borrows the RGBA),
                // then the `FrameCheck` verdict (consumes a copy), matching
                // the pooled `Gpu::render_and_capture` ordering.
                let scorer: CaptureScorer = Arc::new(|rgba, w, h, checks, reference| {
                    let (similarity_score, similarity_pass) =
                        visual::score_similarity(rgba, w, h, reference).unwrap_or((None, None));
                    let verdict = (!checks.is_empty()).then(|| visual::run_checks(rgba.to_vec(), w, h, checks));
                    (verdict, similarity_score, similarity_pass)
                });
                // `..Default::default()` fills `wireframe: None` and — under a
                // feature-unified build that enables aether-render/desktop —
                // the desktop-only `window: None`, so this literal is robust
                // to feature unification.
                let params = PumpedRenderParams {
                    observed_kinds,
                    assets_dir,
                    offscreen_size: Some((width, height)),
                    scorer: Some(scorer),
                    ..Default::default()
                };
                let (slot, _wake_slot) = passive
                    .boot_pumped_actor::<PumpedRenderCapability>(
                        RenderTuningConfig { vertex_buffer_bytes: VERTEX_BUFFER_BYTES },
                        params,
                    )
                    .map_err(|e| anyhow::anyhow!("boot pumped render slot: {e}"))?;
                // The pumped slot registered its inbox under the actor's
                // NAMESPACE, so its id is the name hash — the same id
                // `send_and_await("aether.render", CaptureFrame)` resolves.
                #[allow(clippy::disallowed_methods)] // ctx-less harness setup; no sibling resolver in scope
                let render_mailbox = aether_data::mailbox_id_from_name(PumpedRenderCapability::NAMESPACE);
                Ok(Box::new(GpuFrameHook { slot, mailer, render_mailbox }) as Box<dyn FrameHook>)
            }),
        )
    }
}

/// Harness extension restoring the render-typed overlay accessor the core no
/// longer owns.
pub trait RenderHarnessExt {
    /// Snapshot the ordered, typed overlay submissions from the latest frame
    /// committed by an `advance` or `capture` op. Solid submissions appear
    /// normalized as [`DrawTexturedQuads`] over the renderer's reserved white
    /// texture; batches rejected while recording (missing texture,
    /// invalid/empty clip, past the vertex budget) are absent. Capture uses
    /// replay-cache semantics: with no new overlay mail the snapshot remains
    /// the latest committed frame, and an advance committing an empty overlay
    /// frame clears it. Returned values own their data.
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
