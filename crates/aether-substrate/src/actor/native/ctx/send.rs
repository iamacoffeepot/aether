//! How mail leaves this ctx.
//!
//! Three surfaces over one buffered push. The untyped `send_envelope_*`
//! family carries already-encoded `(kind, bytes)` for endpoints that hold no
//! compile-time types, addressed only by proof (ADR-0230), and `fanout`
//! multicasts one encoding to a runtime recipient set of proofs. A
//! boundary item, proven by
//! [`NativeCtx::accept_bundle`](super::NativeCtx::accept_bundle) or, for a
//! wire `Call`, [`NativeCtx::accept_call`](super::NativeCtx::accept_call),
//! leaves only through `deliver_detached` or `deliver_forwarded`. The call a
//! handler is serving is forwarded, reply target and chain intact, by
//! `deliver_forwarded` for a bundle item and by `forward_to` for a typed
//! payload to a proof. A declared dependency is mailed by type through the
//! flat `send`, `send_with_context` and `send_detached`, which compile only
//! on a ctx whose actor declares the target (ADR-0232 §1). The
//! per-stage capability traits carry the typed vocabulary FFI guests share:
//! [`MailSender`] on every mode and [`OutboundReply`] on [`Manual`] only, so
//! a handler whose class disagrees with what it does fails to unify rather
//! than lying in its manifest.
//!
//! Every typed verb encodes through the envelope encoder (ADR-0238 decision
//! 3): each `Blob` field is shared through the engine store and its entry
//! rides the envelope, so an in-process recipient reads the same bytes. The
//! raw verbs carry pre-encoded bytes and attach nothing.

use aether_actor::{
    CallerAddressable, DependencyResolver, DependsOn, ErasedActorRef, MailSender, Manual, OutboundReply, ReplyMode,
    SendableTo, Singleton, Target,
};
use aether_data::{ActorMail, Kind, KindId, MailId, RequestId};

use crate::actor::native::binding::OutboundSend;
use crate::mail::attachments::{EncodedMail, encode_envelope};
use crate::mail::boundary::is_engine_only;
use crate::mail::{BoundaryMail, Source};

use super::NativeCtx;

