//! Monitor registrations: a notice fires once at the target's close, a
//! target that had already closed is noticed once all the same, a root is
//! watched like any other actor, and a watcher that dies first is pruned
//! from every target's forward index.

use crate::actor::monitor::MonitorHandle;
use crate::actor::native::Dispatch;
use crate::actor::native::ctx::NativeCtx;
use crate::actor::native::spawn::Subname;
use crate::chassis::builder::{Builder, PassiveChassis};
use crate::mail::KindId;
use crate::mail::MailboxId;
use crate::testing::{TestChassis, await_settled, await_signal, bare_substrate};
use crate::{BootError, NativeActor, NativeInitCtx};
use aether_actor::{ActorRef, Addressable, ErasedActorRef, HandlesKind};
use aether_data::Kind;
use crossbeam_channel::{Receiver, Sender};
use std::sync::Arc;

// Closes a `Departing` target or a `DepartingRoot`.
pod_kind!(Depart { tag: u32 }, "test.monitor.depart", 0x7488_0000_0000_0001);

// Tells a `LateWatcher` to monitor the target it was spawned with.
pod_kind!(Watch { tag: u32 }, "test.monitor.watch", 0x7488_0000_0000_0002);

// Asks a `LateWatcher` to record that it has handled everything before it.
pod_kind!(Report { tag: u32 }, "test.monitor.report", 0x7488_0000_0000_0003);

// An instanced target that closes on `Depart`.
unit_shutdown_actor!(Departing, "test.monitor.departing", Depart);

/// A composed root that closes on `Depart`. No root owns a slot in the
/// actor registry, so it is the target a slot check would refuse.
struct DepartingRoot;
impl Addressable for DepartingRoot {
    const NAMESPACE: &'static str = "test.monitor.departing_root";
    type Resolver = aether_actor::One;
}
impl aether_actor::Root for DepartingRoot {}
impl HandlesKind<Depart> for DepartingRoot {
    type Sender = aether_actor::Anyone;
}
impl aether_actor::Lifecycle<Self> for DepartingRoot {
    type Config = ();
    type Params = ();
    type InitError = BootError;
    type InitCtx<'a> = NativeInitCtx<'a>;
    type Ctx<'a> = NativeCtx<'a, Self>;
    fn init((): (), (): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self)
    }
}
impl aether_actor::Declared for DepartingRoot {
    type Depends = ();
    type Spawns = ();
    type Parents = ();
}
impl NativeActor for DepartingRoot {
    type State = Self;
}
shutdown_dispatch!(DepartingRoot, Depart);

/// One thing a [`LateWatcher`] did, in the order it did it.
#[derive(Debug, PartialEq)]
enum Did {
    /// `ctx.monitor` returned, recorded as the monitoring handler's last
    /// step.
    Monitored,
    /// A `MonitorNotice` was handled, with the sender its ctx proved.
    Noticed(Option<ErasedActorRef>),
    /// A `Report` was handled.
    Reported,
}

