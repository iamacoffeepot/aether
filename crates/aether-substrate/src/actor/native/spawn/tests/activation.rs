//! Whole births driven through the ADR-0165 registry owner and the
//! activation barrier: where each lifecycle hook runs, what a rejection at
//! each stage leaves behind, and what the parent is told either way.

use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use aether_actor::Addressable;
use aether_data::{ActorId, Kind as _, Tag, fold_lineage, with_tag};

use crate::actor::native::binding::NativeBinding;
use crate::actor::native::spawn::activation::NativeSpawnFinalizer;
use crate::actor::native::spawn::reservation::ChildReservationKey;
use crate::actor::native::spawn::{SpawnError, SpawnOutcome, Subname};
use crate::config::RegistryQueueCapacities;
use crate::mail::registry::effect::{
    ActivationToken, EffectBatch, PreparedAliasRoute, RegistryApplied, RegistryBatch, RegistryEffect,
    RegistryEffectError,
};
use crate::mail::registry::{RegistryOwnerLease, RouteRelayLease, canonical_mailbox_id, noop_handler};
use crate::mail::{KindId, Mail, MailId, MailboxId, Source};
use crate::runtime::effect_chain::{EffectChain, Uncaused};
use crate::runtime::lifecycle::FatalAbortRecord;
use crate::scheduler::WakeSink;
use crate::testing::boot_authority;

use super::support::{
    ActivationClose, ActivationConfig, ActivationEvent, ActivationPoke, ActivationProbe, activation_fixture,
    activation_sink, await_spawn_done, finalized_probe, finalized_probe_with_hold, prepared_probe,
    prepared_probe_with_lifecycle_target,
};

#[test]
fn held_birth_waits_at_starting_until_exact_release_then_completes() {
    let (spawner, registry, mailer, pool) = activation_fixture();
    let (lifecycle_target, lifecycle_mail) = activation_sink(&registry, "test.activation.held-effects");
    let _relay = RouteRelayLease::attach(&mailer, pool.wake_sink(), RegistryQueueCapacities::default());
    let owner = RegistryOwnerLease::attach(
        boot_authority(),
        &registry,
        &mailer,
        WakeSink::detached(),
        RegistryQueueCapacities::default(),
    );
    let parent =
        Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), canonical_mailbox_id("test.activation.held-parent")));
    let (events_tx, events_rx) = crossbeam_channel::unbounded();
    let (ready_tx, ready_rx) = crossbeam_channel::bounded(1);
    let (commit, dispatch_id, _) =
        finalized_probe_with_hold(&spawner, &parent, "held", events_tx, 1, Some(ready_tx), Some(lifecycle_target));
    let id = commit.route.id;
    let birth = registry.submit(EffectBatch::new(vec![RegistryEffect::PreparedSpawn(commit)])).unwrap();
    owner.run_once();
    assert!(matches!(
        birth.wait_timeout(Duration::from_secs(1)).unwrap().unwrap().as_slice(),
        [RegistryApplied::Starting { .. }]
    ));
    let ready = ready_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(ready.mailbox_id, id);
    assert!(matches!(events_rx.recv_timeout(Duration::from_secs(1)).unwrap(), ActivationEvent::Wire(_)));
    assert!(registry.route_lookup(KindId(0), id).is_starting());
    assert!(registry.entry(id).is_none());
    assert!(registry.inventory().mailboxes.iter().all(|mailbox| mailbox.id != id));
    assert!(lifecycle_mail.try_recv().is_err(), "held wire egress remains buffered");
    assert!(parent.dispatch_take::<SpawnOutcome, ()>(dispatch_id).is_none());

    let wrong = ActivationToken::from_value(ready.token.value() + 1).unwrap();
    let rejected = registry.submit(RegistryBatch::release_held_starting(id, wrong).into_effects()).unwrap();
    owner.run_once();
    assert!(matches!(
        rejected.wait_timeout(Duration::from_secs(1)).unwrap(),
        Err(RegistryEffectError::ActivationRejected)
    ));

    let mixed = registry
        .submit(EffectBatch::new(vec![
            RegistryEffect::ReleaseHeldStarting { id, token: ready.token },
            RegistryEffect::ReleaseHeldStarting { id, token: wrong },
        ]))
        .unwrap();
    owner.run_once();
    assert!(matches!(
        mixed.wait_timeout(Duration::from_secs(1)).unwrap(),
        Err(RegistryEffectError::ActivationRejected)
    ));
    assert!(registry.route_lookup(KindId(0), id).is_starting());

    mailer.push(Mail::new(id, ActivationPoke::ID, ActivationPoke.encode_into_bytes(), 1));
    owner.run_once();
    assert!(events_rx.try_recv().is_err(), "held actor does not dispatch parked mail");
    let release = registry.submit(RegistryBatch::release_held_starting(id, ready.token).into_effects()).unwrap();
    owner.run_once();
    assert!(
        matches!(release.wait_timeout(Duration::from_secs(1)).unwrap().unwrap().as_slice(), [RegistryApplied::HeldReleased(released)] if *released == id)
    );
    let duplicate = registry.submit(RegistryBatch::release_held_starting(id, ready.token).into_effects()).unwrap();
    owner.run_once();
    assert!(matches!(
        duplicate.wait_timeout(Duration::from_secs(1)).unwrap(),
        Err(RegistryEffectError::ActivationRejected)
    ));
    let deadline = Instant::now() + Duration::from_secs(1);
    while registry.entry(id).is_none() {
        owner.run_once();
        assert!(Instant::now() < deadline, "released barrier promotes held actor");
        thread::yield_now();
    }
    let done = await_spawn_done(&parent, dispatch_id);
    assert!(matches!(done.output().result, Ok(())));
    done.release_no_reply();
    assert_eq!(lifecycle_mail.recv_timeout(Duration::from_secs(1)).unwrap(), ActivationPoke::ID);
    assert!(matches!(events_rx.recv_timeout(Duration::from_secs(1)).unwrap(), ActivationEvent::Dispatch(_)));

    spawner.shutdown_instanced(Duration::from_millis(1), Duration::from_secs(1), &FatalAbortRecord::new());
    drop(owner);
    assert!(pool.shutdown_with_results().into_iter().all(|result| result.is_ok()));
}

