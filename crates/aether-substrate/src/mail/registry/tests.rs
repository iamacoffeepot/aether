use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use aether_data::canonical::kind_id_from_parts;
use aether_data::{Kind, KindDescriptor, MailboxCategory, SchemaType};
use aether_kinds::trace::Nanos;

use crate::chassis::settlement::SettlementRegistry;
use crate::mail::mailer::Mailer;
use crate::mail::outbound::{EgressEvent, HubOutbound};
use crate::mail::registry::effect::{
    ActivationToken, EffectBatch, RegistryApplied, RegistryEffect, RegistryEffectError, StartingCancellation,
};
use crate::mail::registry::owner::RegistryOwnerLease;
use crate::mail::registry::relay::RouteRelayLease;
use crate::mail::registry::{
    AddressResolutionError, DropError, InboxHandler, InlineHandler, MailDispatch, MailboxEntry, OwnedDispatch,
    Registry, noop_handler, test_dispatch, test_owned_dispatch,
};
use crate::mail::{KindId, Mail, MailId, MailRef, MailboxId, Source};
use crate::scheduler::{SeizeHandle, WakeSink};

/// ADR-0094: a fresh armed [`OwnedDispatch`] panics on drop if it was
/// neither discharged nor transferred — the headline regression gate
/// for the #846 / #1325 dropped-bracket class. Debug-only (the guard
/// is compiled out in release).
#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "settlement-obligation leak")]
fn armed_dispatch_panics_if_dropped_without_discharge() {
    let env = OwnedDispatch::armed(
        KindId(7),
        "aether.window.set_mode".to_owned(),
        None,
        Source::NONE,
        MailRef::from(vec![1u8, 2, 3]),
        1,
        MailId::new(MailboxId(42), 9),
        MailId::new(MailboxId(42), 9),
        None,
        Nanos(0),
        0,
        MailboxId(42),
    );
    // Drop without discharge/transfer — the InboxHandler contract
    // violation. The panic message names the offending seam.
    drop(env);
}

/// ADR-0094: the panic message names `mail_id` + `kind_name` so the
/// leaking seam is locatable, not anonymous.
#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "aether.window.set_mode")]
fn armed_dispatch_panic_names_the_kind() {
    let env = OwnedDispatch::armed(
        KindId(7),
        "aether.window.set_mode".to_owned(),
        None,
        Source::NONE,
        MailRef::from(Vec::new()),
        1,
        MailId::new(MailboxId(1), 1),
        MailId::new(MailboxId(1), 1),
        None,
        Nanos(0),
        0,
        MailboxId(1),
    );
    drop(env);
}

/// ADR-0094: an armed dispatch that is `discharge()`d before drop
/// does NOT panic — the consumer recorded `Finished`.
#[test]
fn discharged_dispatch_does_not_panic() {
    let env = OwnedDispatch::armed(
        KindId(7),
        "aether.fs.read".to_owned(),
        None,
        Source::NONE,
        MailRef::from(Vec::new()),
        1,
        MailId::new(MailboxId(2), 2),
        MailId::new(MailboxId(2), 2),
        None,
        Nanos(0),
        0,
        MailboxId(2),
    );
    env.discharge();
    drop(env);
}

/// ADR-0114 decision #1: the routed recipient promoted to a real
/// `OwnedDispatch` field survives in every build (not just the
/// debug-only `ObligationGuard`). Both mint sites stamp it from
/// their `recipient` parameter; a clone (for inspection) carries it
/// through too.
#[test]
fn dispatch_carries_routed_recipient() {
    let recipient = MailboxId(0xABCD);
    let env = OwnedDispatch::disarmed(
        KindId(7),
        "aether.fs.read".to_owned(),
        None,
        Source::NONE,
        MailRef::from(Vec::new()),
        1,
        MailId::new(MailboxId(3), 3),
        MailId::new(MailboxId(3), 3),
        None,
        Nanos(0),
        0,
        recipient,
    );
    assert_eq!(env.recipient, recipient);
    // The hand-rolled `Clone` must propagate the new field — a clone
    // is for inspection, but still carries the recipient.
    let cloned = env.clone();
    drop(env);
    assert_eq!(cloned.recipient, recipient);
}

/// ADR-0094: an armed dispatch that is `mark_transferred()` before
/// drop does NOT panic — the obligation moved onward.
#[test]
fn transferred_dispatch_does_not_panic() {
    let env = OwnedDispatch::armed(
        KindId(7),
        "aether.fs.write".to_owned(),
        None,
        Source::NONE,
        MailRef::from(Vec::new()),
        1,
        MailId::new(MailboxId(3), 3),
        MailId::new(MailboxId(3), 3),
        None,
        Nanos(0),
        0,
        MailboxId(3),
    );
    env.mark_transferred();
    drop(env);
}

/// ADR-0094: a disarmed mint (the test/helper path) never panics on
/// drop even without discharge.
#[test]
fn disarmed_dispatch_does_not_panic() {
    let env = OwnedDispatch::disarmed(
        KindId(7),
        "aether.tick".to_owned(),
        None,
        Source::NONE,
        MailRef::from(Vec::new()),
        1,
        MailId::NONE,
        MailId::NONE,
        None,
        Nanos(0),
        0,
        MailboxId(0),
    );
    drop(env);
}

/// ADR-0094 `Clone` note: cloning an armed dispatch produces a
/// **disarmed** clone (a clone is for inspection, never a second
/// obligation), so dropping the clone does not panic. The original is
/// discharged to keep the test itself clean.
#[cfg(debug_assertions)]
#[test]
fn clone_of_armed_dispatch_is_disarmed() {
    let env = OwnedDispatch::armed(
        KindId(7),
        "aether.tick".to_owned(),
        None,
        Source::NONE,
        MailRef::from(vec![9u8]),
        1,
        MailId::new(MailboxId(4), 4),
        MailId::new(MailboxId(4), 4),
        None,
        Nanos(0),
        0,
        MailboxId(4),
    );
    let clone = env.clone();
    // The clone carries no obligation — dropping it must not panic.
    drop(clone);
    // Original still armed: discharge so the test exits cleanly.
    env.discharge();
}

