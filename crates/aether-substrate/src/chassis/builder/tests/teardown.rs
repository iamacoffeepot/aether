//! Shutdown and teardown: a handler's own `ctx.shutdown()`, the `unwire` chassis
//! teardown drives on quiet pooled actors, and the panic attribution that has to
//! survive the close gate.

use crate::actor::native::Dispatch;
use crate::actor::native::ctx::NativeCtx;
use crate::chassis::builder::Builder;
use crate::mail::KindId;
use crate::mail::registry;
use crate::testing::{TestChassis, bare_substrate};
use crate::{BootError, NativeActor, NativeInitCtx};
use aether_actor::{Addressable, HandlesKind};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;
use std::time::Instant;

/// Issue 607 Phase 4a verify: `ctx.shutdown()` from inside an
/// instanced actor's handler triggers the drain → unwire → exit
/// path, flips the `actor_registry` slot to `Dead`, and inserts the
/// id into `tombstones`. A reused subname after retirement returns
/// `SpawnError::SubnameRetired`.
#[test]
fn ctx_shutdown_marks_dead_runs_unwire_tombstones_id() {
    use crate::actor::native::spawn::{SpawnError, Subname};
    use crate::mail::registry::MailboxEntry;
    use aether_actor::HandlesKind;
    use aether_data::Kind;
    use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

    pod_kind!(Quit { tag: u32 }, "test.shutdown.quit", 0xE0E1_E2E3_E4E5_E6E7);

    shutdown_on_kind_actor!(Closer, "test.shutdown.closer", Quit);

    let (registry, mailer) = bare_substrate();
    let close_observed = Arc::new(AtomicU32::new(0));
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .build_passive()
        .expect("empty chassis boots");

    let id = chassis
        .spawn_actor::<Closer>(Subname::Counter, (), Arc::clone(&close_observed))
        .finish()
        .expect("spawn instanced actor");

    // Push a Quit envelope at the spawned mailbox via the
    // registered sink handler. The handler's `ctx.shutdown()`
    // flips the dispatcher's flag; after the handler returns the
    // trampoline drains, runs `unwire`, marks Dead, tombstones.
    let MailboxEntry::Inbox { handler, .. } = registry.entry(id).expect("sink registered") else {
        panic!("expected mailbox entry for instanced actor");
    };
    let bytes = (Quit { tag: 1 }).encode_into_bytes();
    handler.enqueue(registry::test_owned_dispatch(<Quit as Kind>::ID, &bytes, 1));

    // Wait for unwire to run + the registry slot to flip Dead.
    let deadline = Instant::now() + Duration::from_millis(500);
    while close_observed.load(AtomicOrdering::SeqCst) == 0 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        close_observed.load(AtomicOrdering::SeqCst),
        1,
        "unwire fired exactly once after the dispatcher drained"
    );
    // Spin until the slot transitions Dead — the dispatcher
    // thread runs `mark_dead` after `unwire`, so there's a
    // small window between the close-observed bump above and the
    // registry update.
    let deadline = Instant::now() + Duration::from_millis(500);
    while chassis.actor_registry().is_live(id) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert!(!chassis.actor_registry().is_live(id), "registry slot should transition Live → Dead after unwire runs");
    assert!(chassis.actor_registry().is_tombstoned(id), "tombstone insertion forbids reuse of the retired full name");

    // Spawning again under the same `Subname::Counter` would
    // increment the per-Spawner counter (so it'd target a fresh
    // id, not collide); reuse the same `Named` subname to land
    // back at the tombstoned id.
    let err = chassis
        .spawn_actor::<Closer>(Subname::Named("0"), (), Arc::clone(&close_observed))
        .finish()
        .expect_err("retired subname must reject");
    assert!(matches!(err, SpawnError::SubnameRetired { .. }), "expected SubnameRetired, got {err:?}");

    drop(chassis);
}

/// Issue 685: chassis teardown drives `unwire` on every spawned
/// instanced actor, even those that never received a self-shutdown
/// trigger. Pre-685 the Pooled spawn path's slot was reachable
/// from the chassis only through the wake's `Weak`, and nothing
/// signaled shutdown at chassis exit — so spawned actors silently
/// skipped their close path. The Spawner's `shutdown_instanced`
/// step now signals + wakes every spawned slot before the pool
/// drops, and the chassis waits for each `Drainable::is_closed`.
#[test]
fn chassis_teardown_runs_unwire_for_pooled_spawned_actors() {
    use crate::actor::native::spawn::Subname;

    use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

    close_observed_actor!(Quiet, "test.teardown.quiet");

    let (registry, mailer) = bare_substrate();
    let close_observed = Arc::new(AtomicU32::new(0));
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .build_passive()
        .expect("empty chassis boots");

    let id = chassis
        .spawn_actor::<Quiet>(Subname::Counter, (), Arc::clone(&close_observed))
        .finish()
        .expect("spawn instanced actor");

    // No mail at all — the actor sits idle from the moment it
    // spawns. Pre-685 chassis teardown skipped its close path
    // entirely; post-685 the teardown step signals + wakes it and
    // the worker runs the close cycle before the pool drops.
    assert_eq!(close_observed.load(AtomicOrdering::SeqCst), 0);

    drop(chassis);

    assert_eq!(
        close_observed.load(AtomicOrdering::SeqCst),
        1,
        "chassis teardown must drive unwire exactly once for a quiet spawned actor",
    );
    // Drop the unused id binding so clippy stays quiet — its
    // referent (the actor_registry's Live entry) drops with the
    // chassis above.
    let _ = id;
}

