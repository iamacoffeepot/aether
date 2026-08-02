//! Tests for [`super::super::mailbox::birth`] — the reservation a
//! `Starting` route stands on, the mail parked behind it, and its
//! promotion or cancellation.

use std::panic;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::chassis::settlement::SettlementRegistry;
use crate::config::RegistryQueueCapacities;
use crate::mail::mailer::Mailer;
use crate::mail::outbound::{EgressEvent, HubOutbound};
use crate::mail::registry::effect::{
    EffectBatch, RegistryApplied, RegistryEffect, RegistryEffectError, StartingCancellation,
};
use crate::mail::registry::owner::RegistryOwnerLease;
use crate::mail::registry::relay::RouteRelayLease;
use crate::mail::registry::{DropError, InboxHandler, MailboxEntry, OwnedDispatch, Registry, noop_handler};
use crate::mail::{KindId, Mail, MailRef, MailboxId};
use crate::runtime::lifecycle::{FatalAborter, PanicAborter};
use crate::scheduler::{Pool, PoolConfig, SeizeHandle, WakeSink};
use crate::testing::boot_authority as auth;

use super::support::{
    InventorySubscriber, activation_barrier, inventory_subscription_fixture, prepared_test_spawn, starting_token,
    traced_unknown_mail,
};

#[test]
fn prepared_births_publish_together_then_promote_independently_with_exact_cost_cells() {
    let registry = Arc::new(Registry::new());
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
    let owner = RegistryOwnerLease::attach(
        auth(),
        &registry,
        &mailer,
        WakeSink::detached(),
        RegistryQueueCapacities::default(),
    );
    let scheduled = Arc::new(AtomicUsize::new(0));
    let first_id = MailboxId::from_name("prepared-first");
    let second_id = MailboxId::from_name("prepared-second");
    let expected = vec![first_id, second_id];
    let (first_id, first_cell, _, first) = prepared_test_spawn(
        &registry,
        &mailer,
        "prepared-first",
        Arc::new(Mutex::new(Vec::new())),
        Arc::clone(&scheduled),
        expected.clone(),
        1,
    );
    let (second_id, second_cell, _, second) = prepared_test_spawn(
        &registry,
        &mailer,
        "prepared-second",
        Arc::new(Mutex::new(Vec::new())),
        Arc::clone(&scheduled),
        expected,
        2,
    );
    let completion = registry.submit(EffectBatch::new(vec![first, second])).unwrap();
    owner.run_once();
    let applied = completion.wait_timeout(Duration::from_millis(100)).unwrap().unwrap();
    let [RegistryApplied::Starting { token: first_token, .. }, RegistryApplied::Starting { token: second_token, .. }] =
        applied.as_slice()
    else {
        panic!("expected two Starting results: {applied:?}")
    };
    assert_eq!(scheduled.load(Ordering::SeqCst), 2);
    for (id, expected_cell) in [(first_id, &first_cell), (second_id, &second_cell)] {
        let cells = mailer.cost_table().cells_for(id);
        assert_eq!(cells.len(), 1);
        assert!(Arc::ptr_eq(&cells[0].1, expected_cell));
    }

    mailer.push(activation_barrier(second_id, *second_token, 1));
    owner.run_once();
    assert!(registry.entry(second_id).is_some(), "fast activation promotes independently");
    assert!(registry.entry(first_id).is_none(), "slow activation remains Starting");
    mailer.push(activation_barrier(first_id, *first_token, 1));
    owner.run_once();
    assert!(registry.entry(first_id).is_some());
}

