//! Cross-flavour monitor handle returned by `NativeCtx::monitor` (and,
//! pending future lifts, the wasm-side equivalent). Cross-flavour
//! because monitor fan-out is symmetric: a watcher of either flavour
//! receives a `MonitorNotice` mail when its target closes, and the
//! handle itself is just an RAII deregister that any actor with an
//! `Arc<ActorRegistry>` can hold.
//!
//! See ADR-0079 for the lifecycle semantics; the
//! [`crate::actor::registry::ActorRegistry`] holds the forward and
//! reverse indices.

use std::sync::Arc;

use crate::actor::registry::ActorRegistry;

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
/// [`ActorRegistry::deregister_monitor`] which is idempotent —
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
    watcher: aether_data::MailboxId,
    target: aether_data::MailboxId,
}

impl MonitorHandle {
    pub(crate) fn new(
        registry: Arc<ActorRegistry>,
        watcher: aether_data::MailboxId,
        target: aether_data::MailboxId,
    ) -> Self {
        Self {
            registry,
            watcher,
            target,
        }
    }

    /// The target this handle is monitoring. Useful for handlers that
    /// hold many handles and need to identify which one fired a notice.
    pub fn target(&self) -> aether_data::MailboxId {
        self.target
    }
}

impl Drop for MonitorHandle {
    fn drop(&mut self) {
        self.registry.deregister_monitor(self.watcher, self.target);
    }
}