/// Issue 714: stress version of the chassis-teardown contract.
/// Spawn N=64 instanced actors and assert all N `close_observed`
/// counters tick to exactly 1 after `drop(chassis)`. Pre-714 the
/// polling-based `shutdown_instanced` could lose individual wakes
/// under contention; the channel-signal rewrite is deterministic
/// — even one missed `unwire` here fails the test.
#[test]
fn chassis_teardown_runs_unwire_for_many_pooled_actors() {
    use crate::actor::native::spawn::Subname;

    use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

    close_observed_actor!(Quiet, "test.teardown.quiet_many");

    const N: usize = 64;

    let (registry, mailer) = bare_substrate();
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .build_passive()
        .expect("empty chassis boots");

    let counters: Vec<Arc<AtomicU32>> = (0..N).map(|_| Arc::new(AtomicU32::new(0))).collect();
    for (i, counter) in counters.iter().enumerate() {
        let name = format!("inst-{i}");
        chassis
            .spawn_actor::<Quiet>(Subname::Named(&name), (), Arc::clone(counter))
            .finish()
            .expect("spawn instanced actor");
    }

    for counter in &counters {
        assert_eq!(counter.load(AtomicOrdering::SeqCst), 0);
    }

    drop(chassis);

    for (i, counter) in counters.iter().enumerate() {
        assert_eq!(counter.load(AtomicOrdering::SeqCst), 1, "actor {i} must have run unwire exactly once");
    }
}

// Tripwire: a handler panic must reach chassis teardown as the abort
// reason it started as, never as an anonymous close-gate timeout.
//
// The panic escalates through the pool worker's `FatalAborter`, and under
// `PanicAborter` that unwinds the worker mid-turn — so the close-done
// signal teardown waits on has no thread left to fire it, and the gate
// used to wait out its whole budget and report nothing but the wait
// (iamacoffeepot/aether#4193). iamacoffeepot/aether#3752 was triaged as a
// listener bring-up stall on exactly that evidence, for two CI cycles,
// when the cause was a panicking capture closure. The teardown budget is
// squeezed to two seconds here so a regression fails on the message in
// seconds rather than hanging out the five-minute default.
#[test]
fn teardown_reports_the_handler_panic_that_aborted_the_chassis() {
    use crate::actor::native::spawn::Subname;
    use crate::mail::registry::MailboxEntry;
    use crate::runtime::lifecycle::FatalAborter;
    use aether_data::Kind;
    use std::panic::{AssertUnwindSafe, catch_unwind};

    pod_kind!(Boom { tag: u32 }, "test.abort_attribution.boom", 0xB00B_0001_B00B_0001);

    struct Exploder;

    impl Addressable for Exploder {
        const NAMESPACE: &'static str = "test.abort_attribution.exploder";
        type Resolver = aether_actor::Many;
    }
    impl aether_actor::Root for Exploder {}
    impl HandlesKind<Boom> for Exploder {}

    impl aether_actor::Lifecycle<Self> for Exploder {
        type Config = ();
        type Params = ();
        type InitError = BootError;
        type InitCtx<'a> = NativeInitCtx<'a>;
        type Ctx<'a> = NativeCtx<'a>;

        fn init((): (), (): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self)
        }
    }

    impl NativeActor for Exploder {
        type State = Self;
    }

    impl Dispatch<Self> for Exploder {
        fn dispatch(
            _state: &mut Self,
            _ctx: &mut NativeCtx<'_, crate::Manual, Self>,
            kind: KindId,
            _payload: &[u8],
        ) -> Option<()> {
            assert!(kind != Boom::ID, "exploder handler detonated");
            None
        }
    }

    /// A `PanicAborter` that flags the abort before panicking, so the
    /// test observes the escalation without racing it: the recorder runs
    /// ahead of the aborter it wraps, so a set flag means the reason is
    /// already on the chassis's record.
    struct FlaggingAborter {
        aborted: Arc<AtomicBool>,
    }

    impl FatalAborter for FlaggingAborter {
        fn abort(&self, reason: String) -> ! {
            self.aborted.store(true, Ordering::SeqCst);
            panic!("aether-substrate fatal abort: {reason}");
        }
    }

    let (registry, mailer) = bare_substrate();
    let aborted = Arc::new(AtomicBool::new(false));
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), mailer)
        .with_aborter(Arc::new(FlaggingAborter { aborted: Arc::clone(&aborted) }))
        .with_teardown_budget(Duration::from_secs(2))
        .build_passive()
        .expect("empty chassis boots");

    let id = chassis.spawn_actor::<Exploder>(Subname::Named("boom"), (), ()).finish().expect("spawn exploder");
    let MailboxEntry::Inbox { handler, .. } = registry.entry(id).expect("exploder inbox registered") else {
        panic!("expected spawned actor inbox");
    };
    let boom = Boom { tag: 1 }.encode_into_bytes();
    handler.enqueue(registry::test_owned_dispatch(Boom::ID, &boom, 1));

    let deadline = Instant::now() + Duration::from_secs(10);
    while !aborted.load(Ordering::SeqCst) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert!(aborted.load(Ordering::SeqCst), "the panicking handler must escalate through the chassis aborter");

    let teardown = catch_unwind(AssertUnwindSafe(|| drop(chassis)));
    let reported = *teardown
        .expect_err("teardown must fail once the chassis has fatally aborted")
        .downcast::<String>()
        .expect("the teardown gate fails with a formatted reason");
    assert!(
        reported.contains("exploder handler detonated"),
        "teardown must report the panic that aborted the chassis, got: {reported}",
    );
    assert!(
        reported.contains("shutdown_instanced.close_done"),
        "teardown must still name the gate it abandoned, got: {reported}",
    );
}