#[test]
fn held_cancellation_clears_starting_and_rejects_stale_token() {
    let (spawner, registry, mailer, pool) = activation_fixture();
    let (lifecycle_target, lifecycle_mail) = activation_sink(&registry, "test.activation.cancelled-effects");
    let _relay = RouteRelayLease::attach(&mailer, pool.wake_sink(), RegistryQueueCapacities::default());
    let owner = RegistryOwnerLease::attach(
        boot_authority(),
        &registry,
        &mailer,
        WakeSink::detached(),
        RegistryQueueCapacities::default(),
    );
    let parent = Arc::new(NativeBinding::new_for_test(
        Arc::clone(&mailer),
        canonical_mailbox_id("test.activation.held-cancel-parent"),
    ));
    let (events_tx, events_rx) = crossbeam_channel::unbounded();
    let (ready_tx, ready_rx) = crossbeam_channel::bounded(1);
    let (commit, dispatch_id, key) = finalized_probe_with_hold(
        &spawner,
        &parent,
        "held-cancel",
        events_tx,
        1,
        Some(ready_tx),
        Some(lifecycle_target),
    );
    let id = commit.route.id;
    let birth = registry.submit(EffectBatch::new(vec![RegistryEffect::PreparedSpawn(commit)])).unwrap();
    owner.run_once();
    birth.wait_timeout(Duration::from_secs(1)).unwrap().unwrap();
    let ready = ready_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(matches!(events_rx.recv_timeout(Duration::from_secs(1)).unwrap(), ActivationEvent::Wire(_)));

    let alias_name = "test.activation.probe:held-cancel/aether.embedded:child";
    let alias = MailboxId(with_tag(Tag::Mailbox, fold_lineage(id.0, ActorId::instanced("aether.embedded", "child"))));
    let aliases = registry
        .submit(RegistryBatch::publish_aliases(vec![PreparedAliasRoute::new(alias, alias_name, id)]).into_effects())
        .unwrap();
    owner.run_once();
    assert!(aliases.wait_timeout(Duration::from_secs(1)).unwrap().is_ok());

    let stale = ActivationToken::from_value(ready.token.value() + 1).unwrap();
    let rejected = registry.submit(RegistryBatch::cancel_held_starting(id, stale, vec![alias]).into_effects()).unwrap();
    owner.run_once();
    assert!(matches!(
        rejected.wait_timeout(Duration::from_secs(1)).unwrap(),
        Err(RegistryEffectError::ActivationRejected)
    ));
    assert!(registry.route_lookup(KindId(0), id).is_starting());
    assert!(registry.route_lookup(KindId(0), alias).is_starting());

    let missing = canonical_mailbox_id("test.activation.missing-alias");
    let invalid_aliases = registry
        .submit(RegistryBatch::cancel_held_starting(id, ready.token, vec![alias, missing]).into_effects())
        .unwrap();
    owner.run_once();
    assert!(matches!(
        invalid_aliases.wait_timeout(Duration::from_secs(1)).unwrap(),
        Err(RegistryEffectError::ActivationRejected)
    ));
    assert!(registry.route_lookup(KindId(0), alias).is_starting());

    let cancel =
        registry.submit(RegistryBatch::cancel_held_starting(id, ready.token, vec![alias]).into_effects()).unwrap();
    owner.run_once();
    assert!(cancel.wait_timeout(Duration::from_secs(1)).unwrap().is_ok());
    assert_eq!(registry.lookup(alias_name), None, "aborted alias leaves no Dropped tombstone");
    let deadline = Instant::now() + Duration::from_secs(1);
    while registry.route_lookup(KindId(0), id).is_starting() {
        owner.run_once();
        assert!(Instant::now() < deadline, "home cancellation removes exact Starting route");
        thread::yield_now();
    }
    let done = await_spawn_done(&parent, dispatch_id);
    assert!(matches!(done.output().result, Err(SpawnError::ActivationRejected)));
    done.release_no_reply();
    assert!(matches!(events_rx.recv_timeout(Duration::from_secs(1)).unwrap(), ActivationEvent::Unwire(_)));
    assert!(matches!(events_rx.recv_timeout(Duration::from_secs(1)).unwrap(), ActivationEvent::Drop(_)));
    assert!(lifecycle_mail.try_recv().is_err(), "cancel discards wire and unwire egress");
    drop(parent.reserve_child(key).expect("cancelled child releases its parent reservation"));

    drop(owner);
    assert!(pool.shutdown_with_results().into_iter().all(|result| result.is_ok()));
}

