//! Whole births driven through the ADR-0165 registry owner and the
//! activation barrier: where each lifecycle hook runs, what a rejection at
//! each stage leaves behind, and what the parent is told either way.

use std::sync::Arc;
use std::thread;

use aether_actor::Addressable;
use aether_data::{ActorId, Kind as _, RequestId};

use crate::actor::native::spawn::activation::NativeSpawnFinalizer;
use crate::actor::native::spawn::reservation::ChildReservationKey;
use crate::actor::native::spawn::{SpawnError, SpawnOutcome, Subname};
use crate::chassis::builder::TeardownGate;
use crate::chassis::frame_loop;
use crate::config::{RegistryQueueCapacities, SettlementConfig};
use crate::mail::registry::effect::{
    ActivationToken, EffectBatch, RegistryApplied, RegistryEffect, RegistryEffectError,
};
use crate::mail::registry::{NativeHoldRefusal, RegistryOwnerLease, RouteRelayLease, noop_handler};
use crate::mail::{Mail, MailId};
use crate::runtime::effect_chain::EffectChain;
use crate::runtime::lifecycle::{FatalAbortRecord, PanicAborter};
use crate::scheduler::WakeSink;
use crate::testing::{await_event, await_signal, boot_authority};

use super::support::{
    ActivationClose, ActivationConfig, ActivationEvent, ActivationPoke, ActivationProbe, SharedFirst, SharedSecond,
    activation_fixture, activation_parent, activation_sink, await_spawn_done, await_spawn_outcome, finalized_probe,
    finalized_shared, prepared_probe, prepared_probe_with_lifecycle_target,
};