/// ADR-0094 issue 1326: arming a `MailId::NONE` dispatch mints **no**
/// obligation — `record_finished` no-ops on `MailId::NONE`, so the
/// chassis-internal fire-and-forget pushes that stamp it (RPC
/// self-pokes like `aether.rpc.inbound_ready`, window pushes) route
/// through the armed `Inbox` arm but never discharge. The arm site is
/// unconditional; `ObligationGuard::armed` disarms on NONE so the
/// guard's arm condition matches `record_finished` exactly. Dropping
/// such a dispatch without discharge must NOT panic.
#[cfg(debug_assertions)]
#[test]
fn armed_none_mail_id_dispatch_does_not_panic() {
    let env = OwnedDispatch::armed(
        KindId(7),
        "aether.rpc.inbound_ready".to_owned(),
        None,
        Source::NONE,
        MailRef::from(Vec::new()),
        1,
        MailId::NONE,
        MailId::NONE,
        None,
        Nanos(0),
        0,
        MailboxId(63),
    );
    // No discharge / transfer — a NONE dispatch carries no obligation,
    // so the guard must be disarmed and the drop must be silent.
    drop(env);
}

/// ADR-0094 no-leak side of the headline coverage: routing a real mail
/// through the standard actor dispatcher (`DispatcherSlot::dispatch_one`
/// via `register_inbox` + a seized run) discharges the obligation, so
/// no guard panic fires on the production drain path.
#[test]
fn standard_inbox_handler_relay_does_not_panic() {
    // The `register_inbox` relay closure moves the armed dispatch onto
    // a channel (a transfer); the channel's receiver here drains and
    // discharges it explicitly, mirroring `dispatch_one`. A panic here
    // would mean the relay/transfer path false-positives.
    use std::sync::mpsc;

    let (tx, rx) = mpsc::channel::<OwnedDispatch>();
    let handler: Arc<dyn InboxHandler> = Arc::new(move |dispatch: OwnedDispatch| {
        // Relay: the value moves onto the channel, carrying its
        // obligation. No discharge here — the drainer below owns it.
        let _ = tx.send(dispatch);
    });
    // Mint armed exactly as `route_mail`'s Inbox arm does.
    handler.enqueue(OwnedDispatch::armed(
        KindId(11),
        "aether.audio.note_on".to_owned(),
        None,
        Source::NONE,
        MailRef::from(vec![0u8]),
        1,
        MailId::new(MailboxId(5), 5),
        MailId::new(MailboxId(5), 5),
        None,
        Nanos(0),
        0,
        MailboxId(5),
    ));
    let env = rx.recv().expect("relay forwarded the dispatch");
    // Downstream dispatcher discharges (the `dispatch_one` template).
    env.discharge();
    drop(env);
}

#[test]
fn register_and_lookup_closure_mailbox() {
    let r = Registry::new();
    let id = r.register_inbox("physics", noop_handler());
    assert_eq!(id, MailboxId::from_name("physics"));
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
    let kind = r.register_kind("test.seize.kind");

    // Closure-backed inbox: no slot, so no seize handle ever resolves.
    let closure_id = r.register_inbox("closure", noop_handler());
    assert!(
        r.route_lookup(kind, closure_id).seize_handle().is_none(),
        "a closure-backed inbox exposes no seize handle"
    );

    // A `Pooled`-shaped inbox: empty before the slot is wired, then a
    // live handle after `install_seize_handle`.
    let pooled_id = r.register_inbox("pooled", noop_handler());
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
    let installed = r.install_seize_handle(pooled_id, handle.clone());
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
    assert!(!r.install_seize_handle(pooled_id, handle), "a duplicate seize installation is rejected");
    assert_eq!(
        r.route_lookup(kind, pooled_id).generation(),
        installed_generation,
        "a rejected seize installation must not publish a route generation"
    );

    r.register_inbox("reuse-one", noop_handler());
    r.register_inbox("reuse-two", noop_handler());
    assert!(
        r.route_lookup(kind, pooled_id).seize_handle().is_some(),
        "the installed handle survives both alternating buffers being reused"
    );
    let _ = slot_dyn;
}

#[test]
fn route_generations_advance_only_for_successful_mutations() {
    let r = Registry::new();
    let kind = KindId(0);
    let initial = r.route_lookup(kind, MailboxId::NONE).generation();

    assert!(r.try_register_inbox("aether.chassis", noop_handler()).is_err());
    assert_eq!(r.route_lookup(kind, MailboxId::NONE).generation(), initial);

    let id = r.try_register_inbox("generation", noop_handler()).expect("fresh route");
    let inserted = r.route_lookup(kind, id).generation();
    assert!(inserted > initial);

    assert!(r.try_register_inbox("generation", noop_handler()).is_err());
    assert_eq!(r.route_lookup(kind, id).generation(), inserted);

    r.drop_mailbox(id).expect("live route drops");
    let dropped = r.route_lookup(kind, id).generation();
    assert!(dropped > inserted);

    assert!(r.drop_mailbox(id).is_err());
    assert_eq!(r.route_lookup(kind, id).generation(), dropped);

    r.try_register_inbox("generation", noop_handler()).expect("dropped route re-registers");
    let reregistered = r.route_lookup(kind, id).generation();
    assert!(reregistered > dropped);

    assert!(r.remove_closure(id));
    let removed = r.route_lookup(kind, id).generation();
    assert!(removed > reregistered);
    assert!(r.entry(id).is_none());

    assert!(!r.remove_closure(id));
    assert_eq!(r.route_lookup(kind, id).generation(), removed);
}

#[test]
fn closure_handler_runs_on_call() {
    let r = Registry::new();
    let counter = Arc::new(AtomicU32::new(0));
    let c2 = Arc::clone(&counter);
    let id = r.register_inbox(
        "heartbeat",
        Arc::new(move |dispatch: OwnedDispatch| {
            c2.fetch_add(dispatch.count, Ordering::SeqCst);
        }),
    );
    let Some(MailboxEntry::Inbox { handler: h, .. }) = r.entry(id) else {
        panic!("expected closure entry")
    };
    // Test-side id is irrelevant — the handler ignores it.
    h.enqueue(test_owned_dispatch(KindId(0), "aether.tick", &[], 7));
    h.enqueue(OwnedDispatch::disarmed(
        KindId(0),
        "aether.tick".to_owned(),
        Some("physics".to_owned()),
        Source::NONE,
        MailRef::from(Vec::new()),
        3,
        MailId::NONE,
        MailId::NONE,
        None,
        Nanos(0),
        0,
        MailboxId(0),
    ));
    assert_eq!(counter.load(Ordering::SeqCst), 10);
}