impl<M: ReplyMode, A> NativeCtx<'_, A, M> {
    /// Reply to an explicit [`Source`] under an explicit `(root, parent)`
    /// lineage rather than the inbound's own sender / this ctx's in-flight
    /// chain. The ADR-0093 hold-until-resolve path reaches for this:
    /// [`TaskDone::resolve`](crate::actor::native::TaskDone::resolve) re-replies through the *originating* caller's
    /// reply target (captured at dispatch, parked in the in-flight ledger)
    /// under the root the parked [`SettlementHold`](crate::runtime::trace::SettlementHold) keeps open — not the
    /// completion-wake's sender / chain (the worker thread's loopback
    /// mail, which has no caller behind it). Passing the hold's root keeps
    /// the deferred reply's `Sent` in the chain the hold is gating, so the
    /// chain settles only after the reply lands (#1695). Routes through
    /// the same binding reply path (the crate-private
    /// `NativeBinding::send_reply_for_handler`) as [`OutboundReply::reply`].
    pub fn reply_to_target<K: ActorMail>(
        &mut self,
        sender: Source,
        payload: &K,
        root: Option<MailId>,
        parent: Option<MailId>,
    ) {
        self.binding.send_reply_for_handler(sender, payload, root, parent);
    }

    /// [`Self::reply_to_target`] for an already-encoded reply of `kind`, the
    /// body of
    /// [`DeferredReply::reply_envelope`](crate::actor::native::DeferredReply::reply_envelope).
    /// An engine-only `kind` (ADR-0233) is refused with a warning and nothing
    /// is sent, as the `send_envelope_*_to` verbs refuse it.
    pub(crate) fn reply_envelope_to_target(
        &mut self,
        sender: Source,
        kind: KindId,
        bytes: &[u8],
        root: Option<MailId>,
        parent: Option<MailId>,
    ) {
        if refuse_engine_only(kind) {
            return;
        }
        self.binding.send_reply_envelope_for_handler(sender, kind, bytes, root, parent);
    }

    /// Lineage-aware multicast: encode `payload` once, then push one copy
    /// to every `recipient`. The inbound `(mail_id, root)` from this ctx
    /// propagate as `parent_mail` + `inherited_root`, so each fanned-out
    /// copy lands in the same causal chain as the inbound that triggered
    /// the fanout — every subscriber-bound copy gets its own fresh
    /// `MailId` keyed under the same parent edge.
    ///
    /// Recipients still aren't known to share a receiver type at compile site
    /// — subscribers register at runtime — so this keeps taking a runtime set
    /// rather than the typed `R: Singleton + HandlesKind<K>` shape of the
    /// flat [`Self::send`]. What each one is has narrowed: an
    /// [`ErasedActorRef`] the publisher already holds, proven when the
    /// subscription was accepted (ADR-0230), not a position handed over at
    /// the fan-out. The empty recipient set is a fast no-op — encoding only
    /// runs when there's at least one consumer.
    ///
    /// Issue iamacoffeepot/aether#723.
    pub fn fanout<K: ActorMail>(&mut self, recipients: impl IntoIterator<Item = ErasedActorRef>, payload: &K) {
        let mut recipients = recipients.into_iter();
        let Some(first) = recipients.next() else {
            return;
        };
        let encoded = self.encode_in_process(payload);
        let attachments = encoded.attachments.as_deref().unwrap_or_default();
        let parent = self.outbound_parent();
        let root = self.outbound_root();
        let kind = K::ID.0;
        self.binding.push_envelope_buffered(OutboundSend {
            recipient: first.id().0,
            kind,
            bytes: &encoded.bytes,
            attachments,
            count: 1,
            parent_mail: parent,
            inherited_root: root,
        });
        for recipient in recipients {
            self.binding.push_envelope_buffered(OutboundSend {
                recipient: recipient.id().0,
                kind,
                bytes: &encoded.bytes,
                attachments,
                count: 1,
                parent_mail: parent,
                inherited_root: root,
            });
        }
    }

    /// The tracked send through a proof: dispatch already-encoded bytes of
    /// `kind` to the actor `target` proves (ADR-0230), inheriting this
    /// handler's causal chain, and return the minted [`MailId`] for
    /// settlement subscription.
    ///
    /// It takes no `R: HandlesKind<K>` gate, which would need the kind and
    /// receiver at the compile site. An endpoint that routes mail with
    /// runtime kinds holds neither, only the proof and opaque payload bytes,
    /// so this dispatches through the same lineage-aware path the typed
    /// verbs take without that check. A capability fanning
    /// out pre-encoded bytes to its own subscriber table is the shape this
    /// exists for: `SyntheticWindowCapability::on_inject` replays an injected
    /// event to the window subscribers, and `aether-lifecycle`'s
    /// `broadcast_to_subscribers` pushes each stage payload to the proofs its
    /// subscriber table holds.
    ///
    /// At a chassis-root edge (no `in_flight_mail_id`) the returned id
    /// is the root of a fresh causal chain; mid-handler it is the new mail's
    /// id inside the inherited chain, and a settlement subscription on it
    /// fires when *that mail's* descendants settle, not the whole chain.
    ///
    /// Differs from [`Self::fanout`] only in what it carries: `fanout`
    /// encodes one typed `K` and pushes it to many recipients, while this
    /// takes `(KindId, &[u8])` already encoded and dispatches one.
    ///
    /// An engine-only `kind` (ADR-0233) is refused with a warning and
    /// `None`, since no typed bound checks the raw kind here: nothing was
    /// sent, so there is no mail id to hand back.
    #[must_use]
    pub fn send_envelope_tracked_to(&self, target: ErasedActorRef, kind: KindId, bytes: &[u8]) -> Option<MailId> {
        if refuse_engine_only(kind) {
            return None;
        }
        Some(self.binding.push_envelope_buffered(OutboundSend {
            recipient: target.id().0,
            kind: kind.0,
            bytes,
            attachments: &[],
            count: 1,
            parent_mail: self.outbound_parent(),
            inherited_root: self.outbound_root(),
        }))
    }

    /// Dispatch already-encoded bytes of `kind` to the actor `target` proves
    /// (ADR-0230) on a fresh causal chain, ignoring this handler's in-flight
    /// lineage, and return the minted [`MailId`]: the root of the new chain,
    /// so a settlement subscription on it fires when the dispatch's whole
    /// descendant subtree drains.
    ///
    /// Use this when the cap is acting on an external event (wire-borne
    /// RPC call, file watcher, timer) rather than forwarding a mail that
    /// was already in flight. `RpcServerState::handle_call` is the model
    /// consumer: it relays an engine-addressed wire `Call` to the proxy
    /// registered for that engine. The inbound that wakes the cap is an
    /// internal wake mail causally unrelated to the wire-borne `Call`, so
    /// inheriting its chain would attribute the dispatch to the wrong root
    /// and `subscribe_settlement_mail` would never fire (descendants don't
    /// settle individually; only the chain root does).
    ///
    /// An engine-only `kind` (ADR-0233) is refused with a warning and
    /// `None`, as [`Self::send_envelope_tracked_to`] refuses it.
    #[must_use]
    pub fn send_envelope_detached_to(&self, target: ErasedActorRef, kind: KindId, bytes: &[u8]) -> Option<MailId> {
        if refuse_engine_only(kind) {
            return None;
        }
        Some(self.binding.push_envelope_buffered(OutboundSend {
            recipient: target.id().0,
            kind: kind.0,
            bytes,
            attachments: &[],
            count: 1,
            parent_mail: None,
            inherited_root: None,
        }))
    }

    /// Send `payload` through the held reference `target`, inheriting this
    /// handler's causal chain (ADR-0080 §7, ADR-0232 §1).
    ///
    /// An [`ActorRef<R>`](aether_actor::ActorRef) target is kind-checked, so
    /// the send compiles only when `R` handles `K`. An [`ErasedActorRef`] is
    /// not, which ADR-0230 §2 allows for a proof whose actor type the caller
    /// cannot name.
    ///
    /// Its consumer is the fleet server's `TerminateEngine` forward to the
    /// proxy its spawn proved.
    pub fn send_to<K: ActorMail>(&mut self, target: impl Target<K>, payload: &K) {
        let _ = self.push_to(target.erased(), payload, self.outbound_parent(), self.outbound_root());
    }

    /// Send `payload` through the held reference `target` and store `context`
    /// under the minted correlation, for the reply handler to take back with
    /// [`Self::take_context`](super::NativeCtx::take_context).
    ///
    /// It inherits this handler's causal chain as [`Self::send_to`] does and
    /// returns the minted [`MailId`]. The target is kind-checked the same way:
    /// an [`ActorRef<R>`](aether_actor::ActorRef) only for the kinds `R`
    /// handles, an [`ErasedActorRef`] unchecked.
    ///
    /// Its consumers are the bloomery driver's journal reads and appends,
    /// through its typed journal reference, and its four bundle-root sends
    /// (`Invoke`, `Warm`, `Evaluate`, and `StatusQuery`, to the erased root it
    /// kept from its load reply's stamped sender).
    #[must_use]
    pub fn send_to_with_context<K: ActorMail, C: Kind>(
        &mut self,
        target: impl Target<K>,
        payload: &K,
        context: &C,
    ) -> MailId {
        let mail_id = self.push_to(target.erased(), payload, self.outbound_parent(), self.outbound_root());
        self.binding.store_request_context(RequestId(mail_id.correlation_id), context);
        mail_id
    }

    /// Send `payload` through the held reference `target` on a fresh causal
    /// chain and store `context` under the minted correlation, for the reply
    /// handler to take back with
    /// [`Self::take_context`](super::NativeCtx::take_context). The returned
    /// [`MailId`] is the root of the new chain.
    ///
    /// The detached sibling of [`Self::send_to_with_context`], for a request
    /// the running chain did not cause and whose recipient may park the reply
    /// (ADR-0080 §7): inheriting would hold the running chain open for as
    /// long as the recipient waits. The reply roots in the recipient's tree
    /// and still correlates home through the stored context.
    ///
    /// Its consumer is the bloomery driver's `WatchHead`, the long poll the
    /// journal owner parks until the head moves.
    #[must_use]
    pub fn send_detached_to_with_context<K: ActorMail, C: Kind>(
        &mut self,
        target: impl Target<K>,
        payload: &K,
        context: &C,
    ) -> MailId {
        let mail_id = self.push_to(target.erased(), payload, None, None);
        self.binding.store_request_context(RequestId(mail_id.correlation_id), context);
        mail_id
    }

    /// Send `payload` to the declared dependency `R`, inheriting this
    /// handler's causal chain (ADR-0080 §7, ADR-0232 §1).
    ///
    /// Compiles only on a ctx typed by an actor that declares `R` with
    /// `#[actor(depends(R))]` (`A: DependsOn<R>`), and only for a kind `R`
    /// handles; the turbofish names only `R`. It sends through the proof
    /// [`Self::actor_ref`] mints, so it lands exactly where the dependency's
    /// proof points, under the running chain's root with the handled mail as
    /// its parent. [`Self::send_detached`] is the fresh-chain sibling.
    ///
    /// Its consumers are `aether.text`'s render sends: the atlas texture's
    /// creation, glyph uploads, atlas resyncs, and each draw's textured-quad
    /// batch.
    pub fn send<R: Singleton + CallerAddressable>(&mut self, payload: &impl SendableTo<R>)
    where
        A: DependsOn<R>,
        R::Resolver: DependencyResolver,
    {
        let _ = self.push_to(self.actor_ref::<R>().erase(), payload, self.outbound_parent(), self.outbound_root());
    }

    /// Send `payload` to the declared dependency `R` as [`Self::send`] does
    /// and store `context` under the minted correlation, for the reply
    /// handler to take back with
    /// [`Self::take_context`](super::NativeCtx::take_context).
    ///
    /// It carries [`Self::send`]'s bound (`A: DependsOn<R>`), inherits this
    /// handler's causal chain, and returns the minted [`MailId`].
    ///
    /// Its consumers are the `aether.fs` reads `aether.audio` forwards for a
    /// track, an instrument's `.sfz` file and each of its samples, and the
    /// font read `aether.text` forwards.
    #[must_use]
    pub fn send_with_context<R: Singleton + CallerAddressable>(
        &mut self,
        payload: &impl SendableTo<R>,
        context: &impl Kind,
    ) -> MailId
    where
        A: DependsOn<R>,
        R::Resolver: DependencyResolver,
    {
        let mail_id =
            self.push_to(self.actor_ref::<R>().erase(), payload, self.outbound_parent(), self.outbound_root());
        self.binding.store_request_context(RequestId(mail_id.correlation_id), context);
        mail_id
    }

    /// Send `payload` to the declared dependency `R` on a fresh causal chain,
    /// ignoring this handler's in-flight lineage (ADR-0080 §7, ADR-0232 §1–§2).
    ///
    /// Compiles only on a ctx typed by an actor that declares `R` with
    /// `#[actor(depends(R))]`, and only for a kind `R` handles; the turbofish
    /// names only `R`. It sends through the proof [`Self::actor_ref`] mints,
    /// so it lands exactly where the dependency's proof points. A send the
    /// running chain caused inherits it through [`Self::send`] instead.
    ///
    /// Its consumers are the fleet proxy's liveness and death reports to the
    /// fleet server: the `Pong` or connection close behind each is an
    /// external event, causally unrelated to whatever inbound woke the
    /// handler.
    pub fn send_detached<R: Singleton + CallerAddressable>(&mut self, payload: &impl SendableTo<R>)
    where
        A: DependsOn<R>,
        R::Resolver: DependencyResolver,
    {
        let _ = self.push_to(self.actor_ref::<R>().erase(), payload, None, None);
    }

    /// Encode `payload` for an in-process send: each `Blob` field shared
    /// through the engine store and attached (ADR-0238 decision 3).
    fn encode_in_process<K: Kind>(&self, payload: &K) -> EncodedMail {
        encode_envelope(self.binding.mailer().blob_store(), payload)
    }

    /// The push behind the `send_to` family: encode `payload` and push it to
    /// `target` under the `(parent, root)` lineage, returning the minted
    /// [`MailId`].
    fn push_to<K: ActorMail>(
        &self,
        target: ErasedActorRef,
        payload: &K,
        parent: Option<MailId>,
        root: Option<MailId>,
    ) -> MailId {
        let encoded = self.encode_in_process(payload);
        self.binding.push_envelope_buffered(OutboundSend {
            recipient: target.id().0,
            kind: K::ID.0,
            bytes: &encoded.bytes,
            attachments: encoded.attachments.as_deref().unwrap_or_default(),
            count: 1,
            parent_mail: parent,
            inherited_root: root,
        })
    }

    /// Push `payload` to `target` on behalf of an owed reply: the mail's
    /// reply target is pinned to `reply_to`, the caller still waiting, and its
    /// lineage is `root`, the chain the owed reply's hold keeps open (a fresh
    /// chain when `root` is `None`). The push's settlement count is
    /// taken eagerly, so the hold may be released as soon as this returns.
    ///
    /// The body of [`TaskDone::hand_off`](crate::actor::native::TaskDone::hand_off),
    /// which owns the hold and the reply target this reads.
    pub(crate) fn push_handed_off<K: ActorMail>(
        &self,
        target: ErasedActorRef,
        payload: &K,
        root: Option<MailId>,
        reply_to: Source,
    ) {
        let encoded = self.encode_in_process(payload);
        let _ = self.binding.push_envelope_buffered_with_reply_to(
            OutboundSend {
                recipient: target.id().0,
                kind: K::ID.0,
                bytes: &encoded.bytes,
                attachments: encoded.attachments.as_deref().unwrap_or_default(),
                count: 1,
                parent_mail: None,
                inherited_root: root,
            },
            Some(reply_to),
        );
    }

    /// Deliver a proven boundary item on a fresh causal chain, as
    /// [`Self::send_envelope_detached_to`] does, and return the minted
    /// [`MailId`] — the root of that chain, which a settlement subscription
    /// can wait on.
    ///
    /// Its consumers are `aether.render`'s `CaptureFrame`, where each
    /// pre-mail's id feeds the settlement bridge that gates the capture and
    /// each after-mail is released through it once the frame is read back,
    /// and `RpcServerCapability`'s `Call` receipt, which delivers the item
    /// [`NativeCtx::accept_call`](super::NativeCtx::accept_call) proved and
    /// waits on the returned root's settlement to close the call.
    #[must_use]
    pub fn deliver_detached(&self, item: BoundaryMail) -> MailId {
        let BoundaryMail { recipient, kind, payload } = item;
        self.binding.push_envelope_buffered(OutboundSend {
            recipient: recipient.id().0,
            kind: kind.0,
            bytes: &payload,
            attachments: &[],
            count: 1,
            parent_mail: None,
            inherited_root: None,
        })
    }

    /// Deliver a proven boundary bundle item as part of the call this handler
    /// is serving: it inherits this handler's chain, and its reply target is
    /// pinned to the inbound one, so the recipient's reply goes to whoever
    /// sent the inbound mail.
    ///
    /// Its consumer is `aether.trace`'s `DispatchTraced`, whose children must
    /// share the batch root and reply to the original caller (issue 1265).
    pub fn deliver_forwarded(&self, item: BoundaryMail) {
        let BoundaryMail { recipient, kind, payload } = item;
        self.binding.push_envelope_buffered_with_reply_to(
            OutboundSend {
                recipient: recipient.id().0,
                kind: kind.0,
                bytes: &payload,
                attachments: &[],
                count: 1,
                parent_mail: self.outbound_parent(),
                inherited_root: self.outbound_root(),
            },
            Some(self.source),
        );
    }

    /// Forward the call this handler is serving to the actor `target` proves:
    /// `payload` inherits this handler's chain, so the call stays open until
    /// the target replies, and its reply target is pinned to the inbound one,
    /// so the target's reply goes to whoever sent the inbound mail.
    ///
    /// No `HandlesKind` bound checks `payload` against the target, which
    /// ADR-0230 §2 allows for an erased reference, as for any send to an
    /// [`ErasedActorRef`].
    ///
    /// Its consumers are the component host's `DropComponent` forward to the
    /// addressed trampoline and the `aether.window` root's forward of a
    /// per-window command to the sole live window.
    pub fn forward_to<K: ActorMail>(&self, target: &ErasedActorRef, payload: &K) {
        let encoded = self.encode_in_process(payload);
        self.binding.push_envelope_buffered_with_reply_to(
            OutboundSend {
                recipient: target.id().0,
                kind: K::ID.0,
                bytes: &encoded.bytes,
                attachments: encoded.attachments.as_deref().unwrap_or_default(),
                count: 1,
                parent_mail: self.outbound_parent(),
                inherited_root: self.outbound_root(),
            },
            Some(self.source),
        );
    }
}