#[test]
fn rejected_batch_does_not_cancel_an_existing_prepared_birth() {
    let registry = Arc::new(Registry::new());
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
    let owner = RegistryOwnerLease::attach(
        auth(),
        &registry,
        &mailer,
        WakeSink::detached(),
        RegistryQueueCapacities::default(),
    );
    let scheduled = Arc::new(AtomicUsize::new(0));
    let id = MailboxId::from_name("prepared-cancel-rollback");
    let (_, _, cancelled, effect) = prepared_test_spawn(
        &registry,
        &mailer,
        "prepared-cancel-rollback",
        Arc::new(Mutex::new(Vec::new())),
        scheduled,
        vec![id],
        1,
    );
    let completion = registry.submit(EffectBatch::new(vec![effect])).unwrap();
    owner.run_once();
    let token = starting_token(&completion.wait_timeout(Duration::from_millis(100)).unwrap().unwrap());
    registry.register_inbox(&auth(), "prepared-cancel-conflict", noop_handler());
    let rejected = registry
        .submit(EffectBatch::new(vec![
            RegistryEffect::CancelStarting { id, token },
            RegistryEffect::publish_named(
                "prepared-cancel-conflict".to_owned(),
                MailboxEntry::Inbox { handler: noop_handler(), seize: Arc::default() },
            ),
        ]))
        .unwrap();
    owner.run_once();

    assert!(matches!(rejected.wait_timeout(Duration::from_millis(100)).unwrap(), Err(RegistryEffectError::Name(_))));
    assert_eq!(cancelled.load(Ordering::SeqCst), 0, "rejected transaction invokes no cancellation side effect");
    mailer.push(activation_barrier(id, token, 1));
    owner.run_once();
    assert!(registry.entry(id).is_some(), "the original prepared birth can still promote");
}

#[test]
fn bootstrap_then_parked_then_live_mail_is_deterministic_and_stale_barrier_is_consumed() {
    let registry = Arc::new(Registry::new());
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
    let owner = RegistryOwnerLease::attach(
        auth(),
        &registry,
        &mailer,
        WakeSink::detached(),
        RegistryQueueCapacities::default(),
    );
    let deliveries = Arc::new(Mutex::new(Vec::new()));
    let scheduled = Arc::new(AtomicUsize::new(0));
    let id = MailboxId::from_name("prepared-fifo");
    let (_, _, _, effect) =
        prepared_test_spawn(&registry, &mailer, "prepared-fifo", Arc::clone(&deliveries), scheduled, vec![id], 1);
    let completion = registry.submit(EffectBatch::new(vec![effect])).unwrap();
    owner.run_once();
    let token = starting_token(&completion.wait_timeout(Duration::from_millis(100)).unwrap().unwrap());

    mailer.push(Mail::new(id, KindId(7), vec![2], 1));
    owner.run_once();
    let counter = mailer.trace_handle().settlement_counter();
    let forged = activation_barrier(id, token, 2);
    mailer.record_sent(forged.mail_id, forged.root, None, id, id, forged.kind);
    mailer.push(forged);
    owner.run_once();
    assert!(registry.entry(id).is_none(), "forged same-token barrier cannot promote");
    assert_eq!(counter.live_roots(), 0, "forged barrier is consumed and balanced");
    // A later Starting mail in the same owner drain must join the prefix
    // even when readiness appeared first.
    mailer.push(activation_barrier(id, token, 1));
    mailer.push(Mail::new(id, KindId(7), vec![4], 1));
    owner.run_once();
    mailer.push(Mail::new(id, KindId(7), vec![3], 1));
    assert_eq!(*deliveries.lock().unwrap(), [1, 2, 4, 3]);

    let forged = activation_barrier(id, token, 5);
    mailer.record_sent(forged.mail_id, forged.root, None, id, id, forged.kind);
    assert_eq!(counter.live_roots(), 1, "synthetic control obligation is live before owner consumption");
    mailer.push(forged);
    owner.run_once();
    assert_eq!(counter.live_roots(), 0, "consuming a forged barrier balances its obligation exactly");
    assert_eq!(*deliveries.lock().unwrap(), [1, 2, 4, 3], "stale live barrier never reaches actor dispatch");

    let mut malformed = activation_barrier(id, token, 3);
    malformed.payload = MailRef::from(vec![0xFF]);
    mailer.record_sent(malformed.mail_id, malformed.root, None, id, id, malformed.kind);
    mailer.push(malformed);
    owner.run_once();
    assert_eq!(counter.live_roots(), 0, "malformed private control mail is consumed and balanced");
    assert_eq!(*deliveries.lock().unwrap(), [1, 2, 4, 3]);

    let unknown_id = MailboxId::from_name("unknown-activation-control");
    let unknown = activation_barrier(unknown_id, token, 1);
    mailer.record_sent(unknown.mail_id, unknown.root, None, unknown_id, unknown_id, unknown.kind);
    mailer.push(unknown);
    owner.run_once();
    assert_eq!(counter.live_roots(), 0, "unknown private control mail is consumed and balanced");
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
    let owner = RegistryOwnerLease::attach(
        auth(),
        &registry,
        &mailer,
        WakeSink::detached(),
        RegistryQueueCapacities::default(),
    );
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
    assert!(matches!(registry.drop_mailbox(&auth(), id), Err(DropError::UnknownId(found)) if found == id));
    assert!(!registry.remove_closure(&auth(), id), "ordinary removal does not treat Starting as live");
    let slot: Arc<dyn Drainable> = Arc::new(TestSlot);
    let handle = SeizeHandle::new(Arc::new(SlotState::new()), Arc::downgrade(&slot));
    assert!(!registry.install_seize_handle(&auth(), id, handle), "Starting rejects seize installation");
}

