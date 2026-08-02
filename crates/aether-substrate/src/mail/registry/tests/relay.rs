//! Tests for [`super::super::relay`] — the continuations the owner
//! commits and the relay turn that delivers them.

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::chassis::settlement::SettlementRegistry;
use crate::config::RegistryQueueCapacities;
use crate::mail::mailer::Mailer;
use crate::mail::outbound::{EgressEvent, HubOutbound};
use crate::mail::registry::effect::{EffectBatch, RegistryApplied, RegistryEffect, StartingCancellation};
use crate::mail::registry::owner::RegistryOwnerLease;
use crate::mail::registry::relay::RouteRelayLease;
use crate::mail::registry::{MailDispatch, Registry};
use crate::mail::{KindId, Mail, MailboxId};
use crate::scheduler::{BatchBudget, WakeSink};
use crate::testing::boot_authority as auth;

use super::support::{starting_token, traced_unknown_mail};

#[test]
#[allow(clippy::disallowed_methods, reason = "the test deliberately races a scheduler drain with lease teardown")]
fn relay_running_prefix_owns_route_order_ahead_of_lease_close() {
    use std::sync::Barrier;

    let registry = Arc::new(Registry::new());
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
    let relay = RouteRelayLease::attach(&mailer, WakeSink::detached(), RegistryQueueCapacities::default());
    let drainable = relay.drainable_for_test();
    let order = Arc::new(Mutex::new(Vec::new()));
    let order_for_handler = Arc::clone(&order);
    let (entered_sender, entered_receiver) = crossbeam_channel::bounded(1);
    let (release_sender, release_receiver) = crossbeam_channel::bounded(1);
    let target = registry.register_inline(
        &auth(),
        "relay-close-order",
        Arc::new(move |dispatch: MailDispatch<'_>| {
            let value = dispatch.payload[0];
            if value == 1 {
                entered_sender.send(()).expect("ordering test waits for the first continuation");
                release_receiver.recv().expect("ordering test releases the running prefix");
            }
            order_for_handler.lock().unwrap().push(value);
        }),
    );
    mailer.relay_mail(Mail::new(target, KindId(1), vec![1], 1));
    let running = thread::spawn(move || drainable.run_cycle(BatchBudget::standard()));
    entered_receiver.recv_timeout(Duration::from_millis(100)).expect("first continuation starts routing");
    assert!(
        relay.route_serialization_held_for_test(),
        "a running drained prefix retains route serialization through handler dispatch"
    );

    mailer.relay_mail(Mail::new(target, KindId(1), vec![2], 1));
    drop(mailer);
    let close_barrier = Arc::new(Barrier::new(2));
    let close_barrier_for_thread = Arc::clone(&close_barrier);
    let closing = thread::spawn(move || {
        close_barrier_for_thread.wait();
        drop(relay);
    });
    close_barrier.wait();
    release_sender.send(()).unwrap();
    running.join().unwrap();
    closing.join().unwrap();

    assert_eq!(order.lock().unwrap().as_slice(), [1, 2], "lease close cannot overtake the running drained prefix");
}

/// Issue 4122: the relay never sheds. Everything it holds is a continuation
/// the owner already decided the fate of — losing one drops mail the registry
/// committed to delivering and strands its settlement chain — so its bound
/// records pressure rather than refusing work, and every continuation past it
/// still arrives, in order.
#[test]
fn relay_admits_owner_committed_continuations_past_capacity_in_order() {
    let registry = Arc::new(Registry::new());
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
    let relay = RouteRelayLease::attach(&mailer, WakeSink::detached(), RegistryQueueCapacities { owner: 64, relay: 1 });
    let delivered = Arc::new(Mutex::new(Vec::new()));
    let delivered_for_handler = Arc::clone(&delivered);
    let target = registry.register_inline(
        &auth(),
        "relay-past-capacity",
        Arc::new(move |dispatch: MailDispatch<'_>| {
            delivered_for_handler.lock().unwrap().push(dispatch.payload[0]);
        }),
    );
    for payload in 1u8..=3 {
        mailer.relay_mail(Mail::new(target, KindId(1), vec![payload], 1));
    }

    let metrics = mailer.route_relay_metrics().unwrap();
    assert_eq!(metrics.capacity, 1);
    assert_eq!(metrics.admitted, 3, "the relay admits every owner-committed continuation");
    assert_eq!(metrics.shed, 0, "the relay has no sheddable class");
    assert_eq!(metrics.over_capacity, 2, "the two continuations past the bound are counted");
    assert_eq!(metrics.depth_max, 3);

    relay.run_once();
    assert_eq!(delivered.lock().unwrap().as_slice(), [1, 2, 3], "over-capacity admission preserves route order");
    let metrics = mailer.route_relay_metrics().unwrap();
    assert_eq!((metrics.drained, metrics.drains, metrics.depth), (3, 1, 0));
}

#[test]
fn cancellation_holds_settlement_until_relay_terminal_delivery() {
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
