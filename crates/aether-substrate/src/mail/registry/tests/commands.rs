//! Tests for [`super::super::mailbox::commands`] and the
//! [`super::super::owner`] lease that drives it — submission, the drain
//! under one guard acquisition, admission pressure, and close.

use std::any::Any;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use aether_data::{Kind, KindDescriptor, SchemaType};

use crate::actor::native::{DispatchId, NativeBinding, TaskCompletionWake};
use crate::chassis::settlement::SettlementRegistry;
use crate::config::RegistryQueueCapacities;
use crate::mail::mailer::Mailer;
use crate::mail::outbound::{EgressEvent, HubOutbound};
use crate::mail::registry::effect::{
    ActivationReservation, ActivationToken, EffectBatch, LiveActivation, PreparedCostCells, PreparedRoute,
    PreparedSpawnActivation, PreparedSpawnCommit, PreparedSpawnFailure, RegistryApplied, RegistryBatch,
    RegistryBatchError, RegistryBatchResult, RegistryEffect, RegistryEffectError,
};
use crate::mail::registry::owner::RegistryOwnerLease;
use crate::mail::registry::relay::RouteRelayLease;
use crate::mail::registry::{InlineHandler, MailDispatch, MailboxEntry, OwnedDispatch, Registry, noop_handler};
use crate::mail::{KindId, Mail, MailId, MailboxId, Source};
use crate::runtime::lifecycle::{FatalAborter, PanicAborter};
use crate::scheduler::{BatchBudget, CycleResult, Drainable, Pool, PoolConfig, WakeSink};
use crate::testing::boot_authority as auth;

use super::support::{
    InventorySubscriber, activation_barrier, prepared_test_spawn, starting_token, traced_unknown_mail,
};

struct DiscardProbeActivation {
    dropped: crossbeam_channel::Sender<thread::ThreadId>,
}

struct HomeCancelPrepared {
    sink: WakeSink,
    cancel_started: crossbeam_channel::Sender<()>,
}

impl PreparedSpawnActivation for HomeCancelPrepared {
    fn reserve(
        self: Box<Self>,
        _token: ActivationToken,
    ) -> Result<Arc<dyn ActivationReservation>, (Box<dyn PreparedSpawnActivation>, PreparedSpawnFailure)> {
        Ok(Arc::new(HomeCancelReservation {
            sink: self.sink,
            cancel_started: self.cancel_started,
            cancelled: AtomicBool::new(false),
            done: Mutex::new(None),
        }))
    }

    fn discard_at_home(self: Box<Self>, _failure: PreparedSpawnFailure) -> crossbeam_channel::Receiver<()> {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let _ = tx.send(());
        rx
    }
}

struct HomeCancelReservation {
    sink: WakeSink,
    cancel_started: crossbeam_channel::Sender<()>,
    cancelled: AtomicBool,
    done: Mutex<Option<crossbeam_channel::Receiver<()>>>,
}

impl ActivationReservation for HomeCancelReservation {
    fn schedule(&self) {}

    fn take_live(&self) -> Option<Box<dyn LiveActivation>> {
        None
    }

    fn cancel(&self) {
        if self.cancelled.swap(true, Ordering::AcqRel) {
            return;
        }
        let (done_tx, done_rx) = crossbeam_channel::bounded(1);
        self.done.lock().unwrap().replace(done_rx);
        let _ = self.cancel_started.send(());
        self.sink.schedule(Arc::new(HomeCancelJob { done: Mutex::new(Some(done_tx)) }));
    }

    fn join(&self) {
        let done = self.done.lock().unwrap().take();
        if let Some(done) = done {
            let _ = done.recv();
        }
    }

    fn barrier_matches(&self, _mail_id: MailId) -> bool {
        false
    }
}

struct HomeCancelJob {
    done: Mutex<Option<crossbeam_channel::Sender<()>>>,
}

