//! Seam implementations plugging the pumped render runtime into the core
//! harness (ADR-0161).
//!
//! [`GpuFrameHook`] owns the [`PumpedSlot`] for the pumped `aether.render`
//! actor, draining it at the harness's step / capture pump points so draw
//! dispatch, capture readback, and present all run on the harness thread that
//! owns the offscreen GPU. The `with_render` builder extension boots that slot
//! post-`build_passive` via `PassiveChassis::boot_pumped_actor`; the
//! surfaceless GPU boots lazily inside the pumped runtime on the first frame
//! from the `offscreen_size` params.

use std::any::Any;
use std::sync::Arc;

use aether_actor::Addressable;
use aether_data::{Kind, MailId};
use aether_harness_substrate::{
    ExecutionError, FrameHook, HarnessOp, RenderHookWiring, SubstrateHarness, SubstrateHarnessBuilder,
};
use aether_render::{
    DrawTexturedQuads, Frame, ProgramTimings, ProgramTimingsResult, RenderCapability, RenderParams, RenderTuningConfig,
};
use aether_substrate::PumpedSlot;
use aether_substrate::mail::mailer::Mailer;
use aether_substrate::mail::{Mail, MailboxId};
use aether_substrate::render::VERTEX_BUFFER_BYTES;

/// [`FrameHook`] owning the [`PumpedSlot`] for the pumped `aether.render`
/// actor (ADR-0161). The harness drains the slot at its step / capture pump
/// points and mails `aether.render.frame` to record; capture is mail-driven
/// inside the actor, so this hook never touches wgpu directly — the
/// surfaceless GPU boots lazily inside the pumped runtime on the first frame
/// from the `offscreen_size` params.
pub struct GpuFrameHook {
    slot: PumpedSlot<RenderCapability>,
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

    /// Deterministically lose the concrete offscreen device. This is a
    /// host-only scenario seam: it reaches the actor state directly through
    /// the owned pumped slot and introduces no render kind or guest callback.
    // The pumped state type is `pub` inside a private module of aether-render,
    // so it is unnameable here — the closure form is required (the suggested
    // method reference would not compile).
    #[allow(clippy::redundant_closure_for_method_calls)]
    pub fn force_device_loss(&self) -> Result<u64, String> {
        self.slot
            .read_state(|state| state.force_device_loss_for_harness())
            .unwrap_or_else(|| Err("the pumped render slot is closed".to_owned()))
    }
}