#[test]
fn mailbox_ids_are_name_derived() {
    let r = Registry::new();
    let a = r.register_inbox("a", noop_handler());
    let b = r.register_inbox("b", noop_handler());
    let c = r.register_inbox("c", noop_handler());
    assert_eq!(a, MailboxId::from_name("a"));
    assert_eq!(b, MailboxId::from_name("b"));
    assert_eq!(c, MailboxId::from_name("c"));
    // All three distinct names produce distinct ids.
    assert_ne!(a, b);
    assert_ne!(b, c);
    assert_ne!(a, c);
    assert_eq!(r.len(), 3);
}

#[test]
#[should_panic(expected = "mailbox name already registered")]
fn duplicate_name_panics() {
    let r = Registry::new();
    r.register_inbox("x", noop_handler());
    r.register_inbox("x", noop_handler());
}

#[test]
fn lookup_missing_returns_none() {
    let r = Registry::new();
    assert!(r.lookup("nope").is_none());
    assert!(r.entry(MailboxId(42)).is_none());
}

#[test]
fn lookup_over_depth_scope_path_is_resolution_miss() {
    let r = Registry::new();
    // One segment past `MAX_SCOPE_PATH_DEPTH`: rejected before the fold.
    let name = (0..=aether_data::MAX_SCOPE_PATH_DEPTH).map(|i| format!("seg{i}")).collect::<Vec<_>>().join("/");
    assert!(r.lookup(&name).is_none());
}

#[test]
fn lookup_over_bytes_scope_path_is_resolution_miss() {
    let r = Registry::new();
    // Single segment longer than the byte cap (depth stays 1).
    let name = "a".repeat(aether_data::MAX_SCOPE_PATH_BYTES + 1);
    assert!(r.lookup(&name).is_none());
}

#[test]
#[allow(
    clippy::disallowed_methods,
    reason = "the registry canonical-name test must construct the exact lineage-fold id that lookup derives"
)]
fn canonical_resolution_reports_the_registered_path_and_structured_misses() {
    let r = Registry::new();
    let canonical = "root/worker:camera";
    let id = aether_data::mailbox_id_from_path(canonical);
    r.try_register_inbox_with_id(id, canonical, noop_handler()).unwrap();

    let resolved = r.resolve_address(canonical).expect("canonical mailbox is live");
    assert_eq!(resolved.mailbox_id, id);
    assert_eq!(resolved.canonical_path, canonical);
    assert_eq!(
        r.resolve_address("root/worker:missing"),
        Err(AddressResolutionError::NoLiveMailbox { canonical_path: "root/worker:missing".to_owned() })
    );

    let too_deep =
        (0..=aether_data::MAX_SCOPE_PATH_DEPTH).map(|index| format!("seg{index}")).collect::<Vec<_>>().join("/");
    assert_eq!(
        r.resolve_address(&too_deep),
        Err(AddressResolutionError::PathTooDeep { limit: aether_data::MAX_SCOPE_PATH_DEPTH })
    );
}

#[test]
fn mailbox_name_reverse_lookup() {
    let r = Registry::new();
    let a = r.register_inbox("physics", noop_handler());
    let b = r.register_inbox("graphics", noop_handler());
    assert_eq!(r.mailbox_name(a).as_deref(), Some("physics"));
    assert_eq!(r.mailbox_name(b).as_deref(), Some("graphics"));
    assert!(r.mailbox_name(MailboxId(999)).is_none());
}

#[test]
fn kind_ids_are_derived_from_name_and_schema() {
    let r = Registry::new();
    let a = r.register_kind("aether.tick");
    let b = r.register_kind("aether.key");
    let c = r.register_kind("hello.npc_health");
    // Ids are the fnv1a hash of canonical (name, schema) bytes —
    // distinct names under the same default schema must produce
    // distinct ids, and matching the expected const derivation
    // pins the hash contract with the derive.
    assert_ne!(a, b);
    assert_ne!(b, c);
    assert_ne!(a, c);
    assert_eq!(a, KindId(kind_id_from_parts("aether.tick", &SchemaType::Bytes)));
}

#[test]
fn kind_registration_is_idempotent() {
    let r = Registry::new();
    let first = r.register_kind("aether.tick");
    let second = r.register_kind("aether.tick");
    assert_eq!(first, second);
    // Different name produces a different id — the id is a pure
    // function of the input, not an allocation order.
    assert_ne!(r.register_kind("aether.key"), first);
}

#[test]
fn kind_publication_advances_only_for_new_definitions() {
    let r = Registry::new();
    assert_eq!(r.kind_generation(), 0);

    let first = r.register_kind("aether.tick");
    assert_eq!(r.kind_generation(), 1);
    assert_eq!(r.kind_id("aether.tick"), Some(first));

    assert_eq!(r.register_kind("aether.tick"), first);
    assert_eq!(r.kind_generation(), 1, "idempotent registration must not fabricate a generation");

    let second = r.register_kind("aether.key");
    assert_eq!(r.kind_generation(), 2);
    assert_eq!(r.kind_name(second).as_deref(), Some("aether.key"));
    assert_eq!(r.list_kind_descriptors().len(), 2);
}

#[test]
fn kind_id_lookup() {
    let r = Registry::new();
    let id = r.register_kind("aether.tick");
    assert_eq!(r.kind_id("aether.tick"), Some(id));
    assert!(r.kind_id("absent").is_none());
}

#[test]
fn kind_name_reverse_lookup() {
    let r = Registry::new();
    let a = r.register_kind("aether.tick");
    let b = r.register_kind("aether.key");
    assert_eq!(r.kind_name(a).as_deref(), Some("aether.tick"));
    assert_eq!(r.kind_name(b).as_deref(), Some("aether.key"));
    assert!(r.kind_name(KindId(999)).is_none());
}

fn unit_desc(name: &str) -> KindDescriptor {
    KindDescriptor { name: name.to_string(), schema: SchemaType::Unit }
}

fn cast_struct_desc(name: &str) -> KindDescriptor {
    use aether_data::{NamedField, Primitive};
    KindDescriptor {
        name: name.to_string(),
        schema: SchemaType::Struct {
            repr_c: true,
            fields: vec![NamedField { name: "x".into(), ty: SchemaType::Scalar(Primitive::U32) }].into(),
        },
    }
}

