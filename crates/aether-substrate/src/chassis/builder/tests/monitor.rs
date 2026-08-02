//! Monitor registrations: a notice fires once at the target's close, and a
//! watcher that dies first is pruned from every target's forward index.

use crate::actor::monitor::MonitorHandle;
use crate::actor::native::Dispatch;
use crate::actor::native::ctx::NativeCtx;
use crate::chassis::builder::Builder;
use crate::mail::KindId;
use crate::mail::MailboxId;
use crate::mail::registry;
use crate::testing::{TestChassis, bare_substrate};
use crate::{BootError, NativeActor, NativeInitCtx};
use aether_actor::Addressable;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use std::time::Instant;

/// Issue 607 Phase 4b verify: a `ctx.monitor(target)` registration
/// fires exactly one `MonitorNotice` at the watcher when the
/// target self-shuts. Two-actor scenario: Watcher (instanced)
/// holds a `MonitorHandle` against Target (instanced) and counts
/// the notices it receives; Target self-shuts on `Quit`. After
/// the close fan-out we assert (1) the watcher saw the notice
/// once with the right target id, (2) the target's slot is Dead +
/// tombstoned, and (3) the registry's forward index drained.
#[test]
fn ctx_monitor_fires_notice_at_target_close() {
    use crate::actor::native::spawn::Subname;
    use crate::mail::registry::MailboxEntry;
    use aether_actor::HandlesKind;
    use aether_data::Kind;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU32, AtomicU64, Ordering as AtomicOrdering};

    // Self-shutdown trigger for the target.
    pod_kind!(Quit { tag: u32 }, "test.monitor.quit", 0xC0DE_C0DE_4B4B_4B4B);

    // Tells the watcher which target to monitor. The watcher's
    // handler reads `target_id` and calls `ctx.monitor`.
    pod_kind!(WatchOrder { target_id: u64 }, "test.monitor.watch_order", 0x4B4B_C0DE_C0DE_C0DE);

    // Target — handles Quit by self-shutting.
    unit_shutdown_actor!(Target, "test.monitor.target", Quit);

    // Watcher — handles WatchOrder by registering a monitor;
    // handles MonitorNotice by recording the target id and
    // bumping a counter.
    struct Watcher {
        notice_count: Arc<AtomicU32>,
        last_target: Arc<AtomicU64>,
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
        type Params = (Arc<AtomicU32>, Arc<AtomicU64>);
        type InitError = BootError;
        type InitCtx<'a> = NativeInitCtx<'a>;
        type Ctx<'a> = NativeCtx<'a>;
        fn init((): (), params: Self::Params, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self { notice_count: params.0, last_target: params.1, handle: Mutex::new(None) })
        }
    }
    impl NativeActor for Watcher {
        type State = Self;
    }
    impl Dispatch<Self> for Watcher {
        fn dispatch(
            state: &mut Self,
            ctx: &mut NativeCtx<'_, crate::Manual, Self>,
            kind: KindId,
            payload: &[u8],
        ) -> Option<()> {
            if kind.0 == WatchOrder::ID.0 {
                let order = WatchOrder::decode_from_bytes(payload)?;
                let target = MailboxId(order.target_id);
                let h = ctx.monitor(target).expect("target must be Live at order time");
                *state.handle.lock().unwrap() = Some(h);
                return Some(());
            }
            if kind.0 == <aether_kinds::MonitorNotice as Kind>::ID.0 {
                let notice = <aether_kinds::MonitorNotice as Kind>::decode_from_bytes(payload)?;
                state.last_target.store(notice.target.0, AtomicOrdering::SeqCst);
                state.notice_count.fetch_add(1, AtomicOrdering::SeqCst);
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
    let target_id = chassis.spawn_actor::<Target>(Subname::Counter, (), ()).finish().expect("spawn target");

    let notice_count = Arc::new(AtomicU32::new(0));
    let last_target = Arc::new(AtomicU64::new(0));
    let watcher_id = chassis
        .spawn_actor::<Watcher>(Subname::Counter, (), (Arc::clone(&notice_count), Arc::clone(&last_target)))
        .finish()
        .expect("spawn watcher");

    // Drive the watcher to register the monitor by pushing a
    // WatchOrder through its sink handler. After this returns
    // the watcher's handle is stored in `self.handle`.
    let MailboxEntry::Inbox { handler: watcher_handler, .. } =
        registry.entry(watcher_id).expect("watcher sink registered")
    else {
        panic!("expected mailbox entry for watcher");
    };
    let order = WatchOrder { target_id: target_id.0 };
    watcher_handler.enqueue(registry::test_owned_dispatch(<WatchOrder as Kind>::ID, &order.encode_into_bytes(), 1));

    // Wait until the registry sees the monitor entry.
    let deadline = Instant::now() + Duration::from_millis(500);
    while chassis.actor_registry().monitor_count(target_id) == 0 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        chassis.actor_registry().monitor_count(target_id),
        1,
        "watcher's monitor should be registered against target",
    );
    assert_eq!(chassis.actor_registry().monitoring_count(watcher_id), 1, "watcher should appear in the reverse index");

    // Fire Quit at the target — its handler self-shuts; the
    // dispatcher's close path runs `close_actor`, which fans out
    // a MonitorNotice mail to watcher_id.
    let MailboxEntry::Inbox { handler: target_handler, .. } =
        registry.entry(target_id).expect("target sink registered")
    else {
        panic!("expected mailbox entry for target");
    };
    target_handler.enqueue(registry::test_owned_dispatch(
        <Quit as Kind>::ID,
        &(Quit { tag: 1 }).encode_into_bytes(),
        1,
    ));

    // Wait for the notice to land at the watcher.
    let deadline = Instant::now() + Duration::from_millis(500);
    while notice_count.load(AtomicOrdering::SeqCst) == 0 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(notice_count.load(AtomicOrdering::SeqCst), 1, "watcher should have received exactly one MonitorNotice");
    assert_eq!(
        last_target.load(AtomicOrdering::SeqCst),
        target_id.0,
        "MonitorNotice.target should match the closed actor's id",
    );

    // Wait for target slot to flip Dead (the close path runs
    // close_actor → mark_dead after fan-out).
    let deadline = Instant::now() + Duration::from_millis(500);
    while chassis.actor_registry().is_live(target_id) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert!(
        !chassis.actor_registry().is_live(target_id),
        "target slot should transition Live → Dead after close fan-out",
    );
    assert!(chassis.actor_registry().is_tombstoned(target_id), "target id should be tombstoned");
    // Forward index for target was drained.
    assert_eq!(chassis.actor_registry().monitor_count(target_id), 0, "monitors_of[target] must drain after fan-out");

    drop(chassis);
}

/// Issue 607 Phase 4b verify: when the *watcher* dies first, the
/// reverse-index walk prunes the watcher's entry from each
/// monitored target's `monitors_of`. No `MonitorNotice` fires (the
/// watcher is the one closing; targets are still alive).
#[test]
fn watcher_close_prunes_targets_forward_index() {
    use crate::actor::native::spawn::Subname;
    use crate::mail::registry::MailboxEntry;
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
        type Ctx<'a> = NativeCtx<'a>;
        fn init((): Self::Config, _params: (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self)
        }
    }
    impl NativeActor for Target {
        type State = Self;
    }
    impl Dispatch<Self> for Target {
        fn dispatch(
            _state: &mut Self,
            _ctx: &mut NativeCtx<'_, crate::Manual, Self>,
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
        type Ctx<'a> = NativeCtx<'a>;
        fn init((): (), params: Self::Params, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self { handle: Mutex::new(None), close_observed: params })
        }
        fn unwire(state: &mut Self, _ctx: &mut NativeCtx<'_>) {
            state.close_observed.fetch_add(1, AtomicOrdering::SeqCst);
        }
    }
    impl NativeActor for Watcher {
        type State = Self;
    }
    impl Dispatch<Self> for Watcher {
        fn dispatch(
            state: &mut Self,
            ctx: &mut NativeCtx<'_, crate::Manual, Self>,
            kind: KindId,
            payload: &[u8],
        ) -> Option<()> {
            if kind.0 == WatchOrder::ID.0 {
                let order = WatchOrder::decode_from_bytes(payload)?;
                let target = MailboxId(order.target_id);
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

    let target_id = chassis.spawn_actor::<Target>(Subname::Counter, (), ()).finish().expect("spawn target");
    let close_observed = Arc::new(AtomicU32::new(0));
    let watcher_id = chassis
        .spawn_actor::<Watcher>(Subname::Counter, (), Arc::clone(&close_observed))
        .finish()
        .expect("spawn watcher");

    // Watcher registers monitor against target.
    let MailboxEntry::Inbox { handler: watcher_handler, .. } =
        registry.entry(watcher_id).expect("watcher sink registered")
    else {
        panic!("expected mailbox entry for watcher");
    };
    let order = WatchOrder { target_id: target_id.0 };
    watcher_handler.enqueue(registry::test_owned_dispatch(<WatchOrder as Kind>::ID, &order.encode_into_bytes(), 1));

    // Wait for register to land.
    let deadline = Instant::now() + Duration::from_millis(500);
    while chassis.actor_registry().monitor_count(target_id) == 0 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(chassis.actor_registry().monitor_count(target_id), 1);

    // Quit watcher — its close path walks `monitoring[watcher]` and
    // prunes watcher from `monitors_of[target]`.
    watcher_handler.enqueue(registry::test_owned_dispatch(
        <Quit as Kind>::ID,
        &(Quit { tag: 1 }).encode_into_bytes(),
        1,
    ));

    let deadline = Instant::now() + Duration::from_millis(500);
    while close_observed.load(AtomicOrdering::SeqCst) == 0 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(close_observed.load(AtomicOrdering::SeqCst), 1, "watcher's unwire fired exactly once");

    // Watcher slot tombstones; target slot still Live; target's
    // forward index drained of the dead watcher.
    let deadline = Instant::now() + Duration::from_millis(500);
    while chassis.actor_registry().is_live(watcher_id) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert!(chassis.actor_registry().is_tombstoned(watcher_id), "watcher tombstoned");
    assert!(chassis.actor_registry().is_live(target_id), "target should still be Live (watcher closed, not target)");
    assert_eq!(
        chassis.actor_registry().monitor_count(target_id),
        0,
        "target's monitors_of should drop the dead watcher",
    );

    drop(chassis);
}