/// The raw-kind verbs' ADR-0233 door: `true`, after a warning, when `kind` is
/// engine-only mail an actor may not originate.
fn refuse_engine_only(kind: KindId) -> bool {
    let refused = is_engine_only(kind);
    if refused {
        tracing::warn!(target: "aether_substrate::mail", kind = %kind, "actor-originated engine-only mail refused");
    }
    refused
}

// The per-stage capability trait impls (`MailSender` / `OutboundReply`).
// `send_detached_to` suppresses this handler's in-flight lineage
// (ADR-0080 §7). `shutdown` / `monitor`
// are inherent methods on `NativeCtx` that reach into the
// substrate-internal spawner + actor registry.

impl<M: ReplyMode, A> MailSender for NativeCtx<'_, A, M> {
    fn prev_correlation(&self) -> u64 {
        self.binding.prev_correlation()
    }

    // By-id detached send — the by-name body with the caller's id, `None` /
    // `None` lineage minting a fresh root (ADR-0080 §7).
    fn send_detached_to<K: ActorMail>(&mut self, target: ErasedActorRef, payload: &K) {
        let encoded = self.encode_in_process(payload);
        self.binding.push_envelope_buffered(OutboundSend {
            recipient: target.id().0,
            kind: K::ID.0,
            bytes: &encoded.bytes,
            attachments: encoded.attachments.as_deref().unwrap_or_default(),
            count: 1,
            parent_mail: None,
            inherited_root: None,
        });
    }
}