#[test]
fn register_kind_with_descriptor_stores_schema() {
    let r = Registry::new();
    let id = r.register_kind_with_descriptor(cast_struct_desc("aether.foo")).expect("fresh name");
    let stored = r.kind_descriptor(id).expect("descriptor present");
    assert_eq!(stored.schema, cast_struct_desc("aether.foo").schema);
}

#[test]
fn register_kind_with_descriptor_is_idempotent_on_match() {
    let r = Registry::new();
    let first = r.register_kind_with_descriptor(cast_struct_desc("aether.foo")).expect("first");
    let second = r.register_kind_with_descriptor(cast_struct_desc("aether.foo")).expect("same schema should succeed");
    assert_eq!(first, second);
}

/// The first registration stores the schema with named fields
/// (e.g. substrate boot via `aether_kinds::descriptors::all()`); a
/// second registration of the same structural kind with stripped
/// names (e.g. reconstructed from a component's `aether.kinds`
/// canonical bytes) must be accepted as idempotent because both
/// produce the same kind id. This is the path `#[actor]`
/// consumer-crate retention relies on for cross-crate kinds that
/// duplicate boot-registered ones.
#[test]
fn register_kind_with_descriptor_accepts_nominal_only_differences() {
    use aether_data::{NamedField, Primitive};
    let r = Registry::new();
    let named_id = r.register_kind_with_descriptor(cast_struct_desc("aether.foo")).expect("first");

    let unnamed = KindDescriptor {
        name: "aether.foo".into(),
        schema: SchemaType::Struct {
            repr_c: true,
            fields: vec![NamedField { name: "".into(), ty: SchemaType::Scalar(Primitive::U32) }].into(),
        },
    };
    let unnamed_id = r.register_kind_with_descriptor(unnamed).expect("same canonical bytes = same id = idempotent");
    assert_eq!(named_id, unnamed_id);

    // Named version stays in the stored slot — first writer wins.
    let stored = r.kind_descriptor(named_id).expect("still there");
    if let SchemaType::Struct { fields, .. } = &stored.schema {
        assert_eq!(fields[0].name, "x");
    } else {
        panic!("expected struct schema");
    }
}

#[test]
fn register_kind_with_descriptor_distinct_schemas_take_distinct_ids() {
    // Pre-ADR-0030-Phase-2 behavior was: same name + different
    // schema = `KindConflict`. Under hashed ids the id IS the
    // `(name, schema)` pair, so two schemas under the same name
    // land in two separate slots — conflict is only reachable via
    // a genuine hash collision. Document the post-Phase-2 shape
    // and let the conflict path stay exercised via the
    // `_is_idempotent_on_match` test (same-id reentry).
    let r = Registry::new();
    let unit_id = r.register_kind_with_descriptor(unit_desc("aether.foo")).expect("first");
    let struct_id = r
        .register_kind_with_descriptor(cast_struct_desc("aether.foo"))
        .expect("second — different schema, no conflict under hashed ids");
    assert_ne!(unit_id, struct_id);
    assert_eq!(r.kind_descriptor(unit_id).unwrap().schema, SchemaType::Unit);
    assert!(matches!(r.kind_descriptor(struct_id).unwrap().schema, SchemaType::Struct { .. }));
}

#[test]
fn register_kind_defaults_to_bytes() {
    let r = Registry::new();
    let id = r.register_kind("aether.bar");
    let stored = r.kind_descriptor(id).expect("descriptor present");
    assert_eq!(stored.schema, SchemaType::Bytes);
}

#[test]
fn name_only_and_with_descriptor_resolve_to_distinct_ids() {
    // Under hashed ids the id is a function of (name, schema).
    // The same name registered with two different schemas —
    // `Bytes` (via `register_kind`) and a real struct (via
    // `register_kind_with_descriptor`) — produces two *different*
    // ids, each stored under its own slot. `kind_id(name)` returns
    // whichever id was written to `name_index` most recently; this
    // is a test-only hazard and production callers go through
    // `register_kind_with_descriptor` exclusively.
    let r = Registry::new();
    let real = r.register_kind_with_descriptor(cast_struct_desc("aether.foo")).expect("real schema");
    let bytes = r.register_kind("aether.foo");
    assert_ne!(real, bytes);
    assert!(matches!(r.kind_descriptor(real).unwrap().schema, SchemaType::Struct { .. }));
    assert!(matches!(r.kind_descriptor(bytes).unwrap().schema, SchemaType::Bytes,));
}

#[test]
fn try_register_inbox_is_non_panicking_on_collision() {
    let r = Registry::new();
    let first = r.try_register_inbox("loaded", noop_handler()).expect("fresh name");
    let err = r.try_register_inbox("loaded", noop_handler()).expect_err("collision must not panic");
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
    let err = r.try_register_inbox("aether.chassis", noop_handler()).expect_err("reserved name must reject");
    assert_eq!(err.name, "aether.chassis");
    assert_eq!(r.len(), 0);
}

#[test]
fn drop_mailbox_frees_name_and_marks_entry_dropped() {
    let r = Registry::new();
    let id = r.try_register_inbox("loaded", noop_handler()).unwrap();
    let name = r.drop_mailbox(id).expect("drop");
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
    let reloaded = r.try_register_inbox("loaded", noop_handler()).unwrap();
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
    assert!(matches!(r.drop_mailbox(MailboxId(999)), Err(DropError::UnknownId(_))));
    let c = r.try_register_inbox("x", noop_handler()).unwrap();
    r.drop_mailbox(c).unwrap();
    assert!(matches!(r.drop_mailbox(c), Err(DropError::AlreadyDropped(_))));
}

/// Issue iamacoffeepot/aether#730: `list_mailbox_descriptors`
/// snapshots the table sorted by name, categorises each entry by
/// its name prefix, and inserts a synthetic `ChassisSentinel`
/// entry under `aether.chassis` (which is never a real registry
/// row — `insert` rejects the reserved name).
#[test]
fn list_mailbox_descriptors_snapshots_sorted_with_categories() {
    let r = Registry::new();
    r.register_inbox("aether.input", noop_handler());
    r.register_inbox("aether.embedded:cam", noop_handler());
    r.register_inbox("user_thing", noop_handler());

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
    let id = r.register_inbox("aether.audio", noop_handler());
    let entry = r.list_mailbox_descriptors().into_iter().find(|d| d.name == "aether.audio").expect("audio entry");
    assert_eq!(entry.id, id);
    assert_eq!(entry.id, MailboxId::from_name("aether.audio"));
}

