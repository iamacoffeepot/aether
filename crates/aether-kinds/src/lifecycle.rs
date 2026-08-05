//! Lifecycle stage and subscription kind vocabulary.

use alloc::string::String;

use bytemuck::{Pod, Zeroable};

// ADR-0082 lifecycle stage kinds. Most are empty signals. `Tick` carries
// the elapsed time its subscribers need to state motion in seconds rather
// than in an assumed frame cadence (issue 4470).

/// Per-frame lifecycle stage (ADR-0082 §11). `delta_micros` is the elapsed
/// wall-clock time represented by this tick, supplied by the chassis cadence
/// source. Motion subscribers use it so authored seconds remain seconds when
/// frame rate changes (issue 4470).
///
/// ADR-0033 handler dispatch (`#[actor]` synthesized
/// `__aether_dispatch`) decodes every typed handler via
/// `Mail::decode_typed::<K>()`, which requires `K: AnyBitPattern`.
/// The single `u32` field has no padding and accepts every bit pattern,
/// satisfying that contract through `Pod` + `Zeroable`.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Pod, Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "aether.lifecycle.tick")]
pub struct Tick {
    pub delta_micros: u32,
}

impl Tick {
    /// Elapsed time in seconds for rate/period integration.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub const fn delta_seconds(self) -> f32 {
        self.delta_micros as f32 / 1_000_000.0
    }
}

/// Lifecycle stage broadcast — capability init pass (ADR-0082 §5).
/// Fires once at chassis boot, after every capability's actor-framework
/// `claim → init → wire → spawn` completes and before
/// [`InitComponents`] fires. Capabilities that need to send mail to
/// peers during boot subscribe to this stage.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Pod, Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "aether.lifecycle.init_caps")]
pub struct InitCaps;

/// Lifecycle stage broadcast — component init pass (ADR-0082 §5).
/// Fires once after [`InitCaps`] settles, before the per-frame loop
/// begins. Component-category actors subscribe here when they need to
/// reach already-wired capabilities during their boot logic.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Pod, Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "aether.lifecycle.init_components")]
pub struct InitComponents;

/// Lifecycle stage broadcast — render stage (ADR-0082 §1). Fires every
/// frame after the whole [`Tick`] chain has settled (ADR-0080 §6) on
/// chassis that declare a render state in their lifecycle graph (today:
/// desktop and `substrate_harness`). Render-producing actors compute their
/// per-frame state on [`Tick`] and submit it to `aether.render` here, on
/// `Render` — so a submission integrates the fully-settled cross-actor
/// state of the frame rather than racing other actors' Tick handlers.
/// Headless / hub chassis omit this state from their graph; subscribing
/// on a chassis that doesn't declare it rejects fail-fast at wire time
/// per ADR-0082 §7.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Pod, Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "aether.lifecycle.render")]
pub struct Render;

/// Lifecycle stage broadcast — frame-present stage (ADR-0082 §1).
/// Fires every frame after [`Render`] on chassis that drive a display.
/// The default desktop graph routes the quit edge through this stage so
/// the current frame finishes drawing before shutdown.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Pod, Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "aether.lifecycle.present")]
pub struct Present;

/// Lifecycle stage broadcast — shutdown stage (ADR-0082 §1). Fires
/// once when the graph reaches a terminal state. Subscribers perform
/// graceful cleanup with the full mail surface still operational
/// (save game state, flush a write, post a metric) before the chassis
/// runs each actor's `unwire` finaliser. Distinct from the actor
/// framework's per-actor `unwire` hook — ADR-0082 §12.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Pod, Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "aether.lifecycle.shutdown")]
pub struct Shutdown;

/// Lifecycle escape signal (ADR-0082 §3). The one hardcoded signal the
/// driver recognises. Setting `quit_pending = true` on receipt; the
/// flag is consumed at the next state whose graph declares a `quit`
/// edge. Chassis bridges OS-level termination signals (ctrlc, winit
/// `WindowEvent::CloseRequested`, future hub-shutdown mail) to this
/// kind so three trigger sources converge on one consumption point.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Pod, Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "aether.lifecycle.quit")]
pub struct Quit;

/// Driver-internal trigger that advances the lifecycle state machine by one
/// step (ADR-0082 §2). The chassis main loop sends this for every stage in a
/// frame. `delta_micros` is copied into [`Tick`] when the current stage is
/// `Tick`; other stages remain empty signals. The driver then broadcasts,
/// awaits settlement, and advances along the resolved edge (`next` or
/// `quit`). This is the cadence input, not a stage broadcast.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Pod, Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "aether.lifecycle.advance")]
pub struct LifecycleAdvance {
    pub delta_micros: u32,
}