/// A watcher that monitors the one target it was spawned with when told to,
/// and records what it does in handling order. The target is a spawn
/// parameter because a proven reference has no codec to ride in mail, and a
/// target that has closed can no longer be proven from its position.
struct LateWatcher {
    target: ErasedActorRef,
    did: Sender<Did>,
    noticed: Sender<()>,
    handles: Vec<MonitorHandle>,
}
impl Addressable for LateWatcher {
    const NAMESPACE: &'static str = "test.monitor.late_watcher";
    type Resolver = aether_actor::Many;
}
impl aether_actor::Root for LateWatcher {}
impl HandlesKind<Watch> for LateWatcher {
    type Sender = aether_actor::Anyone;
}
impl HandlesKind<Report> for LateWatcher {
    type Sender = aether_actor::Anyone;
}
impl HandlesKind<aether_kinds::MonitorNotice> for LateWatcher {
    type Sender = aether_actor::Anyone;
}
impl aether_actor::Lifecycle<Self> for LateWatcher {
    type Config = ();
    type Params = (ErasedActorRef, Sender<Did>, Sender<()>);
    type InitError = BootError;
    type InitCtx<'a> = NativeInitCtx<'a>;
    type Ctx<'a> = NativeCtx<'a, Self>;
    fn init((): (), (target, did, noticed): Self::Params, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self { target, did, noticed, handles: Vec::new() })
    }
}
impl aether_actor::Declared for LateWatcher {
    type Depends = ();
    type Spawns = ();
    type Parents = ();
}
impl NativeActor for LateWatcher {
    type State = Self;
}
impl Dispatch<Self> for LateWatcher {
    fn dispatch(
        state: &mut Self,
        ctx: &mut NativeCtx<'_, Self, crate::Unchecked>,
        kind: KindId,
        payload: &[u8],
    ) -> Option<()> {
        if kind.0 == Watch::ID.0 {
            Watch::decode_from_bytes(payload)?;
            state.handles.push(ctx.monitor(state.target));
            let _ = state.did.send(Did::Monitored);
            return Some(());
        }
        if kind.0 == <aether_kinds::MonitorNotice as Kind>::ID.0 {
            <aether_kinds::MonitorNotice as Kind>::decode_from_bytes(payload)?;
            let _ = state.did.send(Did::Noticed(ctx.sender()));
            let _ = state.noticed.send(());
            return Some(());
        }
        if kind.0 == Report::ID.0 {
            Report::decode_from_bytes(payload)?;
            let _ = state.did.send(Did::Reported);
            return Some(());
        }
        None
    }
}

/// A spawned [`LateWatcher`] and the channels it records on.
struct LateProbe {
    actor: ActorRef<LateWatcher>,
    did: Receiver<Did>,
    noticed: Receiver<()>,
}

impl LateProbe {
    fn spawn(chassis: &PassiveChassis<TestChassis>, target: ErasedActorRef) -> Self {
        let (did_tx, did) = crossbeam_channel::unbounded();
        let (noticed_tx, noticed) = crossbeam_channel::unbounded();
        let actor = chassis
            .spawn_actor_for_test::<LateWatcher>(Subname::Counter, (), (target, did_tx, noticed_tx))
            .finish()
            .expect("spawn watcher");

        Self { actor, did, noticed }
    }

    /// Send `mail` to the watcher and return its root's settlement.
    fn order<K: Kind>(&self, chassis: &PassiveChassis<TestChassis>, mail: &K) -> Receiver<()>
    where
        LateWatcher: HandlesKind<K>,
    {
        chassis.send_tracked(self.actor, mail, None).1
    }

    /// Everything the watcher has done once it has handled all the mail on
    /// its inbox now. The watcher's inbox is FIFO, so a `Report` sent here
    /// is handled after every notice already posted to it.
    fn everything_done(&self, chassis: &PassiveChassis<TestChassis>) -> Vec<Did> {
        await_settled(&self.order(chassis, &Report { tag: 0 }), "test.monitor.report");

        self.did.try_iter().collect()
    }
}

fn empty_chassis() -> PassiveChassis<TestChassis> {
    let (registry, mailer) = bare_substrate();

    Builder::<TestChassis>::new(registry, mailer).build_passive().expect("empty chassis boots")
}

/// Monitoring a target that has already closed returns, and the watcher
/// handles exactly one notice, stamped with that target, after the handler
/// that monitored has returned. The order the watcher records is the proof:
/// `Monitored` is that handler's last step, so a notice delivered inside
/// `monitor` would be recorded before it; a registration that reported
/// nothing would leave no `Noticed`; and a second notice would have been
/// posted before the `Report` and so be recorded ahead of `Reported`.
#[test]
fn a_target_that_already_closed_is_noticed_once_after_the_monitoring_handler() {
    let chassis = empty_chassis();
    let target = chassis.spawn_actor_for_test::<Departing>(Subname::Counter, (), ()).finish().expect("spawn target");
    let watcher = LateProbe::spawn(&chassis, target.erase());
    let _ = chassis.send_tracked(target, &Depart { tag: 0 }, None);
    chassis.await_closed(target.erase());

    await_settled(&watcher.order(&chassis, &Watch { tag: 0 }), "test.monitor.watch");
    await_signal(&watcher.noticed, "test.monitor.late_notice");

    assert_eq!(watcher.everything_done(&chassis), [Did::Monitored, Did::Noticed(Some(target.erase())), Did::Reported],);
    assert_eq!(chassis.actor_registry().monitor_count(target.id()), 0, "a closed target holds no watcher");
}