#[test]
fn held_cancellation_before_wire_closes_ready_without_running_hooks() {
    let (spawner, _registry, _mailer, pool) = activation_fixture();
    let (events_tx, events_rx) = crossbeam_channel::unbounded();
    let (ready_tx, ready_rx) = crossbeam_channel::bounded(1);
    let identity = spawner.preflight::<ActivationProbe>(Subname::Named("held-before-wire"), None).unwrap();
    let staged = spawner.build::<ActivationProbe>(identity, ActivationConfig::new(events_tx), (), Vec::new()).unwrap();
    let mut commit =
        spawner.prepare_commit_with_hold(staged, None, EffectChain::Uncaused(Uncaused::EmbedderCall), Some(ready_tx));
    let token = ActivationToken::from_value(1).unwrap();
    let activation = commit.take_activation().reserve(token).unwrap_or_else(|_| panic!("reservation accepted"));
    activation.cancel_and_join();
    assert!(ready_rx.recv_timeout(Duration::from_secs(1)).is_err());
    assert!(matches!(events_rx.recv_timeout(Duration::from_secs(1)).unwrap(), ActivationEvent::Drop(_)));
    assert!(events_rx.try_recv().is_err(), "cancelled birth never wires or unwires");
    assert!(pool.shutdown_with_results().into_iter().all(|result| result.is_ok()));
}