impl FrameHook for GpuFrameHook {
    fn send_frame(&mut self, replay_cache_when_idle: bool) {
        // Fire-and-forget internal frame request (disarmed lineage — no
        // settlement obligation): the harness awaits the capture reply / the
        // advance's `LifecycleAdvanceComplete`, not the frame itself.
        let payload = Frame { replay_cache_when_idle, windows: Vec::new() }.encode_into_bytes();
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
    fn capture_ready(&self) -> bool {
        self.slot.read_state(|state| state.capture_ready()).unwrap_or(false)
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
/// (ADR-0161): registers the hook factory that boots the pumped
/// `aether.render` slot against the booted chassis at the builder's offscreen
/// size, passing the offscreen GPU config through [`RenderParams`].
pub trait RenderHarnessBuilderExt {
    #[must_use]
    fn with_render(self) -> Self;

    /// Render support with the per-pass GPU timing instrument on
    /// (iamacoffeepot/aether#4423). An explicit builder call rather than
    /// the cap's `AETHER_RENDER_PASS_TIMINGS` env knob, because a test
    /// binary runs its scenarios in parallel threads and a process-wide
    /// environment mutation would decide the instrument's state for
    /// whichever scenario happened to boot next.
    ///
    /// Timestamp queries can perturb the frame they measure, so the
    /// instrument stays off for every other scenario.
    #[must_use]
    fn with_render_pass_timings(self) -> Self;
}

impl RenderHarnessBuilderExt for SubstrateHarnessBuilder {
    fn with_render(self) -> Self {
        render_hook(self, false)
    }

    fn with_render_pass_timings(self) -> Self {
        render_hook(self, true)
    }
}

/// The shared render hook both builder entry points register, differing
/// only in whether the booted cap measures per-pass GPU durations.
fn render_hook(builder: SubstrateHarnessBuilder, pass_timings: bool) -> SubstrateHarnessBuilder {
    builder.render_hook(Box::new(move |passive, wiring, width, height| {
        let RenderHookWiring { mailer, observed_kinds, assets_dir } = wiring;
        // The `FrameCheck` / similarity scorer lives in
        // `aether_substrate::render::visual` (below aether-render), so the
        // pumped runtime scores capture verdicts + similarity directly in
        // its ready-readback branch — no scorer injection.
        // `..Default::default()` fills `wireframe: None` and — under a
        // feature-unified build that enables aether-render/desktop — the
        // desktop-only `window: None`, so this literal is robust to feature
        // unification.
        let params =
            RenderParams { observed_kinds, assets_dir, offscreen_size: Some((width, height)), ..Default::default() };
        let (slot, _wake_slot) = passive
            .boot_pumped_actor::<RenderCapability>(
                RenderTuningConfig {
                    vertex_buffer_bytes: VERTEX_BUFFER_BYTES,
                    clear_color: aether_render::DEFAULT_CLEAR_COLOR.to_owned(),
                    pass_timings,
                },
                params,
            )
            .map_err(|e| anyhow::anyhow!("boot pumped render slot: {e}"))?;
        // The pumped slot registered its inbox under the actor's NAMESPACE,
        // so its id is the name hash — the same id
        // `send_and_await_reply("aether.render", CaptureFrame)` resolves.
        #[allow(clippy::disallowed_methods)] // ctx-less harness setup; no sibling resolver in scope
        let render_mailbox = aether_data::mailbox_id_from_name(RenderCapability::NAMESPACE);
        Ok(Box::new(GpuFrameHook { slot, mailer, render_mailbox }) as Box<dyn FrameHook>)
    }))
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

    /// Force loss of the currently installed offscreen device and return its
    /// generation. The next request/frame services the normal ADR-0173
    /// replacement transaction. Available only on a harness built with
    /// [`RenderHarnessBuilderExt::with_render`].
    fn force_render_device_loss(&self) -> Result<u64, String>;

    /// Read the asynchronously folded, per-pass GPU timestamp table for
    /// `program_id` (iamacoffeepot/aether#4422/#4423).
    ///
    /// Build the harness with
    /// [`RenderHarnessBuilderExt::with_render_pass_timings`], drive the
    /// program through consecutive [`HarnessOp::advance`] frames, then call
    /// this outside the measured run. The request reads duration state only:
    /// it performs no frame capture, image readback, or PNG encode, so image
    /// entropy cannot contaminate the result.
    ///
    /// [`ProgramTimingsResult::Absent`] preserves why this adapter cannot
    /// answer (or why timing was not enabled); it must not be read as a table
    /// of zero-cost passes. [`ProgramTimingsResult::Err`] reports a bad
    /// program id or missing render GPU. Capture visual evidence separately,
    /// after timing.
    fn program_gpu_timings(&mut self, program_id: u32) -> Result<ProgramTimingsResult, ExecutionError>;
}

impl RenderHarnessExt for SubstrateHarness {
    fn committed_overlay_snapshot(&self) -> Vec<DrawTexturedQuads> {
        self.frame_hook()
            .and_then(|hook| hook.as_any().downcast_ref::<GpuFrameHook>())
            .expect("committed_overlay_snapshot requires a harness built with .with_render() (issue #3764)")
            .committed_overlay_snapshot()
    }

    fn force_render_device_loss(&self) -> Result<u64, String> {
        self.frame_hook()
            .and_then(|hook| hook.as_any().downcast_ref::<GpuFrameHook>())
            .ok_or_else(|| "force_render_device_loss requires a harness built with .with_render()".to_owned())?
            .force_device_loss()
    }

    fn program_gpu_timings(&mut self, program_id: u32) -> Result<ProgramTimingsResult, ExecutionError> {
        const LABEL: &str = "program gpu timings";

        self.execute(vec![(
            LABEL,
            HarnessOp::send_and_await_reply(RenderCapability::NAMESPACE, &ProgramTimings { program_id }),
        )])?
        .reply(LABEL)
    }
}