impl Drainable for HomeCancelJob {
    fn run_cycle(&self, _budget: BatchBudget) -> CycleResult {
        let done = self.done.lock().unwrap().take();
        if let Some(done) = done {
            let _ = done.send(());
        }
        CycleResult::Closed
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

struct WorkerBlocker {
    started: crossbeam_channel::Sender<()>,
    release: crossbeam_channel::Receiver<()>,
}

impl Drainable for WorkerBlocker {
    fn run_cycle(&self, _budget: BatchBudget) -> CycleResult {
        let _ = self.started.send(());
        let _ = self.release.recv();
        CycleResult::Closed
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl Drop for DiscardProbeActivation {
    fn drop(&mut self) {
        let _ = self.dropped.send(thread::current().id());
    }
}

impl PreparedSpawnActivation for DiscardProbeActivation {
    fn reserve(
        self: Box<Self>,
        _token: ActivationToken,
    ) -> Result<Arc<dyn ActivationReservation>, (Box<dyn PreparedSpawnActivation>, PreparedSpawnFailure)> {
        Err((self, PreparedSpawnFailure::ActivationRejected))
    }

    fn discard_at_home(self: Box<Self>, _failure: PreparedSpawnFailure) -> crossbeam_channel::Receiver<()> {
        let (done_tx, done_rx) = crossbeam_channel::bounded(1);
        #[allow(clippy::disallowed_methods, reason = "test probe models an execution-home discard thread")]
        thread::Builder::new()
            .name("activation-discard-probe".to_owned())
            .spawn(move || {
                drop(self);
                let _ = done_tx.send(());
            })
            .unwrap();
        done_rx
    }
}

#[test]
fn owner_shutdown_discards_unapplied_prepared_state_at_home_and_joins() {
    let registry = Arc::new(Registry::new());
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
    let owner = RegistryOwnerLease::attach(
        auth(),
        &registry,
        &mailer,
        WakeSink::detached(),
        RegistryQueueCapacities::default(),
    );
    let (dropped_tx, dropped_rx) = crossbeam_channel::bounded(1);
    let id = MailboxId::from_name("queued-discard");
    let completion = registry
        .submit(EffectBatch::new(vec![RegistryEffect::PreparedSpawn(PreparedSpawnCommit::new(
            PreparedRoute::with_id(id, "queued-discard".to_owned()),
            Box::new(DiscardProbeActivation { dropped: dropped_tx }),
            PreparedCostCells::new(Arc::clone(mailer.cost_table()), Vec::new()),
            Vec::new(),
        ))]))
        .unwrap();
    let owner_thread = thread::current().id();

    drop(owner);

    let dropped_thread = dropped_rx.try_recv().expect("owner shutdown joins the home-side prepared-state drop");
    assert_ne!(dropped_thread, owner_thread, "initialized state never drops on the registry owner");
    assert!(matches!(
        completion.wait_timeout(Duration::from_millis(100)).unwrap(),
        Err(RegistryEffectError::OwnerClosed)
    ));
}

#[test]
fn owner_drop_releases_apply_lock_before_joining_home_cancellation() {
    let registry = Arc::new(Registry::new());
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
    let aborter: Arc<dyn FatalAborter> = Arc::new(PanicAborter);
    let pool = Pool::start(PoolConfig { workers: 1, ..PoolConfig::default() }, aborter);
    let sink = pool.wake_sink();
    let (block_started_tx, block_started_rx) = crossbeam_channel::bounded(1);
    let (block_release_tx, block_release_rx) = crossbeam_channel::bounded(1);
    sink.schedule(Arc::new(WorkerBlocker { started: block_started_tx, release: block_release_rx }));
    block_started_rx.recv_timeout(Duration::from_secs(1)).expect("single worker enters blocker");

    let owner =
        RegistryOwnerLease::attach(auth(), &registry, &mailer, sink.clone(), RegistryQueueCapacities::default());
    let (cancel_started_tx, cancel_started_rx) = crossbeam_channel::bounded(1);
    let id = MailboxId::from_name("owner-drop-home-cancel");
    let birth = RegistryEffect::PreparedSpawn(PreparedSpawnCommit::new(
        PreparedRoute::with_id(id, "owner-drop-home-cancel".to_owned()),
        Box::new(HomeCancelPrepared { sink, cancel_started: cancel_started_tx }),
        PreparedCostCells::new(Arc::clone(mailer.cost_table()), Vec::new()),
        Vec::new(),
    ));
    let started = registry.submit(EffectBatch::new(vec![birth])).unwrap();
    owner.run_once();
    let _ = started.wait_timeout(Duration::from_secs(1)).unwrap().unwrap();
    let queued_owner = registry.submit(EffectBatch::new(Vec::new())).unwrap();
    let (drop_done_tx, drop_done_rx) = crossbeam_channel::bounded(1);
    #[allow(clippy::disallowed_methods, reason = "regression needs lease drop concurrent with the occupied worker")]
    let dropping = thread::spawn(move || {
        drop(owner);
        let _ = drop_done_tx.send(());
    });
    cancel_started_rx.recv_timeout(Duration::from_secs(1)).expect("lease close begins home cancellation");

    let _ = block_release_tx.send(());
    drop_done_rx.recv_timeout(Duration::from_secs(1)).expect("lease drop cannot deadlock behind its queued owner slot");
    dropping.join().unwrap();
    assert!(matches!(
        queued_owner.wait_timeout(Duration::from_secs(1)).unwrap(),
        Err(RegistryEffectError::OwnerClosed)
    ));
    assert!(registry.lookup("owner-drop-home-cancel").is_none());
    assert!(pool.shutdown_with_results().into_iter().all(|result| result.is_ok()));
}

#[test]
fn owner_drains_fifo_batches_with_one_publication_per_dirty_view() {
    let registry = Arc::new(Registry::new());
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
    let owner = RegistryOwnerLease::attach(
        auth(),
        &registry,
        &mailer,
        WakeSink::detached(),
        RegistryQueueCapacities::default(),
    );
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
fn owner_registers_a_kind_batch_atomically_with_one_publication() {
    let registry = Arc::new(Registry::new());
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
    let owner = RegistryOwnerLease::attach(
        auth(),
        &registry,
        &mailer,
        WakeSink::detached(),
        RegistryQueueCapacities::default(),
    );
    let first = KindDescriptor { name: "test.owner.kind.first".to_owned(), schema: SchemaType::Bytes };
    let second = KindDescriptor { name: "test.owner.kind.second".to_owned(), schema: SchemaType::String };

    let completion = registry
        .submit(RegistryBatch::register_kinds(vec![first.clone(), second.clone()]).into_effects())
        .expect("attached owner reserves a prepared kind batch");
    owner.run_once();

    let applied = completion
        .wait_timeout(Duration::from_millis(100))
        .expect("owner completion arrives")
        .expect("kind batch applies");
    assert_eq!(applied.len(), 2);
    assert!(registry.kind_id(&first.name).is_some());
    assert!(registry.kind_id(&second.name).is_some());
    assert_eq!(registry.kind_generation(), 1, "the complete kind batch publishes exactly once");
}

#[test]
fn owner_admission_catches_up_after_transitional_direct_publication() {
    let registry = Arc::new(Registry::new());
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
    let _relay = RouteRelayLease::attach(&mailer, WakeSink::detached(), RegistryQueueCapacities::default());
    let owner = RegistryOwnerLease::attach(
        auth(),
        &registry,
        &mailer,
        WakeSink::detached(),
        RegistryQueueCapacities::default(),
    );
    registry.register_inbox(&auth(), "direct-generation-advance", noop_handler());
    let unknown = MailboxId::from_name("unknown-after-direct-generation-advance");
    let (done_tx, done_rx) = crossbeam_channel::bounded(1);
    let pushing = Arc::clone(&mailer);
    #[allow(clippy::disallowed_methods, reason = "bounded-progress regression needs a joinable caller thread")]
    let thread = thread::spawn(move || {
        pushing.push(Mail::new(unknown, KindId(7), vec![1], 1));
        let _ = done_tx.send(());
    });

    done_rx.recv_timeout(Duration::from_millis(100)).expect("generation catch-up retries once instead of spinning");
    thread.join().unwrap();
    owner.run_once();
}

#[test]
fn owner_captures_authoritative_live_route_but_only_relay_invokes_inline() {
    let registry = Arc::new(Registry::new());
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
    let relay = RouteRelayLease::attach(&mailer, WakeSink::detached(), RegistryQueueCapacities::default());
    let owner = RegistryOwnerLease::attach(
        auth(),
        &registry,
        &mailer,
        WakeSink::detached(),
        RegistryQueueCapacities::default(),
    );
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

#[test]
fn owner_inventory_publication_only_invokes_inline_on_the_relay_turn() {
    let registry = Arc::new(Registry::new());
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
    let relay = RouteRelayLease::attach(&mailer, WakeSink::detached(), RegistryQueueCapacities::default());
    let owner = RegistryOwnerLease::attach(
        auth(),
        &registry,
        &mailer,
        WakeSink::detached(),
        RegistryQueueCapacities::default(),
    );
    let wakes = Arc::new(AtomicU32::new(0));
    let wakes_for_handler = Arc::clone(&wakes);
    let target = registry.register_inline(
        &auth(),
        "inline-inventory-subscriber",
        Arc::new(move |_dispatch: MailDispatch<'_>| {
            wakes_for_handler.fetch_add(1, Ordering::SeqCst);
        }),
    );
    let subscription = registry.subscribe_inventory::<InventorySubscriber>(target, Arc::clone(&mailer));

    assert_eq!(wakes.load(Ordering::SeqCst), 1, "initial subscription notification remains synchronous");
    let initial = registry.inventory();
    subscription.acknowledge(initial.mailbox_generation, initial.kind_generation);
    let changed = registry
        .submit(EffectBatch::new(vec![RegistryEffect::publish_named(
            "owner-published-inventory".to_owned(),
            MailboxEntry::Inbox { handler: noop_handler(), seize: Arc::default() },
        )]))
        .expect("owner accepts inventory-changing effect");

    owner.run_once();
    assert!(changed.wait_timeout(Duration::from_millis(100)).unwrap().is_ok());
    assert_eq!(wakes.load(Ordering::SeqCst), 1, "registry-owner turn cannot invoke the Inline subscriber");

    relay.run_once();
    assert_eq!(wakes.load(Ordering::SeqCst), 2, "relay turn delivers the coalesced inventory notification");
}

/// Issue 4122: at its bound the owner refuses the one class it is allowed to
/// refuse — an ordinary route-view miss — and refuses nothing else.
///
/// The two halves are one test because the policy is the *split*: a bound
/// that also refused effect batches would be a correctness regression that a
/// shed-only test would pass, and a bound that refused nothing would be a
/// memory regression that a batch-only test would pass. Both directions have
/// to be pinned against the same saturated queue.
///
/// The shed envelope must land on the existing unknown-recipient policy —
/// egressed to the attached hub, settlement balanced — not vanish. A shed
/// that skipped `record_finished` would hang every caller's settlement chain,
/// which is the failure mode that makes silent dropping unacceptable.
#[test]
fn owner_sheds_route_misses_at_capacity_but_never_reserved_effects() {
    let registry = Arc::new(Registry::new());
    let (outbound, outbound_rx) = HubOutbound::attached_loopback();
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)).with_outbound(outbound));
    let settlement = Arc::new(SettlementRegistry::new());
    mailer.trace_handle().install_settlement_registry(Arc::clone(&settlement));
    let capacities = RegistryQueueCapacities { owner: 2, relay: 64 };
    let _relay = RouteRelayLease::attach(&mailer, WakeSink::detached(), capacities);
    let owner = RegistryOwnerLease::attach(auth(), &registry, &mailer, WakeSink::detached(), capacities);
    let name = "owner-shed-at-capacity";
    let id = MailboxId::from_name(name);
    let reserved = registry.submit(EffectBatch::new(vec![RegistryEffect::reserve_named(name.to_owned())])).unwrap();
    owner.run_once();
    let _token = starting_token(&reserved.wait_timeout(Duration::from_millis(100)).unwrap().unwrap());

    // Fill the queue to its bound with parkable misses.
    let parked = [1u8, 2].map(|payload| {
        let (mail, settled) = traced_unknown_mail(&mailer, &settlement, id, u64::from(payload), vec![payload]);
        mailer.push(mail);
        settled
    });
    assert_eq!(registry.owner_queue_metrics().unwrap().depth, 2, "both misses parked under the bound");

    // The next miss is refused and takes the unknown-recipient policy.
    let (shed, shed_settled) = traced_unknown_mail(&mailer, &settlement, id, 3, vec![3]);
    mailer.push(shed);
    assert!(shed_settled.recv_timeout(Duration::from_millis(100)).is_ok(), "a shed envelope settles rather than hangs");
    assert!(
        matches!(outbound_rx.recv_timeout(Duration::from_millis(100)).unwrap(), EgressEvent::UnresolvedMail { payload, .. } if payload == [3])
    );
    for settled in &parked {
        assert!(settled.try_recv().is_err(), "shedding one miss does not disturb the misses already parked");
    }

    // The reserved class is admitted past the same bound.
    let queued = registry.submit(EffectBatch::new(vec![RegistryEffect::publish_named(
        "owner-shed-reserved".to_owned(),
        MailboxEntry::Inbox { handler: noop_handler(), seize: Arc::default() },
    )]));
    assert!(queued.is_some(), "an effect batch is never refused by the bound");
    let metrics = registry.owner_queue_metrics().unwrap();
    assert_eq!(metrics.capacity, 2);
    assert_eq!(metrics.shed, 1, "exactly the one over-bound miss was shed");
    assert_eq!(metrics.over_capacity, 1, "the reserved batch is admitted past the bound and counted");
    assert_eq!(metrics.depth, 3);

    owner.run_once();
    assert!(queued.unwrap().wait_timeout(Duration::from_millis(100)).unwrap().is_ok());
    assert_eq!(registry.owner_queue_metrics().unwrap().shed, 1, "draining does not retroactively shed");
}

/// Issue 4204: a departure notice addressed to a mailbox inside its birth
/// window survives owner-queue saturation and still reaches the watcher.
///
/// The bug this catches: a `MonitorNotice` fired at a `Starting` watcher takes
/// the same admission bound an ordinary route-view miss takes, and shedding it
/// routes it to the unknown-recipient policy — `RouteLookup::into_captured`
/// maps a `Starting` lookup to `CapturedDisposition::Unknown`, so the envelope
/// the parked FIFO would have delivered on promotion is instead spent as an
/// unresolved-mail terminal. Nothing re-sends a departure notice, so the
/// watcher never learns its target is gone and holds state keyed by a departed
/// peer for the rest of the process. Reachable because `wire` runs against a
/// full `NativeCtx` while the actor's own route is still `Starting`, so an
/// actor may register a monitor before its activation barrier promotes it.
///
/// Saturation is driven at the bound rather than asserted against the policy
/// predicate: the claim under test is that the notice arrives, and only the
/// admit-park-promote path can show that.
#[test]
fn owner_admits_a_departure_notice_to_a_starting_watcher_past_capacity() {
    let registry = Arc::new(Registry::new());
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
    let capacities = RegistryQueueCapacities { owner: 2, relay: 64 };
    let _relay = RouteRelayLease::attach(&mailer, WakeSink::detached(), capacities);
    let owner = RegistryOwnerLease::attach(auth(), &registry, &mailer, WakeSink::detached(), capacities);
    let deliveries = Arc::new(Mutex::new(Vec::new()));
    let watcher_name = "monitor-notice-starting-watcher";
    let watcher_id = MailboxId::from_name(watcher_name);
    let (_, _, _, birth) = prepared_test_spawn(
        &registry,
        &mailer,
        watcher_name,
        Arc::clone(&deliveries),
        Arc::new(AtomicUsize::new(0)),
        vec![watcher_id],
        1,
    );
    let birth_completion = registry.submit(EffectBatch::new(vec![birth])).unwrap();
    owner.run_once();
    let token = starting_token(&birth_completion.wait_timeout(Duration::from_millis(100)).unwrap().unwrap());

    // Fill the bound with ordinary mail parked behind the same unpromoted birth.
    for payload in [2u8, 3] {
        mailer.push(Mail::new(watcher_id, KindId(7), vec![payload], 1));
    }
    assert_eq!(registry.owner_queue_metrics().unwrap().depth, 2, "ordinary parked mail reaches the bound");

    // The departed target's notice is admitted past it rather than refused.
    let notice = aether_kinds::MonitorNotice { target: MailboxId::from_name("monitor-notice-departed-target") };
    let notice_payload = notice.encode_into_bytes();
    mailer.push(Mail::new(watcher_id, aether_kinds::MonitorNotice::ID, notice_payload.clone(), 1));
    let metrics = registry.owner_queue_metrics().unwrap();
    assert_eq!(metrics.shed, 0, "a departure notice is never shed");
    assert_eq!(metrics.over_capacity, 1, "it is admitted past the bound and counted as pressure");
    assert_eq!(metrics.depth, 3);

    // And it is delivered once the barrier promotes the watcher.
    mailer.push(activation_barrier(watcher_id, token, 1));
    owner.run_once();
    assert_eq!(
        *deliveries.lock().unwrap(),
        [1, 2, 3, notice_payload[0]],
        "the notice rides the parked tail into the promoted watcher, behind the mail that preceded it"
    );
}

/// Issue 4122: the owner's drain accounting is per batch, not per command —
/// ADR-0165's sharding trigger reads a ceiling (commands per busy nanosecond)
/// and a duty cycle, and both collapse if one drain of `n` commands is booked
/// as `n` drains.
#[test]
fn owner_drain_metrics_measure_whole_batches_and_busy_time() {
    let registry = Arc::new(Registry::new());
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
    let owner = RegistryOwnerLease::attach(
        auth(),
        &registry,
        &mailer,
        WakeSink::detached(),
        RegistryQueueCapacities::default(),
    );
    let first = registry.submit(EffectBatch::new(vec![RegistryEffect::reserve_named("drain-metrics-a".to_owned())]));
    let second = registry.submit(EffectBatch::new(vec![RegistryEffect::reserve_named("drain-metrics-b".to_owned())]));
    owner.run_once();
    for completion in [first, second] {
        assert!(completion.unwrap().wait_timeout(Duration::from_millis(100)).unwrap().is_ok());
    }

    let metrics = registry.owner_queue_metrics().unwrap();
    assert_eq!(metrics.admitted, 2);
    assert_eq!(metrics.drained, 2);
    assert_eq!(metrics.drains, 1, "two commands drained together are one drain cycle");
    assert_eq!(metrics.drain_max, 2);
    assert_eq!(metrics.depth_max, 2);
    assert_eq!(metrics.depth, 0);
    assert!(metrics.busy_nanos > 0, "an applied batch records the owner's busy time");

    registry.submit(EffectBatch::new(vec![RegistryEffect::reserve_named("drain-metrics-c".to_owned())])).unwrap();
    owner.run_once();
    let metrics = registry.owner_queue_metrics().unwrap();
    assert_eq!((metrics.drained, metrics.drains, metrics.drain_max), (3, 2, 2), "a smaller drain leaves the max alone");
}

/// A deferred completion can synchronously route a wake whose handler stages
/// more owner work. The owner must release the admission lock before that
/// completion, requeue the newly admitted suffix, and leave `depth` describing
/// that suffix rather than resetting it to zero with the retired prefix.
#[test]
fn owner_completion_reentry_requeues_and_preserves_depth_metric() {
    let registry = Arc::new(Registry::new());
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
    let (reentered_tx, reentered_rx) = mpsc::channel();
    let registry_for_wake = Arc::clone(&registry);
    let actor_mailbox = registry.register_inbox(
        &auth(),
        "test.registry.owner-completion-reentry",
        Arc::new(move |dispatch: OwnedDispatch| {
            dispatch.discharge();
            let completion = registry_for_wake
                .submit(EffectBatch::new(Vec::new()))
                .expect("the completion handler can re-enter the accepting owner");
            reentered_tx.send(completion).expect("test receiver remains live");
        }),
    );
    let binding = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), actor_mailbox));
    let owner = RegistryOwnerLease::attach(
        auth(),
        &registry,
        &mailer,
        WakeSink::detached(),
        RegistryQueueCapacities::default(),
    );
    let completion =
        binding.dispatch_arm::<RegistryBatchResult, _>(mailer.acquire_settlement_hold(MailId::NONE), Source::NONE, ());
    let dispatch_id = completion.dispatch_id();
    assert!(registry.submit_deferred(RegistryBatch::register_kinds(Vec::new()).into_effects(), completion));

    assert_eq!(owner.run_once(), CycleResult::Requeue, "the re-entered suffix schedules another owner turn");
    let reentered = reentered_rx.recv_timeout(Duration::from_millis(100)).expect("completion staged owner work");
    let metrics = registry.owner_queue_metrics().unwrap();
    assert_eq!(metrics.depth, 1, "the meter retains the command admitted during apply");
    assert_eq!((metrics.drained, metrics.drains), (1, 1));

    assert_eq!(owner.run_once(), CycleResult::Idle);
    assert!(reentered.wait_timeout(Duration::from_millis(100)).unwrap().is_ok());
    assert_eq!(registry.owner_queue_metrics().unwrap().depth, 0);
    let done = binding
        .dispatch_take::<RegistryBatchResult, ()>(dispatch_id)
        .expect("the deferred result remains available after its wake");
    assert!(done.output().is_ok());
    done.release_no_reply();
}

