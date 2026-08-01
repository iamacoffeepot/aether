//! What one successful write publishes: the accumulated route updates
//! plus the dirty flags that decide whether the kind and inventory views
//! re-publish with them.

use std::process::abort;

use crate::mail::MailboxId;
use crate::mail::registry::effect::RegistryInventory;
use crate::mail::view::Update;

use super::Inner;
use super::inventory::{kind_inventory, live_inventory};
use super::kinds::KindTable;
use super::route::RouteRecord;

impl Inner {
    pub(super) fn publish(&mut self, publication: Publication) -> bool {
        let inventory_dirty = publication.inventory_dirty;
        let kinds_dirty = publication.kinds_dirty;
        if !publication.route_updates.is_empty() && self.route_publisher.publish(publication.route_updates).is_err() {
            tracing::error!("route publication generation exhausted; registry cannot remain coherent");
            abort();
        }
        if kinds_dirty
            && self
                .kind_publisher
                .publish(KindTable { kinds: self.kinds.clone(), name_index: self.name_index.clone() })
                .is_err()
        {
            tracing::error!("kind publication generation exhausted; registry cannot remain coherent");
            abort();
        }
        if inventory_dirty || kinds_dirty {
            if inventory_dirty {
                self.mailbox_generation = self.mailbox_generation.checked_add(1).unwrap_or_else(|| {
                    tracing::error!("mailbox inventory generation exhausted; registry cannot remain coherent");
                    abort();
                });
            }
            if kinds_dirty {
                self.kind_generation = self.kind_generation.checked_add(1).unwrap_or_else(|| {
                    tracing::error!("kind inventory generation exhausted; registry cannot remain coherent");
                    abort();
                });
            }
            if self
                .inventory_publisher
                .publish(RegistryInventory {
                    mailboxes: live_inventory(&self.mailboxes),
                    kinds: kind_inventory(&self.kinds),
                    mailbox_generation: self.mailbox_generation,
                    kind_generation: self.kind_generation,
                })
                .is_err()
            {
                tracing::error!(
                    "combined registry inventory publication generation exhausted; registry cannot remain coherent"
                );
                abort();
            }
        }
        inventory_dirty || kinds_dirty
    }
}

#[derive(Default)]
pub(super) struct Publication {
    pub(super) route_updates: Vec<Update<MailboxId, RouteRecord>>,
    pub(super) kinds_dirty: bool,
    pub(super) inventory_dirty: bool,
}

impl Publication {
    pub(super) fn append(&mut self, mut other: Self) {
        self.route_updates.append(&mut other.route_updates);
        self.kinds_dirty |= other.kinds_dirty;
        self.inventory_dirty |= other.inventory_dirty;
    }
}
