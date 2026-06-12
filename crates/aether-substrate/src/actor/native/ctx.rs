//! Per-handler and per-init contexts native actors receive when their
//! dispatcher trampoline fires. The trait + dispatch surface live in
//! the parent module (`super`); the cross-flavour `MonitorHandle` lives
//! in `crate::actor::monitor`.
//!
//! Issue 663 phase B added per-stage capability-trait impls
//! ([`MailSender`], [`OutboundReply`], [`LifecycleControl`]) on
//! [`NativeCtx`] / [`NativeInitCtx`] alongside the existing inherent
//! methods, so user-facing handler bodies are now spelled in the same
//! cross-transport vocabulary FFI guests use. Substrate-internal
//! accessors (`mailer`, `publish_handle`, `transport_arc`, `self_id`,
//! plus the `spawn_child` builder) stay inherent — they expose
//! types that don't belong on a cross-transport trait
//! (`Arc<Mailer>`, `Arc<Spawner>`, the chassis `ExportedHandles` map,
//! the substrate-only `SpawnBuilder<'_, A>` whose
//! `A: NativeActor + NativeDispatch` bound can't sit on a trait
//! method declared in `aether-actor`). The inherent + trait surface
//! coexist; cap authors reach for whichever is in scope.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::Arc;

use aether_actor::actor::ctx::{LifecycleControl, MailSender, OutboundReply};
use aether_actor::{Actor, HandlesKind, Singleton};

use crate::actor::native::mailbox::NativeActorMailbox;
use aether_data::{Kind, KindId, MailId, MailboxId, mailbox_id_from_name};

use crate::actor::monitor::MonitorHandle;
use crate::actor::native::binding::NativeBinding;
use crate::actor::registry::MonitorError;
use crate::mail::Source;
use crate::mail::mailer::Mailer;
use crate::runtime::trace::SettlementHold;

use super::{NativeActor, NativeDispatch};
use crate::actor::native::InheritCtx;
use crate::actor::native::RootCtx;
use crate::actor::native::dispatch_blocking::{DispatchId, TaskCompletionWake, TaskDone};
use crate::actor::native::spawn_thread;
use crate::mail::{Mail, SourceAddr};
use std::thread::{Builder as ThreadBuilder, JoinHandle};

/// Per-mail context for a [`NativeActor`] handler. Borrows the
/// actor's [`NativeBinding`] for outbound mail and carries the
/// inbound's reply target so the `OutboundReply::reply::<K>(&payload)` API
/// can route back to the originator without rethreading the handle.
///
/// Stage 1 ships the wiring; the actual reply routing through
/// [`NativeBinding::send_reply_for_handler`] / `Mailer::send_reply` is
/// the stage-2 migration's responsibility (today's caps reply via
/// `mailer.send_reply(...)` directly; stage 2 routes those onto
/// `ctx.reply(...)`).
pub struct NativeCtx<'a> {
    binding: &'a Arc<NativeBinding>,
    source: Source,
    /// ADR-0080 §5: identity of the mail this handler is dispatching.
    /// Outbound `send` paths read this to stamp `parent_mail` on
    /// child mail (so the receiver inherits the right parent in the
    /// causal graph). `MailId::NONE` for ctxs without an inbound
    /// (chassis-root sends, `unwire`, init).
    in_flight_mail_id: MailId,
    /// ADR-0080 §5: root of the causal chain this handler runs in.
    /// Outbound `send` paths read this to stamp `root` on child mail
    /// so descendants share the chain. `MailId::NONE` for ctxs without
    /// an inbound — those sends mint a fresh root from their own
    /// `mail_id` in `NativeBinding::send_mail`.
    in_flight_root: MailId,
}

/// The receiver-addressing methods shared verbatim by [`NativeCtx`] and
/// [`NativeInitCtx`]: both hold the same `binding`, so `actor` /
/// `resolve_actor` / `actor_at` resolve identically. Emitting them from
/// one source keeps the two ctxs from drifting and means the bodies are
/// not a `DuplicatedCode` clone (ADR-0099 §5 / issue 1431).
macro_rules! native_sender_methods {
    () => {
        /// Singleton sender shortcut: returns a typed [`NativeActorMailbox`]
        /// addressing the unique instance of receiver actor `R`.
        #[must_use]
        pub fn actor<R: Singleton>(&self) -> NativeActorMailbox<'_, R> {
            NativeActorMailbox::__new(R::resolve(self.binding.carry()).0, self.binding)
        }

        /// Multi-instance sender: resolve a typed [`NativeActorMailbox`]
        /// from a runtime instance name.
        // Runtime-name escape hatch: the instance name is only known at
        // runtime, so there is no `R::resolve` lineage carry to route through.
        #[must_use]
        #[allow(clippy::disallowed_methods)]
        pub fn resolve_actor<R: Actor>(&self, name: &str) -> NativeActorMailbox<'_, R> {
            NativeActorMailbox::__new(mailbox_id_from_name(name).0, self.binding)
        }

        /// Address an actor by a [`MailboxId`] already in hand — the id a
        /// `spawn_child` returned, or one a peer handed over. ADR-0099 §3:
        /// a hosted / nested actor's id is the lineage fold, not
        /// `hash(name)`, so it cannot be re-derived from a name; a supervisor
        /// that tracks its children's ids addresses them through this rather
        /// than re-resolving by name.
        #[must_use]
        pub fn actor_at<R: Actor>(&self, id: MailboxId) -> NativeActorMailbox<'_, R> {
            NativeActorMailbox::__new(id.0, self.binding)
        }
    };
}