struct InventorySubscriber;

impl aether_actor::Addressable for InventorySubscriber {
    const NAMESPACE: &'static str = "test.inventory-subscriber";
    type Resolver = aether_actor::One;
}

impl aether_actor::HandlesKind<aether_actor::RegistryChanged> for InventorySubscriber {}

fn inventory_subscription_fixture() -> (Arc<Registry>, Arc<Mailer>, crossbeam_channel::Receiver<KindId>, MailboxId) {
    let registry = Arc::new(Registry::new());
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
    let (sender, receiver) = crossbeam_channel::unbounded();
    let target = registry.register_inbox(
        "inventory-subscriber",
        Arc::new(move |dispatch: OwnedDispatch| {
            sender.send(dispatch.kind).expect("inventory test receiver stays connected");
            dispatch.discharge();
        }),
    );
    (registry, mailer, receiver, target)
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

    registry.register_inbox("aether.input", noop_handler());
    registry.register_kind("aether.inventory.test");

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

    registry.register_inbox("aether.input", noop_handler());
    let kind = registry.register_kind("aether.inventory.race");
    subscription.acknowledge(observed.mailbox_generation, observed.kind_generation);

    wakes.recv_timeout(Duration::from_millis(100)).expect("stale pair re-arms a wake");
    let latest = registry.inventory();
    assert_eq!(latest.mailbox_generation, observed.mailbox_generation + 1);
    assert_eq!(latest.kind_generation, observed.kind_generation + 1);
    subscription.acknowledge(latest.mailbox_generation, latest.kind_generation);

    registry.try_register_inbox("aether.input", noop_handler()).expect_err("conflict is observable");
    assert_eq!(registry.register_kind("aether.inventory.race"), kind, "matching kind registration is idempotent");
    assert_eq!(
        (registry.inventory().mailbox_generation, registry.inventory().kind_generation),
        (latest.mailbox_generation, latest.kind_generation),
        "rejection and idempotent kind match publish no inventory generation"
    );
    assert!(wakes.recv_timeout(Duration::from_millis(20)).is_err(), "rejection and idempotent kind match emit no wake");
}

fn starting_token(result: &[RegistryApplied]) -> ActivationToken {
    let [RegistryApplied::Starting { token, .. }] = result else {
        panic!("expected one Starting result, got {result:?}")
    };
    *token
}

