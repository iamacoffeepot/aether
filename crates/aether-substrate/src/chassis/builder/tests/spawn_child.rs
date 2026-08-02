//! Staged child births from a singleton parent's handler: the completion that
//! returns through the pool, the reservation a failed child `init` releases,
//! and the subname validation that runs before anything expensive.

use crate::actor::native::ctx::NativeCtx;
use crate::actor::native::{Dispatch, DispatchId, TaskCompletionWake};
use crate::chassis::builder::Builder;
use crate::mail::KindId;
use crate::mail::MailboxId;
use crate::mail::registry;
use crate::testing::boot_authority;
use crate::testing::{TestChassis, bare_substrate};
use crate::{BootError, NativeActor, NativeInitCtx};
use aether_actor::{Addressable, ChildOf, HandlesKind};
use std::io;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::thread;
use std::time::Duration;
use std::time::Instant;

/// ADR-0165 scheduler proof: on a real one-worker pool a singleton parent's
/// handler stages a child without waiting, the owner and activation make
/// progress after that turn returns, and the typed completion wakes the parent.
/// Asserts the child's `MailboxId` lands in the chassis's
/// `ActorRegistry` as a Live entry, and that the parent-pre-loaded
/// `after_init` mail dispatches as the child's first envelope.
#[test]
fn ctx_spawn_child_routes_through_handler() {
    use crate::actor::native::spawn::Subname;
    use crate::mail::registry::MailboxEntry;
    use aether_actor::HandlesKind;
    use aether_data::Kind;
    use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

    pod_kind!(Hatch { tag: u32 }, "test.spawn_child.hatch", 0xC0C1_C2C3_C4C5_C6C7);

    pod_kind!(Ping { tag: u32 }, "test.spawn_child.ping", 0xD0D1_D2D3_D4D5_D6D7);

    struct ChildCap {
        received: Arc<Mutex<Vec<u32>>>,
    }
    impl Addressable for ChildCap {
        const NAMESPACE: &'static str = "test.spawn_child.child";
        type Resolver = aether_actor::Many;
    }
    impl HandlesKind<Ping> for ChildCap {}
    impl aether_actor::Lifecycle<Self> for ChildCap {
        type Config = ();
        type Params = Arc<Mutex<Vec<u32>>>;
        type InitError = BootError;
        type InitCtx<'a> = NativeInitCtx<'a>;
        type Ctx<'a> = NativeCtx<'a>;

        fn init((): (), received: Self::Params, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self { received })
        }
    }
    impl NativeActor for ChildCap {
        type State = Self;
    }
    impl Dispatch<Self> for ChildCap {
        fn dispatch(
            state: &mut Self,
            _ctx: &mut NativeCtx<'_, crate::Manual, Self>,
            kind: KindId,
            payload: &[u8],
        ) -> Option<()> {
            if kind != Ping::ID {
                return None;
            }
            state.received.lock().unwrap().push(Ping::decode_from_bytes(payload)?.tag);
            Some(())
        }
    }

    struct ParentCap {
        spawn_count: Arc<AtomicU32>,
        failure_count: Arc<AtomicU32>,
        child_received: Arc<Mutex<Vec<u32>>>,
    }
    impl Addressable for ParentCap {
        const NAMESPACE: &'static str = "test.spawn_child.parent";
        type Resolver = aether_actor::One;
    }
    impl aether_actor::Root for ParentCap {}
    impl HandlesKind<Hatch> for ParentCap {}
    impl aether_actor::Lifecycle<Self> for ParentCap {
        type Config = ();
        type Params = (Arc<AtomicU32>, Arc<AtomicU32>, Arc<Mutex<Vec<u32>>>);
        type InitError = BootError;
        type InitCtx<'a> = NativeInitCtx<'a>;
        type Ctx<'a> = NativeCtx<'a>;
        fn init(
            (): (),
            (spawn_count, failure_count, child_received): Self::Params,
            _ctx: &mut NativeInitCtx<'_>,
        ) -> Result<Self, BootError> {
            Ok(Self { spawn_count, failure_count, child_received })
        }
    }
    impl NativeActor for ParentCap {
        type State = Self;
    }
    impl ChildOf<ParentCap> for ChildCap {}
    impl Dispatch<Self> for ParentCap {
        fn dispatch(
            state: &mut Self,
            ctx: &mut NativeCtx<'_, crate::Manual, Self>,
            kind: KindId,
            payload: &[u8],
        ) -> Option<()> {
            if kind.0 == Hatch::ID.0 {
                let hatch = Hatch::decode_from_bytes(payload)?;
                if hatch.tag == 2 {
                    let receipt = ctx
                        .spawn_child::<ChildCap>(Subname::Named("conflict"), (), Arc::clone(&state.child_received))
                        .stage()
                        .expect("the conflict is authoritative owner state, not a local preparation failure");
                    let _ =
                        ctx.send_envelope_tracked(receipt.mailbox_id, Ping::ID, &Ping { tag: 99 }.encode_into_bytes());
                    return Some(());
                }
                let receipt = ctx
                    .spawn_child::<ChildCap>(Subname::Counter, (), Arc::clone(&state.child_received))
                    .after_init(Ping { tag: 42 })
                    .stage()
                    .expect("spawn_child local preparation must succeed");
                assert!(
                    ctx.mailer().registry().entry(receipt.mailbox_id).is_none(),
                    "staging performs no global route write before handler flush"
                );
                let duplicate = ctx
                    .spawn_child::<ChildCap>(Subname::Named("0"), (), Arc::clone(&state.child_received))
                    .stage()
                    .expect_err("the parent-local staged key rejects a duplicate synchronously");
                assert!(matches!(duplicate, crate::SpawnError::SubnameInUse { .. }));
                let _ = ctx.send_envelope_tracked(receipt.mailbox_id, Ping::ID, &Ping { tag: 43 }.encode_into_bytes());
                return Some(());
            }
            if kind == TaskCompletionWake::ID {
                let wake = TaskCompletionWake::decode_from_bytes(payload)?;
                let done = ctx.take_task_done::<crate::SpawnOutcome, ()>(DispatchId(wake.dispatch_id))?;
                match &done.output().result {
                    Ok(()) => {
                        state.spawn_count.fetch_add(1, AtomicOrdering::SeqCst);
                    }
                    Err(crate::SpawnError::SubnameInUse { .. }) => {
                        state.failure_count.fetch_add(1, AtomicOrdering::SeqCst);
                    }
                    Err(error) => panic!("unexpected staged-birth completion: {error:?}"),
                }
                done.release_no_reply();
                return Some(());
            }
            None
        }
    }

    let (registry, mailer) = bare_substrate();
    let spawn_count = Arc::new(AtomicU32::new(0));
    let failure_count = Arc::new(AtomicU32::new(0));
    let child_received = Arc::new(Mutex::new(Vec::new()));

    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<ParentCap>((Arc::clone(&spawn_count), Arc::clone(&failure_count), Arc::clone(&child_received)))
        .build_passive()
        .expect("ParentCap boots");

    // Push Hatch at the parent's mailbox; the parent's handler
    // calls `ctx.spawn_child::<ChildCap>` which in turn pushes a
    // Ping at the new child via the after_init bootstrap.
    let parent_id = registry.lookup(<ParentCap as Addressable>::NAMESPACE).expect("ParentCap claimed");
    let MailboxEntry::Inbox { handler, .. } = registry.entry(parent_id).expect("sink") else {
        panic!("expected mailbox entry");
    };
    let conflict_id = MailboxId(aether_data::with_tag(
        aether_data::Tag::Mailbox,
        aether_data::fold_lineage(parent_id.0, aether_data::ActorId::instanced("test.spawn_child.child", "conflict")),
    ));
    registry
        .try_register_inbox_with_id(
            &boot_authority(),
            conflict_id,
            "test.spawn_child.parent/test.spawn_child.child:conflict".to_owned(),
            registry::noop_handler(),
        )
        .expect("fixture owns the authoritative conflicting route");
    let bytes = (Hatch { tag: 1 }).encode_into_bytes();
    handler.enqueue(registry::test_owned_dispatch(<Hatch as Kind>::ID, &bytes, 1));
    let conflict = (Hatch { tag: 2 }).encode_into_bytes();
    handler.enqueue(registry::test_owned_dispatch(<Hatch as Kind>::ID, &conflict, 1));

    let deadline = Instant::now() + Duration::from_millis(500);
    while (child_received.lock().unwrap().len() < 2
        || spawn_count.load(AtomicOrdering::SeqCst) < 1
        || failure_count.load(AtomicOrdering::SeqCst) < 1)
        && Instant::now() < deadline
    {
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        spawn_count.load(AtomicOrdering::SeqCst),
        1,
        "the staged birth's authoritative completion returned to the parent on one worker"
    );
    assert_eq!(
        failure_count.load(AtomicOrdering::SeqCst),
        1,
        "an authoritative apply conflict returns exactly one typed TaskDone failure"
    );
    assert_eq!(
        mailer.trace_handle().settlement_counter().live_roots(),
        0,
        "authoritative rejection settles same-flush tracked mail retained with the failed birth"
    );
    assert_eq!(
        child_received.lock().unwrap().as_slice(),
        [42, 43],
        "the explicit bootstrap prefix precedes same-flush child mail"
    );

    // Child is Live in the chassis's actor registry under the
    // ADR-0099 §3 lineage fold: the parent is a root cap (depth-1,
    // carry == id), so the child's id folds the child node's ActorId
    // onto the parent's id — not the flat `hash(NAMESPACE:subname)`.
    let child_id = MailboxId(aether_data::with_tag(
        aether_data::Tag::Mailbox,
        aether_data::fold_lineage(parent_id.0, aether_data::ActorId::instanced("test.spawn_child.child", "0")),
    ));
    assert!(
        chassis.actor_registry().is_live(child_id),
        "spawned child should be Live in the actor registry under the lineage-folded id"
    );

    drop(chassis);
}

