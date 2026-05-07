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
//!
//! ## Catch-all caps (issue 576)
//!
//! Caps that fan-out every kind they're addressed at — broadcast
//! today, hub-as-actor in the future — author with a `#[fallback]`
//! method instead of `#[handler]`s. The macro emits a blanket
//! `impl<K: Kind> HandlesKind<K> for X {}` so typed sends like
//! `ctx.actor::<BroadcastCapability>().send(&payload)` compile for every K,
//! and overrides [`NativeDispatch::__aether_dispatch_fallback`] to
//! route every envelope through the user's fallback method. Hybrid
//! shape (typed handlers + fallback as a runtime safety net) is
//! rejected by the macro: strict receivers shouldn't silently swallow
//! unknown kinds.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::Arc;

use aether_actor::{Actor, ActorMailbox, HandlesKind, MailCtx, Sender, Singleton};
use aether_data::{Kind, mailbox_id_from_name};

use crate::mail::KindId;

use crate::capability::{BootError, Envelope};
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

    /// Issue 607 Phase 4a (ADR-0079): last-chance close hook. Runs
    /// after the dispatcher's inbox drain, before the actor value
    /// drops. Triggers:
    ///
    /// - Self-shutdown — actor's handler called `ctx.shutdown()`;
    ///   dispatcher saw the flag set after the handler returned.
    /// - Substrate shutdown — chassis dropped its registry, the sink
    ///   handler's `Weak<Sender>` upgrade fails, the inbox channel
    ///   disconnects, and `recv_blocking` returns `None`.
    /// - Cooperative external — a peer mailed the actor a "please
    ///   close" kind; the actor's handler did its cleanup and called
    ///   `ctx.shutdown()`. From the dispatcher's perspective this is
    ///   identical to self-shutdown.
    ///
    /// `&self` matches the handler convention: caps put any mutable
    /// state behind interior mutability. Default empty — opt-in for
    /// caps that need to publish a final broadcast or flush state.
    fn on_close(&self, _ctx: &mut NativeCtx<'_>) {}
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

    /// Catch-all fallback for envelopes whose kind doesn't match any
    /// `#[handler]` (issue 576). Default returns `false` — the
    /// chassis trampoline warn-logs the unknown-kind miss as today.
    /// The `#[actor]` macro overrides this when the impl carries a
    /// `#[fallback]` method, returning `true` after the user's
    /// fallback runs so the trampoline knows to suppress the warn
    /// log.
    fn __aether_dispatch_fallback(&self, _ctx: &mut NativeCtx<'_>, _envelope: &Envelope) -> bool {
        false
    }
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

    /// Local-component origin of the mail currently being dispatched,
    /// or `None` for mail with no local sender (broadcast,
    /// substrate-generated, hub-bubbled). Issue #581's `LogCapability`
    /// reads this to populate `LogEntry::origin` from the envelope
    /// rather than the payload.
    pub fn origin(&self) -> Option<aether_data::MailboxId> {
        match self.sender.target {
            crate::mail::ReplyTarget::Component(id) => Some(id),
            _ => None,
        }
    }

    /// Singleton sender shortcut: returns a typed [`ActorMailbox`]
    /// addressing the unique instance of receiver actor `R`. Mirrors
    /// [`aether_actor::Ctx::actor`]; same `&self`-receiver `send` /
    /// `send_many` ergonomics.
    pub fn actor<R: Singleton>(&self) -> ActorMailbox<'_, R, NativeTransport> {
        ActorMailbox::__new(mailbox_id_from_name(R::NAMESPACE).0, self.transport)
    }

    /// Multi-instance sender: resolve a typed [`ActorMailbox`] from a
    /// runtime instance name. Mirrors [`aether_actor::Ctx::resolve_actor`].
    pub fn resolve_actor<R: Actor>(&self, name: &str) -> ActorMailbox<'_, R, NativeTransport> {
        ActorMailbox::__new(mailbox_id_from_name(name).0, self.transport)
    }

    /// Issue 607 Phase 4a (ADR-0079): self-shutdown signal. Sets a
    /// flag the actor's dispatcher polls after each handler returns;
    /// when set, the trampoline drains any remaining inbox mail
    /// synchronously, runs `NativeActor::on_close`, and exits the
    /// dispatch loop. After exit the actor's [`crate::MailboxId`]
    /// transitions from `Live` to `Dead` in the chassis's
    /// [`crate::ActorRegistry`] and is added to `tombstones` —
    /// `spawn_child` rejects reuse of the retired full name with
    /// `SpawnError::SubnameRetired`.
    ///
    /// Idempotent — flipping the flag twice is the same as flipping
    /// it once. Singletons booted through `with_actor` rely on the
    /// chassis-shutdown channel-drop path instead of this flag, but
    /// can call `shutdown()` to opt in to flag-based exit.
    pub fn shutdown(&self) {
        self.transport.signal_shutdown();
    }

    /// Issue 607 Phase 3b (ADR-0079): spawn an instanced actor as a
    /// child of the calling actor. The new actor's [`crate::ReplyTo`]
    /// stamps the calling actor's mailbox so any reply addressed to
    /// `ReplyTarget::Component` routes back here.
    ///
    /// Returns a [`crate::SpawnBuilder`] the caller chains
    /// `after_init` / `finish` against. Mirrors the chassis-level
    /// `PassiveChassis::spawn_actor` / `BuiltChassis::spawn_actor`
    /// shape; both flow through the same [`crate::Spawner`].
    ///
    /// Panics if the transport was constructed via
    /// [`NativeTransport::new_for_test`] (which doesn't wire a
    /// spawner). Production transports always carry one, so handler
    /// code never reaches the panic.
    pub fn spawn_child<'b, A>(
        &'b self,
        subname: crate::spawn::Subname<'b>,
        config: A::Config,
    ) -> crate::SpawnBuilder<'b, A>
    where
        A: aether_actor::Instanced + NativeActor + crate::NativeDispatch,
    {
        let spawner = self
            .transport
            .spawner()
            .expect("NativeCtx::spawn_child requires a chassis-built transport (no spawner installed — likely a `new_for_test` transport)");
        let sender = ReplyTo {
            target: crate::mail::ReplyTarget::Component(self.transport.self_mailbox()),
            correlation_id: ReplyTo::NO_CORRELATION,
        };
        crate::SpawnBuilder::new(Arc::clone(spawner), subname, config, sender)
    }
}