impl<'a> NativeCtx<'a> {
    /// Internal constructor — the chassis dispatcher trampoline (in
    /// `chassis::builder`) builds these. Cap-side test fixtures in
    /// `aether-capabilities` also reach for it directly so they can
    /// drive a handler without spinning up a full chassis; that's why
    /// it's `pub` rather than `pub(crate)`.
    pub fn new(
        binding: &'a Arc<NativeBinding>,
        sender: Source,
        in_flight_mail_id: MailId,
        in_flight_root: MailId,
    ) -> Self {
        Self {
            binding,
            source: sender,
            in_flight_mail_id,
            in_flight_root,
        }
    }

    /// Borrow the wired `Mailer`. Issue 953: surfaced so cap handlers
    /// (`TraceDispatchCapability` is the motivating consumer) can
    /// reach the per-chassis trace handle for `now_nanos` without
    /// going through `binding()`. Mirrors the `NativeInitCtx::mailer`
    /// accessor but returns a borrow rather than a clone — handler
    /// paths usually just need a `&Mailer` for one call.
    #[must_use]
    pub fn mailer(&self) -> &Arc<Mailer> {
        self.binding.mailer()
    }

    /// ADR-0080 §12 spawn primitive: launch a worker thread that
    /// inherits this handler's in-flight `(mail_id, root)` so its
    /// sends fold into the current causal chain. The closure `f`
    /// receives a [`InheritCtx<A>`] — sends
    /// from inside `f` carry `parent_mail = self.in_flight_mail_id()`
    /// and `root = self.in_flight_root()` automatically.
    ///
    /// Use for short-burst CPU offload that is *part of* the current
    /// handler's causal closure (e.g., parsing, encoding,
    /// pixel-pushing). For long-lived workers responding to external
    /// events with no caller context (TCP per-connection workers,
    /// pollers), use [`Self::spawn_detached`] instead.
    ///
    /// **Settlement contract gap (issue iamacoffeepot/aether#716):**
    /// the parent chain may settle before the worker's first send
    /// arrives; callers gate-sensitive to settlement should not
    /// rely on the parent chain staying open for the worker's
    /// lifetime today.
    pub fn spawn_inherit<A, F>(&self, f: F) -> JoinHandle<()>
    where
        A: Actor + Singleton + 'static,
        F: FnOnce(InheritCtx<A>) + Send + 'static,
    {
        spawn_thread::spawn_inherit::<A, F>(
            Arc::clone(self.binding),
            self.in_flight_mail_id,
            self.in_flight_root,
            f,
        )
    }

    /// ADR-0080 §12 spawn primitive: launch a worker thread with no
    /// in-flight inheritance. The closure `f` receives a
    /// [`RootCtx<A>`] — each send mints a
    /// fresh root chain with `A`'s mailbox as the producer.
    ///
    /// Use for long-lived workers that respond to external events
    /// (TCP per-connection workers, pollers). For short-burst CPU
    /// offload that is part of the current handler's causal closure,
    /// use [`Self::spawn_inherit`].
    pub fn spawn_detached<A, F>(&self, f: F) -> JoinHandle<()>
    where
        A: Actor + Singleton + 'static,
        F: FnOnce(RootCtx<A>) + Send + 'static,
    {
        spawn_thread::spawn_detached::<A, F>(Arc::clone(self.binding), f)
    }

    /// ADR-0093 hold-until-resolve dispatch: run the blocking closure
    /// `f` on a worker thread and reply to the current caller in a
    /// *later* handler turn, when the worker's output lands.
    ///
    /// The settlement hold is acquired **eagerly on this thread, before
    /// the worker spawns** (so `HoldOpen` precedes this handler's
    /// `Finished` and the #716 premature-settlement window is closed by
    /// construction), then parked in the per-actor in-flight ledger
    /// alongside the originating [`Source`] — it outlives the worker
    /// (which holds nothing) and releases only when the completion is
    /// resolved. `MailId::NONE` for [`Self::in_flight_root`] skips the
    /// hold cleanly (no chain to hold), matching `spawn_inherit`.
    ///
    /// When `f` returns, the worker stores the output in the ledger's
    /// completion slot and pushes a [`TaskCompletionWake`] to this
    /// actor's own mailbox (the loopback-wake mechanism). The actor's
    /// completion handler decodes that wake's [`DispatchId`] and calls
    /// [`Self::take_task_done`] to rebuild the [`TaskDone`], then
    /// `resolve`s it.
    ///
    /// Returns the [`DispatchId`] for *optional* cancellation; the happy
    /// path ignores it.
    pub fn dispatch_blocking<O, F>(&mut self, f: F) -> DispatchId
    where
        O: Send + 'static,
        F: FnOnce() -> O + Send + 'static,
    {
        self.dispatch_blocking_with::<O, (), F>((), f)
    }