#[test]
fn prepared_activation_lifecycle_stays_on_scheduler_home() {
    let (spawner, _registry, _mailer, pool) = activation_fixture();
    let caller = thread::current().id();

    let (discard_tx, discard_rx) = crossbeam_channel::unbounded();
    await_signal(&prepared_probe(&spawner, "discard", discard_tx).discard_at_home(), "test.activation.discard_done");
    let discarded = await_event(&discard_rx, "test.activation.discard_drop");
    assert!(matches!(discarded, ActivationEvent::Drop(home) if home != caller));
    assert!(discard_rx.try_recv().is_err(), "unwired lifecycle never ran for an unwired discard");

    let (cancel_tx, cancel_rx) = crossbeam_channel::unbounded();
    let mut commit = prepared_probe(&spawner, "cancel", cancel_tx);
    let token = ActivationToken::from_value(1).unwrap();
    let activation = commit.take_activation().reserve(token).unwrap_or_else(|_| panic!("reservation accepted"));
    activation.schedule();
    let ActivationEvent::Wire(home) = await_event(&cancel_rx, "test.activation.cancel_wire") else {
        panic!("wire runs first")
    };
    activation.cancel_and_join();
    assert_eq!(await_event(&cancel_rx, "test.activation.cancel_unwire"), ActivationEvent::Unwire(home));
    assert_eq!(await_event(&cancel_rx, "test.activation.cancel_drop"), ActivationEvent::Drop(home));
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
    let (parent, wakes) = activation_parent(&registry, &mailer, "test.activation.owner-close-parent");
    let parent_id = parent.self_mailbox();
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
    let deferred = parent.dispatch_stage::<SpawnOutcome<ActivationProbe>>(
        Some(mailer.acquire_settlement_hold(causing_chain)),
        RequestId(parent.mint_correlation()),
    );
    let dispatch_id = deferred.dispatch_id();
    let finalizer = NativeSpawnFinalizer::<ActivationProbe>::parented(
        parent_reservation,
        deferred,
        staged.identity.id,
        staged.identity.canonical_name.clone(),
        Arc::downgrade(&staged.transport),
    );
    let commit = spawner.prepare_commit(staged, Some(finalizer), EffectChain::Held(causing_chain));
    let child_id = commit.id;
    let child_name = commit.canonical_name.clone();
    let completion = registry.submit(EffectBatch::new(vec![RegistryEffect::PreparedSpawn(commit)])).unwrap();

    drop(owner);

    assert!(matches!(completion.wait(), Err(RegistryEffectError::OwnerClosed)));
    let dropped = await_event(&events_rx, "test.activation.owner_close_before_apply_drop");
    assert!(matches!(dropped, ActivationEvent::Drop(home) if home != caller));
    assert!(events_rx.try_recv().is_err(), "pre-apply rejection drops without running unwire");
    assert!(registry.entry_at(child_id).is_none(), "owner-close rejection publishes no route");

    let done = await_spawn_done(&parent, &wakes, dispatch_id);
    assert_eq!(done.output().canonical_name, child_name, "a rejection still names the birth it belongs to");
    assert!(matches!(done.output().result, Err(SpawnError::OwnerClosed)));
    drop(done);
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
    let (parent, wakes) = activation_parent(&registry, &mailer, "test.activation.rejected-batch-parent");
    let (first_tx, first_rx) = crossbeam_channel::unbounded();
    let (middle_tx, middle_rx) = crossbeam_channel::unbounded();
    let (later_tx, later_rx) = crossbeam_channel::unbounded();
    let (first, first_dispatch, first_key) = finalized_probe(&spawner, &parent, "batch-first", first_tx, 1);
    let (middle, middle_dispatch, middle_key) = finalized_probe(&spawner, &parent, "batch-middle", middle_tx, 2);
    let (later, later_dispatch, later_key) = finalized_probe(&spawner, &parent, "batch-later", later_tx, 3);
    let first_id = first.id;
    let middle_id = middle.id;
    let later_id = later.id;
    let first_name = first.canonical_name.clone();
    let middle_name = middle.canonical_name.clone();
    let later_name = later.canonical_name.clone();
    registry
        .try_register_inbox_with_id(&boot_authority(), middle_id, middle.canonical_name.to_string(), noop_handler())
        .unwrap();
    let completion = registry
        .submit(EffectBatch::new(vec![
            RegistryEffect::PreparedSpawn(first),
            RegistryEffect::PreparedSpawn(middle),
            RegistryEffect::PreparedSpawn(later),
        ]))
        .unwrap();

    owner.run_once();

    assert!(matches!(completion.wait(), Err(RegistryEffectError::Name(_))));
    for events in [&first_rx, &middle_rx, &later_rx] {
        let dropped = await_event(events, "test.activation.rejected_batch_drop");
        assert!(matches!(dropped, ActivationEvent::Drop(home) if home != caller));
        assert!(events.try_recv().is_err(), "rejected pre-wire state drops without unwire");
    }
    assert!(registry.entry_at(first_id).is_none());
    assert!(registry.entry_at(middle_id).is_some(), "the pre-existing middle conflict remains unchanged");
    assert!(registry.entry_at(later_id).is_none());

    let first_done = await_spawn_done(&parent, &wakes, first_dispatch);
    assert_eq!(first_done.output().canonical_name, first_name, "each rejection names its own birth");
    assert!(matches!(first_done.output().result, Err(SpawnError::ActivationRejected)));
    drop(first_done);
    let middle_done = await_spawn_done(&parent, &wakes, middle_dispatch);
    assert_eq!(middle_done.output().canonical_name, middle_name);
    assert!(matches!(middle_done.output().result, Err(SpawnError::SubnameInUse { .. })));
    drop(middle_done);
    let later_done = await_spawn_done(&parent, &wakes, later_dispatch);
    assert_eq!(later_done.output().canonical_name, later_name);
    assert!(matches!(later_done.output().result, Err(SpawnError::ActivationRejected)));
    drop(later_done);
    for key in [first_key, middle_key, later_key] {
        drop(parent.reserve_child(key).expect("transactional rejection releases every parent key"));
    }

    drop(owner);
    assert!(pool.shutdown_with_results().into_iter().all(|result| result.is_ok()));
}

