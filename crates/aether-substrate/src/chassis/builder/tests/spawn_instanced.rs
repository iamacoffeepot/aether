//! Instanced spawning: an instanced parent hatching an instanced grandchild, and
//! the canonical registered name a finished spawn hands back, which the typed
//! path constructors write from the actor types alone.

use crate::actor::native::Dispatch;
use crate::actor::native::ctx::NativeCtx;
use crate::chassis::builder::Builder;
use crate::mail::KindId;
use crate::mail::MailboxId;
use crate::testing::{TestChassis, await_settled, await_signal, bare_substrate};
use crate::{BootError, NativeActor, NativeInitCtx};
use aether_actor::{ActorPath, Addressable, ChildOf};
use aether_data::LoadName;
use crossbeam_channel::Sender;
use std::sync::Arc;
use std::sync::Mutex;

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
///   2. Its nested canonical name resolves to that id in the registry.
///   3. Grandchild's `after_init` mail dispatches as its first
///      envelope (received counter bumps to 1).
///   4. Closing the parent does NOT cascade-close the grandchild —
///      no parent-child shutdown coupling is wired by default;
///      that's monitor-driven, opt-in.
#[test]
fn instanced_can_spawn_grandchild() {
    use crate::actor::native::spawn::Subname;
    use aether_actor::HandlesKind;
    use aether_data::Kind;
    use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

    // Trigger to make the parent spawn its grandchild.
    pod_kind!(Hatch { tag: u32 }, "test.recursive.hatch", 0xA00A_A00A_A00A_A00A);

    // Pre-loaded onto the grandchild via after_init.
    pod_kind!(Ping { tag: u32 }, "test.recursive.ping", 0xB00B_B00B_B00B_B00B);

    // Self-shutdown trigger for the parent.
    pod_kind!(Quit { tag: u32 }, "test.recursive.quit", 0xC00C_C00C_C00C_C00C);

    // Counts its Pings and signals the test on each: the test holds no
    // proof of the grandchild, so its `after_init` Ping is observed here.
    struct Grandchild {
        received: Arc<AtomicU32>,
        pinged: Sender<()>,
    }
    impl Addressable for Grandchild {
        const NAMESPACE: &'static str = "test.recursive.grandchild";
        type Resolver = aether_actor::Many;
    }
    impl HandlesKind<Ping> for Grandchild {}
    impl aether_actor::Lifecycle<Self> for Grandchild {
        type Config = ();
        type Params = (Arc<AtomicU32>, Sender<()>);
        type InitError = BootError;
        type InitCtx<'a> = NativeInitCtx<'a>;
        type Ctx<'a> = NativeCtx<'a, Self>;
        fn init((): (), (received, pinged): Self::Params, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self { received, pinged })
        }
    }
    impl aether_actor::Declared for Grandchild {
        type Depends = ();
        type Spawns = ();
    }
    impl NativeActor for Grandchild {
        type State = Self;
    }
    impl Dispatch<Self> for Grandchild {
        fn dispatch(
            state: &mut Self,
            _ctx: &mut NativeCtx<'_, Self, crate::Manual>,
            kind: KindId,
            payload: &[u8],
        ) -> Option<()> {
            if kind.0 == Ping::ID.0 {
                let _ = Ping::decode_from_bytes(payload)?;
                state.received.fetch_add(1, AtomicOrdering::SeqCst);
                let _ = state.pinged.send(());
                return Some(());
            }
            None
        }
    }

    struct Parent {
        grandchild_received: Arc<AtomicU32>,
        grandchild_pinged: Sender<()>,
        spawned_name: Arc<Mutex<Option<String>>>,
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
        type Params = (Arc<AtomicU32>, Sender<()>, Arc<Mutex<Option<String>>>);
        type InitError = BootError;
        type InitCtx<'a> = NativeInitCtx<'a>;
        type Ctx<'a> = NativeCtx<'a, Self>;
        fn init(
            (): (),
            (grandchild_received, grandchild_pinged, spawned_name): Self::Params,
            _ctx: &mut NativeInitCtx<'_>,
        ) -> Result<Self, BootError> {
            Ok(Self { grandchild_received, grandchild_pinged, spawned_name })
        }
    }
    impl aether_actor::Declared for Parent {
        type Depends = ();
        type Spawns = ();
    }
    impl NativeActor for Parent {
        type State = Self;
    }
    impl ChildOf<Parent> for Grandchild {}
    impl Dispatch<Self> for Parent {
        fn dispatch(
            state: &mut Self,
            ctx: &mut NativeCtx<'_, Self, crate::Manual>,
            kind: KindId,
            payload: &[u8],
        ) -> Option<()> {
            if kind.0 == Hatch::ID.0 {
                let _ = Hatch::decode_from_bytes(payload)?;
                // Recursive spawn: instanced parent → instanced
                // grandchild. Pre-load a Ping so the grandchild's
                // first envelope dispatches without an external
                // mail step.
                let grandchild_params = (Arc::clone(&state.grandchild_received), state.grandchild_pinged.clone());
                let receipt = ctx
                    .spawn_child::<Grandchild>(Subname::Named("only"), (), grandchild_params)
                    .after_init(Ping { tag: 0xCAFE })
                    .stage()
                    .expect("recursive spawn must succeed");
                *state.spawned_name.lock().expect("spawned-name mutex poisoned") =
                    Some(receipt.canonical_name.to_string());
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
    let (grandchild_pinged, pinged_rx) = crossbeam_channel::unbounded();
    let spawned_name = Arc::new(Mutex::new(None));
    let parent = chassis
        .spawn_actor_for_test::<Parent>(
            Subname::Named("p1"),
            (),
            (Arc::clone(&grandchild_received), grandchild_pinged, Arc::clone(&spawned_name)),
        )
        .finish()
        .expect("spawn parent");
    let parent_id = parent.id();

    // Trigger parent → grandchild spawn. The handler records the staged
    // receipt's name before its root settles.
    let (_, settled) = chassis.send_tracked(parent, &Hatch { tag: 1 }, None);
    await_settled(&settled, "test.recursive.hatch");

    // Wait for the grandchild's after_init Ping to dispatch (proves
    // the recursive spawn happened AND the after_init plumbing
    // works through it).
    await_signal(&pinged_rx, "test.recursive.grandchild_ping");
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
        Some("test.recursive.parent:p1/test.recursive.grandchild:only".to_owned()),
        "the staged receipt must carry the exact nested canonical registration name",
    );

    let p1 = LoadName::new("p1").expect("a valid key");
    let only = LoadName::new("only").expect("a valid key");
    let written = ActorPath::<Grandchild>::child(&ActorPath::<Parent>::instance(&p1), &only).expect("under the caps");
    assert_eq!(
        spawned_name.lock().expect("spawned-name mutex poisoned").as_deref(),
        Some(written.to_string().as_str()),
        "the child constructor must write the name the staged receipt carried",
    );

    assert!(
        chassis.actor_registry().is_live_at(grandchild_id),
        "grandchild should be Live in the registry under the lineage-folded id",
    );

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
    let _ = chassis.send_tracked(parent, &Quit { tag: 1 }, None);
    chassis.await_closed(parent.erase());
    assert!(chassis.actor_registry().is_tombstoned(parent_id), "parent should have tombstoned");
    // Grandchild survives — no cascade.
    assert!(
        chassis.actor_registry().is_live_at(grandchild_id),
        "grandchild should outlive parent (no automatic cascade-close)",
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
        type Ctx<'a> = NativeCtx<'a, Self>;

        fn init((): (), (): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self)
        }
    }
    impl aether_actor::Declared for NamedReturn {
        type Depends = ();
        type Spawns = ();
    }
    impl NativeActor for NamedReturn {
        type State = Self;
    }
    impl Dispatch<Self> for NamedReturn {
        fn dispatch(
            _state: &mut Self,
            _ctx: &mut NativeCtx<'_, Self, crate::Manual>,
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

    let proven = |name: &str| registry.resolve_live(registry.lookup(name).expect("registered name resolves"));
    assert_eq!(proven("test.spawn_name.return:id-only"), Ok(id_only.erase()));
    assert_eq!(canonical_name.as_str(), "test.spawn_name.return:exact-name");
    assert_eq!(proven(canonical_name.as_str()), Ok(named_id.erase()));

    drop(chassis);
}

/// A root instanced actor, spawned by key.
struct KeyedUnit;

impl Addressable for KeyedUnit {
    const NAMESPACE: &'static str = "test.path.unit";
    type Resolver = aether_actor::Many;
}
impl aether_actor::Root for KeyedUnit {}
impl aether_actor::Lifecycle<Self> for KeyedUnit {
    type Config = ();
    type Params = ();
    type InitError = BootError;
    type InitCtx<'a> = NativeInitCtx<'a>;
    type Ctx<'a> = NativeCtx<'a, Self>;

    fn init((): (), (): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self)
    }
}
impl aether_actor::Declared for KeyedUnit {
    type Depends = ();
    type Spawns = ();
}
impl NativeActor for KeyedUnit {
    type State = Self;
}
impl Dispatch<Self> for KeyedUnit {
    fn dispatch(
        _state: &mut Self,
        _ctx: &mut NativeCtx<'_, Self, crate::Manual>,
        _kind: KindId,
        _payload: &[u8],
    ) -> Option<()> {
        None
    }
}

/// `ActorPath::<R>::instance(&key)` writes the name the registry gives the
/// root instance spawned under that key. A constructor that disagreed with the
/// registry, with a wrong separator or a dropped key, would hand `resolve` a
/// path that names no route. The expected text is the registry's own answer.
#[test]
fn instance_writes_the_name_the_registry_gives_the_spawned_instance() {
    use crate::actor::native::spawn::Subname;

    let (registry, mailer) = bare_substrate();
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .build_passive()
        .expect("empty chassis boots");
    let key = LoadName::new("unit-7").expect("a valid key");
    let (_unit, canonical_name) = chassis
        .spawn_actor::<KeyedUnit>(Subname::Named(key.as_str()), (), ())
        .finish_with_name()
        .expect("named spawn succeeds");

    assert_eq!(ActorPath::<KeyedUnit>::instance(&key).to_string(), canonical_name.as_str());

    drop(chassis);
}