#[test]
fn prepared_activation_lifecycle_stays_on_scheduler_home() {
    let (spawner, _registry, _mailer, pool) = activation_fixture();
    let caller = thread::current().id();

    let (discard_tx, discard_rx) = crossbeam_channel::unbounded();
    prepared_probe(&spawner, "discard", discard_tx).discard_at_home().recv_timeout(Duration::from_secs(1)).unwrap();
    let discarded = discard_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(matches!(discarded, ActivationEvent::Drop(home) if home != caller));
    assert!(discard_rx.try_recv().is_err(), "unwired lifecycle never ran for an unwired discard");

    let (cancel_tx, cancel_rx) = crossbeam_channel::unbounded();
    let mut commit = prepared_probe(&spawner, "cancel", cancel_tx);
    let token = ActivationToken::from_value(1).unwrap();
    let activation = commit.take_activation().reserve(token).unwrap_or_else(|_| panic!("reservation accepted"));
    activation.schedule();
    let ActivationEvent::Wire(home) = cancel_rx.recv_timeout(Duration::from_secs(1)).unwrap() else {
        panic!("wire runs first")
    };
    activation.cancel_and_join();
    assert_eq!(cancel_rx.recv_timeout(Duration::from_secs(1)).unwrap(), ActivationEvent::Unwire(home));
    assert_eq!(cancel_rx.recv_timeout(Duration::from_secs(1)).unwrap(), ActivationEvent::Drop(home));
    assert!(cancel_rx.try_recv().is_err(), "post-wire cancellation unwires exactly once");

    assert!(pool.shutdown_with_results().into_iter().all(|result| result.is_ok()));
}

#[test]
fn owner_close_before_apply_rejects_native_finalizer_at_home_and_releases_parent_key() {
    let (spawner, registry, mailer, pool) = activation_fixture();
    let owner = RegistryOwnerLease::attach(
        boot_authority(),
        &registry,
        &mailer,
        WakeSink::detached(),
        RegistryQueueCapacities::default(),
    );
    let caller = thread::current().id();
    let parent_id = canonical_mailbox_id("test.activation.owner-close-parent");
    let parent = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), parent_id));
    let key = ChildReservationKey::new(
        parent_id,
        ActorId::singleton(ActivationProbe::NAMESPACE),
        ActorId::instanced(ActivationProbe::NAMESPACE, "owner-close-before-apply"),
    );
    let parent_reservation = parent.reserve_child(key).expect("first staged parent key reservation wins");
    let (events_tx, events_rx) = crossbeam_channel::unbounded();
    let identity =
        spawner.prepare_identity::<ActivationProbe>(Subname::Named("owner-close-before-apply"), None).unwrap();
    let staged = spawner.build::<ActivationProbe>(identity, ActivationConfig::new(events_tx), (), Vec::new()).unwrap();
    let causing_chain = MailId::new(parent_id, 1);
    let deferred =
        parent.dispatch_arm::<SpawnOutcome, _>(mailer.acquire_settlement_hold(causing_chain), Source::NONE, ());
    let dispatch_id = deferred.dispatch_id();
    let finalizer = NativeSpawnFinalizer::parented(
        parent_reservation,
        deferred,
        staged.identity.id,
        Arc::clone(&staged.identity.canonical_name),
        Arc::downgrade(&staged.transport),
        Arc::clone(&mailer),
    );
    let commit = spawner.prepare_commit(staged, Some(finalizer), EffectChain::Held(causing_chain));
    let child_id = commit.route.id;
    let completion = registry.submit(EffectBatch::new(vec![RegistryEffect::PreparedSpawn(commit)])).unwrap();

    drop(owner);

    assert!(matches!(completion.wait_timeout(Duration::from_secs(1)).unwrap(), Err(RegistryEffectError::OwnerClosed)));
    let dropped = events_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(matches!(dropped, ActivationEvent::Drop(home) if home != caller));
    assert!(events_rx.try_recv().is_err(), "pre-apply rejection drops without running unwire");
    assert!(registry.entry(child_id).is_none(), "owner-close rejection publishes no route");

    let done = parent
        .dispatch_take::<SpawnOutcome, ()>(dispatch_id)
        .expect("owner-close finalization fills the typed deferred result");
    assert_eq!(done.output().mailbox_id, child_id, "a rejection still names the birth it belongs to");
    assert!(matches!(done.output().result, Err(SpawnError::OwnerClosed)));
    done.release_no_reply();
    drop(parent.reserve_child(key).expect("owner-close rejection releases the staged parent key"));

    assert!(pool.shutdown_with_results().into_iter().all(|result| result.is_ok()));
}

