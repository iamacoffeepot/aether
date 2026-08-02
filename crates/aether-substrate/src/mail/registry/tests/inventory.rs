//! Tests for [`super::super::mailbox::inventory`] — the descriptor
//! projections a write publishes outward and the subscribers it wakes.

use std::panic;
use std::time::Duration;

use aether_data::{Kind, MailboxCategory};

use crate::mail::MailboxId;
use crate::mail::registry::{Registry, noop_handler};
use crate::testing::boot_authority as auth;

use super::support::{InventorySubscriber, inventory_subscription_fixture};

/// Issue iamacoffeepot/aether#730: `list_mailbox_descriptors`
/// snapshots the table sorted by name, categorises each entry by
/// its name prefix, and inserts a synthetic `ChassisSentinel`
/// entry under `aether.chassis` (which is never a real registry
/// row — `insert` rejects the reserved name).
#[test]
fn list_mailbox_descriptors_snapshots_sorted_with_categories() {
    let r = Registry::new();
    r.register_inbox(&auth(), "aether.input", noop_handler());
    r.register_inbox(&auth(), "aether.embedded:cam", noop_handler());
    r.register_inbox(&auth(), "user_thing", noop_handler());

    let snap = r.list_mailbox_descriptors();
    // Four entries: 3 registered + 1 synthetic chassis sentinel.
    assert_eq!(snap.len(), 4, "got: {snap:#?}");

    // Sorted by name.
    let names: Vec<&str> = snap.iter().map(|d| d.name.as_str()).collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(names, sorted, "snapshot must be sorted by name");

    // Each name maps to the expected category.
    let cat = |n: &str| {
        snap.iter().find(|d| d.name == n).and_then(|d| d.category).unwrap_or_else(|| panic!("missing entry for {n}"))
    };
    assert_eq!(cat("aether.chassis"), MailboxCategory::ChassisSentinel);
    assert_eq!(cat("aether.input"), MailboxCategory::Actor);
    assert_eq!(cat("aether.embedded:cam"), MailboxCategory::Trampoline);
    // User-space names fall outside any of the recognised
    // categories; the hub's downstream renderer treats them as
    // raw tagged ids without a type prefix.
    assert!(
        snap.iter().find(|d| d.name == "user_thing").unwrap().category.is_none(),
        "non-aether names categorise as None",
    );

    // The synthetic chassis sentinel uses the canonical id —
    // hub-side resolution of trace senders against this id finds
    // the right name without re-hashing.
    let chassis = snap.iter().find(|d| d.name == "aether.chassis").unwrap();
    assert_eq!(chassis.id, MailboxId::CHASSIS_MAILBOX_ID);
}

/// Each registered descriptor's id matches the deterministic hash
/// of its name (ADR-0029) — same id space the hub already knows.
#[test]
fn list_mailbox_descriptors_ids_match_name_hashes() {
    let r = Registry::new();
    let id = r.register_inbox(&auth(), "aether.audio", noop_handler());
    let entry = r.list_mailbox_descriptors().into_iter().find(|d| d.name == "aether.audio").expect("audio entry");
    assert_eq!(entry.id, id);
    assert_eq!(entry.id, MailboxId::from_name("aether.audio"));
}

#[test]
fn inventory_wake_follows_coherent_publication_and_coalesces() {
    let (registry, mailer, wakes, target) = inventory_subscription_fixture();
    let subscription = registry.subscribe_inventory::<InventorySubscriber>(target, mailer);
    assert_eq!(
        wakes.recv_timeout(Duration::from_millis(100)).expect("subscription emits an initial local wake"),
        aether_actor::RegistryChanged::ID
    );
    let initial = registry.inventory();
    subscription.acknowledge(initial.mailbox_generation, initial.kind_generation);

    registry.register_inbox(&auth(), "aether.input", noop_handler());
    registry.register_kind(&auth(), "aether.inventory.test");

    assert_eq!(
        wakes.recv_timeout(Duration::from_millis(100)).expect("published inventory emits one wake"),
        aether_actor::RegistryChanged::ID
    );
    let inventory = registry.inventory();
    assert_eq!(inventory.mailbox_generation, initial.mailbox_generation + 1);
    assert_eq!(inventory.kind_generation, initial.kind_generation + 1);
    assert!(inventory.mailboxes.iter().any(|descriptor| descriptor.name == "aether.input"));
    assert!(inventory.kinds.iter().any(|descriptor| descriptor.name == "aether.inventory.test"));
    subscription.acknowledge(inventory.mailbox_generation, inventory.kind_generation);
    assert!(wakes.recv_timeout(Duration::from_millis(20)).is_err(), "unacknowledged publications coalesce");
}

#[test]
fn inventory_acknowledgement_rearms_from_one_coherent_generation_pair() {
    let (registry, mailer, wakes, target) = inventory_subscription_fixture();
    let subscription = registry.subscribe_inventory::<InventorySubscriber>(target, mailer);
    wakes.recv_timeout(Duration::from_millis(100)).expect("initial wake");
    let observed = registry.inventory();

    registry.register_inbox(&auth(), "aether.input", noop_handler());
    let kind = registry.register_kind(&auth(), "aether.inventory.race");
    subscription.acknowledge(observed.mailbox_generation, observed.kind_generation);

    wakes.recv_timeout(Duration::from_millis(100)).expect("stale pair re-arms a wake");
    let latest = registry.inventory();
    assert_eq!(latest.mailbox_generation, observed.mailbox_generation + 1);
    assert_eq!(latest.kind_generation, observed.kind_generation + 1);
    subscription.acknowledge(latest.mailbox_generation, latest.kind_generation);

    registry.try_register_inbox(&auth(), "aether.input", noop_handler()).expect_err("conflict is observable");
    assert_eq!(
        registry.register_kind(&auth(), "aether.inventory.race"),
        kind,
        "matching kind registration is idempotent"
    );
    assert_eq!(
        (registry.inventory().mailbox_generation, registry.inventory().kind_generation),
        (latest.mailbox_generation, latest.kind_generation),
        "rejection and idempotent kind match publish no inventory generation"
    );
    assert!(wakes.recv_timeout(Duration::from_millis(20)).is_err(), "rejection and idempotent kind match emit no wake");
}
