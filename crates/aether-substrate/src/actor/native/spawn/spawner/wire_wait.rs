//! The test-support wait for one spawned actor's `wire`: every mail it sent,
//! and everything those mails caused, has been handled (ADR-0244 §7).

use std::mem;

use crossbeam_channel::Receiver;

use crate::mail::MailboxId;
use crate::mail::mailer::Mailer;
use crate::runtime::wire_root::WireRoot;
use crate::testing::await_settled;

use super::Spawner;

/// What a retained slot keeps for [`Spawner::await_wire_settled`].
pub(super) enum WireSettled {
    /// The subscription to the birth's wire root, taken while the birth
    /// still held that root open.
    Pending(Receiver<()>),
    /// A wait has already taken the subscription.
    Awaited,
    /// The birth opened no wire root of its own: its `wire` ran under the
    /// boot's root, or under none.
    Unrooted,
}

impl WireSettled {
    /// Subscribe to `wire_root`'s settlement, or record that the birth has
    /// no root of its own. The caller still holds `wire_root` open, so the
    /// subscription cannot miss the settle.
    pub(super) fn subscribe(wire_root: Option<&WireRoot>, mailer: &Mailer) -> Self {
        wire_root.map_or(Self::Unrooted, |root| Self::Pending(root.subscribe(mailer)))
    }
}

impl Spawner {
    /// Block until the root the pooled instanced actor at `id` ran its
    /// `wire` under has settled: every mail its `wire` sent, and everything
    /// those mails caused, has been handled (ADR-0244 §7). This is the
    /// test-support wait behind `PassiveChassis::await_wire_settled`.
    ///
    /// The subscription was taken when the birth's slot was retained, while
    /// the birth still held its wire root open, so the wait cannot miss a
    /// settle that happened before it was called. The first wait takes that
    /// subscription, and any later wait on the same actor returns at once.
    /// `gate` names the wait in the slow-log and in the panic at the
    /// settlement cap.
    ///
    /// # Panics
    /// Panics when `id` is not a pooled instanced actor this spawner
    /// retained, when its `wire` ran under the boot's root rather than one
    /// of its own (await `PassiveChassis::await_boot_settled` for it), or
    /// when the wait passes the settlement cap.
    pub(crate) fn await_wire_settled(&self, id: MailboxId, gate: &str) {
        let taken = mem::replace(
            &mut self
                .instanced_slots
                .lock()
                .expect("instanced_slots mutex poisoned; fail-fast per ADR-0063")
                .get_mut(&id)
                .unwrap_or_else(|| {
                    panic!("{gate}: {id} is not a pooled instanced actor; await_wire_settled covers only those")
                })
                .wire_settled,
            WireSettled::Awaited,
        );
        match taken {
            WireSettled::Pending(settled) => await_settled(&settled, gate),
            WireSettled::Awaited => {}
            WireSettled::Unrooted => panic!(
                "{gate}: {id}'s wire ran under the boot's wire root, not one of its own; await PassiveChassis::await_boot_settled"
            ),
        }
    }
}