#[test]
fn rejected_multi_birth_batch_marks_unvisited_native_finalizer_as_activation_rejected() {
    let (spawner, registry, mailer, pool) = activation_fixture();
    let _relay = RouteRelayLease::attach(&mailer, pool.wake_sink(), RegistryQueueCapacities::default());
    let owner = RegistryOwnerLease::attach(
        boot_authority(),
        &registry,
        &mailer,
        WakeSink::detached(),
        RegistryQueueCapacities::default(),
    );
    let caller = thread::current().id();
    let parent = Arc::new(NativeBinding::new_for_test(
        Arc::clone(&mailer),
        canonical_mailbox_id("test.activation.rejected-batch-parent"),
    ));
    let (first_tx, first_rx) = crossbeam_channel::unbounded();
    let (middle_tx, middle_rx) = crossbeam_channel::unbounded();
    let (later_tx, later_rx) = crossbeam_channel::unbounded();
    let (first, first_dispatch, first_key) = finalized_probe(&spawner, &parent, "batch-first", first_tx, 1);
    let (middle, middle_dispatch, middle_key) = finalized_probe(&spawner, &parent, "batch-middle", middle_tx, 2);
    let (later, later_dispatch, later_key) = finalized_probe(&spawner, &parent, "batch-later", later_tx, 3);
    let first_id = first.route.id;
    let middle_id = middle.route.id;
    let later_id = later.route.id;
    registry
        .try_register_inbox_with_id(&boot_authority(), middle_id, middle.route.canonical_name.clone(), noop_handler())
        .unwrap();
    let completion = registry
        .submit(EffectBatch::new(vec![
            RegistryEffect::PreparedSpawn(first),
            RegistryEffect::PreparedSpawn(middle),
            RegistryEffect::PreparedSpawn(later),
        ]))
        .unwrap();

    owner.run_once();

    assert!(matches!(completion.wait_timeout(Duration::from_secs(1)).unwrap(), Err(RegistryEffectError::Name(_))));
    for events in [&first_rx, &middle_rx, &later_rx] {
        let dropped = events.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(matches!(dropped, ActivationEvent::Drop(home) if home != caller));
        assert!(events.try_recv().is_err(), "rejected pre-wire state drops without unwire");
    }
    assert!(registry.entry(first_id).is_none());
    assert!(registry.entry(middle_id).is_some(), "the pre-existing middle conflict remains unchanged");
    assert!(registry.entry(later_id).is_none());

    let first_done = await_spawn_done(&parent, first_dispatch);
    assert_eq!(first_done.output().mailbox_id, first_id, "each rejection names its own birth");
    assert!(matches!(first_done.output().result, Err(SpawnError::ActivationRejected)));
    first_done.release_no_reply();
    let middle_done = await_spawn_done(&parent, middle_dispatch);
    assert_eq!(middle_done.output().mailbox_id, middle_id);
    assert!(matches!(middle_done.output().result, Err(SpawnError::SubnameInUse { .. })));
    middle_done.release_no_reply();
    let later_done = await_spawn_done(&parent, later_dispatch);
    assert_eq!(later_done.output().mailbox_id, later_id);
    assert!(matches!(later_done.output().result, Err(SpawnError::ActivationRejected)));
    later_done.release_no_reply();
    for key in [first_key, middle_key, later_key] {
        drop(parent.reserve_child(key).expect("transactional rejection releases every parent key"));
    }

    drop(owner);
    assert!(pool.shutdown_with_results().into_iter().all(|result| result.is_ok()));
}

