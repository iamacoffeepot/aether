//! Issue 552 stage 1: native chassis-cap actor surface.
//!
//! The native counterpart of `aether_actor::WasmActor`. Stage 1
//! introduces the type-level vocabulary; Stage 2 migrates the
//! existing capabilities (Log, Handle, Io, Net, Audio, Render) onto
//! it. Stage 1's deliverable is the trait + ctx + dispatch
//! infrastructure plus a working boot path through
//! [`crate::chassis_builder::Builder::with_actor`]. No existing cap
//! changes shape during stage 1 — the legacy `with(cap)` path that
//! takes `Actor + Dispatch` continues to work alongside.
//!
//! ## Shape
//!
//! ```ignore
//! #[capability]
//! #[derive(Singleton)]
//! pub struct ExampleCap { /* state behind interior mutability */ }
//!
//! #[actor]
//! impl NativeActor for ExampleCap {
//!     type Config = ();
//!     const NAMESPACE: &'static str = "aether.example";
//!
//!     fn init(_: (), ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> { … }
//!
//!     #[handler] fn on_hello(&self, ctx: &mut NativeCtx<'_>, mail: Hello) { … }
//! }
//! ```
//!
//! Per-handler `&self` (Arc-shared) — caps put their mutable state
//! behind interior mutability (today: `Arc<Mutex<…>>`,
//! `Arc<HandleStore>`, `crossbeam_queue::ArrayQueue`). The dispatcher
//! borrows `&Arc<Self>` from the chassis [`Actors`] map, builds a
//! per-mail [`NativeCtx`], and calls
//! [`NativeDispatch::dispatch`].
//!
//! ## What does NOT live here
//!
//! - `actor::<A>()` lookups on per-handler ctx. Once dispatchers are
//!   running, caps and components communicate via mail — peering at
//!   sibling state recreates the shared-state coupling the actor
//!   model is designed to eliminate. Lookups live on
//!   [`NativeInitCtx`], `DriverCtx`, and `PassiveChassis` only.
//! - `#[fallback]` on a `NativeActor` impl. Native caps are typed
//!   receivers; an unknown-kind delivery is a programming error,
//!   not a fallback path. The macro rejects this at expansion time.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::Arc;

use aether_actor::{Actor, HandlesKind, MailCtx, Sender};
use aether_data::Kind;

use crate::mail::KindId;

use crate::capability::BootError;
use crate::mail::ReplyTo;
use crate::mailer::Mailer;
use crate::native_transport::NativeTransport;

/// Native chassis-cap actor trait. Per-cap shape: one struct, one
/// `#[actor] impl NativeActor for X` block. The `Config` associated
/// type is moved into [`Self::init`] by the chassis builder; pass
/// `()` for caps with no configuration.
///
/// `: Actor + Send + Sync` ensures `Arc<Self>` is shareable across
/// the dispatcher thread, the chassis lookup map, and any post-init
/// driver/embedder consumer. `Sync` is the new requirement vs
/// pre-552 caps — every existing cap satisfies it (mutable state is
/// behind `Arc<Mutex<…>>` / `Arc<HandleStore>` / atomic counters
/// already), so the migration in stage 2 is mechanical.
pub trait NativeActor: Actor + Sync {
    /// Configuration the chassis builder threads through to
    /// [`Self::init`]. `()` for caps without configuration; the
    /// actual config struct (e.g. `AudioConfig`) for caps that
    /// take one.
    type Config: Send + 'static;

