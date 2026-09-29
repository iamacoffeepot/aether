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
use crate::testing::{TestChassis, await_settled, bare_substrate};
use crate::{BootError, NativeActor, NativeInitCtx};
use aether_actor::{Addressable, ChildOf, HandlesKind};
use std::io;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;

/// ADR-0165 scheduler proof: on a real one-worker pool a singleton parent's
/// handler stages a child without waiting, the owner and activation make
/// progress after that turn returns, and the typed completion wakes the parent.
/// Asserts the child's `MailboxId` lands in the chassis's
/// `ActorRegistry` as a Live entry, and that the parent-pre-loaded
/// `after_init` mail dispatches as the child's first envelope.
///
/// Tripwire: the completion arm addresses the newborn child through the
/// proof its `SpawnOutcome` carries. A completion whose reference named any
/// position other than the registered child would warn-drop the mail instead
/// of delivering it (the second element of `child_received` disappears).
#[test]
fn ctx_spawn_child_routes_through_handler() {
    use crate::actor::native::spawn::Subname;
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
        type Ctx<'a> = NativeCtx<'a, Self>;

        fn init((): (), received: Self::Params, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self { received })
        }
    }
    impl aether_actor::Declared for ChildCap {
        type Depends = ();
        type Spawns = ();
    }
    impl NativeActor for ChildCap {
        type State = Self;
    }
    impl Dispatch<Self> for ChildCap {
        fn dispatch(
            state: &mut Self,
            _ctx: &mut NativeCtx<'_, Self, crate::Manual>,
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
        registry: Arc<registry::Registry>,
    }
    impl Addressable for ParentCap {
        const NAMESPACE: &'static str = "test.spawn_child.parent";
        type Resolver = aether_actor::One;
    }
    impl aether_actor::Root for ParentCap {}
    impl HandlesKind<Hatch> for ParentCap {}
    impl aether_actor::Lifecycle<Self> for ParentCap {
        type Config = ();
        type Params = (Arc<AtomicU32>, Arc<AtomicU32>, Arc<Mutex<Vec<u32>>>, Arc<registry::Registry>);
        type InitError = BootError;
        type InitCtx<'a> = NativeInitCtx<'a>;
        type Ctx<'a> = NativeCtx<'a, Self>;
        fn init(
            (): (),
            (spawn_count, failure_count, child_received, registry): Self::Params,
            _ctx: &mut NativeInitCtx<'_>,
        ) -> Result<Self, BootError> {
            Ok(Self { spawn_count, failure_count, child_received, registry })
        }
    }
    impl aether_actor::Declared for ParentCap {
        type Depends = ();
        type Spawns = ();
    }
    impl NativeActor for ParentCap {
        type State = Self;
    }
    impl ChildOf<ParentCap> for ChildCap {}
    impl Dispatch<Self> for ParentCap {
        fn dispatch(
            state: &mut Self,
            ctx: &mut NativeCtx<'_, Self, crate::Manual>,
            kind: KindId,
            payload: &[u8],
        ) -> Option<()> {
            if kind.0 == Hatch::ID.0 {
                let hatch = Hatch::decode_from_bytes(payload)?;
                if hatch.tag == 2 {
                    ctx.spawn_child::<ChildCap>(Subname::Named("conflict"), (), Arc::clone(&state.child_received))
                        .stage()
                        .expect("the conflict is authoritative owner state, not a local preparation failure");
                    return Some(());
                }
                let receipt = ctx
                    .spawn_child::<ChildCap>(Subname::Counter, (), Arc::clone(&state.child_received))
                    .after_init(Ping { tag: 42 })
                    .stage()
                    .expect("spawn_child local preparation must succeed");
                assert!(
                    state.registry.lookup(receipt.canonical_name.as_str()).is_none(),
                    "staging performs no global route write before handler flush"
                );
                let duplicate = ctx
                    .spawn_child::<ChildCap>(Subname::Named("0"), (), Arc::clone(&state.child_received))
                    .stage()
                    .expect_err("the parent-local staged key rejects a duplicate synchronously");
                assert!(matches!(duplicate, crate::SpawnError::SubnameInUse { .. }));
                return Some(());
            }
            if kind == TaskCompletionWake::ID {
                let wake = TaskCompletionWake::decode_from_bytes(payload)?;
                let done = ctx.take_task_done::<crate::SpawnOutcome<ChildCap>, ()>(DispatchId(wake.dispatch_id))?;
                match &done.output().result {
                    Ok(child) => {
                        state.spawn_count.fetch_add(1, AtomicOrdering::SeqCst);
                        ctx.send_to(child, &Ping { tag: 44 });
                    }
                    Err(crate::SpawnError::SubnameInUse { .. }) => {
                        state.failure_count.fetch_add(1, AtomicOrdering::SeqCst);
                    }
                    Err(error) => panic!("unexpected staged-birth completion: {error:?}"),
                }
                drop(done);
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
        .with_actor::<ParentCap>((
            Arc::clone(&spawn_count),
            Arc::clone(&failure_count),
            Arc::clone(&child_received),
            Arc::clone(&registry),
        ))
        .build_passive()
        .expect("ParentCap boots");

    // Send Hatch to the parent; the parent's handler calls
    // `ctx.spawn_child::<ChildCap>` which in turn pushes a Ping at the
    // new child via the after_init bootstrap.
    let parent = chassis.actor_ref::<ParentCap>();
    let parent_id = parent.id();
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
    let (_, hatched) = chassis.send_tracked(parent, &Hatch { tag: 1 }, None);
    let (_, conflicted) = chassis.send_tracked(parent, &Hatch { tag: 2 }, None);
    await_settled(&hatched, "test.spawn_child.hatch");
    await_settled(&conflicted, "test.spawn_child.conflict");
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
        "every chain the handler started settled, including the rejected birth's"
    );
    assert_eq!(
        child_received.lock().unwrap().as_slice(),
        [42, 44],
        "the explicit bootstrap prefix precedes the completion arm's send, and the rendered lineage \
         name the completion arm sent to resolves to the same child"
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
        chassis.actor_registry().is_live_at(child_id),
        "spawned child should be Live in the actor registry under the lineage-folded id"
    );

    drop(chassis);
}

#[test]
fn staged_child_init_failure_releases_parent_reservation_without_registry_write() {
    use crate::actor::native::spawn::{SpawnError, Subname};
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
        type Ctx<'a> = NativeCtx<'a, Self>;

        fn init((): (), attempts: Self::Params, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            attempts.fetch_add(1, AtomicOrdering::SeqCst);
            Err(BootError::Other(Box::new(io::Error::other("intentional staged child init failure"))))
        }
    }
    impl aether_actor::Declared for FailingChild {
        type Depends = ();
        type Spawns = ();
    }
    impl NativeActor for FailingChild {
        type State = Self;
    }
    impl Dispatch<Self> for FailingChild {
        fn dispatch(
            _state: &mut Self,
            _ctx: &mut NativeCtx<'_, Self, crate::Manual>,
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
        type Ctx<'a> = NativeCtx<'a, Self>;

        fn init((): (), (attempts, observed): Self::Params, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self { attempts, observed })
        }
    }
    impl aether_actor::Declared for ParentCap {
        type Depends = ();
        type Spawns = ();
    }
    impl NativeActor for ParentCap {
        type State = Self;
    }
    impl Dispatch<Self> for ParentCap {
        fn dispatch(
            state: &mut Self,
            ctx: &mut NativeCtx<'_, Self, crate::Manual>,
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
    let parent = chassis.actor_ref::<ParentCap>();
    let (_, settled) = chassis.send_tracked(parent, &Hatch { tag: 1 }, None);
    await_settled(&settled, "test.spawn_init_failure.hatch");
    assert!(observed.load(AtomicOrdering::SeqCst), "parent handler completed both local attempts");
    assert_eq!(attempts.load(AtomicOrdering::SeqCst), 2, "init failure releases the parent-local key for retry");
    let child_id = MailboxId(aether_data::with_tag(
        aether_data::Tag::Mailbox,
        aether_data::fold_lineage(parent.id().0, aether_data::ActorId::instanced(FailingChild::NAMESPACE, "retry")),
    ));
    assert!(registry.entry_at(child_id).is_none(), "failed initialization performs no registry write");

    drop(chassis);
}

#[test]
fn ctx_spawn_child_rejects_an_invalid_subname_before_child_init_or_registration() {
    use crate::actor::native::spawn::{SpawnError, Subname};
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
        type Ctx<'a> = NativeCtx<'a, Self>;

        fn init((): (), init_count: Self::Params, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            init_count.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(Self)
        }
    }
    impl aether_actor::Declared for Child {
        type Depends = ();
        type Spawns = ();
    }
    impl NativeActor for Child {
        type State = Self;
    }
    impl Dispatch<Self> for Child {
        fn dispatch(
            _state: &mut Self,
            _ctx: &mut NativeCtx<'_, Self, crate::Manual>,
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
        type Ctx<'a> = NativeCtx<'a, Self>;

        fn init(
            (): (),
            (init_count, invalid_subname_observed): Self::Params,
            _ctx: &mut NativeInitCtx<'_>,
        ) -> Result<Self, BootError> {
            Ok(Self { init_count, invalid_subname_observed })
        }
    }
    impl aether_actor::Declared for ActualParent {
        type Depends = ();
        type Spawns = ();
    }
    impl NativeActor for ActualParent {
        type State = Self;
    }
    impl Dispatch<Self> for ActualParent {
        fn dispatch(
            state: &mut Self,
            ctx: &mut NativeCtx<'_, Self, crate::Manual>,
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

    let (_, settled) = chassis.send_tracked(chassis.actor_ref::<ActualParent>(), &Hatch { tag: 1 }, None);
    await_settled(&settled, "test.checked_spawn.hatch");

    // Tripwire: `HandlerSpawnBuilder::stage` validates the named subname
    // *first*, before anything the birth cannot cheaply undo. Every assertion
    // below names one such effect, so moving the check later — behind
    // `A::init`, the counter, or the registry write —
    // fails here rather than leaking a half-born actor on a typo.
    assert!(invalid_subname_observed.load(AtomicOrdering::SeqCst), "an invalid named subname must be rejected locally");
    assert_eq!(init_count.load(AtomicOrdering::SeqCst), 0, "an invalid subname must not construct or init the child");
    assert_eq!(
        chassis.booted.spawner.next_counter(),
        0,
        "an invalid subname must be rejected before allocating a counter"
    );
    assert!(
        registry.lookup("test.checked_spawn.actual/test.checked_spawn.child:invalid:name").is_none(),
        "an invalid subname must not mutate the mailbox registry",
    );

    drop(chassis);
}
