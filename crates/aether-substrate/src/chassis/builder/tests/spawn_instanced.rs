//! Instanced spawning: an instanced parent hatching an instanced grandchild, and
//! the canonical registered name a finished spawn hands back.

use crate::actor::native::Dispatch;
use crate::actor::native::ctx::NativeCtx;
use crate::chassis::builder::Builder;
use crate::mail::KindId;
use crate::mail::MailboxId;
use crate::mail::registry;
use crate::testing::{TestChassis, bare_substrate};
use crate::{BootError, NativeActor, NativeInitCtx};
use aether_actor::{Addressable, ChildOf};
use std::sync::Arc;
use std::sync::Mutex;
use std::thread;
use std::time::Duration;
use std::time::Instant;

/// Issue 607 Phase 5.5 verify: an instanced parent's handler calls
/// `ctx.spawn_child::<Grandchild>(...)` to launch an instanced
/// grandchild. Phase 3b shipped `Arc<Spawner>` threading through
/// every spawned actor's transport precisely so this works; this
/// test is the first end-to-end coverage of the instanced→instanced
/// path. Phase 6b (`TcpListenerActor` → `TcpSessionActor`) structurally
/// depends on this — listeners spawning sessions IS the recursive
/// case.
///
/// Asserts:
///   1. Grandchild's `MailboxId` is `Live` in the registry.
///   2. `chassis.resolve_actor::<Grandchild>(name)` resolves it.
///   3. Grandchild's `after_init` mail dispatches as its first
///      envelope (received counter bumps to 1).
///   4. Closing the parent does NOT cascade-close the grandchild —
///      no parent-child shutdown coupling is wired by default;
///      that's monitor-driven, opt-in.
#[test]
fn instanced_can_spawn_grandchild() {
    use crate::actor::native::spawn::Subname;
    use crate::mail::registry::MailboxEntry;
    use aether_actor::HandlesKind;
    use aether_data::Kind;
    use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

    // Trigger to make the parent spawn its grandchild.
    pod_kind!(Hatch { tag: u32 }, "test.recursive.hatch", 0xA00A_A00A_A00A_A00A);

    // Pre-loaded onto the grandchild via after_init.
    pod_kind!(Ping { tag: u32 }, "test.recursive.ping", 0xB00B_B00B_B00B_B00B);

    // Self-shutdown trigger for the parent.
    pod_kind!(Quit { tag: u32 }, "test.recursive.quit", 0xC00C_C00C_C00C_C00C);

    struct Grandchild {
        received: Arc<AtomicU32>,
    }
    impl Addressable for Grandchild {
        const NAMESPACE: &'static str = "test.recursive.grandchild";
        type Resolver = aether_actor::Many;
    }
    impl HandlesKind<Ping> for Grandchild {}
    impl aether_actor::Lifecycle<Self> for Grandchild {
        type Config = ();
        type Params = Arc<AtomicU32>;
        type InitError = BootError;
        type InitCtx<'a> = NativeInitCtx<'a>;
        type Ctx<'a> = NativeCtx<'a>;
        fn init((): (), params: Self::Params, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self { received: params })
        }
    }
    impl NativeActor for Grandchild {
        type State = Self;
    }
    impl Dispatch<Self> for Grandchild {
        fn dispatch(
            state: &mut Self,
            _ctx: &mut NativeCtx<'_, crate::Manual, Self>,
            kind: KindId,
            payload: &[u8],
        ) -> Option<()> {
            if kind.0 == Ping::ID.0 {
                let _ = Ping::decode_from_bytes(payload)?;
                state.received.fetch_add(1, AtomicOrdering::SeqCst);
                return Some(());
            }
            None
        }
    }

    struct Parent {
        grandchild_received: Arc<AtomicU32>,
        spawned_name: Arc<Mutex<Option<(MailboxId, String)>>>,
    }
    impl Addressable for Parent {
        const NAMESPACE: &'static str = "test.recursive.parent";
        type Resolver = aether_actor::Many;
    }
    impl aether_actor::Root for Parent {}
    impl HandlesKind<Hatch> for Parent {}
    impl HandlesKind<Quit> for Parent {}
    impl aether_actor::Lifecycle<Self> for Parent {
        type Config = ();
        type Params = (Arc<AtomicU32>, Arc<Mutex<Option<(MailboxId, String)>>>);
        type InitError = BootError;
        type InitCtx<'a> = NativeInitCtx<'a>;
        type Ctx<'a> = NativeCtx<'a>;
        fn init(
            (): (),
            (grandchild_received, spawned_name): Self::Params,
            _ctx: &mut NativeInitCtx<'_>,
        ) -> Result<Self, BootError> {
            Ok(Self { grandchild_received, spawned_name })
        }
    }
    impl NativeActor for Parent {
        type State = Self;
    }
    impl ChildOf<Parent> for Grandchild {}
    impl Dispatch<Self> for Parent {
        fn dispatch(
            state: &mut Self,
            ctx: &mut NativeCtx<'_, crate::Manual, Self>,
            kind: KindId,
            payload: &[u8],
        ) -> Option<()> {
            if kind.0 == Hatch::ID.0 {
                let _ = Hatch::decode_from_bytes(payload)?;
                // Recursive spawn: instanced parent → instanced
                // grandchild. Pre-load a Ping so the grandchild's
                // first envelope dispatches without an external
                // mail step.
                let receipt = ctx
                    .spawn_child::<Grandchild>(Subname::Named("only"), (), Arc::clone(&state.grandchild_received))
                    .after_init(Ping { tag: 0xCAFE })
                    .stage()
                    .expect("recursive spawn must succeed");
                *state.spawned_name.lock().expect("spawned-name mutex poisoned") =
                    Some((receipt.mailbox_id, receipt.canonical_name.to_string()));
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

    let grandchild_received = Arc::new(AtomicU32::new(0));
    let spawned_name = Arc::new(Mutex::new(None));
    let parent_id = chassis
        .spawn_actor::<Parent>(Subname::Named("p1"), (), (Arc::clone(&grandchild_received), Arc::clone(&spawned_name)))
        .finish()
        .expect("spawn parent");

    // Trigger parent → grandchild spawn.
    let MailboxEntry::Inbox { handler: parent_handler, .. } =
        registry.entry(parent_id).expect("parent sink registered")
    else {
        panic!("expected mailbox entry for parent");
    };
    parent_handler.enqueue(registry::test_owned_dispatch(
        <Hatch as Kind>::ID,
        &(Hatch { tag: 1 }).encode_into_bytes(),
        1,
    ));

    // Wait for the grandchild's after_init Ping to dispatch (proves
    // the recursive spawn happened AND the after_init plumbing
    // works through it).
    let deadline = Instant::now() + Duration::from_millis(500);
    while (grandchild_received.load(AtomicOrdering::SeqCst) == 0
        || spawned_name.lock().expect("spawned-name mutex poisoned").is_none())
        && Instant::now() < deadline
    {
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        grandchild_received.load(AtomicOrdering::SeqCst),
        1,
        "grandchild's after_init Ping should dispatch as its first envelope",
    );

    // Grandchild is Live under the ADR-0099 §3 lineage fold. The
    // parent was chassis-spawned (no parent → depth-1, carry == id),
    // so the grandchild's id folds its node's ActorId onto the
    // parent's id — not the flat `hash(NAMESPACE:subname)`.
    let grandchild_id = MailboxId(aether_data::with_tag(
        aether_data::Tag::Mailbox,
        aether_data::fold_lineage(parent_id.0, aether_data::ActorId::instanced("test.recursive.grandchild", "only")),
    ));
    assert_eq!(
        *spawned_name.lock().expect("spawned-name mutex poisoned"),
        Some((grandchild_id, "test.recursive.parent:p1/test.recursive.grandchild:only".to_owned())),
        "the staged receipt must carry the exact nested canonical registration name",
    );
    assert!(
        chassis.actor_registry().is_live(grandchild_id),
        "grandchild should be Live in the registry under the lineage-folded id",
    );

    // Issue 629 / Phase A: resolve_actor returns the address.
    // Verify it resolves and matches the registry id.
    let resolved = chassis.resolve_actor::<Grandchild>("only").expect("resolve_actor must find the grandchild");
    assert_eq!(resolved, grandchild_id, "resolve_actor returns the matching MailboxId");
    assert_eq!(
        registry.lookup("test.recursive.parent:p1/test.recursive.grandchild:only"),
        Some(grandchild_id),
        "the recursive child canonical name must extend the captured parent identity",
    );
    // The grandchild is alive (verifies the dispatcher's Arc<AtomicU32>
    // is the same one passed in via config — the test's `received`
    // counter sees handler dispatches against the live instance).
    let _ = &grandchild_received;

    // Closing the parent does NOT cascade-close the grandchild.
    // Parent-child shutdown coupling is opt-in via monitor; without
    // it, the grandchild keeps running.
    parent_handler.enqueue(registry::test_owned_dispatch(
        <Quit as Kind>::ID,
        &(Quit { tag: 1 }).encode_into_bytes(),
        1,
    ));

    // Wait for parent slot to flip Dead.
    let deadline = Instant::now() + Duration::from_millis(500);
    while chassis.actor_registry().is_live(parent_id) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert!(chassis.actor_registry().is_tombstoned(parent_id), "parent should have tombstoned");
    // Grandchild survives — no cascade.
    assert!(
        chassis.actor_registry().is_live(grandchild_id),
        "grandchild should outlive parent (no automatic cascade-close)",
    );
    assert!(
        chassis.resolve_actor::<Grandchild>("only").is_some(),
        "grandchild remains resolvable after parent's death",
    );

    drop(chassis);
}

#[test]
fn spawn_finish_with_name_returns_the_registered_top_level_name() {
    use crate::actor::native::spawn::Subname;

    struct NamedReturn;
    impl Addressable for NamedReturn {
        const NAMESPACE: &'static str = "test.spawn_name.return";
        type Resolver = aether_actor::Many;
    }
    impl aether_actor::Root for NamedReturn {}
    impl aether_actor::Lifecycle<Self> for NamedReturn {
        type Config = ();
        type Params = ();
        type InitError = BootError;
        type InitCtx<'a> = NativeInitCtx<'a>;
        type Ctx<'a> = NativeCtx<'a>;

        fn init((): (), (): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self)
        }
    }
    impl NativeActor for NamedReturn {
        type State = Self;
    }
    impl Dispatch<Self> for NamedReturn {
        fn dispatch(
            _state: &mut Self,
            _ctx: &mut NativeCtx<'_, crate::Manual, Self>,
            _kind: KindId,
            _payload: &[u8],
        ) -> Option<()> {
            None
        }
    }

    let (registry, mailer) = bare_substrate();
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .build_passive()
        .expect("empty chassis boots");

    let id_only =
        chassis.spawn_actor::<NamedReturn>(Subname::Named("id-only"), (), ()).finish().expect("id-only spawn succeeds");
    let (named_id, canonical_name) = chassis
        .spawn_actor::<NamedReturn>(Subname::Named("exact-name"), (), ())
        .finish_with_name()
        .expect("named spawn succeeds");

    assert_eq!(registry.lookup("test.spawn_name.return:id-only"), Some(id_only));
    assert_eq!(canonical_name, "test.spawn_name.return:exact-name");
    assert_eq!(registry.lookup(&canonical_name), Some(named_id));

    drop(chassis);
}