    /// Boot the cap. The chassis has already claimed the cap's
    /// mailbox under `Actor::NAMESPACE` and built a fresh
    /// `NativeTransport` whose self-mailbox is that claim — the
    /// `ctx` exposes those (and the actors-so-far map for boot-time
    /// peer lookups) plus the universal handle-store for caps that
    /// hold typed handles.
    fn init(config: Self::Config, ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError>
    where
        Self: Sized;
}

/// Sum dispatch entry-point — emitted once per `#[actor] impl
/// NativeActor for X` block. Takes the inbound mail's `(kind, payload)`
/// pair, routes by kind id to the right `#[handler]` method, and
/// returns `Some(())` on match + decode success or `None` on unknown
/// kind / decode failure.
///
/// `&self` because the actor lives behind an `Arc<Self>` shared with
/// the chassis lookup map and any post-init driver/embedder consumer.
/// Per-handler-kind compile checks come from
/// [`aether_actor::HandlesKind`] (one impl per handler the macro
/// emits); a future per-K `NativeDispatch<K>` may layer on top if a
/// caller wants a typed `dispatch_kind::<K>` entry, but stage 1
/// doesn't need it.
///
/// Distinct from [`aether_actor::Dispatch`] (the legacy substrate-
/// side dispatch on `&mut self`, used by today's `with(cap)` boot
/// path). Stage 2 migrates caps onto the new entry; for stage 1
/// both coexist.
pub trait NativeDispatch: Send + Sync + 'static {
    fn __aether_dispatch_envelope(
        &self,
        ctx: &mut NativeCtx<'_>,
        kind: KindId,
        payload: &[u8],
    ) -> Option<()>;
}

/// Per-mail context for a [`NativeActor`] handler. Borrows the
/// actor's [`NativeTransport`] for outbound mail and carries the
/// inbound's reply target so [`MailCtx::reply::<K>(&payload)`] can
/// route back to the originator without rethreading the handle.
///
/// Stage 1 ships the wiring; the actual reply routing through
/// [`NativeTransport::reply_mail`] / `Mailer::send_reply` is the
/// stage-2 migration's responsibility (today's caps reply via
/// `mailer.send_reply(...)` directly; stage 2 routes those onto
/// `ctx.reply(...)`).
pub struct NativeCtx<'a> {
    transport: &'a NativeTransport,
    sender: ReplyTo,
}

impl<'a> NativeCtx<'a> {
    /// Internal constructor — the chassis dispatcher trampoline (in
    /// `chassis_builder`) builds these. Cap-side test fixtures in
    /// `aether-capabilities` also reach for it directly so they can
    /// drive a handler without spinning up a full chassis; that's why
    /// it's `pub` rather than `pub(crate)`.
    pub fn new(transport: &'a NativeTransport, sender: ReplyTo) -> Self {
        Self { transport, sender }
    }

    /// Borrow the actor's transport. Exposed for stage-2 caps that
    /// need to call low-level transport helpers the SDK doesn't yet
    /// wrap.
    pub fn transport(&self) -> &NativeTransport {
        self.transport
    }

    /// The reply target for the mail currently being dispatched.
    /// Useful when a handler wants to inspect the originator (audit
    /// trails, multi-tenant routing) without going through
    /// [`MailCtx::reply`]. `target == ReplyTarget::None` means the
    /// inbound was broadcast or peer-component mail with no reply
    /// destination.
    pub fn reply_target(&self) -> ReplyTo {
        self.sender
    }
}

impl<'a> Sender for NativeCtx<'a> {
    fn send<R, K>(&mut self, payload: &K)
    where
        R: Actor + HandlesKind<K>,
        K: Kind,
    {
        aether_actor::resolve_actor::<R, NativeTransport>().send(self.transport, payload);
    }

    fn send_many<R, K>(&mut self, payloads: &[K])
    where
        R: Actor + HandlesKind<K>,
        K: Kind + bytemuck::NoUninit,
    {
        aether_actor::resolve_actor::<R, NativeTransport>().send_many(self.transport, payloads);
    }

    fn send_to_named<K: Kind>(&mut self, name: &str, payload: &K) {
        // resolve_mailbox is generic over T: MailTransport — the
        // const-construction path lands at the same MailboxId as
        // ADR-0029's name hash, so the substrate routes the mail
        // identically on either transport.
        let mailbox = aether_actor::resolve_mailbox::<K, NativeTransport>(name);
        mailbox.send(self.transport, payload);
    }
}