#[test]
fn successful_prepared_activation_enters_ordinary_dispatch_once() {
    let (spawner, registry, mailer, pool) = activation_fixture();
    let (lifecycle_target, lifecycle_mail) = activation_sink(&registry, "test.activation.live-effects");
    let owner = RegistryOwnerLease::attach(
        boot_authority(),
        &registry,
        &mailer,
        WakeSink::detached(),
        RegistryQueueCapacities::default(),
    );
    let (events_tx, events_rx) = crossbeam_channel::unbounded();
    let commit = prepared_probe_with_lifecycle_target(&spawner, "live", events_tx, lifecycle_target);
    let id = commit.route.id;
    let completion = registry.submit(EffectBatch::new(vec![RegistryEffect::PreparedSpawn(commit)])).unwrap();
    owner.apply_once_then_observe_before_next_apply_for_test(|| {
        assert!(lifecycle_mail.try_recv().is_err(), "wire effects remain quarantined while the route is Starting");
        assert!(registry.entry(id).is_none(), "the owner has not yet promoted the Starting route");
    });
    let _ = completion.wait_timeout(Duration::from_secs(1)).unwrap().unwrap();
    let ActivationEvent::Wire(home) = events_rx.recv_timeout(Duration::from_secs(1)).unwrap() else {
        panic!("wire runs before live dispatch")
    };
    assert!(registry.entry(id).is_some(), "barrier promotes the actor to Live");
    assert_eq!(
        lifecycle_mail.recv_timeout(Duration::from_secs(1)).unwrap(),
        ActivationPoke::ID,
        "the owner's post-publication suffix releases wire effects"
    );

    mailer.push(Mail::new(id, ActivationPoke::ID, ActivationPoke.encode_into_bytes(), 1));
    assert_eq!(events_rx.recv_timeout(Duration::from_secs(1)).unwrap(), ActivationEvent::Dispatch(home));
    assert!(events_rx.try_recv().is_err(), "one live mail performs one ordinary dispatcher drain");

    spawner.shutdown_instanced(Duration::from_millis(1), Duration::from_secs(1), &FatalAbortRecord::new());
    drop(owner);
    assert!(pool.shutdown_with_results().into_iter().all(|result| result.is_ok()));
}

/// Re-staging a self-closed child's subname reports the authoritative
/// `SubnameRetired`, and the parent's key comes back at the child's own
/// close path rather than at chassis teardown. Issue 4152's two
/// independent regressions, in the order a caller meets them: the live
/// key rode the child's binding inside `Spawner::instanced_slots`,
/// which only `shutdown_instanced` ever empties, so the re-stage was
/// rejected locally as `SubnameInUse` and one table entry leaked per
/// dead child; and the owner then rejected the birth on its surviving
/// route — also as `SubnameInUse` — before
/// [`LegacyPreparedActivation::reserve`](crate::actor::native::spawn::activation::LegacyPreparedActivation::reserve) could
/// report the retirement. Either one alone turns the retired-name
/// diagnostic (ADR-0165) into a "name in use" lie.
#[test]
fn closed_child_subname_restages_as_retired_not_in_use() {
    let (spawner, registry, mailer, pool) = activation_fixture();
    let _relay = RouteRelayLease::attach(&mailer, pool.wake_sink(), RegistryQueueCapacities::default());
    let owner = RegistryOwnerLease::attach(
        boot_authority(),
        &registry,
        &mailer,
        WakeSink::detached(),
        RegistryQueueCapacities::default(),
    );
    let parent = Arc::new(NativeBinding::new_for_test(
        Arc::clone(&mailer),
        canonical_mailbox_id("test.activation.self-close-parent"),
    ));
    let (events_tx, _events_rx) = crossbeam_channel::unbounded();
    let (commit, dispatch_id, key) = finalized_probe(&spawner, &parent, "self-close", events_tx, 1);
    let child_id = commit.route.id;
    let completion = registry.submit(EffectBatch::new(vec![RegistryEffect::PreparedSpawn(commit)])).unwrap();

    owner.apply_once_then_observe_before_next_apply_for_test(|| {
        assert!(parent.reserve_child(key).is_none(), "the staged key stays held while the child is Starting");
    });
    completion.wait_timeout(Duration::from_secs(1)).unwrap().unwrap();

    let done = await_spawn_done(&parent, dispatch_id);
    assert!(matches!(done.output(), SpawnOutcome { mailbox_id, result: Ok(()), .. } if *mailbox_id == child_id));
    done.release_no_reply();
    assert!(parent.reserve_child(key).is_none(), "Live promotion carries the same key into the live-child set");

    mailer.push(Mail::new(child_id, ActivationClose::ID, ActivationClose.encode_into_bytes(), 1));

    let deadline = Instant::now() + Duration::from_secs(5);
    let restaged = loop {
        if let Some(restaged) = parent.reserve_child(key) {
            break restaged;
        }
        assert!(Instant::now() < deadline, "the closed child's close path released its parent-local key");
        thread::yield_now();
    };
    assert!(
        spawner.instanced_slots.lock().expect("instanced_slots mutex poisoned").contains_key(&child_id),
        "the key came back from the actor's close path — its slot is still parked for chassis teardown"
    );
    drop(restaged);

    let (events_tx, _events_rx) = crossbeam_channel::unbounded();
    let (reborn, reborn_dispatch, _) = finalized_probe(&spawner, &parent, "self-close", events_tx, 2);
    let rejection = registry.submit(EffectBatch::new(vec![RegistryEffect::PreparedSpawn(reborn)])).unwrap();
    owner.run_once();

    assert!(matches!(rejection.wait_timeout(Duration::from_secs(1)).unwrap(), Err(RegistryEffectError::Name(_))));
    let reborn_done = await_spawn_done(&parent, reborn_dispatch);
    assert!(
        matches!(reborn_done.output().result, Err(SpawnError::SubnameRetired { .. })),
        "the owner classified the surviving route of a retired id, not a live occupant: {:?}",
        reborn_done.output()
    );
    reborn_done.release_no_reply();

    spawner.shutdown_instanced(Duration::from_millis(1), Duration::from_secs(1), &FatalAbortRecord::new());
    drop(owner);
    assert!(pool.shutdown_with_results().into_iter().all(|result| result.is_ok()));
}

