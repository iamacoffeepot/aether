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
//! pub struct ExampleCap { /* plain fields — single-threaded ownership */ }
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
//! Issue 629 / Phase A: actors are owned by their dispatcher thread
//! as `Box<A>` — the cross-thread `Arc<dyn Any + Send + Sync>` storage
//! is retired. [`NativeDispatch`] takes `&mut self`; `#[handler]`
//! methods can take either `&self` or `&mut self` (Phase B sweeps caps
//! to `&mut self` cap by cap as state migrates off interior mutability).
//!
//! Cross-thread access from drivers / embedders flows through
//! cap-exported sub-handles published in `init` via
//! [`NativeInitCtx::publish_handle`] and retrieved via
//! [`crate::DriverCtx::handle`]. The actor itself never escapes its
//! dispatcher thread.
//!
//! ## What does NOT live here
//!
//! - `actor::<A>()` lookups on per-handler ctx. Once dispatchers are
//!   running, caps and components communicate via mail — peering at
//!   sibling state recreates the shared-state coupling the actor
//!   model is designed to eliminate. The chassis-level
//!   `chassis.actor::<X>() -> Arc<X>` retired with issue 629 / Phase A;
//!   external runtimes (drivers, TestBench, MCP) reach for
//!   cap-exported handles instead.
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
/// Issue 629 / Phase A: bound is `: Actor` only (which gives
/// `Send + 'static`). The dispatcher thread owns the cap as `Box<Self>`
/// for its lifetime — no cross-thread `Arc` share, no `Sync` bound.
/// Cap state can be plain fields without interior-mutability gymnastics
/// once Phase B sweeps each cap.
pub trait NativeActor: Actor {
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
    /// Issue 629 / Phase A: `&mut self` since the dispatcher thread
    /// owns the cap exclusively. Default empty — opt-in for caps that
    /// need to publish a final broadcast or flush state.
    fn on_close(&mut self, _ctx: &mut NativeCtx<'_>) {}
}

/// Sum dispatch entry-point — emitted once per `#[actor] impl
/// NativeActor for X` block. Takes the inbound mail's `(kind, payload)`
/// pair, routes by kind id to the right `#[handler]` method, and
/// returns `Some(())` on match + decode success or `None` on unknown
/// kind / decode failure.
///
/// Issue 629 / Phase A: `&mut self` since the dispatcher thread owns
/// the cap as `Box<Self>` and is the sole entry-point. `: Send +
/// 'static` (no `Sync`) — the cap is not shared across threads.
///
/// Per-handler-kind compile checks come from
/// [`aether_actor::HandlesKind`] (one impl per handler the macro
/// emits); a future per-K `NativeDispatch<K>` may layer on top if a
/// caller wants a typed `dispatch_kind::<K>` entry, but Phase A
/// doesn't need it.
pub trait NativeDispatch: Send + 'static {
    fn __aether_dispatch_envelope(
        &mut self,
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
    fn __aether_dispatch_fallback(
        &mut self,
        _ctx: &mut NativeCtx<'_>,
        _envelope: &Envelope,
    ) -> bool {
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

    /// Issue 607 Phase 4b (ADR-0079): register the calling actor as a
    /// monitor of `target`. Returns a [`MonitorHandle`] whose `Drop`
    /// deregisters the entry, so a handler that wants to unwatch
    /// before the watcher itself dies just drops the handle.
    ///
    /// On the target's close, the substrate drains its monitor list
    /// and fires one [`aether_kinds::MonitorNotice`] per watcher
    /// before the slot transitions `Live` → `Dead`. The watcher
    /// receives that notice as ordinary mail and reads the `target`
    /// field to identify the closing actor.
    ///
    /// Validation: `target` must currently be `Live` in the
    /// [`crate::ActorRegistry`]; tombstoned (closed) and unknown ids
    /// surface as [`crate::MonitorError`]. Singletons today don't sit
    /// in the actor registry as `Live` entries (their entries live in
    /// the routing [`crate::Registry`] only); a future lift inserts
    /// them so monitoring a singleton works the same way. Until then,
    /// monitor only addresses instanced actors.
    ///
    /// Panics if the transport was constructed via
    /// [`NativeTransport::new_for_test`] (no spawner / actor registry
    /// wired). Production transports always carry both.
    pub fn monitor(
        &self,
        target: aether_data::MailboxId,
    ) -> Result<MonitorHandle, crate::actor_registry::MonitorError> {
        let spawner = self
            .transport
            .spawner()
            .expect("NativeCtx::monitor requires a chassis-built transport (no spawner installed — likely a `new_for_test` transport)");
        let registry = Arc::clone(spawner.actor_registry());
        let watcher = self.transport.self_mailbox();
        registry.register_monitor(watcher, target)?;
        Ok(MonitorHandle::new(registry, watcher, target))
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
/// chassis's [`ExportedHandles`] map (so the cap can publish a
/// driver-facing sub-handle via [`Self::publish_handle`]), and a
/// clone of the substrate's mailer for caps that need to register an
/// outbound hook at boot.
///
/// Issue 629 / Phase A: the legacy `peer::<A>() -> Arc<A>` accessor
/// retired here (closes issue 628). Sibling caps communicate via mail
/// at runtime ([`Self::actor`] / [`Self::resolve_actor`] return
/// typed senders). Caps that genuinely need cross-thread state
/// access from drivers / embedders publish a handle bundle via
/// [`Self::publish_handle`] and the consumer retrieves it through
/// [`crate::DriverCtx::handle`].
pub struct NativeInitCtx<'a> {
    transport: &'a NativeTransport,
    handles: &'a mut ExportedHandles,
    mailer: Arc<Mailer>,
}

impl<'a> NativeInitCtx<'a> {
    /// Internal constructor — only [`crate::chassis_builder::Builder::with_actor`]
    /// builds these.
    pub(crate) fn new(
        transport: &'a NativeTransport,
        handles: &'a mut ExportedHandles,
        mailer: Arc<Mailer>,
    ) -> Self {
        Self {
            transport,
            handles,
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

    /// Issue 629 / Phase A: publish a sub-handle bundle for cross-
    /// thread access from drivers / embedders. The handle is stored in
    /// the chassis's [`ExportedHandles`] map keyed by `TypeId::of::<H>`
    /// and retrieved via [`crate::DriverCtx::handle`]. Caps that don't
    /// need driver-side state access never call this.
    ///
    /// `H: Any + Send + Sync` so the chassis-side map can hand the
    /// stored bundle back across thread boundaries; typically `H` is
    /// a `Clone` struct of `Arc`-wrapped fields (e.g. `RenderHandles`
    /// from ADR-0078).
    pub fn publish_handle<H: Any + Send + Sync + 'static>(&mut self, handle: H) {
        self.handles
            .by_type
            .insert(TypeId::of::<H>(), Box::new(handle));
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

/// Issue 629 / Phase A: type-keyed map of cap-exported sub-handles
/// for cross-thread access from drivers / embedders. Caps publish
/// during `init` via [`NativeInitCtx::publish_handle`]; consumers
/// retrieve via [`crate::DriverCtx::handle`]. Owned by
/// `BootedPassives`; borrowed mutably into each cap's [`NativeInitCtx`]
/// in turn, then borrowed immutably by `DriverCtx`.
///
/// Replaces the pre-629 `Actors` struct that stored `Arc<dyn Any +
/// Send + Sync>` per booted cap — the cross-thread `Arc<A>` share was
/// the worker-pool-era legacy ADR-0038 made obsolete. Handles are
/// keyed by *handle* `TypeId` (e.g. `RenderHandles`), not by *actor*
/// `TypeId`, since the actor itself never escapes its dispatcher
/// thread.
pub struct ExportedHandles {
    pub(crate) by_type: HashMap<TypeId, Box<dyn Any + Send + Sync>>,
}

impl Default for ExportedHandles {
    fn default() -> Self {
        Self::new()
    }
}

impl ExportedHandles {
    pub fn new() -> Self {
        Self {
            by_type: HashMap::new(),
        }
    }

    /// Retrieve a cloned copy of the published handle bundle of type
    /// `H`, or `None` if no cap published one. The chassis-side
    /// reader; caps publish via [`NativeInitCtx::publish_handle`].
    pub fn get<H: Any + Send + Sync + Clone + 'static>(&self) -> Option<H> {
        self.by_type
            .get(&TypeId::of::<H>())
            .and_then(|b| b.downcast_ref::<H>())
            .cloned()
    }

    /// `true` when no cap has published a handle yet. Useful for tests.
    pub fn is_empty(&self) -> bool {
        self.by_type.is_empty()
    }

    /// Number of published handle bundles.
    pub fn len(&self) -> usize {
        self.by_type.len()
    }
}

/// Issue 607 Phase 4b (ADR-0079): RAII handle returned by
/// [`NativeCtx::monitor`]. Holds the registered `(watcher, target)`
/// pair plus an `Arc` to the chassis's [`crate::ActorRegistry`] so
/// `Drop` can deregister without rethreading the registry through the
/// caller.
///
/// The framework also prunes the monitor entry on either party's
/// close (the target's close drains `monitors_of[target]` after firing
/// `MonitorNotice`; the watcher's close walks `monitoring[watcher]` to
/// remove `watcher` from each target's forward list). `Drop` calls
/// [`crate::ActorRegistry::deregister_monitor`] which is idempotent —
/// dropping a handle whose entry the close path already removed is a
/// no-op.
///
/// Not `Clone` — a monitor is a unique (watcher, target) registration;
/// duplicating the handle would duplicate the deregistration on Drop
/// (still benign because deregister is idempotent, but cloneable
/// handles encourage holding multiple references whose semantics
/// surface as silent multi-prune).
pub struct MonitorHandle {
    registry: Arc<crate::actor_registry::ActorRegistry>,
    watcher: aether_data::MailboxId,
    target: aether_data::MailboxId,
}

impl MonitorHandle {
    pub(crate) fn new(
        registry: Arc<crate::actor_registry::ActorRegistry>,
        watcher: aether_data::MailboxId,
        target: aether_data::MailboxId,
    ) -> Self {
        Self {
            registry,
            watcher,
            target,
        }
    }

    /// The target this handle is monitoring. Useful for handlers that
    /// hold many handles and need to identify which one fired a notice.
    pub fn target(&self) -> aether_data::MailboxId {
        self.target
    }
}

impl Drop for MonitorHandle {
    fn drop(&mut self) {
        self.registry.deregister_monitor(self.watcher, self.target);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_data::KindId as DataKindId;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Hand-rolled `Actor` impl referenced only by the `_assert_actor_send`
    /// type-level check below. The struct never gets constructed at
    /// runtime — its purpose is to fail to instantiate the assert if
    /// `NativeActor` ever loses its `Send + 'static` bound.
    #[allow(dead_code)]
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

    /// Issue 629 / Phase A: handle-export round-trip. Caps publish a
    /// handle bundle during `init`; consumers retrieve a clone via
    /// `get::<H>()`.
    #[derive(Clone)]
    struct StubHandles {
        counter: Arc<AtomicU32>,
    }

    #[test]
    fn handles_insert_and_get_roundtrip() {
        let mut handles = ExportedHandles::new();
        assert!(handles.is_empty());
        let counter = Arc::new(AtomicU32::new(0));
        handles.by_type.insert(
            TypeId::of::<StubHandles>(),
            Box::new(StubHandles {
                counter: Arc::clone(&counter),
            }),
        );
        assert_eq!(handles.len(), 1);

        let retrieved: StubHandles = handles.get::<StubHandles>().expect("StubHandles published");
        retrieved.counter.fetch_add(1, Ordering::SeqCst);
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn handles_get_returns_none_for_unpublished_type() {
        let handles = ExportedHandles::new();
        assert!(handles.get::<StubHandles>().is_none());
    }

    /// Compile-time signal that `NativeActor` is `Send + 'static` (no
    /// `Sync`), and that `ExportedHandles` values are
    /// `Send + Sync + Clone` so cross-thread driver access works.
    /// If a future change to `Actor` drops `Send + 'static`, the
    /// asserts here fail to instantiate.
    fn _assert_actor_send() {
        fn requires_send<T: Send + 'static>() {}
        fn requires_handle<H: Any + Send + Sync + Clone + 'static>() {}
        requires_send::<StubActor>();
        requires_handle::<StubHandles>();
        // Avoid an unused-import diagnostic when the compiler
        // dead-code-eliminates the helper.
        let _ = DataKindId(0);
    }
}