#[test]
fn staged_child_init_failure_releases_parent_reservation_without_registry_write() {
    use crate::actor::native::spawn::{SpawnError, Subname};
    use crate::mail::registry::MailboxEntry;
    use aether_data::Kind;
    use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

    pod_kind!(Hatch { tag: u32 }, "test.spawn_init_failure.hatch", 0xD1D2_D3D4_D5D6_D7D8);

    struct FailingChild;
    impl Addressable for FailingChild {
        const NAMESPACE: &'static str = "test.spawn_init_failure.child";
        type Resolver = aether_actor::Many;
    }
    impl aether_actor::Lifecycle<Self> for FailingChild {
        type Config = ();
        type Params = Arc<AtomicU32>;
        type InitError = BootError;
        type InitCtx<'a> = NativeInitCtx<'a>;
        type Ctx<'a> = NativeCtx<'a>;

        fn init((): (), attempts: Self::Params, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            attempts.fetch_add(1, AtomicOrdering::SeqCst);
            Err(BootError::Other(Box::new(io::Error::other("intentional staged child init failure"))))
        }
    }
    impl NativeActor for FailingChild {
        type State = Self;
    }
    impl Dispatch<Self> for FailingChild {
        fn dispatch(
            _state: &mut Self,
            _ctx: &mut NativeCtx<'_, crate::Manual, Self>,
            _kind: KindId,
            _payload: &[u8],
        ) -> Option<()> {
            None
        }
    }

    struct ParentCap {
        attempts: Arc<AtomicU32>,
        observed: Arc<AtomicBool>,
    }
    impl Addressable for ParentCap {
        const NAMESPACE: &'static str = "test.spawn_init_failure.parent";
        type Resolver = aether_actor::One;
    }
    impl aether_actor::Root for ParentCap {}
    impl HandlesKind<Hatch> for ParentCap {}
    impl ChildOf<ParentCap> for FailingChild {}
    impl aether_actor::Lifecycle<Self> for ParentCap {
        type Config = ();
        type Params = (Arc<AtomicU32>, Arc<AtomicBool>);
        type InitError = BootError;
        type InitCtx<'a> = NativeInitCtx<'a>;
        type Ctx<'a> = NativeCtx<'a>;

        fn init((): (), (attempts, observed): Self::Params, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self { attempts, observed })
        }
    }
    impl NativeActor for ParentCap {
        type State = Self;
    }
    impl Dispatch<Self> for ParentCap {
        fn dispatch(
            state: &mut Self,
            ctx: &mut NativeCtx<'_, crate::Manual, Self>,
            kind: KindId,
            payload: &[u8],
        ) -> Option<()> {
            if kind != Hatch::ID {
                return None;
            }
            let _ = Hatch::decode_from_bytes(payload)?;
            for _ in 0..2 {
                let error = ctx
                    .spawn_child::<FailingChild>(Subname::Named("retry"), (), Arc::clone(&state.attempts))
                    .stage()
                    .expect_err("the child fixture always fails initialization");
                assert!(matches!(error, SpawnError::InitFailed(_)));
            }
            state.observed.store(true, AtomicOrdering::SeqCst);
            Some(())
        }
    }

    let (registry, mailer) = bare_substrate();
    let attempts = Arc::new(AtomicU32::new(0));
    let observed = Arc::new(AtomicBool::new(false));
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<ParentCap>((Arc::clone(&attempts), Arc::clone(&observed)))
        .build_passive()
        .expect("ParentCap boots");
    let parent_id = registry.lookup(ParentCap::NAMESPACE).expect("ParentCap claimed");
    let MailboxEntry::Inbox { handler, .. } = registry.entry(parent_id).expect("parent sink") else {
        panic!("expected parent inbox")
    };
    let bytes = (Hatch { tag: 1 }).encode_into_bytes();
    handler.enqueue(registry::test_owned_dispatch(Hatch::ID, &bytes, 1));

    let deadline = Instant::now() + Duration::from_millis(500);
    while !observed.load(AtomicOrdering::SeqCst) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert!(observed.load(AtomicOrdering::SeqCst), "parent handler completed both local attempts");
    assert_eq!(attempts.load(AtomicOrdering::SeqCst), 2, "init failure releases the parent-local key for retry");
    let child_id = MailboxId(aether_data::with_tag(
        aether_data::Tag::Mailbox,
        aether_data::fold_lineage(parent_id.0, aether_data::ActorId::instanced(FailingChild::NAMESPACE, "retry")),
    ));
    assert!(registry.entry(child_id).is_none(), "failed initialization performs no registry write");

    drop(chassis);
}