    /// Context-carrying variant of [`Self::dispatch_blocking`]
    /// (ADR-0093 §5): parks `cx` in the in-flight ledger alongside the
    /// hold + reply target so the completion handler receives a
    /// [`TaskDone<O, C>`] whose [`TaskDone::context`] is `cx`. Use when
    /// the completion genuinely needs actor-thread state the pure worker
    /// shouldn't take.
    pub fn dispatch_blocking_with<O, C, F>(&mut self, cx: C, f: F) -> DispatchId
    where
        O: Send + 'static,
        C: Send + 'static,
        F: FnOnce() -> O + Send + 'static,
    {
        // ADR-0093 §1 / ADR-0080 §12: acquire the hold on the current root
        // and capture the reply target from *this* handler, then hand them
        // to the resumed core. The `MailId::NONE` root skips the hold — no
        // chain to keep open. A bounded `TaskQueue` (aether-capabilities)
        // instead captures `(hold, reply_to)` at accept time and replays
        // them via `dispatch_blocking_resumed` when a slot frees, so a
        // deferred request keeps its own chain held and replies to its own
        // caller.
        let hold = self.acquire_settlement_hold();
        let reply_to = self.reply_target();
        self.dispatch_blocking_resumed_with(hold, reply_to, cx, f)
    }

    /// Acquire a [`SettlementHold`] on the current in-flight root
    /// (ADR-0080 §12). `MailId::NONE` (no inbound chain) yields a no-op
    /// hold. Use to keep a chain open across deferred work — e.g. a
    /// `TaskQueue` buffering an over-limit request holds it until a slot
    /// frees, then moves it into [`Self::dispatch_blocking_resumed`].
    #[must_use]
    pub fn acquire_settlement_hold(&self) -> SettlementHold {
        self.mailer().acquire_settlement_hold(self.in_flight_root)
    }

    /// ADR-0093: dispatch a blocking closure with an externally-supplied
    /// `(hold, reply_to)` — *moved in* rather than read from this ctx.
    /// [`Self::dispatch_blocking`] is sugar over this that supplies them
    /// from the current handler. The bound/queue path (`TaskQueue`)
    /// captures the hold + reply target when a request is accepted and
    /// replays them here when the request finally dispatches from a later
    /// handler turn — so the deferred work keeps its *own* chain held and
    /// replies to its *own* caller, not the completion handler's.
    pub fn dispatch_blocking_resumed<O, F>(
        &mut self,
        hold: SettlementHold,
        reply_to: Source,
        f: F,
    ) -> DispatchId
    where
        O: Send + 'static,
        F: FnOnce() -> O + Send + 'static,
    {
        self.dispatch_blocking_resumed_with(hold, reply_to, (), f)
    }

    /// Context-carrying core of the resumed dispatch — the single worker
    /// spawn site for every `dispatch_blocking*` path. Inserts the ledger
    /// entry with the supplied `(hold, reply_to, cx)` and spawns the
    /// worker that runs `f`, parks its output, and wakes the actor.
    pub fn dispatch_blocking_resumed_with<O, C, F>(
        &mut self,
        hold: SettlementHold,
        reply_to: Source,
        cx: C,
        f: F,
    ) -> DispatchId
    where
        O: Send + 'static,
        C: Send + 'static,
        F: FnOnce() -> O + Send + 'static,
    {
        let id = self.binding.dispatch_insert(hold, reply_to, Box::new(cx));

        // The worker captures the binding + dispatch id, runs the
        // blocking closure, parks its output in the ledger, then pushes
        // the completion-wake to the actor's own mailbox. It touches no
        // actor state beyond the ledger slot it owns and dies after the
        // push. This is the one sanctioned raw spawn for the
        // hold-until-resolve shape (ADR-0093) — umbrella-aware because
        // the hold (held in the ledger, not here) keeps the chain open
        // until the resolve. The per-request spawn is a placeholder; the
        // scalable form is a reused work-stealing blocking pool isolated
        // from the cooperative scheduler (#1322).
        let binding = Arc::clone(self.binding);
        // This IS the ADR-0093 dispatch_blocking primitive — the hold lives in the
        // ledger (not on this worker), so the chain stays open until the resolve.
        #[allow(clippy::disallowed_methods)]
        let spawned = ThreadBuilder::new()
            .name(String::from("aether-dispatch-blocking"))
            .spawn(move || {
                let output = f();
                binding.dispatch_fill_output(id, Box::new(output));
                let wake = TaskCompletionWake { dispatch_id: id.0 };
                let self_id = binding.self_mailbox();
                binding.mailer().push(Mail::new(
                    self_id,
                    TaskCompletionWake::ID,
                    wake.encode_into_bytes(),
                    1,
                ));
            });
        if let Err(e) = spawned {
            tracing::error!(
                target: "aether_substrate::actor::native::dispatch_blocking",
                error = %e,
                "failed to spawn dispatch_blocking worker thread",
            );
        }
        id
    }