#[test]
fn owner_close_rejects_queued_and_future_submissions_without_stranding_completion() {
    let registry = Arc::new(Registry::new());
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
    let owner = RegistryOwnerLease::attach(
        auth(),
        &registry,
        &mailer,
        WakeSink::detached(),
        RegistryQueueCapacities::default(),
    );
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
fn deferred_batch_owner_close_wakes_exactly_once_with_public_error() {
    let registry = Arc::new(Registry::new());
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
    let (wake_tx, wake_rx) = mpsc::channel::<OwnedDispatch>();
    let actor_mailbox = registry.register_inbox(
        &auth(),
        "test.registry.deferred-owner-close",
        Arc::new(move |dispatch: OwnedDispatch| {
            dispatch.discharge();
            wake_tx.send(dispatch).expect("test wake receiver remains live");
        }),
    );
    let binding = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), actor_mailbox));
    let owner = RegistryOwnerLease::attach(
        auth(),
        &registry,
        &mailer,
        WakeSink::detached(),
        RegistryQueueCapacities::default(),
    );
    let completion =
        binding.dispatch_arm::<RegistryBatchResult, _>(mailer.acquire_settlement_hold(MailId::NONE), Source::NONE, ());
    let dispatch_id = completion.dispatch_id();
    assert!(registry.submit_deferred(RegistryBatch::register_kinds(Vec::new()).into_effects(), completion));

    drop(owner);

    let wake = wake_rx.recv_timeout(Duration::from_millis(100)).expect("owner close emits deferred completion wake");
    assert_eq!(wake.kind, TaskCompletionWake::ID);
    let wake = TaskCompletionWake::decode_from_bytes(wake.payload.bytes()).expect("completion wake decodes");
    assert_eq!(DispatchId(wake.dispatch_id), dispatch_id);
    let done = binding
        .dispatch_take::<RegistryBatchResult, ()>(dispatch_id)
        .expect("public deferred result is retained in the actor ledger");
    assert!(matches!(done.output(), Err(RegistryBatchError::OwnerClosed)));
    done.release_no_reply();
    assert!(wake_rx.try_recv().is_err(), "owner close emits exactly one completion wake");
}

#[test]
#[allow(clippy::disallowed_methods, reason = "the test deliberately races owner submit against owner close")]
fn owner_submit_racing_close_is_rejected_or_completed() {
    use std::sync::Barrier;

    let registry = Arc::new(Registry::new());
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
    let owner = RegistryOwnerLease::attach(
        auth(),
        &registry,
        &mailer,
        WakeSink::detached(),
        RegistryQueueCapacities::default(),
    );
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