#[test]
fn ctx_spawn_child_rejects_an_invalid_subname_before_child_init_or_registration() {
    use crate::actor::native::spawn::{SpawnError, Subname};
    use crate::mail::registry::MailboxEntry;
    use aether_data::Kind;
    use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

    pod_kind!(Hatch { tag: u32 }, "test.checked_spawn.hatch", 0x4058_0000_0000_0001);

    struct Child;
    impl Addressable for Child {
        const NAMESPACE: &'static str = "test.checked_spawn.child";
        type Resolver = aether_actor::Many;
    }
    impl ChildOf<ActualParent> for Child {}
    impl aether_actor::Lifecycle<Self> for Child {
        type Config = ();
        type Params = Arc<AtomicU32>;
        type InitError = BootError;
        type InitCtx<'a> = NativeInitCtx<'a>;
        type Ctx<'a> = NativeCtx<'a>;

        fn init((): (), init_count: Self::Params, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            init_count.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(Self)
        }
    }
    impl NativeActor for Child {
        type State = Self;
    }
    impl Dispatch<Self> for Child {
        fn dispatch(
            _state: &mut Self,
            _ctx: &mut NativeCtx<'_, crate::Manual, Self>,
            _kind: KindId,
            _payload: &[u8],
        ) -> Option<()> {
            None
        }
    }

    struct ActualParent {
        init_count: Arc<AtomicU32>,
        invalid_subname_observed: Arc<AtomicBool>,
    }
    impl Addressable for ActualParent {
        const NAMESPACE: &'static str = "test.checked_spawn.actual";
        type Resolver = aether_actor::One;
    }
    impl aether_actor::Root for ActualParent {}
    impl HandlesKind<Hatch> for ActualParent {}
    impl aether_actor::Lifecycle<Self> for ActualParent {
        type Config = ();
        type Params = (Arc<AtomicU32>, Arc<AtomicBool>);
        type InitError = BootError;
        type InitCtx<'a> = NativeInitCtx<'a>;
        type Ctx<'a> = NativeCtx<'a>;

        fn init(
            (): (),
            (init_count, invalid_subname_observed): Self::Params,
            _ctx: &mut NativeInitCtx<'_>,
        ) -> Result<Self, BootError> {
            Ok(Self { init_count, invalid_subname_observed })
        }
    }
    impl NativeActor for ActualParent {
        type State = Self;
    }
    impl Dispatch<Self> for ActualParent {
        fn dispatch(
            state: &mut Self,
            ctx: &mut NativeCtx<'_, crate::Manual, Self>,
            kind: KindId,
            payload: &[u8],
        ) -> Option<()> {
            if kind.0 != Hatch::ID.0 {
                return None;
            }
            let _ = Hatch::decode_from_bytes(payload)?;
            let error = ctx
                .spawn_child::<Child>(Subname::Named("invalid:name"), (), Arc::clone(&state.init_count))
                .stage()
                .expect_err("the invalid subname must be rejected");
            if matches!(error, SpawnError::SubnameInvalid(_)) {
                state.invalid_subname_observed.store(true, AtomicOrdering::SeqCst);
            }
            Some(())
        }
    }

    let (registry, mailer) = bare_substrate();
    let init_count = Arc::new(AtomicU32::new(0));
    let invalid_subname_observed = Arc::new(AtomicBool::new(false));
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<ActualParent>((Arc::clone(&init_count), Arc::clone(&invalid_subname_observed)))
        .build_passive()
        .expect("ActualParent boots");

    let parent_id = registry.lookup(ActualParent::NAMESPACE).expect("ActualParent claimed");
    let MailboxEntry::Inbox { handler, .. } = registry.entry(parent_id).expect("parent sink") else {
        panic!("expected parent inbox");
    };
    let bytes = (Hatch { tag: 1 }).encode_into_bytes();
    handler.enqueue(registry::test_owned_dispatch(Hatch::ID, &bytes, 1));

    let deadline = Instant::now() + Duration::from_millis(500);
    while !invalid_subname_observed.load(AtomicOrdering::SeqCst) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }

    // Tripwire: `HandlerSpawnBuilder::stage` validates the named subname
    // *first*, before anything the birth cannot cheaply undo. Every assertion
    // below names one such effect, so moving the check later — behind
    // `A::init`, the counter, the namespace claim, or the registry write —
    // fails here rather than leaking a half-born actor on a typo.
    assert!(invalid_subname_observed.load(AtomicOrdering::SeqCst), "an invalid named subname must be rejected locally");
    assert_eq!(init_count.load(AtomicOrdering::SeqCst), 0, "an invalid subname must not construct or init the child");
    assert_eq!(
        chassis.booted.spawner.next_counter(),
        0,
        "an invalid subname must be rejected before allocating a counter"
    );
    assert!(
        chassis.actor_registry().namespace_owner(Child::NAMESPACE).is_none(),
        "an invalid subname must be rejected before claiming the child namespace",
    );
    assert!(
        registry.lookup("test.checked_spawn.actual/test.checked_spawn.child:invalid:name").is_none(),
        "an invalid subname must not mutate the mailbox registry",
    );

    drop(chassis);
}