#[test]
fn starting_is_keyed_only_and_excluded_from_every_live_surface() {
    use std::any::Any;

    use crate::scheduler::{BatchBudget, CycleResult, Drainable, SlotState};

    struct TestSlot;
    impl Drainable for TestSlot {
        fn run_cycle(&self, _budget: BatchBudget) -> CycleResult {
            CycleResult::Idle
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    let (registry, mailer, wakes, target) = inventory_subscription_fixture();
    let subscription = registry.subscribe_inventory::<InventorySubscriber>(target, Arc::clone(&mailer));
    wakes.recv_timeout(Duration::from_millis(100)).expect("initial inventory wake");
    let acknowledged = registry.inventory();
    subscription.acknowledge(acknowledged.mailbox_generation, acknowledged.kind_generation);
    let owner = RegistryOwnerLease::attach(&registry, &mailer, WakeSink::detached());
    let initial_route_generation = registry.route_generation();
    let initial_mailbox_generation = registry.mailbox_generation();
    let name = "aether.component/starting-only";
    #[allow(clippy::disallowed_methods, reason = "the test exercises the registry's canonical path lookup")]
    let id = aether_data::mailbox_id_from_path(name);
    let completion = registry
        .submit(EffectBatch::new(vec![RegistryEffect::reserve_with_id(id, name.to_owned())]))
        .expect("owner accepts Starting reservation");

    owner.run_once();
    let _token = starting_token(&completion.wait_timeout(Duration::from_millis(100)).unwrap().unwrap());

    assert_eq!(registry.lookup(name), Some(id), "exact-name keyed lookup sees Starting");
    assert_eq!(registry.mailbox_name(id).as_deref(), Some(name), "keyed reverse lookup sees Starting");
    assert!(registry.entry(id).is_none(), "compatibility entry does not project Starting as live");
    assert!(registry.route_lookup(KindId(1), id).is_starting(), "dispatch lookup identifies Starting privately");
    assert!(registry.route_lookup(KindId(1), id).seize_handle().is_none(), "Starting has no seize handle");
    assert!(registry.list_mailbox_descriptors().iter().all(|descriptor| descriptor.id != id));
    assert_eq!(registry.mailbox_generation(), initial_mailbox_generation, "Starting is not public inventory");
    assert!(wakes.recv_timeout(Duration::from_millis(20)).is_err(), "Starting emits no public inventory event");
    assert!(registry.route_generation() > initial_route_generation, "Starting advances only the keyed generation");
    assert!(matches!(registry.drop_mailbox(id), Err(DropError::UnknownId(found)) if found == id));
    assert!(!registry.remove_closure(id), "ordinary removal does not treat Starting as live");
    let slot: Arc<dyn Drainable> = Arc::new(TestSlot);
    let handle = SeizeHandle::new(Arc::new(SlotState::new()), Arc::downgrade(&slot));
    assert!(!registry.install_seize_handle(id, handle), "Starting rejects seize installation");
}

#[test]
fn starting_tokens_are_unique_stale_safe_and_transactional() {
    let registry = Arc::new(Registry::new());
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
    let owner = RegistryOwnerLease::attach(&registry, &mailer, WakeSink::detached());
    registry.register_inbox("occupied", noop_handler());
    let before_rollback = registry.route_generation();
    let rolled_back = registry
        .submit(EffectBatch::new(vec![
            RegistryEffect::reserve_named("must-rollback-starting".to_owned()),
            RegistryEffect::publish_named(
                "occupied".to_owned(),
                MailboxEntry::Inbox { handler: noop_handler(), seize: Arc::default() },
            ),
        ]))
        .unwrap();
    owner.run_once();
    assert!(matches!(rolled_back.wait_timeout(Duration::from_millis(100)).unwrap(), Err(RegistryEffectError::Name(_))));
    assert!(registry.lookup("must-rollback-starting").is_none());
    assert_eq!(registry.route_generation(), before_rollback, "rejected transaction publishes no partial Starting");

    let name = "token-reuse";
    let id = MailboxId::from_name(name);
    let first = registry.submit(EffectBatch::new(vec![RegistryEffect::reserve_named(name.to_owned())])).unwrap();
    owner.run_once();
    let first_token = starting_token(&first.wait_timeout(Duration::from_millis(100)).unwrap().unwrap());
    let cancelled =
        registry.submit(EffectBatch::new(vec![RegistryEffect::CancelStarting { id, token: first_token }])).unwrap();
    owner.run_once();
    assert_eq!(
        cancelled.wait_timeout(Duration::from_millis(100)).unwrap().unwrap(),
        [RegistryApplied::StartingCancellation(StartingCancellation::Cancelled(id))]
    );

    let second = registry.submit(EffectBatch::new(vec![RegistryEffect::reserve_with_id(id, name.to_owned())])).unwrap();
    owner.run_once();
    let second_token = starting_token(&second.wait_timeout(Duration::from_millis(100)).unwrap().unwrap());
    assert_ne!(first_token, second_token, "a reused key receives a fresh activation token");

    let stale =
        registry.submit(EffectBatch::new(vec![RegistryEffect::CancelStarting { id, token: first_token }])).unwrap();
    owner.run_once();
    assert_eq!(
        stale.wait_timeout(Duration::from_millis(100)).unwrap().unwrap(),
        [RegistryApplied::StartingCancellation(StartingCancellation::TokenMismatch(id))]
    );
    assert_eq!(registry.lookup(name), Some(id), "stale cancellation cannot consume the newer reservation");
}

#[test]
fn owner_drains_fifo_batches_with_one_publication_per_dirty_view() {
    let registry = Arc::new(Registry::new());
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
    let owner = RegistryOwnerLease::attach(&registry, &mailer, WakeSink::detached());
    let id = MailboxId::from_name("ordered");
    let endpoint = || MailboxEntry::Inbox { handler: noop_handler(), seize: Arc::default() };
    let first = registry
        .submit(EffectBatch::new(vec![
            RegistryEffect::publish_named("ordered".to_owned(), endpoint()),
            RegistryEffect::DropMailbox(id),
            RegistryEffect::publish_named("ordered".to_owned(), endpoint()),
        ]))
        .expect("attached owner accepts effects");
    let rejected = registry
        .submit(EffectBatch::new(vec![RegistryEffect::publish_named("ordered".to_owned(), endpoint())]))
        .expect("attached owner accepts the conflicting batch");
    let rolled_back = registry
        .submit(EffectBatch::new(vec![
            RegistryEffect::publish_named("must-rollback".to_owned(), endpoint()),
            RegistryEffect::publish_named("ordered".to_owned(), endpoint()),
        ]))
        .expect("attached owner accepts the transactional batch");

    assert_eq!(registry.route_generation(), 0);
    assert_eq!(registry.mailbox_generation(), 0);
    owner.run_once();

    assert_eq!(
        first.wait_timeout(Duration::from_millis(100)).expect("completion arrives").expect("batch applies"),
        [RegistryApplied::Mailbox(id), RegistryApplied::Dropped("ordered".to_owned()), RegistryApplied::Mailbox(id),]
    );
    assert!(matches!(
        rejected.wait_timeout(Duration::from_millis(100)).expect("rejection arrives"),
        Err(RegistryEffectError::Name(_))
    ));
    assert!(matches!(
        rolled_back.wait_timeout(Duration::from_millis(100)).expect("rollback rejection arrives"),
        Err(RegistryEffectError::Name(_))
    ));
    assert!(registry.lookup("must-rollback").is_none(), "a rejected batch commits none of its staged keys");
    assert_eq!(registry.route_generation(), 1, "one self-sized drain publishes the keyed view once");
    assert_eq!(registry.mailbox_generation(), 1, "one self-sized drain publishes inventory once");
    assert_eq!(registry.lookup("ordered"), Some(id));
}

#[test]
fn owner_captures_authoritative_live_route_but_only_relay_invokes_inline() {
    let registry = Arc::new(Registry::new());
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
    let relay = RouteRelayLease::attach(&mailer, WakeSink::detached());
    let owner = RegistryOwnerLease::attach(&registry, &mailer, WakeSink::detached());
    let received = Arc::new(Mutex::new(Vec::new()));
    let received_for_handler = Arc::clone(&received);
    let handler: Arc<dyn InlineHandler> = Arc::new(move |dispatch: MailDispatch<'_>| {
        received_for_handler.lock().unwrap().push(dispatch.payload.to_vec());
    });
    let name = "captured-live-then-dropped";
    let id = MailboxId::from_name(name);
    let live = registry
        .submit(EffectBatch::new(vec![RegistryEffect::publish_named(name.to_owned(), MailboxEntry::Inline(handler))]))
        .unwrap();

    mailer.push(Mail::new(id, KindId(77), vec![7], 1));
    let dropped = registry.submit(EffectBatch::new(vec![RegistryEffect::DropMailbox(id)])).unwrap();
    owner.run_once();

    assert!(live.wait_timeout(Duration::from_millis(100)).unwrap().is_ok());
    assert!(dropped.wait_timeout(Duration::from_millis(100)).unwrap().is_ok());
    assert!(matches!(registry.entry(id), Some(MailboxEntry::Dropped)));
    assert!(received.lock().unwrap().is_empty(), "the registry-owner turn never invokes captured Inline code");

    relay.run_once();
    assert_eq!(
        received.lock().unwrap().as_slice(),
        [vec![7]],
        "relay uses the owner's captured Live endpoint even though the published route is now Dropped"
    );
}

fn traced_unknown_mail(
    mailer: &Mailer,
    settlement: &SettlementRegistry,
    recipient: MailboxId,
    sequence: u64,
    payload: Vec<u8>,
) -> (Mail, crossbeam_channel::Receiver<()>) {
    let root = MailId::new(MailboxId(0x4111), sequence);
    let settled = settlement.subscribe_settlement(root);
    mailer.record_sent(root, root, None, root.sender, recipient, KindId(0x4111));
    (Mail::new(recipient, KindId(0x4111), payload, 1).with_lineage(root, root, None), settled)
}

#[test]
fn starting_parks_fifo_and_owner_close_routes_every_accepted_mail_once() {
    let registry = Arc::new(Registry::new());
    let (outbound, outbound_rx) = HubOutbound::attached_loopback();
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)).with_outbound(outbound));
    let settlement = Arc::new(SettlementRegistry::new());
    mailer.trace_handle().install_settlement_registry(Arc::clone(&settlement));
    let relay = RouteRelayLease::attach(&mailer, WakeSink::detached());
    let owner = RegistryOwnerLease::attach(&registry, &mailer, WakeSink::detached());
    let name = "starting-close-fifo";
    let id = MailboxId::from_name(name);
    let reserved = registry.submit(EffectBatch::new(vec![RegistryEffect::reserve_named(name.to_owned())])).unwrap();
    owner.run_once();
    let _token = starting_token(&reserved.wait_timeout(Duration::from_millis(100)).unwrap().unwrap());

    let (first, first_settled) = traced_unknown_mail(&mailer, &settlement, id, 1, vec![1]);
    mailer.push(first);
    owner.run_once();
    assert!(first_settled.try_recv().is_err(), "parked mail keeps settlement open");
    let (second, second_settled) = traced_unknown_mail(&mailer, &settlement, id, 2, vec![2]);
    mailer.push(second);

    drop(owner);
    assert!(first_settled.try_recv().is_err());
    assert!(second_settled.try_recv().is_err());
    assert!(outbound_rx.try_recv().is_err(), "owner close only transfers accepted mail to the relay");

    drop(relay);
    assert!(first_settled.recv_timeout(Duration::from_millis(100)).is_ok());
    assert!(second_settled.recv_timeout(Duration::from_millis(100)).is_ok());
    let payloads = [
        outbound_rx.recv_timeout(Duration::from_millis(100)).unwrap(),
        outbound_rx.recv_timeout(Duration::from_millis(100)).unwrap(),
    ]
    .map(|event| match event {
        EgressEvent::UnresolvedMail { payload, .. } => payload,
        other => panic!("expected unresolved continuation, got {other:?}"),
    });
    assert_eq!(payloads, [vec![1], vec![2]], "pending and close-racing mail retain per-recipient FIFO");
    assert!(outbound_rx.try_recv().is_err(), "each accepted Mail routes exactly once");
}