/// A composed root is watched, and its watcher is noticed when the root
/// shuts itself down. A registration that required a slot in the actor
/// registry would find none for any root: the entry would never be
/// written, and the watcher would hold state for a root that had gone.
#[test]
fn a_composed_root_is_watched_and_noticed_when_it_closes() {
    let (registry, mailer) = bare_substrate();
    let chassis = Builder::<TestChassis>::new(registry, mailer)
        .with_actor::<DepartingRoot>(())
        .build_passive()
        .expect("the chassis boots its root");
    let root = chassis.actor_ref::<DepartingRoot>();
    let watcher = LateProbe::spawn(&chassis, root.erase());

    await_settled(&watcher.order(&chassis, &Watch { tag: 0 }), "test.monitor.watch_root");
    assert_eq!(chassis.actor_registry().monitor_count(root.id()), 1, "the root's watcher is registered");
    let _ = chassis.send_tracked(root, &Depart { tag: 0 }, None);
    await_signal(&watcher.noticed, "test.monitor.root_notice");

    assert_eq!(watcher.everything_done(&chassis), [Did::Monitored, Did::Noticed(Some(root.erase())), Did::Reported],);
}

/// Pairs of a target and its watcher raced by
/// [`monitor_racing_a_close_is_answered_by_exactly_one_notice`].
const RACED_PAIRS: u32 = 300;

/// A `monitor` that races its target's close is answered by exactly one
/// notice, whichever side wins. Each pair's `Depart` and `Watch` are sent
/// back to back, so the target's close cycle and the watcher's handler run
/// on two pool workers at once, with the send order alternating so that
/// neither side always starts first. Once the close has run and the watch
/// has settled, every notice the pair will ever be owed is on the watcher's
/// inbox: the close posts inside its tail, and the registration inside its
/// handler. The `Report` behind them then reads exactly one.
///
/// The registry's own race test covers the interleaving at the lock; this
/// one covers what the two callers do with its answers, where a close that
/// dropped its drained list or a `monitor` that posted for a live target
/// would show as zero notices or two.
#[test]
fn monitor_racing_a_close_is_answered_by_exactly_one_notice() {
    let chassis = empty_chassis();

    for pair in 0..RACED_PAIRS {
        let target =
            chassis.spawn_actor_for_test::<Departing>(Subname::Counter, (), ()).finish().expect("spawn target");
        let watcher = LateProbe::spawn(&chassis, target.erase());

        let watch_first = pair % 2 == 0;
        let watch_settled = if watch_first {
            let watch_settled = watcher.order(&chassis, &Watch { tag: pair });
            let _ = chassis.send_tracked(target, &Depart { tag: pair }, None);
            watch_settled
        } else {
            let _ = chassis.send_tracked(target, &Depart { tag: pair }, None);
            watcher.order(&chassis, &Watch { tag: pair })
        };
        chassis.await_closed(target.erase());
        await_settled(&watch_settled, "test.monitor.raced_watch");

        assert_eq!(
            watcher.everything_done(&chassis),
            [Did::Monitored, Did::Noticed(Some(target.erase())), Did::Reported],
            "pair {pair}: a monitor racing a close is owed exactly one notice",
        );
        assert_eq!(chassis.actor_registry().monitor_count(target.id()), 0, "pair {pair}: an entry outlived the close");
    }
}