#[test]
fn owner_close_after_wire_cleans_starting_activation_at_home() {
    let (spawner, registry, mailer, pool) = activation_fixture();
    let _relay = RouteRelayLease::attach(&mailer, pool.wake_sink(), RegistryQueueCapacities::default());
    let (lifecycle_target, lifecycle_mail) = activation_sink(&registry, "test.activation.cancelled-effects");
    let owner = RegistryOwnerLease::attach(
        boot_authority(),
        &registry,
        &mailer,
        WakeSink::detached(),
        RegistryQueueCapacities::default(),
    );
    let (events_tx, events_rx) = crossbeam_channel::unbounded();
    let commit = prepared_probe_with_lifecycle_target(&spawner, "owner-close", events_tx, lifecycle_target);
    let id = commit.route.id;
    let canonical_name = commit.route.canonical_name.clone();
    let completion = registry.submit(EffectBatch::new(vec![RegistryEffect::PreparedSpawn(commit)])).unwrap();
    owner.apply_once_then_close_after_next_command();
    let applied = completion.wait_timeout(Duration::from_secs(1)).unwrap().unwrap();
    let [RegistryApplied::Starting { token, .. }] = applied.as_slice() else {
        panic!("prepared birth publishes Starting")
    };
    let token = *token;
    let ActivationEvent::Wire(home) = events_rx.recv_timeout(Duration::from_secs(1)).unwrap() else {
        panic!("activation wires before owner closure")
    };
    drop(owner);

    assert_eq!(events_rx.recv_timeout(Duration::from_secs(1)).unwrap(), ActivationEvent::Unwire(home));
    assert_eq!(events_rx.recv_timeout(Duration::from_secs(1)).unwrap(), ActivationEvent::Drop(home));
    assert!(events_rx.try_recv().is_err(), "owner closure unwires exactly once");
    assert!(
        lifecycle_mail.try_recv().is_err(),
        "neither wire nor rejection-time unwire effects escape a never-Live actor"
    );
    assert!(registry.lookup(&canonical_name).is_none(), "Starting route is rolled back without Live publication");
    assert!(registry.entry(id).is_none());
    assert!(mailer.cost_table().cells_for(id).is_empty(), "token-owned cost rows are rolled back");
    let fresh = ActivationToken::from_value(token.value() + 1).unwrap();
    assert!(spawner.actor_registry().reserve_starting(id, fresh), "actor lifecycle reservation was removed");
    spawner.actor_registry().rollback_starting(id, fresh);

    assert!(pool.shutdown_with_results().into_iter().all(|result| result.is_ok()));
}