// Catches: a refused batch leaving the native namespace hold it took in the
// committed publication table, so a type that was never born blocks every
// other type sharing its namespace; and a fix that stops installing the hold
// when a batch commits, so a second type is admitted beside the first.
#[test]
fn a_refused_batch_leaves_its_native_namespace_unheld() {
    let (spawner, registry, mailer, pool) = activation_fixture();
    let _relay = RouteRelayLease::attach(&mailer, pool.wake_sink(), RegistryQueueCapacities::default());
    let owner = RegistryOwnerLease::attach(
        boot_authority(),
        &registry,
        &mailer,
        WakeSink::detached(),
        RegistryQueueCapacities::default(),
    );
    let (parent, wakes) = activation_parent(&registry, &mailer, "test.activation.shared-parent");

    let (held, held_dispatch) = finalized_shared::<SharedFirst>(&spawner, &parent, "held", 1);
    let (occupied, occupied_dispatch) = finalized_shared::<SharedFirst>(&spawner, &parent, "occupied", 2);
    registry
        .try_register_inbox_with_id(&boot_authority(), occupied.id, occupied.canonical_name.to_string(), noop_handler())
        .unwrap();
    let refused = registry
        .submit(EffectBatch::new(vec![RegistryEffect::PreparedSpawn(held), RegistryEffect::PreparedSpawn(occupied)]))
        .unwrap();
    owner.run_once();
    assert!(matches!(refused.wait(), Err(RegistryEffectError::Name(_))));
    drop(await_spawn_outcome::<SharedFirst>(&parent, &wakes, held_dispatch));
    drop(await_spawn_outcome::<SharedFirst>(&parent, &wakes, occupied_dispatch));

    let (second, second_dispatch) = finalized_shared::<SharedSecond>(&spawner, &parent, "second", 3);
    let admitted = registry.submit(EffectBatch::new(vec![RegistryEffect::PreparedSpawn(second)])).unwrap();
    owner.run_once();
    let applied = admitted.wait().expect("the refused batch left the namespace unheld for the second type");
    assert!(matches!(applied.as_slice(), [RegistryApplied::Starting { .. }]));
    let second_done = await_spawn_outcome::<SharedSecond>(&parent, &wakes, second_dispatch);
    assert!(second_done.output().result.is_ok());
    drop(second_done);

    let (third, third_dispatch) = finalized_shared::<SharedFirst>(&spawner, &parent, "third", 4);
    let held_by_other = registry.submit(EffectBatch::new(vec![RegistryEffect::PreparedSpawn(third)])).unwrap();
    owner.run_once();
    assert!(matches!(held_by_other.wait(), Err(RegistryEffectError::ActivationRejected)));
    let third_done = await_spawn_outcome::<SharedFirst>(&parent, &wakes, third_dispatch);
    assert!(matches!(third_done.output().result, Err(SpawnError::NativeHold(NativeHoldRefusal::HeldByOther { .. }))));
    drop(third_done);

    spawner.shutdown_instanced(&TeardownGate {
        round_budget: frame_loop::DRAIN_BUDGET,
        cumulative_cap: SettlementConfig::from_env().to_cap(),
        abort_record: &FatalAbortRecord::new(),
        aborter: &PanicAborter,
    });
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
    let id = commit.id;
    let completion = registry.submit(EffectBatch::new(vec![RegistryEffect::PreparedSpawn(commit)])).unwrap();
    owner.apply_once_then_observe_before_next_apply_for_test(|| {
        assert!(lifecycle_mail.try_recv().is_err(), "wire effects remain quarantined while the route is Starting");
        assert!(registry.entry_at(id).is_none(), "the owner has not yet promoted the Starting route");
    });
    let _ = completion.wait().unwrap();
    let ActivationEvent::Wire(home) = await_event(&events_rx, "test.activation.live_wire") else {
        panic!("wire runs before live dispatch")
    };
    assert!(registry.entry_at(id).is_some(), "barrier promotes the actor to Live");
    assert_eq!(
        await_event(&lifecycle_mail, "test.activation.live_wire_poke"),
        ActivationPoke::ID,
        "the owner's post-publication suffix releases wire effects"
    );

    mailer.push(Mail::new(id, ActivationPoke::ID, ActivationPoke.encode_into_bytes(), 1));
    assert_eq!(await_event(&events_rx, "test.activation.live_dispatch"), ActivationEvent::Dispatch(home));
    assert!(events_rx.try_recv().is_err(), "one live mail performs one ordinary dispatcher drain");

    spawner.shutdown_instanced(&TeardownGate {
        round_budget: frame_loop::DRAIN_BUDGET,
        cumulative_cap: SettlementConfig::from_env().to_cap(),
        abort_record: &FatalAbortRecord::new(),
        aborter: &PanicAborter,
    });
    drop(owner);
    assert!(pool.shutdown_with_results().into_iter().all(|result| result.is_ok()));
}

