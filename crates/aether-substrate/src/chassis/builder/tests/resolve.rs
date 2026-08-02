//! Actor resolution off the chassis handle: a type mismatch resolves to `None`,
//! and named lookup plus enumeration track instances as they close.

use crate::actor::native::Dispatch;
use crate::actor::native::ctx::NativeCtx;
use crate::chassis::builder::Builder;
use crate::mail::KindId;
use crate::mail::registry;
use crate::testing::{TestChassis, bare_substrate};
use crate::{BootError, NativeActor, NativeInitCtx};
use aether_actor::Addressable;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use std::time::Instant;

/// Issue 607 Phase 5: type mismatch through `resolve_actor` returns
/// `None` rather than a downcast that succeeds against the wrong
/// type. Two instanced types live under different namespaces; a
/// lookup with one type at the other's id mismatches and returns
/// None.
#[test]
fn resolve_actor_returns_none_on_type_mismatch() {
    use crate::actor::native::spawn::Subname;

    struct Foo;
    impl Addressable for Foo {
        const NAMESPACE: &'static str = "test.resolve_mismatch.foo";
        type Resolver = aether_actor::Many;
    }
    impl aether_actor::Root for Foo {}
    impl aether_actor::Lifecycle<Self> for Foo {
        type Config = ();
        type Params = ();
        type InitError = BootError;
        type InitCtx<'a> = NativeInitCtx<'a>;
        type Ctx<'a> = NativeCtx<'a>;
        fn init((): (), _params: (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self)
        }
    }
    impl NativeActor for Foo {
        type State = Self;
    }
    impl Dispatch<Self> for Foo {
        fn dispatch(
            _state: &mut Self,
            _ctx: &mut NativeCtx<'_, crate::Manual, Self>,
            _kind: KindId,
            _payload: &[u8],
        ) -> Option<()> {
            None
        }
    }

    struct Bar;
    impl Addressable for Bar {
        const NAMESPACE: &'static str = "test.resolve_mismatch.bar";
        type Resolver = aether_actor::Many;
    }
    impl aether_actor::Lifecycle<Self> for Bar {
        type Config = ();
        type Params = ();
        type InitError = BootError;
        type InitCtx<'a> = NativeInitCtx<'a>;
        type Ctx<'a> = NativeCtx<'a>;
        fn init((): (), _params: (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self)
        }
    }
    impl NativeActor for Bar {
        type State = Self;
    }
    impl Dispatch<Self> for Bar {
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

    let _ = chassis.spawn_actor::<Foo>(Subname::Named("only"), (), ()).finish().expect("spawn foo");

    // Resolving with the same subname but the wrong type returns
    // None — the namespaces differ so the hashed full names differ
    // and Bar's "only" is just not present. (The TypeId guard
    // would catch a hash collision.)
    assert!(chassis.resolve_actor::<Bar>("only").is_none());

    // resolve_actors::<Bar>() is empty because no Bar instances
    // were spawned, even though a Foo with the same subname exists.
    assert_eq!(chassis.resolve_actors::<Bar>().len(), 0);
    assert_eq!(chassis.resolve_actors::<Foo>().len(), 1);

    drop(chassis);
}

/// Issue 607 Phase 5 verify: `resolve_actor` and `resolve_actors`
/// against a multi-instance fixture. Spawns three instanced actors
/// under one type, asserts:
///   - `resolve_actor::<A>("a")` finds the named instance.
///   - `resolve_actor::<A>("missing")` returns `None`.
///   - `resolve_actors::<A>()` enumerates all three (subname-keyed).
///   - After one closes, the iterator drops to two and the closed
///     name returns `None` from `resolve_actor`.
#[test]
fn resolve_actor_finds_named_instance_resolve_actors_enumerates() {
    use crate::actor::native::spawn::Subname;
    use crate::mail::registry::MailboxEntry;
    use aether_actor::HandlesKind;
    use aether_data::Kind;
    use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

    pod_kind!(Quit { tag: u32 }, "test.resolve.quit", 0xF00D_F00D_F00D_F00D);

    // The `tag` field is set at init from the per-instance config
    // and would be read by handler code; Phase A's resolve_actor
    // returns MailboxId rather than `Arc<Member>` so the tag is no
    // longer externally observable. Kept as an init payload so the
    // spawn path covers the full Config-threaded shape.
    #[allow(dead_code)]
    struct Member {
        tag: u32,
    }
    impl Addressable for Member {
        const NAMESPACE: &'static str = "test.resolve.member";
        type Resolver = aether_actor::Many;
    }
    impl aether_actor::Root for Member {}
    impl HandlesKind<Quit> for Member {}
    impl aether_actor::Lifecycle<Self> for Member {
        type Config = ();
        type Params = u32;
        type InitError = BootError;
        type InitCtx<'a> = NativeInitCtx<'a>;
        type Ctx<'a> = NativeCtx<'a>;
        fn init((): (), tag: Self::Params, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self { tag })
        }
    }
    impl NativeActor for Member {
        type State = Self;
    }
    impl Dispatch<Self> for Member {
        fn dispatch(
            _state: &mut Self,
            ctx: &mut NativeCtx<'_, crate::Manual, Self>,
            kind: KindId,
            payload: &[u8],
        ) -> Option<()> {
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

    let id_a = chassis.spawn_actor::<Member>(Subname::Named("a"), (), 1).finish().expect("spawn a");
    let _id_b = chassis.spawn_actor::<Member>(Subname::Named("b"), (), 2).finish().expect("spawn b");
    let id_c = chassis.spawn_actor::<Member>(Subname::Named("c"), (), 3).finish().expect("spawn c");

    // Issue 629 / Phase A: resolve_actor returns the address
    // (`MailboxId`), not `Arc<A>`. Verify the address resolves and
    // matches the spawn-time id.
    let a_id = chassis.resolve_actor::<Member>("a").expect("a is live");
    assert_eq!(a_id, id_a, "resolve_actor returns the matching MailboxId");

    // Missing subname → None.
    assert!(chassis.resolve_actor::<Member>("missing").is_none(), "unknown subname should return None");

    // resolve_actors enumerates all three. Order is registry-defined
    // (HashMap iteration), so collect into a sorted subname vec for
    // assertions. The Member's per-instance tag is dispatcher-thread
    // owned (Phase A) and not externally observable here; the
    // subname uniquely identifies the instance.
    let mut all: Vec<String> = chassis.resolve_actors::<Member>().into_iter().map(|(name, _id)| name).collect();
    all.sort();
    assert_eq!(
        all,
        vec!["a".to_owned(), "b".to_owned(), "c".to_owned()],
        "resolve_actors should enumerate every Live instance subname",
    );

    // Close c — Quit it through the sink handler. After close,
    // resolve_actors drops to two and resolve_actor::<Member>("c")
    // returns None.
    let MailboxEntry::Inbox { handler, .. } = registry.entry(id_c).expect("c sink registered") else {
        panic!("expected mailbox entry for c");
    };
    handler.enqueue(registry::test_owned_dispatch(<Quit as Kind>::ID, &(Quit { tag: 1 }).encode_into_bytes(), 1));

    // Wait for c's slot to flip Dead.
    let deadline = Instant::now() + Duration::from_millis(500);
    while chassis.actor_registry().is_live(id_c) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }

    assert!(chassis.resolve_actor::<Member>("c").is_none(), "closed instance should disappear from resolve_actor");
    let mut after: Vec<String> = chassis.resolve_actors::<Member>().into_iter().map(|(name, _id)| name).collect();
    after.sort();
    assert_eq!(after, vec!["a".to_owned(), "b".to_owned()], "resolve_actors should drop the closed instance");

    // Counter for unused warning. (`_id_a` / `_id_b` retain their
    // names elsewhere; this guard keeps the compiler happy.)
    let _ = AtomicU32::new(0).load(AtomicOrdering::SeqCst);

    drop(chassis);
}