/// Issue 607 Phase 4b verify: a `ctx.monitor(target)` registration
/// fires exactly one `MonitorNotice` at the watcher when the
/// target self-shuts. Two-actor scenario: Watcher (instanced)
/// holds a `MonitorHandle` against Target (instanced) and counts
/// the notices it receives; Target self-shuts on `Quit`. After
/// the close fan-out we assert (1) the watcher saw the notice
/// once, sent from the reference it monitored, (2) the target's slot is Dead +
/// tombstoned, and (3) the registry's forward index drained.
#[test]
fn ctx_monitor_fires_notice_at_target_close() {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering as AtomicOrdering};

    // Self-shutdown trigger for the target.
    pod_kind!(Quit { tag: u32 }, "test.monitor.quit", 0xC0DE_C0DE_4B4B_4B4B);

    // Tells the watcher which target to monitor. The watcher's
    // handler reads `target_id` and calls `ctx.monitor`.
    pod_kind!(WatchOrder { target_id: u64 }, "test.monitor.watch_order", 0x4B4B_C0DE_C0DE_C0DE);

    // Target — handles Quit by self-shutting.
    unit_shutdown_actor!(Target, "test.monitor.target", Quit);

    // Watcher — handles WatchOrder by registering a monitor;
    // handles MonitorNotice by recording whether the notice's
    // sender is the reference it monitored, bumping a counter, and
    // signalling the test: the notice is a detached send from the
    // target's close tail, outside any root the test holds.
    struct Watcher {
        notice_count: Arc<AtomicU32>,
        sender_matched: Arc<AtomicBool>,
        noticed: Sender<()>,
        monitored: Option<ErasedActorRef>,
        handle: Mutex<Option<MonitorHandle>>,
    }
    impl Addressable for Watcher {
        const NAMESPACE: &'static str = "test.monitor.watcher";
        type Resolver = aether_actor::Many;
    }
    impl aether_actor::Root for Watcher {}
    impl HandlesKind<WatchOrder> for Watcher {
        type Sender = aether_actor::Anyone;
    }
    impl HandlesKind<aether_kinds::MonitorNotice> for Watcher {
        type Sender = aether_actor::Anyone;
    }
    impl aether_actor::Lifecycle<Self> for Watcher {
        type Config = ();
        type Params = (Arc<AtomicU32>, Arc<AtomicBool>, Sender<()>);
        type InitError = BootError;
        type InitCtx<'a> = NativeInitCtx<'a>;
        type Ctx<'a> = NativeCtx<'a, Self>;
        fn init((): (), params: Self::Params, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self {
                notice_count: params.0,
                sender_matched: params.1,
                noticed: params.2,
                monitored: None,
                handle: Mutex::new(None),
            })
        }
    }
    impl aether_actor::Declared for Watcher {
        type Depends = ();
        type Spawns = ();
        type Parents = ();
    }
    impl NativeActor for Watcher {
        type State = Self;
    }
    impl Dispatch<Self> for Watcher {
        fn dispatch(
            state: &mut Self,
            ctx: &mut NativeCtx<'_, Self, crate::Unchecked>,
            kind: KindId,
            payload: &[u8],
        ) -> Option<()> {
            if kind.0 == WatchOrder::ID.0 {
                let order = WatchOrder::decode_from_bytes(payload)?;
                let Ok(target) = ctx.resolve_live(MailboxId(order.target_id)) else {
                    panic!("target must be Live at order time");
                };
                let h = ctx.monitor(target);
                state.monitored = Some(target);
                *state.handle.lock().unwrap() = Some(h);
                return Some(());
            }
            if kind.0 == <aether_kinds::MonitorNotice as Kind>::ID.0 {
                <aether_kinds::MonitorNotice as Kind>::decode_from_bytes(payload)?;
                state.sender_matched.store(ctx.sender() == state.monitored, AtomicOrdering::SeqCst);
                state.notice_count.fetch_add(1, AtomicOrdering::SeqCst);
                let _ = state.noticed.send(());
                return Some(());
            }
            None
        }
    }

    let (registry, mailer) = bare_substrate();
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .build_passive()
        .expect("empty chassis boots");

    // Spawn target first so the watcher can register against a
    // Live id.
    let target = chassis.spawn_actor_for_test::<Target>(Subname::Counter, (), ()).finish().expect("spawn target");

    let notice_count = Arc::new(AtomicU32::new(0));
    let sender_matched = Arc::new(AtomicBool::new(false));
    let (noticed, notice_rx) = crossbeam_channel::unbounded();
    let watcher = chassis
        .spawn_actor_for_test::<Watcher>(
            Subname::Counter,
            (),
            (Arc::clone(&notice_count), Arc::clone(&sender_matched), noticed),
        )
        .finish()
        .expect("spawn watcher");

    // The watcher registers the monitor inside its WatchOrder
    // handler, so the registration lands before the root settles.
    let (_, settled) = chassis.send_tracked(watcher, &WatchOrder { target_id: target.id().0 }, None);
    await_settled(&settled, "test.monitor.watch_order");
    assert_eq!(
        chassis.actor_registry().monitor_count(target.id()),
        1,
        "watcher's monitor should be registered against target",
    );
    assert_eq!(
        chassis.actor_registry().monitoring_count(watcher.id()),
        1,
        "watcher should appear in the reverse index"
    );

    // Fire Quit at the target — its handler self-shuts; the
    // close path runs `close_actor`, which marks the slot Dead and
    // fans out a MonitorNotice mail to the watcher.
    let _ = chassis.send_tracked(target, &Quit { tag: 1 }, None);
    chassis.await_closed(target.erase());
    await_signal(&notice_rx, "test.monitor.notice");
    assert_eq!(notice_count.load(AtomicOrdering::SeqCst), 1, "watcher should have received exactly one MonitorNotice");
    assert!(
        sender_matched.load(AtomicOrdering::SeqCst),
        "the MonitorNotice's sender should be the reference the watcher monitored",
    );

    assert!(
        !chassis.actor_registry().is_live_at(target.id()),
        "target slot should transition Live → Dead after close fan-out",
    );
    assert!(chassis.actor_registry().is_tombstoned(target.id()), "target id should be tombstoned");
    // Forward index for target was drained.
    assert_eq!(chassis.actor_registry().monitor_count(target.id()), 0, "monitors_of[target] must drain after fan-out");

    drop(chassis);
}