/// Re-staging a self-closed child's subname reports the authoritative
/// `SubnameRetired`, and the parent's key comes back at the child's own
/// close path rather than at chassis teardown. Issue 4152's two
/// independent regressions, in the order a caller meets them: the live
/// key rode the child's binding, which at the time stayed in
/// `Spawner::instanced_slots` until chassis teardown, so the re-stage was
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
    let (parent, wakes) = activation_parent(&registry, &mailer, "test.activation.self-close-parent");
    let (events_tx, _events_rx) = crossbeam_channel::unbounded();
    let (commit, dispatch_id, key) = finalized_probe(&spawner, &parent, "self-close", events_tx, 1);
    let child_id = commit.id;
    let completion = registry.submit(EffectBatch::new(vec![RegistryEffect::PreparedSpawn(commit)])).unwrap();

    owner.apply_once_then_observe_before_next_apply_for_test(|| {
        assert!(parent.reserve_child(key).is_none(), "the staged key stays held while the child is Starting");
    });
    completion.wait().unwrap();

    let done = await_spawn_done(&parent, &wakes, dispatch_id);
    assert!(matches!(
        done.output(),
        SpawnOutcome { result: Ok(child), .. } if child.id() == child_id
    ));
    drop(done);
    assert!(parent.reserve_child(key).is_none(), "Live promotion carries the same key into the live-child set");

    // Close-done fires after the close sequence that releases the key, so
    // one read of the key after it sees the release.
    let (closed_tx, closed_rx) = crossbeam_channel::bounded(1);
    spawner
        .instanced_slots
        .lock()
        .expect("instanced_slots mutex poisoned")
        .get(&child_id)
        .expect("a live pooled actor's slot is retained")
        .slot
        .set_close_done_tx(closed_tx);

    mailer.push(Mail::new(child_id, ActivationClose::ID, ActivationClose.encode_into_bytes(), 1));
    await_signal(&closed_rx, "test.activation.close_done");
    drop(parent.reserve_child(key).expect("the closed child's close path released its parent-local key"));

    let (events_tx, _events_rx) = crossbeam_channel::unbounded();
    let (reborn, reborn_dispatch, _) = finalized_probe(&spawner, &parent, "self-close", events_tx, 2);
    let rejection = registry.submit(EffectBatch::new(vec![RegistryEffect::PreparedSpawn(reborn)])).unwrap();
    owner.run_once();

    assert!(matches!(rejection.wait(), Err(RegistryEffectError::Name(_))));
    let reborn_done = await_spawn_done(&parent, &wakes, reborn_dispatch);
    assert!(
        matches!(reborn_done.output().result, Err(SpawnError::SubnameRetired { .. })),
        "the owner classified the surviving route of a retired id, not a live occupant: {:?}",
        reborn_done.output()
    );
    drop(reborn_done);

    spawner.shutdown_instanced(&TeardownGate {
        round_budget: frame_loop::DRAIN_BUDGET,
        cumulative_cap: SettlementConfig::from_env().to_cap(),
        abort_record: &FatalAbortRecord::new(),
        aborter: &PanicAborter,
    });
    drop(owner);
    assert!(pool.shutdown_with_results().into_iter().all(|result| result.is_ok()));
}