    /// ADR-0093 completion-routing entry point: remove the in-flight
    /// ledger entry named by `id` (decoded from a landed
    /// [`TaskCompletionWake`]) and rebuild its [`TaskDone<O, C>`]. The
    /// (future) `#[handler(task)]` macro — and, for now, a hand-wired
    /// completion handler — calls this and then `resolve`s the result.
    ///
    /// `None` for an unknown id (cancelled or double-landed) or an `O` /
    /// `C` that doesn't match the dispatch's types (a wiring bug).
    pub fn take_task_done<O: 'static, C: 'static>(
        &mut self,
        id: DispatchId,
    ) -> Option<TaskDone<O, C>> {
        self.binding.dispatch_take::<O, C>(id)
    }

    /// Non-consuming sibling of [`Self::take_task_done`]: probe the
    /// in-flight entry named by `id` against `O` / `C` and only remove +
    /// rebuild the [`TaskDone<O, C>`] on a match, leaving the entry intact
    /// on a mismatch.
    ///
    /// This is the routing primitive behind `#[handler(task)]`. Multiple
    /// task handlers on one actor are discriminated by their `TaskDone<O>`
    /// output type, not a kind id — all completions arrive as the single
    /// [`TaskCompletionWake`] kind. The generated dispatch arm tries each
    /// task handler's `(O, C)` in
    /// turn; a wrong-type probe must *not* consume the entry, or the first
    /// handler tried would swallow a completion destined for a later one.
    ///
    /// `None` for an unknown id (cancelled / double-landed), an unfilled
    /// output, or an `O` / `C` that doesn't match this entry's dispatch.
    pub fn try_take_task_done<O: 'static, C: 'static>(
        &mut self,
        id: DispatchId,
    ) -> Option<TaskDone<O, C>> {
        self.binding.dispatch_try_take::<O, C>(id)
    }

    /// ADR-0080 §5: the [`MailId`] of the mail currently being
    /// dispatched. Read by outbound `send` paths to stamp
    /// `parent_mail` on child mail. `MailId::NONE` when the ctx was
    /// built without an inbound (close hook, init, chassis-pushed).
    #[must_use]
    pub fn in_flight_mail_id(&self) -> MailId {
        self.in_flight_mail_id
    }

    /// ADR-0080 §5: the root [`MailId`] of the causal chain this
    /// handler is running in. Read by outbound `send` paths to inherit
    /// `root` on child mail so descendants share the chain. The
    /// chassis-root case (no inbound) leaves this `MailId::NONE` and
    /// `NativeBinding::send_mail` mints a fresh root.
    #[must_use]
    pub fn in_flight_root(&self) -> MailId {
        self.in_flight_root
    }

    /// The reply target for the mail currently being dispatched.
    /// Useful when a handler wants to inspect the originator (audit
    /// trails, multi-tenant routing) without going through
    /// [`OutboundReply::reply`]. `target == SourceAddr::None` means the
    /// inbound was broadcast or peer-component mail with no reply
    /// destination.
    #[must_use]
    pub fn reply_target(&self) -> Source {
        self.source
    }

    /// Immediate-sender mailbox of the mail currently being dispatched,
    /// or `None` for mail with no local sender (broadcast,
    /// substrate-generated, hub-bubbled). This is the *immediate*
    /// sender (one hop, the addressing layer's `Source`), not the chain
    /// origin — the origin lives in the tracing layer (`root` /
    /// `parent_mail`, ADR-0080). Issue #581's `LogCapability` reads this
    /// to populate `LogEntry::origin` from the envelope rather than the
    /// payload.
    #[must_use]
    pub fn source_mailbox(&self) -> Option<MailboxId> {
        match self.source.addr {
            SourceAddr::Component(id) => Some(id),
            _ => None,
        }
    }

    native_sender_methods!();

    /// Reply to an explicit [`Source`] under an explicit `(root, parent)`
    /// lineage rather than the inbound's own sender / this ctx's in-flight
    /// chain. The ADR-0093 hold-until-resolve path reaches for this:
    /// [`TaskDone::resolve`] re-replies through the *originating* caller's
    /// reply target (captured at dispatch, parked in the in-flight ledger)
    /// under the root the parked [`SettlementHold`] keeps open — not the
    /// completion-wake's sender / chain (the worker thread's loopback
    /// mail, which has no caller behind it). Passing the hold's root keeps
    /// the deferred reply's `Sent` in the chain the hold is gating, so the
    /// chain settles only after the reply lands (#1695). Routes through
    /// the same [`NativeBinding::send_reply_for_handler`] path as
    /// [`OutboundReply::reply`].
    pub fn reply_to_target<K: Kind>(
        &mut self,
        sender: Source,
        payload: &K,
        root: MailId,
        parent: Option<MailId>,
    ) {
        self.binding
            .send_reply_for_handler(sender, payload, root, parent);
    }

    /// Issue 607 Phase 4a (ADR-0079): self-shutdown signal. Sets a
    /// flag the actor's dispatcher polls after each handler returns;
    /// when set, the trampoline drains any remaining inbox mail
    /// synchronously, runs `NativeActor::unwire`, and exits the
    /// dispatch loop. After exit the actor's [`MailboxId`]
    /// transitions from `Live` to `Dead` in the chassis's
    /// [`ActorRegistry`](crate::ActorRegistry) and is added to `tombstones` —
    /// `spawn_child` rejects reuse of the retired full name with
    /// `SpawnError::SubnameRetired`.
    ///
    /// Idempotent — flipping the flag twice is the same as flipping
    /// it once. Singletons booted through `with_actor` rely on the
    /// chassis-shutdown channel-drop path instead of this flag, but
    /// can call `shutdown()` to opt in to flag-based exit.
    pub fn shutdown(&self) {
        self.binding.signal_shutdown();
    }