/// Reply to [`LifecycleAdvance`] signalling that the stage's broadcast
/// root has settled (ADR-0082 §6). The chassis main loop wait-replies
/// on this so cadence couples to actual work completion — back-pressure
/// flows from subscriber drain time back to the chassis. `completed`
/// is the kind id of the state the driver just finished broadcasting;
/// `next` is the kind id of the state the driver will broadcast on the
/// next [`LifecycleAdvance`], or `0` when the lifecycle reached a
/// terminal state.
#[derive(
    aether_data::Kind,
    aether_data::Schema,
    serde::Serialize,
    serde::Deserialize,
    Copy,
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
)]
#[kind(name = "aether.lifecycle.advance_complete")]
pub struct LifecycleAdvanceComplete {
    pub completed: u64,
    pub next: u64,
}

/// Subscribe a mailbox to a lifecycle stage broadcast (ADR-0082 §7).
/// `stage` is the [`KindId`](aether_data::KindId) of the stage kind
/// (e.g. `<Tick as Kind>::ID.0`); `mailbox` is the subscriber's mailbox
/// id. Substrate replies with [`LifecycleSubscribeResult`] —
/// `Err { reason: UnsupportedStage }` when the chassis's lifecycle
/// graph doesn't declare a state at that kind, fail-fast at wire time
/// per ADR-0082 §7.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Pod, Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "aether.lifecycle.subscribe")]
pub struct LifecycleSubscribe {
    pub stage: u64,
    pub mailbox: u64,
}

/// Reflexive counterpart of [`LifecycleSubscribe`]: subscribe the
/// *sending* actor to a lifecycle stage broadcast, with no explicit
/// `mailbox` field. The cap resolves the subscriber from the inbound
/// envelope's host-stamped `Source` (ADR-0083) via
/// `ctx.source_mailbox()`, so the subscriber cannot be forged and the
/// op is gated to in-process actors by construction — an external
/// session or another engine has no local mailbox and gets an `Err`
/// reply, pushing it onto the named [`LifecycleSubscribe`] form. This
/// is the common "subscribe me" case; `stage` carries the same
/// [`KindId`](aether_data::KindId) as [`LifecycleSubscribe`]. Substrate
/// replies with [`LifecycleSubscribeResult`].
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Pod, Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "aether.lifecycle.subscribe_self")]
pub struct LifecycleSubscribeSelf {
    pub stage: u64,
}

/// Unsubscribe counterpart of [`LifecycleSubscribe`]. Idempotent on
/// "not currently subscribed."
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Pod, Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "aether.lifecycle.unsubscribe")]
pub struct LifecycleUnsubscribe {
    pub stage: u64,
    pub mailbox: u64,
}

/// Reflexive counterpart of [`LifecycleUnsubscribe`]: unsubscribe the
/// *sending* actor from a lifecycle stage, with no explicit `mailbox`
/// field. The cap resolves the subscriber from the inbound envelope's
/// host-stamped `Source` (ADR-0083), the same gating as
/// [`LifecycleSubscribeSelf`]. Idempotent on "not currently
/// subscribed." Substrate replies with [`LifecycleSubscribeResult`].
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Pod, Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "aether.lifecycle.unsubscribe_self")]
pub struct LifecycleUnsubscribeSelf {
    pub stage: u64,
}

/// `aether.lifecycle.unsubscribe_all` — remove `mailbox` from every
/// lifecycle stage's subscriber set in one shot. The externally
/// sendable bulk form; drop-time cleanup rides the ADR-0079
/// vacate/close `MonitorNotice` instead, so the per-stage broadcast
/// stops firing at a dropped trampoline without anyone mailing this —
/// the lifecycle-family counterpart of `UnsubscribeAll` for
/// `aether.input`. Idempotent: a mailbox with no stage subscriptions
/// is still a no-op. Fire-and-forget; no reply. Cast-shape (Pod), one
/// `mailbox` field, matching the sibling lifecycle kinds' raw-`u64`
/// shape.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Pod, Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "aether.lifecycle.unsubscribe_all")]
pub struct LifecycleUnsubscribeAll {
    pub mailbox: u64,
}

/// Reply to [`LifecycleSubscribe`] / [`LifecycleUnsubscribe`].
/// `Err` carries the stage kind id and a human-readable reason —
/// fail-fast subscribe per ADR-0082 §7. Same shape and rationale as
/// `SubscribeInputResult` for input subscriptions.
#[derive(aether_data::Kind, aether_data::Schema, serde::Serialize, serde::Deserialize, Debug, Clone)]
#[kind(name = "aether.lifecycle.subscribe_result")]
pub enum LifecycleSubscribeResult {
    Ok,
    Err { stage: u64, error: String },
}

#[cfg(test)]
mod tests {
    use aether_data::Kind;

    use super::*;

    #[test]
    fn tick_elapsed_time_round_trips_on_the_cast_wire() {
        let tick = Tick { delta_micros: 83_335 };
        assert_eq!(Tick::decode_from_bytes(&tick.encode_into_bytes()), Some(tick));
        assert!((tick.delta_seconds() - 0.083_335).abs() < f32::EPSILON);
    }
}
