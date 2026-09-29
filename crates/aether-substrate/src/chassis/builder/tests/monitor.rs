//! Monitor registrations: a notice fires once at the target's close, and a
//! watcher that dies first is pruned from every target's forward index.

use crate::actor::monitor::MonitorHandle;
use crate::actor::native::Dispatch;
use crate::actor::native::ctx::NativeCtx;
use crate::chassis::builder::Builder;
use crate::mail::KindId;
use crate::mail::MailboxId;
use crate::testing::{TestChassis, await_settled, await_signal, bare_substrate};
use crate::{BootError, NativeActor, NativeInitCtx};
use aether_actor::Addressable;
use crossbeam_channel::Sender;
use std::sync::Arc;

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
    use crate::actor::native::spawn::Subname;
    use aether_actor::ErasedActorRef;
    use aether_actor::HandlesKind;
    use aether_data::Kind;
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
    impl HandlesKind<WatchOrder> for Watcher {}
    impl HandlesKind<aether_kinds::MonitorNotice> for Watcher {}
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
    }
    impl NativeActor for Watcher {
        type State = Self;
    }
    impl Dispatch<Self> for Watcher {
        fn dispatch(
            state: &mut Self,
            ctx: &mut NativeCtx<'_, Self, crate::Manual>,
            kind: KindId,
            payload: &[u8],
        ) -> Option<()> {
            if kind.0 == WatchOrder::ID.0 {
                let order = WatchOrder::decode_from_bytes(payload)?;
                let Ok(target) = ctx.resolve_live(MailboxId(order.target_id)) else {
                    panic!("target must be Live at order time");
                };
                let h = ctx.monitor(target).expect("target must be Live at order time");
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
    use crate::actor::native::spawn::Subname;
    use aether_actor::HandlesKind;
    use aether_data::Kind;
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
    }
    impl NativeActor for Target {
        type State = Self;
    }
    impl Dispatch<Self> for Target {
        fn dispatch(
            _state: &mut Self,
            _ctx: &mut NativeCtx<'_, Self, crate::Manual>,
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
    impl HandlesKind<WatchOrder> for Watcher {}
    impl HandlesKind<Quit> for Watcher {}
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
    }
    impl NativeActor for Watcher {
        type State = Self;
    }
    impl Dispatch<Self> for Watcher {
        fn dispatch(
            state: &mut Self,
            ctx: &mut NativeCtx<'_, Self, crate::Manual>,
            kind: KindId,
            payload: &[u8],
        ) -> Option<()> {
            if kind.0 == WatchOrder::ID.0 {
                let order = WatchOrder::decode_from_bytes(payload)?;
                let Ok(target) = ctx.resolve_live(MailboxId(order.target_id)) else {
                    panic!("target Live");
                };
                let h = ctx.monitor(target).expect("target Live");
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