    /// ADR-0063 fail-fast: bring the substrate down with `reason`.
    /// Diverging — does not return. Used by handlers that observe a
    /// non-recoverable invariant violation (today: the wasm trampoline
    /// on a guest trap). Native impl forwards to
    /// [`NativeBinding::fatal_abort`]. See also the
    /// [`aether_actor::ffi::FfiCtx`] counterpart, which `panic!`s — the
    /// substrate's wasm runtime catches the trap and ADR-0063 escalates
    /// symmetrically.
    pub fn fatal_abort(&self, reason: String) -> ! {
        self.binding.fatal_abort(reason);
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
    /// [`ActorRegistry`](crate::ActorRegistry); tombstoned (closed) and unknown ids
    /// surface as [`MonitorError`]. Singletons today don't sit
    /// in the actor registry as `Live` entries (their entries live in
    /// the routing [`Registry`](crate::Registry) only); a future lift inserts
    /// them so monitoring a singleton works the same way. Until then,
    /// monitor only addresses instanced actors.
    ///
    /// # Panics
    /// Panics if the transport was constructed via
    /// [`NativeBinding::new_for_test`] (no spawner / actor registry
    /// wired) — fail-fast per ADR-0063: production transports always
    /// carry both, so handler code never reaches the panic.
    pub fn monitor(&self, target: MailboxId) -> Result<MonitorHandle, MonitorError> {
        let spawner = self
            .binding
            .spawner()
            .expect("NativeCtx::monitor requires a chassis-built binding (no spawner installed — likely a `new_for_test` binding)");
        let registry = Arc::clone(spawner.actor_registry());
        let watcher = self.binding.self_mailbox();
        registry.register_monitor(watcher, target)?;
        Ok(MonitorHandle::new(registry, watcher, target))
    }

    /// Issue 607 Phase 3b (ADR-0079): spawn an instanced actor as a
    /// child of the calling actor. The new actor's [`Source`]
    /// stamps the calling actor's mailbox so any reply addressed to
    /// `SourceAddr::Component` routes back here.
    ///
    /// Returns a [`SpawnBuilder`](crate::SpawnBuilder) the caller chains
    /// `after_init` / `finish` against. Mirrors the chassis-level
    /// `PassiveChassis::spawn_actor` / `BuiltChassis::spawn_actor`
    /// shape; both flow through the same [`crate::Spawner`].
    ///
    /// # Panics
    /// Panics if the transport was constructed via
    /// [`NativeBinding::new_for_test`] (which doesn't wire a
    /// spawner) — fail-fast per ADR-0063: production transports always
    /// carry one, so handler code never reaches the panic.
    pub fn spawn_child<'b, A>(
        &'b self,
        subname: super::spawn::Subname<'b>,
        config: A::Config,
    ) -> super::spawn::SpawnBuilder<'b, A>
    where
        A: aether_actor::Instanced + NativeActor + NativeDispatch,
    {
        let spawner = self
            .binding
            .spawner()
            .expect("NativeCtx::spawn_child requires a chassis-built binding (no spawner installed — likely a `new_for_test` binding)");
        let sender = Source {
            addr: SourceAddr::Component(self.binding.self_mailbox()),
            correlation_id: Source::NO_CORRELATION,
        };
        // ADR-0099 §3: the child nests under this actor — its id folds
        // the new node's `ActorId` onto this actor's lineage carry, and
        // it registers under this actor's rendered name.
        super::spawn::SpawnBuilder::new(
            Arc::clone(spawner),
            subname,
            config,
            sender,
            Some((self.binding.carry(), self.binding.self_mailbox())),
        )
    }
}

impl NativeCtx<'_> {
    /// ADR-0080 §5: derive the `parent_mail` to stamp on outbound
    /// mail from this ctx's in-flight context. `MailId::NONE` collapses
    /// to `None` (chassis-root or close/init ctx).
    pub(crate) fn outbound_parent(&self) -> Option<MailId> {
        if self.in_flight_mail_id == MailId::NONE {
            None
        } else {
            Some(self.in_flight_mail_id)
        }
    }

    /// ADR-0080 §5: derive the inherited `root` to stamp on outbound
    /// mail from this ctx's in-flight context. `MailId::NONE` collapses
    /// to `None`, in which case `NativeBinding::send_mail_with_lineage`
    /// mints a fresh root from the outbound's own `mail_id`.
    pub(crate) fn outbound_root(&self) -> Option<MailId> {
        if self.in_flight_root == MailId::NONE {
            None
        } else {
            Some(self.in_flight_root)
        }
    }
}