impl<'a> MailCtx for NativeCtx<'a> {
    /// Stage 2: route the reply through the substrate's `Mailer::send_reply`
    /// (Component recipient → push as Mail with the originator's
    /// correlation echoed and reply-to None; Session / EngineMailbox
    /// → outbound bridge; None → silent drop). This is the same path
    /// pre-stage-2 caps walked manually via `self.mailer.send_reply(sender, &result)`
    /// — caps now reach for `ctx.reply(&result)` and the per-mail
    /// ctx already holds both the mailer reference (via the transport)
    /// and the inbound's reply target.
    fn reply<K: Kind + serde::Serialize>(&mut self, payload: &K) {
        self.transport.send_reply_for_handler(self.sender, payload);
    }
}

/// Boot-time context for [`NativeActor::init`]. Carries a borrow of
/// the actor's transport (for init-time mail), a borrow of the
/// actors-so-far [`Actors`] map (so init can peer at caps booted
/// earlier in the chain via [`Self::actor`]), and a clone of the
/// substrate's mailer for caps that need to register an outbound
/// hook at boot.
///
/// Stage-1 doesn't expose the handle store directly — caps that
/// migrate in stage 2 retrieve it through
/// `ctx.actor::<HandleCapability>()` once `HandleCapability` itself
/// has migrated and surfaces an `Arc<HandleStore>` field. Boot order
/// (chain order) ensures the lookup succeeds.
pub struct NativeInitCtx<'a> {
    transport: &'a NativeTransport,
    actors: &'a Actors,
    mailer: Arc<Mailer>,
}

impl<'a> NativeInitCtx<'a> {
    /// Internal constructor — only [`crate::chassis_builder::Builder::with_actor`]
    /// builds these.
    pub(crate) fn new(
        transport: &'a NativeTransport,
        actors: &'a Actors,
        mailer: Arc<Mailer>,
    ) -> Self {
        Self {
            transport,
            actors,
            mailer,
        }
    }

    /// Borrow the cap-bound transport. Caps rarely reach for this —
    /// `Sender::send::<R>(...)` covers the typed-send path — but
    /// stage-2 migrations may want it during init for one-shot
    /// outbound mail.
    pub fn transport(&self) -> &NativeTransport {
        self.transport
    }

    /// Clone the substrate's mailer. Caps that need to register a
    /// `Mailer::set_outbound`-style hook (Hub client, future
    /// fallback routers) reach for this; most caps don't need it.
    pub fn mailer(&self) -> Arc<Mailer> {
        Arc::clone(&self.mailer)
    }

    /// Look up an earlier-booted cap by type and clone its `Arc`.
    /// `None` if the cap hasn't booted yet (chain order matters —
    /// `with::<A>(…)` → `with::<B>(…)` lets B's `init` see A but
    /// not vice versa). Stage-1 use cases are limited to the boot
    /// trampoline itself; stage-2 migrations may use this for
    /// driver pre-build.
    pub fn actor<A: NativeActor>(&self) -> Option<Arc<A>> {
        self.actors.get::<A>()
    }
}

impl<'a> Sender for NativeInitCtx<'a> {
    fn send<R, K>(&mut self, payload: &K)
    where
        R: Actor + HandlesKind<K>,
        K: Kind,
    {
        aether_actor::resolve_actor::<R, NativeTransport>().send(self.transport, payload);
    }

    fn send_many<R, K>(&mut self, payloads: &[K])
    where
        R: Actor + HandlesKind<K>,
        K: Kind + bytemuck::NoUninit,
    {
        aether_actor::resolve_actor::<R, NativeTransport>().send_many(self.transport, payloads);
    }

    fn send_to_named<K: Kind>(&mut self, name: &str, payload: &K) {
        let mailbox = aether_actor::resolve_mailbox::<K, NativeTransport>(name);
        mailbox.send(self.transport, payload);
    }
}

/// Type-keyed map of booted native actors. Owned by
/// `BootedPassives`; borrowed into [`NativeInitCtx`],
/// `DriverCtx`, and `PassiveChassis` via accessor methods. Stage 1's
/// minimal storage — stage 2's actor migrations populate it; stage 3+
/// consumers (drivers, embedders) read from it.
pub struct Actors {
    by_type: HashMap<TypeId, Arc<dyn Any + Send + Sync>>,
}

impl Default for Actors {
    fn default() -> Self {
        Self::new()
    }
}