/// Issue #7402: an instanced actor that closes leaves nothing of itself in
/// the spawner, and its slot is freed once the worker that ran the close
/// cycle is done with it. The slot carries the actor's log and trace rings
/// and its binding, so a close path that left the entry in
/// `Spawner::instanced_slots` (the map still has the id), or any other
/// holder that kept the dead slot alive (the `Weak` still upgrades), grows
/// by one slot for every actor ever born: every load and drop of a
/// component.
#[test]
fn closed_actor_slot_is_released_and_freed() {
    let (spawner, registry, mailer, pool) = activation_fixture();
    let _relay = RouteRelayLease::attach(&mailer, pool.wake_sink(), RegistryQueueCapacities::default());
    let owner = RegistryOwnerLease::attach(
        boot_authority(),
        &registry,
        &mailer,
        WakeSink::detached(),
        RegistryQueueCapacities::default(),
    );
    let (events_tx, _events_rx) = crossbeam_channel::unbounded();
    let commit = prepared_probe(&spawner, "released", events_tx);
    let child_id = commit.id;
    let completion = registry.submit(EffectBatch::new(vec![RegistryEffect::PreparedSpawn(commit)])).unwrap();
    owner.apply_once_then_observe_before_next_apply_for_test(|| {});
    completion.wait().unwrap();

    let slot = Arc::clone(
        &spawner
            .instanced_slots
            .lock()
            .expect("instanced_slots mutex poisoned")
            .get(&child_id)
            .expect("a live pooled actor's slot is retained")
            .slot,
    );
    let (closed_tx, closed_rx) = crossbeam_channel::bounded(1);
    slot.set_close_done_tx(closed_tx);
    let freed = Arc::downgrade(&slot);
    drop(slot);

    mailer.push(Mail::new(child_id, ActivationClose::ID, ActivationClose.encode_into_bytes(), 1));
    await_signal(&closed_rx, "test.activation.close_done");
    assert!(
        !spawner.instanced_slots.lock().expect("instanced_slots mutex poisoned").contains_key(&child_id),
        "the close cycle released the spawner's entry before it signalled close-done"
    );

    drop(owner);
    assert!(pool.shutdown_with_results().into_iter().all(|result| result.is_ok()));
    assert!(freed.upgrade().is_none(), "nothing holds the closed actor's slot once the workers have joined");
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
    let id = commit.id;
    let canonical_name = commit.canonical_name.clone();
    let completion = registry.submit(EffectBatch::new(vec![RegistryEffect::PreparedSpawn(commit)])).unwrap();
    owner.apply_once_then_close_after_next_command();
    let applied = completion.wait().unwrap();
    let [RegistryApplied::Starting { token, .. }] = applied.as_slice() else {
        panic!("prepared birth publishes Starting")
    };
    let token = *token;
    let ActivationEvent::Wire(home) = await_event(&events_rx, "test.activation.owner_close_wire") else {
        panic!("activation wires before owner closure")
    };
    drop(owner);

    assert_eq!(await_event(&events_rx, "test.activation.owner_close_unwire"), ActivationEvent::Unwire(home));
    assert_eq!(await_event(&events_rx, "test.activation.owner_close_drop"), ActivationEvent::Drop(home));
    assert!(events_rx.try_recv().is_err(), "owner closure unwires exactly once");
    assert!(
        lifecycle_mail.try_recv().is_err(),
        "neither wire nor rejection-time unwire effects escape a never-Live actor"
    );
    assert!(
        registry.lookup(canonical_name.as_str()).is_none(),
        "Starting route is rolled back without Live publication"
    );
    assert!(registry.entry_at(id).is_none());
    assert!(mailer.cost_table().cells_for(id).is_empty(), "token-owned cost rows are rolled back");
    let fresh = ActivationToken::from_value(token.value() + 1).unwrap();
    assert!(spawner.actor_registry().reserve_starting(id, fresh), "actor lifecycle reservation was removed");
    spawner.actor_registry().rollback_starting(id, fresh);

    assert!(pool.shutdown_with_results().into_iter().all(|result| result.is_ok()));
}
