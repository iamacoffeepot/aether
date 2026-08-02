//! Tests for [`super::super::mailbox::register`] — claiming a mailbox
//! name, retiring it, and installing a pooled actor's seize handle.

use std::sync::Arc;
use std::time::Duration;

use crate::mail::MailboxId;
use crate::mail::registry::{DropError, MailboxEntry, Registry, noop_handler};
use crate::scheduler::SeizeHandle;
use crate::testing::boot_authority as auth;

use super::support::{InventorySubscriber, inventory_subscription_fixture};

#[test]
fn register_and_lookup_closure_mailbox() {
    let r = Registry::new();
    let id = r.register_inbox(&auth(), "physics", noop_handler());
    assert_eq!(r.lookup("physics"), Some(id));
    assert!(matches!(r.entry(id), Some(MailboxEntry::Inbox { .. })));
}

/// iamacoffeepot/aether#1135: a `Pooled` actor's `Inbox` entry exposes
/// a live seize handle once the slot is wired in; a closure-backed
/// inbox (no slot) exposes none.
#[test]
fn pooled_inbox_exposes_seize_handle_closure_does_not() {
    use crate::scheduler::{BatchBudget, CycleResult, Drainable, SlotState};
    use std::any::Any;

    // Minimal `Drainable` carrying a real `SlotState` so the installed
    // seize handle can drive the `Idle → Running` CAS.
    struct StatefulSlot {
        state: Arc<SlotState>,
    }
    impl Drainable for StatefulSlot {
        fn run_cycle(&self, _budget: BatchBudget) -> CycleResult {
            CycleResult::Idle
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    let (r, mailer, wakes, target) = inventory_subscription_fixture();
    let subscription = r.subscribe_inventory::<InventorySubscriber>(target, mailer);
    wakes.recv_timeout(Duration::from_millis(100)).expect("initial inventory wake");
    let initial_inventory = r.inventory();
    subscription.acknowledge(initial_inventory.mailbox_generation, initial_inventory.kind_generation);
    let kind = r.register_kind(&auth(), "test.seize.kind");

    // Closure-backed inbox: no slot, so no seize handle ever resolves.
    let closure_id = r.register_inbox(&auth(), "closure", noop_handler());
    assert!(
        r.route_lookup(kind, closure_id).seize_handle().is_none(),
        "a closure-backed inbox exposes no seize handle"
    );

    // A `Pooled`-shaped inbox: empty before the slot is wired, then a
    // live handle after `install_seize_handle`.
    let pooled_id = r.register_inbox(&auth(), "pooled", noop_handler());
    let inventory_generation = r.mailbox_generation();
    wakes.recv_timeout(Duration::from_millis(100)).expect("live and kind publications coalesce");
    let published = r.inventory();
    subscription.acknowledge(published.mailbox_generation, published.kind_generation);
    let before_install = r.route_lookup(kind, pooled_id);
    assert!(before_install.seize_handle().is_none(), "the seize cell is empty until the Pooled slot is wired");
    let before_install_generation = before_install.generation();

    let slot = Arc::new(StatefulSlot { state: Arc::new(SlotState::new()) });
    let slot_dyn: Arc<dyn Drainable> = slot.clone();
    let handle = SeizeHandle::new(Arc::clone(&slot.state), Arc::downgrade(&slot_dyn));
    let installed = r.install_seize_handle(&auth(), pooled_id, handle.clone());
    assert!(installed, "install lands on a live Inbox entry");
    assert_eq!(r.mailbox_generation(), inventory_generation, "seize-only publication is not inventory");
    assert!(wakes.recv_timeout(Duration::from_millis(20)).is_err(), "seize-only publication emits no inventory wake");

    assert!(
        before_install.seize_handle().is_none(),
        "installing a handle must not mutate an endpoint reachable from an older lookup"
    );
    let after_install = r.route_lookup(kind, pooled_id);
    assert!(
        after_install.generation() > before_install_generation,
        "successful seize installation publishes a new route generation"
    );
    let resolved = after_install.seize_handle().expect("Pooled inbox now exposes a seize handle");
    // The handle is live: it wins the `Idle → Running` seize CAS and
    // upgrades to the same slot.
    assert!(resolved.try_seize().is_some(), "the resolved handle seizes a live slot");

    let installed_generation = after_install.generation();
    assert!(!r.install_seize_handle(&auth(), pooled_id, handle), "a duplicate seize installation is rejected");
    assert_eq!(
        r.route_lookup(kind, pooled_id).generation(),
        installed_generation,
        "a rejected seize installation must not publish a route generation"
    );

    r.register_inbox(&auth(), "reuse-one", noop_handler());
    r.register_inbox(&auth(), "reuse-two", noop_handler());
    assert!(
        r.route_lookup(kind, pooled_id).seize_handle().is_some(),
        "the installed handle survives both alternating buffers being reused"
    );
    let _ = slot_dyn;
}

#[test]
#[should_panic(expected = "mailbox name already registered")]
fn duplicate_name_panics() {
    let r = Registry::new();
    r.register_inbox(&auth(), "x", noop_handler());
    r.register_inbox(&auth(), "x", noop_handler());
}

#[test]
fn try_register_inbox_is_non_panicking_on_collision() {
    let r = Registry::new();
    let first = r.try_register_inbox(&auth(), "loaded", noop_handler()).expect("fresh name");
    let err = r.try_register_inbox(&auth(), "loaded", noop_handler()).expect_err("collision must not panic");
    assert_eq!(err.name, "loaded");
    assert_eq!(r.lookup("loaded"), Some(first));
    // Entries count unchanged after the failed second attempt.
    assert_eq!(r.len(), 1);
}

/// Issue iamacoffeepot/aether#725: registering a real handler at the
/// reserved `"aether.chassis"` name would silently shadow the
/// chassis-router short-circuit in `Mailer::route_mail` (mail to
/// `CHASSIS_MAILBOX_ID` never reaches the registry). Reject at the
/// registration boundary so the routing path stays unambiguous.
#[test]
fn try_register_inbox_rejects_reserved_chassis_name() {
    let r = Registry::new();
    let err = r.try_register_inbox(&auth(), "aether.chassis", noop_handler()).expect_err("reserved name must reject");
    assert_eq!(err.name, "aether.chassis");
    assert_eq!(r.len(), 0);
}

#[test]
fn drop_mailbox_frees_name_and_marks_entry_dropped() {
    let r = Registry::new();
    let id = r.try_register_inbox(&auth(), "loaded", noop_handler()).unwrap();
    let name = r.drop_mailbox(&auth(), id).expect("drop");
    assert_eq!(name, "loaded");
    assert!(r.lookup("loaded").is_none(), "name should be reusable");
    assert!(matches!(r.entry(id), Some(MailboxEntry::Dropped)), "entry must mark id as dropped");
    assert!(
        r.list_mailbox_descriptors().iter().all(|descriptor| descriptor.id != id),
        "a retained Dropped route is absent from public live inventory"
    );
    // Under ADR-0029 the id is a function of the name, so a
    // re-register produces the *same* id and flips the entry back
    // to `Component`.
    let reloaded = r.try_register_inbox(&auth(), "loaded", noop_handler()).unwrap();
    assert_eq!(reloaded, id);
    assert_eq!(r.lookup("loaded"), Some(reloaded));
    assert!(matches!(r.entry(reloaded), Some(MailboxEntry::Inbox { .. })));
    assert!(
        r.list_mailbox_descriptors().iter().any(|descriptor| descriptor.id == id),
        "re-registration restores the route to public live inventory"
    );
}

#[test]
fn drop_mailbox_rejects_unknown_and_repeat() {
    let r = Registry::new();
    assert!(matches!(r.drop_mailbox(&auth(), MailboxId(999)), Err(DropError::UnknownId(_))));
    let c = r.try_register_inbox(&auth(), "x", noop_handler()).unwrap();
    r.drop_mailbox(&auth(), c).unwrap();
    assert!(matches!(r.drop_mailbox(&auth(), c), Err(DropError::AlreadyDropped(_))));
}

#[test]
fn registration_through_shared_arc() {
    // Interior mutability means Arc<Registry> can register after
    // it's already been shared — the dispatch path today never
    // exercises this, but PR 2+ will when `load_component` adds
    // mailboxes and kinds from a handler that holds an Arc.
    let r = Arc::new(Registry::new());
    let r2 = Arc::clone(&r);
    let id = r2.register_inbox(&auth(), "late", noop_handler());
    assert_eq!(r.lookup("late"), Some(id));
    let kind_id = r.register_kind(&auth(), "aether.late");
    assert_eq!(
        r.kind_id("aether.late"),
        Some(kind_id),
        "shared Arc registrations are visible through the original handle"
    );
}