#[test]
fn cancellation_holds_settlement_until_relay_terminal_delivery() {
    let registry = Arc::new(Registry::new());
    let (outbound, outbound_rx) = HubOutbound::attached_loopback();
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)).with_outbound(outbound));
    let settlement = Arc::new(SettlementRegistry::new());
    mailer.trace_handle().install_settlement_registry(Arc::clone(&settlement));
    let relay = RouteRelayLease::attach(&mailer, WakeSink::detached());
    let owner = RegistryOwnerLease::attach(&registry, &mailer, WakeSink::detached());
    let name = "starting-cancel-settlement";
    let id = MailboxId::from_name(name);
    let reserved = registry.submit(EffectBatch::new(vec![RegistryEffect::reserve_named(name.to_owned())])).unwrap();
    owner.run_once();
    let token = starting_token(&reserved.wait_timeout(Duration::from_millis(100)).unwrap().unwrap());
    let (mail, settled) = traced_unknown_mail(&mailer, &settlement, id, 3, vec![3]);
    mailer.push(mail);
    owner.run_once();
    assert!(settled.try_recv().is_err());

    let cancelled = registry.submit(EffectBatch::new(vec![RegistryEffect::CancelStarting { id, token }])).unwrap();
    owner.run_once();
    assert_eq!(
        cancelled.wait_timeout(Duration::from_millis(100)).unwrap().unwrap(),
        [RegistryApplied::StartingCancellation(StartingCancellation::Cancelled(id))]
    );
    assert!(settled.try_recv().is_err(), "owner cancellation captures but does not run the terminal tail");

    relay.run_once();
    assert!(settled.recv_timeout(Duration::from_millis(100)).is_ok());
    assert!(
        matches!(outbound_rx.recv_timeout(Duration::from_millis(100)).unwrap(), EgressEvent::UnresolvedMail { payload, .. } if payload == [3])
    );
    assert!(outbound_rx.try_recv().is_err(), "cancelled parked mail settles and egresses exactly once");
}

#[test]
#[allow(clippy::disallowed_methods, reason = "the test deliberately races the two writer entry points")]
fn direct_and_owner_paths_share_the_transitional_writer() {
    use std::sync::Barrier;

    let registry = Arc::new(Registry::new());
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
    let owner = RegistryOwnerLease::attach(&registry, &mailer, WakeSink::detached());
    let completion = registry
        .submit(EffectBatch::new(vec![RegistryEffect::publish_named(
            "shared-writer".to_owned(),
            MailboxEntry::Inbox { handler: noop_handler(), seize: Arc::default() },
        )]))
        .expect("owner accepts effect");
    let barrier = Arc::new(Barrier::new(2));
    let direct_registry = Arc::clone(&registry);
    let direct_barrier = Arc::clone(&barrier);
    let direct = thread::spawn(move || {
        direct_barrier.wait();
        direct_registry.try_register_inbox("shared-writer", noop_handler())
    });
    barrier.wait();
    owner.run_once();

    let owner_result = completion.wait_timeout(Duration::from_millis(100)).expect("owner completes");
    let direct_result = direct.join().expect("direct writer does not panic");
    assert_ne!(owner_result.is_ok(), direct_result.is_ok(), "exactly one serialized writer claims the route");
    assert_eq!(registry.list_mailbox_descriptors().iter().filter(|entry| entry.name == "shared-writer").count(), 1);
}

#[test]
fn owner_close_rejects_queued_and_future_submissions_without_stranding_completion() {
    let registry = Arc::new(Registry::new());
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
    let owner = RegistryOwnerLease::attach(&registry, &mailer, WakeSink::detached());
    let completion = registry
        .submit(EffectBatch::new(vec![RegistryEffect::publish_named(
            "queued-at-close".to_owned(),
            MailboxEntry::Inbox { handler: noop_handler(), seize: Arc::default() },
        )]))
        .expect("owner accepts before close");

    drop(owner);

    assert!(matches!(
        completion.wait_timeout(Duration::from_millis(100)).expect("close resolves queued completion"),
        Err(RegistryEffectError::OwnerClosed)
    ));
    assert!(registry.submit(EffectBatch::new(Vec::new())).is_none(), "closed owner rejects future submissions");
}