impl NativeCtx<'_> {
    /// Lineage-aware multicast: encode `payload` once, then push one copy
    /// to every `recipient`. The inbound `(mail_id, root)` from this ctx
    /// propagate as `parent_mail` + `inherited_root`, so each fanned-out
    /// copy lands in the same causal chain as the inbound that triggered
    /// the fanout — every subscriber-bound copy gets its own fresh
    /// `MailId` keyed under the same parent edge.
    ///
    /// Recipients aren't known to share a receiver type at compile site
    /// (subscribers register at runtime by mailbox id), so this takes
    /// mailbox ids directly rather than the typed
    /// `R: Singleton + HandlesKind<K>` shape of [`MailSender::send`]. The empty
    /// recipient set is a fast no-op — encoding only runs when there's at
    /// least one consumer.
    ///
    /// Issue iamacoffeepot/aether#723.
    pub fn fanout<K: Kind>(
        &mut self,
        recipients: impl IntoIterator<Item = MailboxId>,
        payload: &K,
    ) {
        let mut recipients = recipients.into_iter();
        let Some(first) = recipients.next() else {
            return;
        };
        let bytes = payload.encode_into_bytes();
        let parent = self.outbound_parent();
        let root = self.outbound_root();
        let kind = K::ID.0;
        self.binding
            .push_envelope_buffered(first.0, kind, &bytes, 1, parent, root);
        for recipient in recipients {
            self.binding
                .push_envelope_buffered(recipient.0, kind, &bytes, 1, parent, root);
        }
    }

    /// Untyped sibling of [`NativeActorMailbox::send_traced`]: dispatch
    /// an already-encoded mail payload with runtime `recipient` /
    /// `kind` ids and return the minted [`MailId`] for settlement
    /// subscription.
    ///
    /// Issue 750: the typed `send_traced` path is gated on
    /// `R: HandlesKind<K>`, which requires the kind and receiver to be
    /// known at compile site. Endpoints that route mail with runtime
    /// ids (the RPC server forwarding `Call.envelope` from the wire is
    /// the motivating case) have neither — they hold a `MailboxId` +
    /// `KindId` + opaque payload bytes. This method is the escape
    /// hatch: skips the type-system check, dispatches the raw bytes
    /// through the same lineage-aware path the typed helpers go
    /// through.
    ///
    /// When `ctx` represents a chassis-root edge (`in_flight_mail_id`
    /// is `NONE`) the returned id is the root of a fresh causal chain;
    /// when mid-handler, the returned id is the new mail's id inside
    /// the inherited chain. Settlement subscription against a mid-
    /// handler return only fires on settlement of *that mail's*
    /// descendants, not the whole chain — callers wanting chain-root
    /// settlement should be at chassis-root.
    ///
    /// No untraced counterpart at this layer — callers reaching for
    /// untyped dispatch always want the returned `MailId`. The typed
    /// `send` / `send_many` on `NativeActorMailbox` cover the
    /// fire-and-forget case.
    #[must_use]
    pub fn send_envelope_traced(&self, recipient: MailboxId, kind: KindId, bytes: &[u8]) -> MailId {
        self.binding.push_envelope_buffered(
            recipient.0,
            kind.0,
            bytes,
            1,
            self.outbound_parent(),
            self.outbound_root(),
        )
    }

    /// Re-dispatch variant of [`Self::send_envelope_traced`] that pins the
    /// child mail's `reply_to` to the supplied [`Source`] instead of
    /// stamping the default `(Component(self_mailbox), auto_correlation)`.
    /// The minted [`MailId`] and the chain's `in_flight` accounting are
    /// unchanged — only the recipient's
    /// [`OutboundReply::reply_target`]
    /// view changes.
    ///
    /// Use this when a cap is **forwarding** another actor's call rather
    /// than originating one: the trace cap servicing `DispatchTraced`
    /// (issue 1265 — the `send_mail_traced` batched-dispatch path)
    /// re-dispatches each child envelope but wants the child's deferred
    /// reply to land at the **original** caller's `reply_to` (the RPC
    /// server holding the wire `cid`'s in-flight entry), not stranded at
    /// the trace cap's own mailbox where no handler exists for it.
    ///
    /// Pass `ctx.reply_target()` as `reply_to` to forward to whoever
    /// invoked this cap. Single-Call paths (the RPC server's
    /// `send_envelope_as_root` dispatching directly at the receiver)
    /// never reach this method — the default `reply_to` lands at the
    /// dispatcher which is also the call-correlation owner.
    #[must_use]
    pub fn send_envelope_traced_with_reply_to(
        &self,
        recipient: MailboxId,
        kind: KindId,
        bytes: &[u8],
        reply_to: Source,
    ) -> MailId {
        self.binding.push_envelope_buffered_with_reply_to(
            recipient.0,
            kind.0,
            bytes,
            1,
            self.outbound_parent(),
            self.outbound_root(),
            Some(reply_to),
        )
    }

    /// Like [`Self::send_envelope_traced`] but always starts a fresh
    /// causal chain — ignores the ctx's in-flight lineage and passes
    /// `parent_mail = None, inherited_root = None` to the dispatch
    /// path. The returned [`MailId`] is the root of the new chain, so
    /// subscribing to its settlement via
    /// `SettlementRegistry::subscribe_settlement_mail` fires when the
    /// dispatch's entire descendant subtree drains.
    ///
    /// Use this when the cap is acting on an external event (wire-
    /// borne RPC call, file watcher, timer) rather than forwarding a
    /// mail that was already in flight. The `RpcServer` cap's `Call`
    /// handler is the motivating case: the inbound that wakes the cap
    /// is an internal wake mail causally unrelated to the wire-borne
    /// `Call` — inheriting its chain would attribute the dispatch to
    /// the wrong root and `subscribe_settlement_mail` would never fire
    /// (descendants don't settle individually; only the chain root
    /// does).
    #[must_use]
    pub fn send_envelope_as_root(
        &self,
        recipient: MailboxId,
        kind: KindId,
        bytes: &[u8],
    ) -> MailId {
        self.binding
            .push_envelope_buffered(recipient.0, kind.0, bytes, 1, None, None)
    }
}