// ADR-0112: the reply surface is per-mode. `Manual` carries it (a
// manual-class handler issues its own replies); `Single` deliberately
// does not, so a `-> ()` single handler is provably silent and a stray
// single-ctx `ctx.reply` is a compile error rather than a manifest lie.
impl<A> OutboundReply for NativeCtx<'_, A, Manual> {
    type ReplyHandle = Source;

    /// Always `Some` on native — the substrate's per-handler dispatcher
    /// builds a `Source` for every inbound (broadcast / no-reply mail
    /// rides as `SourceAddr::None` inside the wrapper). The
    /// always-Some invariant is preserved by [`NativeCtx::sender`] /
    /// [`Self::reply`] inspecting the inner `SourceAddr`; the trait's
    /// `Option<Self::ReplyHandle>` shape exists for the FFI side,
    /// where a guest genuinely sees no reply target.
    fn reply_target(&self) -> Option<Source> {
        Some(self.source)
    }

    fn reply<K: ActorMail>(&mut self, payload: &K) {
        // ADR-0080 §5/§6 (#1695): a synchronous reply joins the handler's
        // causal chain — inherit this ctx's `root` + `parent` so the
        // reply's `Sent` lands in the caller's chain.
        self.binding.send_reply_for_handler(self.source, payload, self.in_flight_root, self.outbound_parent());
    }

    fn reply_to<K: ActorMail>(&mut self, sender: Source, payload: &K) {
        self.binding.send_reply_for_handler(sender, payload, self.in_flight_root, self.outbound_parent());
    }
}