impl Actors {
    pub fn new() -> Self {
        Self {
            by_type: HashMap::new(),
        }
    }

    /// Insert a freshly-booted `Arc<A>`. The chassis builder calls
    /// this once per `with_actor::<A>(config)` after `A::init`
    /// returns Ok. Returns the prior value (if any) so the builder
    /// can detect double-`with_actor::<A>` and surface a typed
    /// error.
    pub fn insert<A: NativeActor>(&mut self, actor: Arc<A>) -> Option<Arc<dyn Any + Send + Sync>> {
        self.by_type.insert(TypeId::of::<A>(), actor)
    }

    /// Retrieve a clone of the booted `Arc<A>` if `A` has booted.
    /// `None` if `A` hasn't been added yet, or if a different type
    /// happens to share the `TypeId` slot (impossible in safe Rust
    /// modulo unsoundness bugs).
    pub fn get<A: NativeActor>(&self) -> Option<Arc<A>> {
        let any_arc = self.by_type.get(&TypeId::of::<A>())?;
        Arc::downcast::<A>(Arc::clone(any_arc)).ok()
    }

    /// `true` when no actors have booted yet. Useful for tests.
    pub fn is_empty(&self) -> bool {
        self.by_type.is_empty()
    }

    /// Number of booted actors. Useful for tests.
    pub fn len(&self) -> usize {
        self.by_type.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_data::KindId as DataKindId;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Hand-rolled `Actor` impl so the test doesn't depend on the
    /// macro arm (which lands later in the same PR).
    struct StubActor {
        boots: AtomicU32,
    }

    impl Actor for StubActor {
        const NAMESPACE: &'static str = "test.stub";
    }

    impl aether_actor::Singleton for StubActor {}

    impl NativeActor for StubActor {
        type Config = ();
        fn init(_: (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self {
                boots: AtomicU32::new(0),
            })
        }
    }

    /// Second stub actor type so we can exercise type-keyed lookup.
    struct OtherActor;

    impl Actor for OtherActor {
        const NAMESPACE: &'static str = "test.other";
    }

    impl aether_actor::Singleton for OtherActor {}

    impl NativeActor for OtherActor {
        type Config = ();
        fn init(_: (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self)
        }
    }

    #[test]
    fn actors_insert_and_get_roundtrip() {
        let mut actors = Actors::new();
        assert!(actors.is_empty());
        let stub = Arc::new(StubActor {
            boots: AtomicU32::new(0),
        });
        let prior = actors.insert::<StubActor>(Arc::clone(&stub));
        assert!(prior.is_none());
        assert_eq!(actors.len(), 1);

        let retrieved: Arc<StubActor> = actors.get::<StubActor>().expect("StubActor was inserted");
        retrieved.boots.fetch_add(1, Ordering::SeqCst);
        assert_eq!(stub.boots.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn actors_get_distinguishes_types() {
        let mut actors = Actors::new();
        actors.insert::<StubActor>(Arc::new(StubActor {
            boots: AtomicU32::new(0),
        }));
        assert!(actors.get::<StubActor>().is_some());
        assert!(actors.get::<OtherActor>().is_none());
    }

    #[test]
    fn actors_double_insert_returns_prior() {
        let mut actors = Actors::new();
        actors.insert::<StubActor>(Arc::new(StubActor {
            boots: AtomicU32::new(0),
        }));
        let second = actors.insert::<StubActor>(Arc::new(StubActor {
            boots: AtomicU32::new(0),
        }));
        assert!(second.is_some(), "second insert returns the displaced Arc");
    }

    /// Compile-time signal that `NativeActor: Actor + Sync` plus
    /// `Arc<A>` round-trips through `Arc<dyn Any + Send + Sync>`.
    /// If a future change to `Actor` drops `Send + 'static` or to
    /// `NativeActor` drops `Sync`, the asserts here fail to
    /// instantiate.
    fn _assert_arc_shareable() {
        fn requires<T: Any + Send + Sync>() {}
        requires::<StubActor>();
        // Avoid an unused-import diagnostic when the compiler
        // dead-code-eliminates the helper.
        let _ = DataKindId(0);
    }
}
