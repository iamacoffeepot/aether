//! The outward inventory publication: the descriptor projections the hub
//! and the component cap read, and the subscribers woken when they change.

use std::sync::{Arc, Weak};

use rustc_hash::FxHashMap;

use aether_actor::{HandlesKind, RegistryChanged};
use aether_data::{KindDescriptor, MailboxCategory, MailboxDescriptor};

use crate::mail::mailer::Mailer;
use crate::mail::registry::effect::{ChangeSubscriber, RegistryInventory, RegistrySubscription, subscriber};
use crate::mail::registry::names::categorise_mailbox_name;
use crate::mail::{KindId, MailboxId};

use super::Registry;
use super::kinds::KindSlot;
use super::route::{RouteLifecycle, RouteRecord};

pub(super) fn live_inventory(mailboxes: &FxHashMap<MailboxId, RouteRecord>) -> Vec<MailboxDescriptor> {
    let mut inventory = mailboxes
        .iter()
        .filter(|(_, route)| match &route.lifecycle {
            RouteLifecycle::Live { .. } => true,
            RouteLifecycle::Alias { target_parent } => mailboxes
                .get(target_parent)
                .is_some_and(|target| matches!(&target.lifecycle, RouteLifecycle::Live { .. })),
            RouteLifecycle::Starting { .. } | RouteLifecycle::Dropped => false,
        })
        .map(|(id, route)| MailboxDescriptor {
            id: *id,
            name: route.canonical_name.clone(),
            category: categorise_mailbox_name(&route.canonical_name),
        })
        .collect::<Vec<_>>();
    inventory.push(MailboxDescriptor {
        id: MailboxId::CHASSIS_MAILBOX_ID,
        name: "aether.chassis".to_owned(),
        category: Some(MailboxCategory::ChassisSentinel),
    });
    inventory.sort_by(|left, right| left.name.cmp(&right.name));
    inventory
}

pub(super) fn kind_inventory(kinds: &FxHashMap<KindId, KindSlot>) -> Vec<KindDescriptor> {
    let mut descriptors = kinds.values().map(|slot| slot.descriptor.clone()).collect::<Vec<_>>();
    descriptors.sort_by(|left, right| left.name.cmp(&right.name));
    descriptors
}

impl Registry {
    #[doc(hidden)]
    pub fn subscribe_inventory<A>(&self, target: MailboxId, mailer: Arc<Mailer>) -> RegistrySubscription
    where
        A: HandlesKind<RegistryChanged>,
    {
        let mut subscribers =
            self.subscribers.lock().expect("registry subscriber lock poisoned; fail-fast per ADR-0063");
        let (subscriber, subscription) = subscriber(target, mailer, self.inventory.clone());
        subscribers.retain(|subscriber| subscriber.strong_count() != 0);
        subscribers.push(Arc::downgrade(&subscriber));
        drop(subscribers);
        subscriber.notify();
        subscription
    }

    #[doc(hidden)]
    #[must_use]
    pub fn inventory(&self) -> RegistryInventory {
        self.inventory.load().table().clone()
    }

    pub(super) fn notify_inventory_changed(&self) {
        for subscriber in self.inventory_subscribers() {
            subscriber.notify();
        }
    }

    pub(super) fn relay_inventory_changed(&self) {
        for subscriber in self.inventory_subscribers() {
            subscriber.notify_via_relay();
        }
    }

    fn inventory_subscribers(&self) -> Vec<Arc<ChangeSubscriber>> {
        let mut retained = self.subscribers.lock().expect("registry subscriber lock poisoned; fail-fast per ADR-0063");
        let subscribers = retained.iter().filter_map(Weak::upgrade).collect::<Vec<_>>();
        retained.retain(|subscriber| subscriber.strong_count() != 0);
        subscribers
    }

    /// Snapshot of every mailbox descriptor currently registered, plus
    /// a synthetic entry for the chassis-router sentinel
    /// (`aether.chassis` / [`MailboxId::CHASSIS_MAILBOX_ID`]). Sorted
    /// by name. Used by the hub-client handshake to ship the
    /// authoritative inventory in `Hello.mailboxes`, and by the
    /// component cap to re-ship via `MailboxesChanged` after a load
    /// registers a new trampoline mailbox (issue iamacoffeepot/aether#730).
    ///
    /// Only live entries are included. Keyed routes retain `Dropped`
    /// records for dispatch and trace-name resolution, but public inventory
    /// is a distinct publication and removes them.
    pub fn list_mailbox_descriptors(&self) -> Vec<MailboxDescriptor> {
        self.inventory.load().table().mailboxes.clone()
    }
}