/// Issue 607 Phase 4b verify: when the *watcher* dies first, the
/// reverse-index walk prunes the watcher's entry from each
/// monitored target's `monitors_of`. No `MonitorNotice` fires (the
/// watcher is the one closing; targets are still alive).
#[test]
fn watcher_close_prunes_targets_forward_index() {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

    // Re-use Quit + WatchOrder shape inline (test isolation).
    pod_kind!(Quit { tag: u32 }, "test.monitor.quit2", 0xCAFE_BABE_DEAD_BEEF);
    pod_kind!(WatchOrder { target_id: u64 }, "test.monitor.watch_order2", 0xBEEF_DEAD_BABE_CAFE);

    struct Target;
    impl Addressable for Target {
        const NAMESPACE: &'static str = "test.monitor.target2";
        type Resolver = aether_actor::Many;
    }
    impl aether_actor::Root for Target {}
    impl aether_actor::Lifecycle<Self> for Target {
        type Config = ();
        type Params = ();
        type InitError = BootError;
        type InitCtx<'a> = NativeInitCtx<'a>;
        type Ctx<'a> = NativeCtx<'a, Self>;
        fn init((): Self::Config, _params: (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self)
        }
    }
    impl aether_actor::Declared for Target {
        type Depends = ();
        type Spawns = ();
        type Parents = ();
    }
    impl NativeActor for Target {
        type State = Self;
    }
    impl Dispatch<Self> for Target {
        fn dispatch(
            _state: &mut Self,
            _ctx: &mut NativeCtx<'_, Self, crate::Unchecked>,
            _kind: KindId,
            _payload: &[u8],
        ) -> Option<()> {
            None
        }
    }

    struct Watcher {
        handle: Mutex<Option<MonitorHandle>>,
        close_observed: Arc<AtomicU32>,
    }
    impl Addressable for Watcher {
        const NAMESPACE: &'static str = "test.monitor.watcher2";
        type Resolver = aether_actor::Many;
    }
    impl aether_actor::Root for Watcher {}
    impl HandlesKind<WatchOrder> for Watcher {
        type Sender = aether_actor::Anyone;
    }
    impl HandlesKind<Quit> for Watcher {
        type Sender = aether_actor::Anyone;
    }
    impl aether_actor::Lifecycle<Self> for Watcher {
        type Config = ();
        type Params = Arc<AtomicU32>;
        type InitError = BootError;
        type InitCtx<'a> = NativeInitCtx<'a>;
        type Ctx<'a> = NativeCtx<'a, Self>;
        fn init((): (), params: Self::Params, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self { handle: Mutex::new(None), close_observed: params })
        }
        fn unwire(state: &mut Self, _ctx: &mut NativeCtx<'_, Self>) {
            state.close_observed.fetch_add(1, AtomicOrdering::SeqCst);
        }
    }
    impl aether_actor::Declared for Watcher {
        type Depends = ();
        type Spawns = ();
        type Parents = ();
    }
    impl NativeActor for Watcher {
        type State = Self;
    }
    impl Dispatch<Self> for Watcher {
        fn dispatch(
            state: &mut Self,
            ctx: &mut NativeCtx<'_, Self, crate::Unchecked>,
            kind: KindId,
            payload: &[u8],
        ) -> Option<()> {
            if kind.0 == WatchOrder::ID.0 {
                let order = WatchOrder::decode_from_bytes(payload)?;
                let Ok(target) = ctx.resolve_live(MailboxId(order.target_id)) else {
                    panic!("target Live");
                };
                let h = ctx.monitor(target);
                *state.handle.lock().unwrap() = Some(h);
                return Some(());
            }
            if kind.0 == Quit::ID.0 {
                let _ = Quit::decode_from_bytes(payload)?;
                ctx.shutdown();
                return Some(());
            }
            None
        }
    }

    let (registry, mailer) = bare_substrate();
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .build_passive()
        .expect("empty chassis boots");

    let target = chassis.spawn_actor_for_test::<Target>(Subname::Counter, (), ()).finish().expect("spawn target");
    let close_observed = Arc::new(AtomicU32::new(0));
    let watcher = chassis
        .spawn_actor_for_test::<Watcher>(Subname::Counter, (), Arc::clone(&close_observed))
        .finish()
        .expect("spawn watcher");

    // Watcher registers monitor against target inside its handler.
    let (_, settled) = chassis.send_tracked(watcher, &WatchOrder { target_id: target.id().0 }, None);
    await_settled(&settled, "test.monitor.watch_order2");
    assert_eq!(chassis.actor_registry().monitor_count(target.id()), 1);

    // Quit watcher — its close path runs `unwire`, walks
    // `monitoring[watcher]` and prunes watcher from
    // `monitors_of[target]`.
    let _ = chassis.send_tracked(watcher, &Quit { tag: 1 }, None);
    chassis.await_closed(watcher.erase());
    assert_eq!(close_observed.load(AtomicOrdering::SeqCst), 1, "watcher's unwire fired exactly once");

    // Watcher slot tombstones; target slot still Live; target's
    // forward index drained of the dead watcher.
    assert!(chassis.actor_registry().is_tombstoned(watcher.id()), "watcher tombstoned");
    assert!(
        chassis.actor_registry().is_live_at(target.id()),
        "target should still be Live (watcher closed, not target)"
    );
    assert_eq!(
        chassis.actor_registry().monitor_count(target.id()),
        0,
        "target's monitors_of should drop the dead watcher",
    );

    drop(chassis);
}