impl Drop for NativeCtx<'_> {
    /// ADR-0087 / 2b (iamacoffeepot/aether#1105): handler-end flush. One
    /// `NativeCtx` is built per dispatched envelope (and one for
    /// `unwire`), so its scope *is* the handler's lifetime — dropping it
    /// is the universal "handler finished" hook. Flushing the binding's
    /// outbound buffer here forms the handler's buffered sends into one
    /// ring blob and routes them, covering the main dispatch loop, the
    /// shutdown-drain loop, and `unwire` with a single hook (no
    /// per-call-site flush to forget and silently drop mail).
    /// Idempotent — an empty buffer no-ops.
    fn drop(&mut self) {
        self.binding.flush_outbound();
    }
}

/// Boot-time context for [`NativeActor::init`]. Carries a borrow of
/// the actor's transport (for init-time mail), a borrow of the
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
    binding: &'a Arc<NativeBinding>,
    handles: &'a mut ExportedHandles,
    mailer: Arc<Mailer>,
}

impl<'a> NativeInitCtx<'a> {
    /// Internal constructor — only [`crate::chassis::builder::Builder::with_actor`]
    /// builds these.
    pub(crate) fn new(
        binding: &'a Arc<NativeBinding>,
        handles: &'a mut ExportedHandles,
        mailer: Arc<Mailer>,
    ) -> Self {
        Self {
            binding,
            handles,
            mailer,
        }
    }

    /// Borrow the Arc'd cap-bound [`NativeBinding`]. Used by the wasm
    /// trampoline at init to install itself on the
    /// [`crate::actor::wasm::component::ComponentCtx`] so the
    /// reply / outbound-mail host fns can route through this binding.
    /// Promoted from `pub(crate)` to `pub` by issue 654 when the
    /// trampoline moved to `aether-capabilities` next to its consumer;
    /// no other external caller is intended.
    #[must_use]
    pub fn binding(&self) -> &Arc<NativeBinding> {
        self.binding
    }

    /// The actor's own [`MailboxId`] — the deterministic FNV-1a hash
    /// of its full registered name (ADR-0029). For singletons that's
    /// `Actor::NAMESPACE`; for instanced actors it's
    /// `"{NAMESPACE}:{subname}"` (ADR-0079). Init may use this to
    /// publish its own address — e.g. dispatch
    /// `aether.input.subscribe { mailbox: ctx.self_id() }` before
    /// registration completes; replies route correctly once the spawn
    /// lifecycle finishes inserting the entry.
    #[must_use]
    pub fn self_id(&self) -> MailboxId {
        self.binding.self_mailbox()
    }

    /// Clone the substrate's mailer. Caps that need to register a
    /// `Mailer::set_outbound`-style hook (Hub client, future
    /// fallback routers) reach for this; most caps don't need it.
    #[must_use]
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

    native_sender_methods!();
}

// Issue 703: NativeInitCtx no longer impls `MailSender`.
// `init` is the sync constructor (ADR-0079) and must NOT mail —
// subscriptions, peer hellos, and self-mail kickoffs all belong in
// `wire`, where `NativeCtx` provides the full mail surface.

// The per-stage capability trait impls (`MailSender` / `OutboundReply`
// / `LifecycleControl`). Default-impl bodies on
// `MailSender` cover `send_detached` / `send_detached_to_named`,
// so each impl below spells out the stage-specific accessors and
// routing methods. `LifecycleControl::shutdown` /
// `monitor` forward to the existing inherent methods that today
// reach into the substrate-internal spawner + actor registry; future
// FFI-side wiring (issue 607 phase 4 / ADR-0079) will program against
// the trait the same way native callers do.

impl MailSender for NativeCtx<'_> {
    //noinspection DuplicatedCode
    fn send<R, K>(&mut self, payload: &K)
    where
        R: Singleton + HandlesKind<K>,
        K: Kind,
    {
        let bytes = payload.encode_into_bytes();
        self.binding.push_envelope_buffered(
            R::resolve(self.binding.carry()).0,
            K::ID.0,
            &bytes,
            1,
            self.outbound_parent(),
            self.outbound_root(),
        );
    }

    //noinspection DuplicatedCode
    fn send_many<R, K>(&mut self, payloads: &[K])
    where
        R: Singleton + HandlesKind<K>,
        K: Kind + bytemuck::NoUninit,
    {
        let bytes: &[u8] = bytemuck::cast_slice(payloads);
        // Batch count rides as `u32` on the wire (matches the FFI ABI);
        // realistic mail batches stay well below `u32::MAX`.
        #[allow(clippy::cast_possible_truncation)]
        let count = payloads.len() as u32;
        self.binding.push_envelope_buffered(
            R::resolve(self.binding.carry()).0,
            K::ID.0,
            bytes,
            count,
            self.outbound_parent(),
            self.outbound_root(),
        );
    }

    //noinspection DuplicatedCode
    // Runtime-name send escape hatch (the `Resolver::send_to_named` contract):
    // the recipient name is supplied at runtime, no compile-time `R` to resolve.
    #[allow(clippy::disallowed_methods)]
    fn send_to_named<K: Kind>(&mut self, name: &str, payload: &K) {
        let bytes = payload.encode_into_bytes();
        self.binding.push_envelope_buffered(
            mailbox_id_from_name(name).0,
            K::ID.0,
            &bytes,
            1,
            self.outbound_parent(),
            self.outbound_root(),
        );
    }

    fn prev_correlation(&self) -> u64 {
        self.binding.prev_correlation()
    }
}

