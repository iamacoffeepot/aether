//! Lifecycle stage and subscription kind vocabulary.

use alloc::string::String;

use bytemuck::{Pod, Zeroable};

// ADR-0082 lifecycle stage kinds. Empty payload — the broadcast is the
// signal. Future revisions may add per-stage fields (frame_no on Tick,
// vp matrix on Render) once stage payload semantics settle; v1 keeps
// the wire shape minimal so the application-declared graph can drive
// stage timing without committing to a fixed payload schema.

/// Per-frame lifecycle stage (ADR-0082 §11). Empty payload —
/// elapsed-time is parked until a subscriber actually needs it. The
/// kind moved from `aether.tick` into the `aether.lifecycle.*` family
/// in PR 4 so the lifecycle stage vocabulary reads as one namespace.
///
/// ADR-0033 handler dispatch (`#[actor]` synthesized
/// `__aether_dispatch`) decodes every typed handler via
/// `Mail::decode_typed::<K>()`, which requires `K: AnyBitPattern`.
/// Zero-sized unit kinds like `Tick` trivially satisfy that through
/// `Pod` + `Zeroable` — no padding, no uninitialized bits.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Pod, Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "aether.lifecycle.tick")]
pub struct Tick;

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
/// desktop and `substrate_bench`). Render-producing actors compute their
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

/// Driver-internal trigger that advances the lifecycle state machine
/// by one step (ADR-0082 §2). The chassis main loop sends this each
/// frame; the driver responds by minting the current state's payload
/// via its factory, broadcasting to subscribers, awaiting settlement,
/// and advancing the internal state pointer along the resolved edge
/// (`next` or `quit`). Not exposed via the `aether.lifecycle.*` stage
/// vocabulary because it carries no semantic meaning to subscribers;
/// it's the cadence input, not a stage broadcast.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Pod, Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "aether.lifecycle.advance")]
pub struct LifecycleAdvance;

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