#[test]
#[allow(clippy::disallowed_methods, reason = "the test deliberately races owner submit against owner close")]
fn owner_submit_racing_close_is_rejected_or_completed() {
    use std::sync::Barrier;

    let registry = Arc::new(Registry::new());
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
    let owner = RegistryOwnerLease::attach(&registry, &mailer, WakeSink::detached());
    let barrier = Arc::new(Barrier::new(2));
    let submitting_registry = Arc::clone(&registry);
    let submitting_barrier = Arc::clone(&barrier);
    let submit = thread::spawn(move || {
        submitting_barrier.wait();
        submitting_registry.submit(EffectBatch::new(Vec::new()))
    });

    barrier.wait();
    drop(owner);

    if let Some(completion) = submit.join().expect("submitter does not panic") {
        assert!(matches!(
            completion.wait_timeout(Duration::from_millis(100)).expect("accepted race resolves on close"),
            Err(RegistryEffectError::OwnerClosed)
        ));
    }
}

#[test]
fn registration_through_shared_arc() {
    // Interior mutability means Arc<Registry> can register after
    // it's already been shared — the dispatch path today never
    // exercises this, but PR 2+ will when `load_component` adds
    // mailboxes and kinds from a handler that holds an Arc.
    let r = Arc::new(Registry::new());
    let r2 = Arc::clone(&r);
    let id = r2.register_inbox("late", noop_handler());
    assert_eq!(r.lookup("late"), Some(id));
    let kind_id = r.register_kind("aether.late");
    assert_eq!(
        r.kind_id("aether.late"),
        Some(kind_id),
        "shared Arc registrations are visible through the original handle"
    );
}

/// Issue iamacoffeepot/aether#848 Phase 1: a bare
/// `Fn(MailDispatch<'_>)` closure satisfies `InlineHandler` via
/// the blanket impl, and dispatching through
/// `<dyn InlineHandler>::dispatch` invokes the body once per
/// call. No mailer / registry plumbing is wired through yet —
/// that lands in PR 2.
#[test]
fn inline_handler_blanket_impl_dispatches_closure_body() {
    let counter = Arc::new(AtomicU32::new(0));
    let c2 = Arc::clone(&counter);
    let handler: Arc<dyn InlineHandler> = Arc::new(move |dispatch: MailDispatch<'_>| {
        c2.fetch_add(dispatch.count, Ordering::SeqCst);
    });
    handler.dispatch(test_dispatch(KindId(0), "aether.tick", &[], 5));
    handler.dispatch(test_dispatch(KindId(0), "aether.tick", &[], 7));
    assert_eq!(
        counter.load(Ordering::SeqCst),
        12,
        "blanket InlineHandler impl should forward each dispatch to the closure body once",
    );
}

/// Issue iamacoffeepot/aether#848 Phase 1: a bare
/// `Fn(OwnedDispatch)` closure satisfies `InboxHandler` via the
/// blanket impl. The closure body moves the payload into a
/// captured Vec, demonstrating the ownership transfer the trait
/// exists to enable — the hot-path "no `to_vec()` clone" win
/// called out in iamacoffeepot/aether#848.
#[test]
fn inbox_handler_blanket_impl_moves_owned_payload() {
    let collected = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
    let collected_for_handler = Arc::clone(&collected);
    let handler: Arc<dyn InboxHandler> = Arc::new(move |dispatch: OwnedDispatch| {
        // Payload moves straight into the captured Vec — no clone
        // or `to_vec()` on a borrowed slice.
        collected_for_handler.lock().unwrap().push(dispatch.payload.into_vec());
    });

    handler.enqueue(OwnedDispatch::disarmed(
        KindId(0),
        "aether.audio.note_on".to_owned(),
        None,
        Source::NONE,
        MailRef::from(vec![1, 2, 3]),
        1,
        MailId::NONE,
        MailId::NONE,
        None,
        Nanos(0),
        0,
        MailboxId(0),
    ));
    handler.enqueue(OwnedDispatch::disarmed(
        KindId(0),
        "aether.audio.note_on".to_owned(),
        None,
        Source::NONE,
        MailRef::from(vec![4, 5, 6, 7]),
        1,
        MailId::NONE,
        MailId::NONE,
        None,
        Nanos(0),
        0,
        MailboxId(0),
    ));

    let collected = collected.lock().unwrap();
    assert_eq!(collected.len(), 2);
    assert_eq!(collected[0], vec![1, 2, 3]);
    assert_eq!(collected[1], vec![4, 5, 6, 7]);
    drop(collected);
}

/// Issue iamacoffeepot/aether#848 Phase 1: hand-rolled
/// `impl InboxHandler for MyStruct` compiles and dispatches
/// alongside the blanket-impl path. This is the cap-authoring
/// shape PR 3 will reach for (a struct holding the mpsc Sender);
/// a regression here means caps can't migrate.
#[test]
fn inbox_handler_hand_rolled_impl_dispatches_per_call() {
    use std::sync::mpsc;

    struct ChannelForwarder {
        tx: mpsc::Sender<OwnedDispatch>,
    }
    impl InboxHandler for ChannelForwarder {
        fn enqueue(&self, dispatch: OwnedDispatch) {
            let _ = self.tx.send(dispatch);
        }
    }

    let (tx, rx) = mpsc::channel();
    let handler: Arc<dyn InboxHandler> = Arc::new(ChannelForwarder { tx });
    handler.enqueue(OwnedDispatch::disarmed(
        KindId(42),
        "aether.fs.write".to_owned(),
        Some("aether.fs".to_owned()),
        Source::NONE,
        MailRef::from(vec![0xAB, 0xCD]),
        1,
        MailId::NONE,
        MailId::NONE,
        None,
        Nanos(0),
        0,
        MailboxId(0),
    ));

    let received = rx.try_recv().expect("hand-rolled enqueue should send");
    assert_eq!(received.kind, KindId(42));
    assert_eq!(received.kind_name, "aether.fs.write");
    assert_eq!(received.payload.into_vec(), vec![0xAB, 0xCD]);
    assert!(rx.try_recv().is_err(), "exactly one enqueue should send exactly one envelope");
}