#[test]
fn starting_tokens_are_unique_stale_safe_and_transactional() {
    let registry = Arc::new(Registry::new());
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
    let owner = RegistryOwnerLease::attach(
        auth(),
        &registry,
        &mailer,
        WakeSink::detached(),
        RegistryQueueCapacities::default(),
    );
    registry.register_inbox(&auth(), "occupied", noop_handler());
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
fn starting_parks_fifo_and_owner_close_routes_every_accepted_mail_once() {
    let registry = Arc::new(Registry::new());
    let (outbound, outbound_rx) = HubOutbound::attached_loopback();
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)).with_outbound(outbound));
    let settlement = Arc::new(SettlementRegistry::new());
    mailer.trace_handle().install_settlement_registry(Arc::clone(&settlement));
    let relay = RouteRelayLease::attach(&mailer, WakeSink::detached(), RegistryQueueCapacities::default());
    let owner = RegistryOwnerLease::attach(
        auth(),
        &registry,
        &mailer,
        WakeSink::detached(),
        RegistryQueueCapacities::default(),
    );
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

/// ADR-0165 pumped activation, second ack: promoting a reservation publishes
/// the endpoint the caller thread wired and releases the mail that parked
/// behind the reservation, in the order the owner observed it.
///
/// Tripwire: the promote arm has to drain `pending_births[id].parked` itself.
/// Clearing the reservation and letting the generic staged-pending path
/// reclaim it would route those envelopes as unknown-recipient instead —
/// silently losing every mail addressed during the caller's `init` / `wire`
/// window, which is the whole reason the reservation exists.
#[test]
fn promoting_a_reservation_publishes_the_endpoint_and_releases_parked_mail() {
    let registry = Arc::new(Registry::new());
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
    let aborter: Arc<dyn FatalAborter> = Arc::new(PanicAborter);
    let pool = Pool::start(PoolConfig { workers: 2, ..PoolConfig::default() }, aborter);
    let _relay = RouteRelayLease::attach(&mailer, pool.wake_sink(), RegistryQueueCapacities::default());
    let _owner =
        RegistryOwnerLease::attach(auth(), &registry, &mailer, pool.wake_sink(), RegistryQueueCapacities::default());

    let kind = registry.register_kind(&auth(), "test.registry.promote_starting");
    let name = "test.registry.promote_starting.actor";
    let (id, token) = registry.reserve_starting_through_owner(name).expect("owner accepts the reservation");
    assert!(registry.entry(id).is_none(), "the reservation is not live while its caller wires");

    // Two envelopes arrive while the caller thread is still wiring.
    for value in [1_u32, 2] {
        mailer.push(Mail::new(id, kind, value.to_le_bytes().to_vec(), 1));
    }

    let seen: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
    let seen_for_handler = Arc::clone(&seen);
    let (delivered_tx, delivered_rx) = crossbeam_channel::bounded::<()>(2);
    let handler: Arc<dyn InboxHandler> = Arc::new(move |dispatch: OwnedDispatch| {
        let mut bytes = [0; 4];
        bytes.copy_from_slice(dispatch.payload.bytes());
        seen_for_handler.lock().expect("test capture mutex").push(u32::from_le_bytes(bytes));
        dispatch.discharge();
        let _ = delivered_tx.try_send(());
    });

    registry.promote_starting_through_owner(id, token, handler).expect("owner accepts the second ack");
    assert!(registry.entry(id).is_some(), "the second ack published the caller's endpoint as Live");
    for _ in 0..2 {
        delivered_rx.recv_timeout(Duration::from_secs(5)).expect("parked mail reaches the promoted endpoint");
    }

    assert_eq!(
        seen.lock().expect("test capture mutex").as_slice(),
        &[1, 2],
        "parked mail continues to the promoted endpoint in owner-observed order"
    );
}
