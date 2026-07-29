use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use aether_data::canonical::kind_id_from_parts;
use aether_data::{KindDescriptor, MailboxCategory, SchemaType};
use aether_kinds::trace::Nanos;

use crate::mail::registry::{
    AddressResolutionError, DropError, InboxHandler, InlineHandler, MailDispatch, MailboxEntry, OwnedDispatch,
    Registry, noop_handler, test_dispatch, test_owned_dispatch,
};
use crate::mail::{KindId, MailId, MailRef, MailboxId, Source};
use crate::scheduler::SeizeHandle;

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

    let r = Registry::new();
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
    let before_install = r.route_lookup(kind, pooled_id);
    assert!(before_install.seize_handle().is_none(), "the seize cell is empty until the Pooled slot is wired");
    let before_install_generation = before_install.generation();

    let slot = Arc::new(StatefulSlot { state: Arc::new(SlotState::new()) });
    let slot_dyn: Arc<dyn Drainable> = slot.clone();
    let handle = SeizeHandle::new(Arc::clone(&slot.state), Arc::downgrade(&slot_dyn));
    let installed = r.install_seize_handle(pooled_id, handle.clone());
    assert!(installed, "install lands on a live Inbox entry");

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
    // Under ADR-0029 the id is a function of the name, so a
    // re-register produces the *same* id and flips the entry back
    // to `Component`.
    let reloaded = r.try_register_inbox("loaded", noop_handler()).unwrap();
    assert_eq!(reloaded, id);
    assert_eq!(r.lookup("loaded"), Some(reloaded));
    assert!(matches!(r.entry(reloaded), Some(MailboxEntry::Inbox { .. })));
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

/// Issue iamacoffeepot/aether#742: every successful
/// `register_inbox` fires the installed change hook with the
/// post-registration inventory snapshot. The chassis wires this
/// hook to push to the hub via `egress_mailboxes_changed` so any
/// chassis-builder cap that registers post-Hello shows up in the
/// hub's inventory cache without an explicit publish.
#[test]
fn mailbox_change_hook_fires_on_register_inbox() {
    use std::sync::Mutex;

    let r = Arc::new(Registry::new());
    let snapshots: Arc<Mutex<Vec<Vec<String>>>> = Arc::new(Mutex::new(Vec::new()));
    let snapshots_for_hook = Arc::clone(&snapshots);
    r.set_on_mailbox_change(Arc::new(move |descriptors| {
        let names: Vec<String> = descriptors.into_iter().map(|d| d.name).collect();
        snapshots_for_hook.lock().unwrap().push(names);
    }));

    r.register_inbox("aether.input", noop_handler());
    r.register_inbox("aether.render", noop_handler());

    let captured = snapshots.lock().unwrap();
    assert_eq!(captured.len(), 2, "hook should fire once per successful register_inbox");
    // Each snapshot is the FULL inventory at that moment (matches
    // the wire `MailboxesChanged` semantics — full replace, not
    // delta), so the second snapshot strictly contains the first.
    assert!(captured[0].contains(&"aether.input".to_owned()));
    assert!(captured[1].contains(&"aether.input".to_owned()));
    assert!(captured[1].contains(&"aether.render".to_owned()));
    drop(captured);
}

/// Issue 742: `try_register_inbox` fires the hook on the Ok
/// branch and stays silent on `NameConflict`.
#[test]
fn mailbox_change_hook_fires_on_try_register_inbox_ok_only() {
    use std::sync::Mutex;

    let r = Arc::new(Registry::new());
    let count: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));
    let count_for_hook = Arc::clone(&count);
    r.set_on_mailbox_change(Arc::new(move |_| {
        *count_for_hook.lock().unwrap() += 1;
    }));

    let _ = r.try_register_inbox("aether.input", noop_handler()).expect("first register OK");
    // Second registration with the same name conflicts.
    let _ = r.try_register_inbox("aether.input", noop_handler()).expect_err("second register should NameConflict");

    assert_eq!(*count.lock().unwrap(), 1, "hook fires once on Ok only");
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
