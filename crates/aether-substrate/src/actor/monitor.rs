//! The monitor handle [`NativeCtx::monitor`] returns, the registration
//! behind it, and the [`MonitorNotice`](aether_kinds::MonitorNotice) post
//! every departure goes through.
//!
//! Monitoring never fails (ADR-0079 §8). `MonitorHandle::register` is the
//! one entry point, and it has two callers. [`NativeCtx::monitor`] registers
//! its own actor. A wasm component's `watch_p32` host fn registers once per
//! watcher and target for a guest, through the instance's watch table
//! (`crate::actor::wasm::watch_table`), where the watcher is the guest's
//! mailbox or one of its inline-child aliases. The [`ActorRegistry`] holds
//! the forward and reverse indices and the argument that a registration
//! racing a close is answered by exactly one notice.
//!
//! [`NativeCtx::monitor`]: crate::actor::native::ctx::NativeCtx::monitor

use std::sync::Arc;

use aether_data::{Kind, MailboxId, Source, SourceAddr};

use crate::actor::native::binding::NativeBinding;
use crate::actor::registry::ActorRegistry;
use crate::mail::{Mail, Mailer};

/// The registration [`NativeCtx::monitor`] returns (ADR-0079 §8): the
/// `(watcher, target)` pair and the [`ActorRegistry`] it is registered in,
/// so `Drop` deregisters without the caller threading the registry.
///
/// Dropping the handle stops the watch: a target that closes afterwards
/// sends this watcher nothing. It cannot take back mail already posted, so
/// a notice posted before the drop, by the target's close or by the
/// registration itself for a target that had already closed, still arrives.
/// A watcher therefore reads the departed actor from the notice's sender
/// and does nothing when it holds no state under it.
///
/// Either party's close prunes the entry too: the target's close drains
/// `monitors_of[target]` and posts the notices, and the watcher's close
/// walks `monitoring[watcher]`. Deregistering an entry a close already took
/// is a no-op.
///
/// A native actor holds the handle itself. A wasm guest holds none: its
/// instance's watch table keeps one handle per watcher and target, whatever
/// the number of watched types on that pair, and drops it when the pair's
/// last watch ends or the instance drops.
///
/// Not `Clone` — a monitor is a unique (watcher, target) registration;
/// duplicating the handle would duplicate the deregistration on Drop
/// (still benign because deregister is idempotent, but cloneable
/// handles encourage holding multiple references whose semantics
/// surface as silent multi-prune).
///
/// [`NativeCtx::monitor`]: crate::actor::native::ctx::NativeCtx::monitor
pub struct MonitorHandle {
    registry: Arc<ActorRegistry>,
    watcher: MailboxId,
    target: MailboxId,
}

impl MonitorHandle {
    /// Register `watcher` as a monitor of `target` and return the handle.
    /// It never refuses. A target that had already closed is not entered in
    /// the index; its notice is posted to `watcher` here, through the post
    /// its close would have used, so the watcher handles the same mail
    /// either way.
    ///
    /// `watcher` is the position the notice is mailed to: the calling
    /// actor's own mailbox for [`NativeCtx::monitor`], and for a wasm guest
    /// its mailbox or the alias of the inline child that watched, which the
    /// host fn has already checked against the guest's cluster.
    ///
    /// The lifecycle table is the route registry's, reached through the
    /// binding's mailer, so a binding with no spawner registers in the
    /// table a chassis over the same routes closes against.
    ///
    /// [`NativeCtx::monitor`]: crate::actor::native::ctx::NativeCtx::monitor
    pub(crate) fn register(binding: &NativeBinding, watcher: MailboxId, target: MailboxId) -> Self {
        let registry = Arc::clone(binding.mailer().registry().actor_registry());

        let watching = registry.register_monitor(watcher, target);
        if !watching {
            notify_departure(binding, target, vec![watcher]);
        }

        Self { registry, watcher, target }
    }
}

impl Drop for MonitorHandle {
    fn drop(&mut self) {
        self.registry.deregister_monitor(self.watcher, self.target);
    }
}

/// Post one [`aether_kinds::MonitorNotice`] to every watcher of a departed
/// `target`, with `target` stamped as the envelope sender. The notice has
/// no fields: a watcher reads the departed actor as a proven reference from
/// `ctx.sender()` (ADR-0230), never as a position in the payload. `target`
/// is an actor the watcher proved before it monitored it, so the reference
/// the watcher mints from the sender proves exactly what it claims.
///
/// The notice is pushed root-shaped (no parent chain) from both callers:
/// a close's fan-out runs past the closing chain's settlement and can hold
/// nothing, and a registration that found its target closed posts the same
/// mail so that a watcher cannot tell the two apart. It is ordinary mail on
/// the watcher's inbox, dispatched after whatever handler the watcher is
/// running.
pub(crate) fn notify_departure(binding: &NativeBinding, target: MailboxId, watchers: Vec<MailboxId>) {
    post_notices(binding.mailer(), target, watchers);
}

/// The post behind [`notify_departure`], over a bare mailer: the boot
/// unwind retires a claim whose actor has no binding
/// (`ChassisCtx::retire_claim`).
pub(crate) fn post_notices(mailer: &Mailer, target: MailboxId, watchers: Vec<MailboxId>) {
    if watchers.is_empty() {
        return;
    }
    let payload = aether_kinds::MonitorNotice.encode_into_bytes();
    let sender = Source::to(SourceAddr::Component(target));
    for watcher in watchers {
        mailer.push(Mail::new(watcher, aether_kinds::MonitorNotice::ID, payload.clone(), 1).with_reply_to(sender));
    }
}

/// Drain and notify the watchers of every inline-child alias folded onto
/// `occupant` (ADR-0114 §2), which departs when `occupant` does — one
/// notice per alias, with the alias as its sender.
///
/// An inline child's sends stamp its alias as their dispatch identity
/// (ADR-0114 §4), so a cap that keys state on the host-stamped source files
/// the child's rows under that alias and reclaims them on a notice whose
/// sender is that alias. A fan-out sent only from `occupant` would leave
/// every such row behind, outliving the actor that claimed it.
///
/// Each alias closes with its parent and tombstones (ADR-0241 §8), so it is
/// never spawned again, and a later watch on it is answered with its notice
/// at once.
///
/// The alias route itself is left in place: it resolves through its parent,
/// so it reads `Dropped` when the parent's route does.
pub(crate) fn notify_alias_departures(actor_registry: &ActorRegistry, binding: &NativeBinding, occupant: MailboxId) {
    for alias in binding.mailer().registry().aliases_of(occupant) {
        notify_departure(binding, alias, actor_registry.close_alias(alias));
    }
}
