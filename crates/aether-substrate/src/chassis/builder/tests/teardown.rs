//! Shutdown and teardown: a handler's own `ctx.shutdown()`, the `unwire` chassis
//! teardown drives on quiet pooled actors, and the panic attribution that has to
//! survive the close gate.

use crate::actor::native::Dispatch;
use crate::actor::native::ctx::NativeCtx;
use crate::actor::native::envelope::Envelope;
use crate::chassis::builder::Builder;
use crate::mail::KindId;
use crate::testing::{TestChassis, await_signal, bare_substrate};
use crate::{BootError, NativeActor, NativeInitCtx};
use aether_actor::{Addressable, HandlesKind};
use crossbeam_channel::Sender;
use std::sync::Arc;
use std::time::Duration;

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
    use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

    pod_kind!(Quit { tag: u32 }, "test.shutdown.quit", 0xE0E1_E2E3_E4E5_E6E7);

    shutdown_on_kind_actor!(Closer, "test.shutdown.closer", Quit);

    let (registry, mailer) = bare_substrate();
    let close_observed = Arc::new(AtomicU32::new(0));
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .build_passive()
        .expect("empty chassis boots");

    let closer = chassis
        .spawn_actor::<Closer>(Subname::Counter, (), Arc::clone(&close_observed))
        .finish()
        .expect("spawn instanced actor");

    // The handler's `ctx.shutdown()` flips the dispatcher's flag; after
    // the handler returns the trampoline drains, runs `unwire`, marks
    // Dead, tombstones, and the close tail stages the route's retirement
    // through the registry owner. `await_closed` returns once that
    // route drop has applied.
    let _ = chassis.send_tracked(closer, &Quit { tag: 1 }, None);
    chassis.await_closed(closer.erase());
    assert_eq!(
        close_observed.load(AtomicOrdering::SeqCst),
        1,
        "unwire fired exactly once after the dispatcher drained"
    );
    assert!(
        !chassis.actor_registry().is_live_at(closer.id()),
        "registry slot should transition Live → Dead after unwire runs"
    );
    assert!(
        chassis.actor_registry().is_tombstoned(closer.id()),
        "tombstone insertion forbids reuse of the retired full name"
    );
    assert!(
        matches!(registry.entry_at(closer.id()), Some(MailboxEntry::Dropped)),
        "a closed actor's route retires to Dropped"
    );

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

