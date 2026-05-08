//! [`LifecycleControl`] — runtime-only self-shutdown + monitor surface.
//!
//! Per-stage capability trait under the issue 663 refactor. Runtime
//! ctxs that participate in the ADR-0079 lifecycle (`shutdown`,
//! `monitor`, `spawn_child`) impl the relevant subset.
//!
//! Phase A of issue 663 ships the trait shape; Phase B impls it on
//! substrate's `NativeCtx`. The FFI side does not impl it in this PR —
//! issue 607 phase 4 / ADR-0079 wires the host fns when the
//! implementation lands; the trait is here so the lift is mechanical
//! at that point.
//!
//! `spawn_child` is deliberately absent from the trait surface in
//! this Phase A scaffolding — substrate's `spawn_child` requires
//! `A: NativeActor + NativeDispatch` bounds beyond the actor crate's
//! `Instanced`, which Rust's coherence rules don't let us add to a
//! trait method's impl. `spawn_child` stays inherent on substrate's
//! `NativeCtx` for now; a follow-up may introduce a substrate-side
//! sub-trait that adds the substrate-specific bounds.

use aether_data::MailboxId;

/// Runtime self-control surface: signal shutdown, register monitors.
/// Init ctxs deliberately don't implement this — boot-time control
/// flows through chassis builders, not from inside the actor itself.
///
/// `MonitorHandle` and `MonitorError` are associated types so the
/// trait stays free of substrate-internal types. Substrate's impl
/// pins them to its concrete `MonitorHandle` /
/// `aether_substrate::actor::registry::MonitorError`; FFI impls (when
/// they land) will pin them to FFI-specific equivalents.
pub trait LifecycleControl {
    /// Drop-on-deregister handle returned by [`Self::monitor`].
    type MonitorHandle;

    /// Error returned by [`Self::monitor`] when the watcher cannot
    /// be registered (target unknown, target tombstoned, recursion
    /// limit reached).
    type MonitorError;

    /// Issue 607 Phase 4a (ADR-0079): self-shutdown signal. Sets a
    /// flag the actor's dispatcher polls after each handler returns;
    /// when set, the trampoline drains any remaining inbox mail
    /// synchronously, runs the close hook, and exits the dispatch
    /// loop.
    ///
    /// Idempotent — flipping the flag twice is the same as flipping
    /// it once. Singletons booted through the chassis-builder rely on
    /// the chassis-shutdown channel-drop path instead of this flag,
    /// but can call `shutdown()` to opt in to flag-based exit.
    fn shutdown(&self);

    /// Issue 607 Phase 4b (ADR-0079): register the calling actor as
    /// a monitor of `target`. Returns a [`Self::MonitorHandle`] whose
    /// `Drop` deregisters the entry.
    ///
    /// On the target's close, the substrate drains its monitor list
    /// and fires one notice per watcher before the slot transitions
    /// `Live` → `Dead`. The watcher receives that notice as ordinary
    /// mail and reads the `target` field to identify the closing
    /// actor.
    fn monitor(&self, target: MailboxId) -> Result<Self::MonitorHandle, Self::MonitorError>;
}