impl<'a> Sender for NativeCtx<'a> {
    fn send<R, K>(&mut self, payload: &K)
    where
        R: Actor + HandlesKind<K>,
        K: Kind,
    {
        ActorMailbox::<R, NativeTransport>::__new(
            mailbox_id_from_name(R::NAMESPACE).0,
            self.transport,
        )
        .send(payload);
    }

    fn send_many<R, K>(&mut self, payloads: &[K])
    where
        R: Actor + HandlesKind<K>,
        K: Kind + bytemuck::NoUninit,
    {
        ActorMailbox::<R, NativeTransport>::__new(
            mailbox_id_from_name(R::NAMESPACE).0,
            self.transport,
        )
        .send_many(payloads);
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
/// earlier in the chain via [`Self::peer`]), and a clone of the
/// substrate's mailer for caps that need to register an outbound
/// hook at boot.
///
/// `peer::<HandleCapability>()` (the type-keyed `Arc` lookup) is
/// distinct from `actor::<R>()` (the typed sender shortcut, mirror of
/// [`aether_actor::Ctx::actor`]) — peer is for boot-time setup that
/// genuinely needs to inspect a sibling cap's `Arc`-shared state;
/// actor is for sending mail. Memory: "actor::<A>() lookup is
/// bootstrap-only" stays in force for `peer`; messaging is the
/// runtime shape.
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

    /// The actor's own [`MailboxId`] — the deterministic FNV-1a hash
    /// of its full registered name (ADR-0029). For singletons that's
    /// `Actor::NAMESPACE`; for instanced actors it's
    /// `"{NAMESPACE}:{subname}"` (ADR-0079). Init may use this to
    /// publish its own address — e.g. dispatch
    /// `aether.control.subscribe_input { mailbox: ctx.self_id() }`
    /// before registration completes; replies route correctly once the
    /// spawn lifecycle finishes inserting the entry.
    pub fn self_id(&self) -> crate::mail::MailboxId {
        self.transport.self_mailbox()
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
    /// not vice versa). Use cases are limited to boot-time wiring
    /// (driver pre-build, capability chains that genuinely need a
    /// sibling's `Arc`-shared state); for sending mail to a sibling,
    /// reach for [`Self::actor`] / [`Self::resolve_actor`] instead.
    pub fn peer<A: NativeActor>(&self) -> Option<Arc<A>> {
        self.actors.get::<A>()
    }

    /// Singleton sender shortcut: returns a typed [`ActorMailbox`]
    /// addressing the unique instance of receiver actor `R`. Mirrors
    /// [`aether_actor::Ctx::actor`].
    pub fn actor<R: Singleton>(&self) -> ActorMailbox<'_, R, NativeTransport> {
        ActorMailbox::__new(mailbox_id_from_name(R::NAMESPACE).0, self.transport)
    }

    /// Multi-instance sender: resolve a typed [`ActorMailbox`] from a
    /// runtime instance name. Mirrors [`aether_actor::Ctx::resolve_actor`].
    pub fn resolve_actor<R: Actor>(&self, name: &str) -> ActorMailbox<'_, R, NativeTransport> {
        ActorMailbox::__new(mailbox_id_from_name(name).0, self.transport)
    }
}

impl<'a> Sender for NativeInitCtx<'a> {
    fn send<R, K>(&mut self, payload: &K)
    where
        R: Actor + HandlesKind<K>,
        K: Kind,
    {
        ActorMailbox::<R, NativeTransport>::__new(
            mailbox_id_from_name(R::NAMESPACE).0,
            self.transport,
        )
        .send(payload);
    }

    fn send_many<R, K>(&mut self, payloads: &[K])
    where
        R: Actor + HandlesKind<K>,
        K: Kind + bytemuck::NoUninit,
    {
        ActorMailbox::<R, NativeTransport>::__new(
            mailbox_id_from_name(R::NAMESPACE).0,
            self.transport,
        )
        .send_many(payloads);
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
