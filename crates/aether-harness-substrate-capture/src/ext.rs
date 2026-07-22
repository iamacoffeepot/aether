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
use aether_harness_substrate::{FrameHook, RenderHookWiring, SubstrateHarness, SubstrateHarnessBuilder};
use aether_render::{DrawTexturedQuads, Frame, RenderCapability, RenderParams, RenderTuningConfig};
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
}

impl RenderHarnessBuilderExt for SubstrateHarnessBuilder {
    fn with_render(self) -> Self {
        self.render_hook(Box::new(|passive, wiring, width, height| {
            let RenderHookWiring { mailer, observed_kinds, assets_dir } = wiring;
            // The `FrameCheck` / similarity scorer lives in
            // `aether_substrate::render::visual` (below aether-render), so the
            // pumped runtime scores capture verdicts + similarity directly in
            // its ready-readback branch — no scorer injection.
            // `..Default::default()` fills `wireframe: None` and — under a
            // feature-unified build that enables aether-render/desktop — the
            // desktop-only `window: None`, so this literal is robust to feature
            // unification.
            let params = RenderParams {
                observed_kinds,
                assets_dir,
                offscreen_size: Some((width, height)),
                ..Default::default()
            };
            let (slot, _wake_slot) = passive
                .boot_pumped_actor::<RenderCapability>(
                    RenderTuningConfig { vertex_buffer_bytes: VERTEX_BUFFER_BYTES },
                    params,
                )
                .map_err(|e| anyhow::anyhow!("boot pumped render slot: {e}"))?;
            // The pumped slot registered its inbox under the actor's NAMESPACE,
            // so its id is the name hash — the same id
            // `send_and_await("aether.render", CaptureFrame)` resolves.
            #[allow(clippy::disallowed_methods)] // ctx-less harness setup; no sibling resolver in scope
            let render_mailbox = aether_data::mailbox_id_from_name(RenderCapability::NAMESPACE);
            Ok(Box::new(GpuFrameHook { slot, mailer, render_mailbox }) as Box<dyn FrameHook>)
        }))
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