impl OutboundReply for NativeCtx<'_> {
    type ReplyHandle = Source;

    /// Always `Some` on native — the substrate's per-handler dispatcher
    /// builds a `Source` for every inbound (broadcast / no-reply mail
    /// rides as `SourceAddr::None` inside the wrapper). The
    /// always-Some invariant is preserved by [`Self::source_mailbox`] /
    /// [`Self::reply`] inspecting the inner `SourceAddr`; the trait's
    /// `Option<Self::ReplyHandle>` shape exists for the FFI side,
    /// where a guest genuinely sees no reply target.
    fn reply_target(&self) -> Option<Source> {
        Some(self.source)
    }

    fn source_mailbox(&self) -> Option<MailboxId> {
        match self.source.addr {
            SourceAddr::Component(id) => Some(id),
            _ => None,
        }
    }

    fn reply<K: Kind>(&mut self, payload: &K) {
        // ADR-0080 §5/§6 (#1695): a synchronous reply joins the handler's
        // causal chain — inherit this ctx's `root` + `parent` so the
        // reply's `Sent` lands in the caller's chain.
        self.binding.send_reply_for_handler(
            self.source,
            payload,
            self.in_flight_root,
            self.outbound_parent(),
        );
    }

    fn reply_to<K: Kind>(&mut self, sender: Source, payload: &K) {
        self.binding.send_reply_for_handler(
            sender,
            payload,
            self.in_flight_root,
            self.outbound_parent(),
        );
    }
}

impl LifecycleControl for NativeCtx<'_> {
    type MonitorHandle = MonitorHandle;
    type MonitorError = MonitorError;

    fn shutdown(&self) {
        self.binding.signal_shutdown();
    }

    fn monitor(&self, target: MailboxId) -> Result<MonitorHandle, MonitorError> {
        let spawner = self.binding.spawner().expect(
            "NativeCtx::monitor requires a chassis-built transport (no spawner installed — likely a `new_for_test` transport)",
        );
        let registry = Arc::clone(spawner.actor_registry());
        let watcher = self.binding.self_mailbox();
        registry.register_monitor(watcher, target)?;
        Ok(MonitorHandle::new(registry, watcher, target))
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
    #[must_use]
    pub fn new() -> Self {
        Self {
            by_type: HashMap::new(),
        }
    }

    /// Retrieve a cloned copy of the published handle bundle of type
    /// `H`, or `None` if no cap published one. The chassis-side
    /// reader; caps publish via [`NativeInitCtx::publish_handle`].
    #[must_use]
    pub fn get<H: Any + Send + Sync + Clone + 'static>(&self) -> Option<H> {
        self.by_type
            .get(&TypeId::of::<H>())
            .and_then(|b| b.downcast_ref::<H>())
            .cloned()
    }

    /// `true` when no cap has published a handle yet. Useful for tests.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_type.is_empty()
    }

    /// Number of published handle bundles.
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_type.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chassis::error::BootError;
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

    impl Singleton for StubActor {}

    impl NativeActor for StubActor {
        type Config = ();
        fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
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

    /// A cast kind that is `Pod` but derives neither `Serialize` nor
    /// `Deserialize` — the kind ADR-0100's reply path must accept.
    #[repr(C)]
    #[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
    struct CastOnly {
        code: u32,
    }

    impl Kind for CastOnly {
        const NAME: &'static str = "test.cast_only_reply";
        const ID: KindId = KindId(0xDEAD_BEEF_0009_0001);

        fn encode_into_bytes(&self) -> Vec<u8> {
            bytemuck::bytes_of(self).to_vec()
        }
    }

    /// Type-level proof (ADR-0100): a `Pod`-without-`Serialize` cast kind
    /// is repliable through every native reply entry point — the bounds
    /// relaxed from `K: Kind + serde::Serialize` to `K: Kind`. Never
    /// called; the compile is the assertion. If a reply bound regains a
    /// `serde::Serialize` half, this stops compiling.
    #[allow(dead_code)]
    fn _assert_cast_kind_repliable(ctx: &mut NativeCtx<'_>, sender: Source) {
        OutboundReply::reply(ctx, &CastOnly { code: 2 });
        OutboundReply::reply_to(ctx, sender, &CastOnly { code: 3 });
        ctx.reply_to_target(sender, &CastOnly { code: 4 }, MailId::NONE, None);
    }
}
