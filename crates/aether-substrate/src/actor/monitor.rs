//! Cross-flavour monitor handle returned by `NativeCtx::monitor` (and,
//! pending future lifts, the wasm-side equivalent). Cross-flavour
//! because monitor fan-out is symmetric: a watcher of either flavour
//! receives a `MonitorNotice` mail when its target closes, and the
//! handle itself is just an RAII deregister that any actor with an
//! `Arc<ActorRegistry>` can hold.
//!
//! See ADR-0079 for the lifecycle semantics; the
//! [`ActorRegistry`] holds the forward and
//! reverse indices.

use std::sync::Arc;

use aether_data::{Kind, MailboxId};

use crate::actor::native::binding::NativeBinding;
use crate::actor::registry::ActorRegistry;
use crate::mail::Mail;

/// Issue 607 Phase 4b (ADR-0079): RAII handle returned by
/// `NativeCtx::monitor`. Holds the registered `(watcher, target)`
/// pair plus an `Arc` to the chassis's [`ActorRegistry`] so
/// `Drop` can deregister without rethreading the registry through the
/// caller.
///
/// The framework also prunes the monitor entry on either party's
/// close (the target's close drains `monitors_of[target]` after firing
/// `MonitorNotice`; the watcher's close walks `monitoring[watcher]` to
/// remove `watcher` from each target's forward list). `Drop` calls
/// the registry's internal `deregister_monitor`, which is idempotent —
/// dropping a handle whose entry the close path already removed is a
/// no-op.
///
/// Not `Clone` — a monitor is a unique (watcher, target) registration;
/// duplicating the handle would duplicate the deregistration on Drop
/// (still benign because deregister is idempotent, but cloneable
/// handles encourage holding multiple references whose semantics
/// surface as silent multi-prune).
pub struct MonitorHandle {
    registry: Arc<ActorRegistry>,
    watcher: MailboxId,
    target: MailboxId,
}

impl MonitorHandle {
    pub(crate) fn new(registry: Arc<ActorRegistry>, watcher: MailboxId, target: MailboxId) -> Self {
        Self { registry, watcher, target }
    }

    /// The target this handle is monitoring. Useful for handlers that
    /// hold many handles and need to identify which one fired a notice.
    #[must_use]
    pub fn target(&self) -> MailboxId {
        self.target
    }
}

impl Drop for MonitorHandle {
    fn drop(&mut self) {
        self.registry.deregister_monitor(self.watcher, self.target);
    }
}

/// Fan one [`aether_kinds::MonitorNotice`] naming `target` out to every
/// watcher the departure drained. The notice is pushed root-shaped (no
/// parent chain): a departure fan-out runs past the closing chain's
/// settlement, so it can hold nothing.
pub(crate) fn notify_departure(binding: &NativeBinding, target: MailboxId, watchers: Vec<MailboxId>) {
    if watchers.is_empty() {
        return;
    }
    let payload = aether_kinds::MonitorNotice { target }.encode_into_bytes();
    for watcher in watchers {
        binding.mailer().push(Mail::new(watcher, aether_kinds::MonitorNotice::ID, payload.clone(), 1));
    }
}

/// Drain and notify the watchers of every inline-child alias folded onto
/// `occupant` (ADR-0114 §2), which departs when `occupant` does — one
/// notice per alias, naming the alias.
///
/// An inline child's sends stamp its alias as their dispatch identity
/// (ADR-0114 §4), so a cap that keys state on the host-stamped source files
/// the child's rows under that alias and reclaims them on a notice naming
/// it. A fan-out that named only `occupant` would leave every such row
/// behind, outliving the actor that claimed it.
///
/// The alias route itself is left in place: it is served by the parent's
/// slot, which stays addressable and refillable across a vacate, and a
/// close retires the parent's own name rather than the alias's.
pub(crate) fn notify_alias_departures(actor_registry: &ActorRegistry, binding: &NativeBinding, occupant: MailboxId) {
    for alias in binding.mailer().registry().aliases_of(occupant) {
        notify_departure(binding, alias, actor_registry.vacate_actor(alias));
    }
}