/// Issue #7074: `await_closed` returns only once a self-closing pooled
/// actor's route drop has been applied at the registry owner. The close tail
/// queues that drop after the closing chain settles, so a wait that returned
/// on the close cycle alone, or on settlement, would still read the route
/// `Live` here. A second call finds the actor closed and its slot released
/// (issue #7402): it reads the close off the actor registry and waits on the
/// registry barrier alone, where a wait that still looked for the slot would
/// panic, and one that parked for a close-done signal would never return.
#[test]
fn await_closed_returns_once_the_route_drop_applies() {
    use crate::actor::native::spawn::Subname;

    pod_kind!(Quit { tag: u32 }, "test.await_closed.quit", 0x7074_C105_ED00_0001);

    unit_shutdown_actor!(Closer, "test.await_closed.closer", Quit);

    let (registry, mailer) = bare_substrate();
    let chassis = Builder::<TestChassis>::new(registry, mailer).build_passive().expect("empty chassis boots");

    let closer = chassis
        .spawn_actor_for_test::<Closer>(Subname::Named("closer"), (), ())
        .finish()
        .expect("spawn instanced actor");
    assert!(chassis.published_contract(closer.erase()).is_some(), "a spawned actor's route is live");

    let _ = chassis.send_tracked(closer, &Quit { tag: 1 }, None);
    chassis.await_closed(closer.erase());
    assert!(chassis.published_contract(closer.erase()).is_none(), "the closed actor's route drop has applied");

    chassis.await_closed(closer.erase());
    assert!(
        chassis.published_contract(closer.erase()).is_none(),
        "the wait on a released actor leaves its route dropped"
    );
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
        .finish_commit()
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

/// Catches a teardown that goes back to flagging a composed root and
/// releasing its slot with nothing to wake it: a root that received no mail
/// of its own still runs `unwire`, and its name is tombstoned, when its
/// chassis drops (ADR-0247 rule 5).
#[test]
fn chassis_teardown_closes_an_idle_root() {
    use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

    struct IdleRoot {
        unwired: Arc<AtomicU32>,
    }

    #[aether_actor::actor(root)]
    impl NativeActor for IdleRoot {
        const NAMESPACE: &'static str = "test.teardown.idle_root";
        type Config = ();
        type Params = Arc<AtomicU32>;

        fn init((): (), params: Arc<AtomicU32>, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self { unwired: params })
        }

        fn unwire(state: &mut Self, _ctx: &mut NativeCtx<'_>) {
            state.unwired.fetch_add(1, AtomicOrdering::SeqCst);
        }

        #[fallback]
        fn fallback(&mut self, _ctx: &mut NativeCtx<'_>, _env: &Envelope) {
            let _ = self;
        }
    }

    let (registry, mailer) = bare_substrate();
    let unwired = Arc::new(AtomicU32::new(0));
    let chassis = Builder::<TestChassis>::new(registry, mailer)
        .with_actor::<IdleRoot>(Arc::clone(&unwired))
        .build_passive()
        .expect("a chassis with one root boots");
    let root = chassis.actor_ref::<IdleRoot>();
    let actor_registry = Arc::clone(chassis.actor_registry());
    assert_eq!(unwired.load(AtomicOrdering::SeqCst), 0, "a live root has not run unwire");

    drop(chassis);

    assert_eq!(unwired.load(AtomicOrdering::SeqCst), 1, "teardown runs an idle root's unwire exactly once");
    assert!(actor_registry.is_tombstoned(root.id()), "the root's close ran the registry tail");
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
    impl HandlesKind<Boom> for Exploder {
        type Sender = aether_actor::Anyone;
    }

    impl aether_actor::Lifecycle<Self> for Exploder {
        type Config = ();
        type Params = ();
        type InitError = BootError;
        type InitCtx<'a> = NativeInitCtx<'a>;
        type Ctx<'a> = NativeCtx<'a, Self>;

        fn init((): (), (): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self)
        }
    }

    impl aether_actor::Declared for Exploder {
        type Depends = ();
        type Spawns = ();
        type Parents = ();
    }
    impl NativeActor for Exploder {
        type State = Self;
    }

    impl Dispatch<Self> for Exploder {
        fn dispatch(
            _state: &mut Self,
            _ctx: &mut NativeCtx<'_, Self, crate::Unchecked>,
            kind: KindId,
            _payload: &[u8],
        ) -> Option<()> {
            assert!(kind != Boom::ID, "exploder handler detonated");
            None
        }
    }

    /// A `PanicAborter` that signals the abort before panicking, so the
    /// test observes the escalation without racing it: the recorder runs
    /// ahead of the aborter it wraps, so a received signal means the
    /// reason is already on the chassis's record.
    struct FlaggingAborter {
        aborted: Sender<()>,
    }

    impl FatalAborter for FlaggingAborter {
        fn abort(&self, reason: String) -> ! {
            let _ = self.aborted.send(());
            panic!("aether-substrate fatal abort: {reason}");
        }
    }

    let (registry, mailer) = bare_substrate();
    let (aborted, aborted_rx) = crossbeam_channel::unbounded();
    let chassis = Builder::<TestChassis>::new(registry, mailer)
        .with_aborter(Arc::new(FlaggingAborter { aborted }))
        .with_teardown_budget(Duration::from_secs(2))
        .build_passive()
        .expect("empty chassis boots");

    let exploder = chassis.spawn_actor::<Exploder>(Subname::Named("boom"), (), ()).finish().expect("spawn exploder");
    let _ = chassis.send_tracked(exploder, &Boom { tag: 1 }, None);
    await_signal(&aborted_rx, "test.abort_attribution.aborted");

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
