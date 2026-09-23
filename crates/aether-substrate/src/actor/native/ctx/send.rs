//! How mail leaves this ctx.
//!
//! Three surfaces over one buffered push. The untyped `send_envelope_*`
//! family carries already-encoded `(kind, bytes)` for endpoints that hold no
//! compile-time types, addressed only by proof (ADR-0230) — including the
//! wire recipient the RPC server proves at receipt — and `fanout`
//! multicasts one encoding to a runtime recipient set of proofs. A
//! boundary bundle item, proven by
//! [`NativeCtx::accept_bundle`](super::NativeCtx::accept_bundle), leaves
//! only through `deliver_detached` or `deliver_forwarded`. The call a
//! handler is serving is forwarded, reply target and chain intact, by
//! `deliver_forwarded` for a bundle item and by `forward_to` for a typed
//! payload to a proof. The
//! per-stage capability traits carry the typed vocabulary FFI guests share:
//! [`MailSender`] on every mode, [`OutboundReply`] on [`Manual`] only, and
//! [`Emit`] on [`Multi<K>`] only, so a handler whose class disagrees with
//! what it does fails to unify rather than lying in its manifest.

use aether_actor::{
    Addressable, CallerAddressable, CallerScoped, Emit, ErasedActorRef, HandlesKind, MailSender, Manual, Multi,
    OutboundReply, ReplyMode, Singleton,
};
use aether_data::{Kind, KindId, MailId, RequestId};

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
    pub fn reply_to_target<K: Kind>(&mut self, sender: Source, payload: &K, root: MailId, parent: Option<MailId>) {
        self.binding.send_reply_for_handler(sender, payload, root, parent);
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
    /// rather than the typed `R: Singleton + HandlesKind<K>` shape of
    /// [`MailSender::send`]. What each one is has narrowed: an
    /// [`ErasedActorRef`] the publisher already holds, proven when the
    /// subscription was accepted (ADR-0230), not a position handed over at
    /// the fan-out. The empty recipient set is a fast no-op — encoding only
    /// runs when there's at least one consumer.
    ///
    /// Issue iamacoffeepot/aether#723.
    pub fn fanout<K: Kind>(&mut self, recipients: impl IntoIterator<Item = ErasedActorRef>, payload: &K) {
        let mut recipients = recipients.into_iter();
        let Some(first) = recipients.next() else {
            return;
        };
        let bytes = payload.encode_into_bytes();
        let parent = self.outbound_parent();
        let root = self.outbound_root();
        let kind = K::ID.0;
        self.binding.push_envelope_buffered(first.id().0, kind, &bytes, 1, parent, root);
        for recipient in recipients {
            self.binding.push_envelope_buffered(recipient.id().0, kind, &bytes, 1, parent, root);
        }
    }

    /// Untyped sibling of [`NativeActorMailbox::send_tracked`](crate::actor::native::mailbox::NativeActorMailbox::send_tracked):
    /// dispatch already-encoded bytes of `kind` to the actor `target` proves
    /// (ADR-0230), inheriting this handler's causal chain, and return the
    /// minted [`MailId`] for settlement subscription.
    ///
    /// The typed `send_tracked` is gated on `R: HandlesKind<K>`, which needs
    /// the kind and receiver at the compile site. An endpoint that routes
    /// mail with runtime kinds holds neither, only the proof and opaque
    /// payload bytes, so this skips that check and dispatches through the
    /// same lineage-aware path the typed helpers take. A capability fanning
    /// out pre-encoded bytes to its own subscriber table is the shape this
    /// exists for: `SyntheticWindowCapability::on_inject` replays an injected
    /// event to the window subscribers, and `aether-lifecycle`'s
    /// `broadcast_to_subscribers` pushes each stage payload to the proofs its
    /// subscriber table holds.
    ///
    /// At a chassis-root edge (`in_flight_mail_id` is `NONE`) the returned id
    /// is the root of a fresh causal chain; mid-handler it is the new mail's
    /// id inside the inherited chain, and a settlement subscription on it
    /// fires when *that mail's* descendants settle, not the whole chain.
    ///
    /// Differs from [`Self::fanout`] only in what it carries: `fanout`
    /// encodes one typed `K` and pushes it to many recipients, while this
    /// takes `(KindId, &[u8])` already encoded and dispatches one.
    #[must_use]
    pub fn send_envelope_tracked_to(&self, target: ErasedActorRef, kind: KindId, bytes: &[u8]) -> MailId {
        self.binding.push_envelope_buffered(
            target.id().0,
            kind.0,
            bytes,
            1,
            self.outbound_parent(),
            self.outbound_root(),
        )
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
    /// consumer: it proves the wire `Call`'s recipient once at receipt and
    /// sends through the proof. The inbound that wakes the cap is an
    /// internal wake mail causally unrelated to the wire-borne `Call`, so
    /// inheriting its chain would attribute the dispatch to the wrong root
    /// and `subscribe_settlement_mail` would never fire (descendants don't
    /// settle individually; only the chain root does).
    #[must_use]
    pub fn send_envelope_detached_to(&self, target: ErasedActorRef, kind: KindId, bytes: &[u8]) -> MailId {
        self.binding.push_envelope_buffered(target.id().0, kind.0, bytes, 1, None, None)
    }

    /// Send `payload` to the actor `target` proves and store `context` under
    /// the minted correlation, for the reply handler to take back with
    /// [`Self::take_context`](super::NativeCtx::take_context).
    ///
    /// The erased form of `ctx.to(&actor_ref).with_context(&context).send(&payload)`:
    /// it inherits this handler's causal chain the same way and returns the
    /// minted [`MailId`]. No `HandlesKind` bound checks `payload` against the
    /// target, which ADR-0230 §2 allows for an erased reference — the caller
    /// holds a proof of a live actor whose type it cannot name.
    ///
    /// Its consumers are the bloomery driver's four bundle-root sends
    /// (`Invoke`, `Warm`, `Evaluate`, and `StatusQuery`, to the root it kept
    /// from its load reply's stamped sender) and the chassis-bloomery boot
    /// probe's `AwaitProcessed`.
    #[must_use]
    pub fn send_with_context<K: Kind, C: Kind>(&self, target: &ErasedActorRef, payload: &K, context: &C) -> MailId {
        self.push_with_context(*target, payload, context, self.outbound_parent(), self.outbound_root())
    }

    /// Send `payload` to the actor `target` proves on a fresh causal chain
    /// and store `context` under the minted correlation, for the reply
    /// handler to take back with
    /// [`Self::take_context`](super::NativeCtx::take_context). The returned
    /// [`MailId`] is the root of the new chain.
    ///
    /// The detached sibling of [`Self::send_with_context`], for a request the
    /// running chain did not cause and whose recipient may park the reply
    /// (ADR-0080 §7): inheriting would hold the running chain open for as
    /// long as the recipient waits. The reply roots in the recipient's tree
    /// and still correlates home through the stored context.
    ///
    /// Its consumer is the bloomery driver's `WatchHead`, the long poll the
    /// journal owner parks until the head moves.
    #[must_use]
    pub fn send_detached_with_context<K: Kind, C: Kind>(
        &self,
        target: &ErasedActorRef,
        payload: &K,
        context: &C,
    ) -> MailId {
        self.push_with_context(*target, payload, context, None, None)
    }

    /// The push behind [`Self::send_with_context`] and
    /// [`Self::send_detached_with_context`]: encode `payload`, push it to
    /// `target` under the `(parent, root)` lineage, and store `context` under
    /// the minted correlation.
    fn push_with_context<K: Kind, C: Kind>(
        &self,
        target: ErasedActorRef,
        payload: &K,
        context: &C,
        parent: Option<MailId>,
        root: Option<MailId>,
    ) -> MailId {
        let mail_id =
            self.binding.push_envelope_buffered(target.id().0, K::ID.0, &payload.encode_into_bytes(), 1, parent, root);
        self.binding.store_request_context(RequestId(mail_id.correlation_id), context);
        mail_id
    }

    /// Push `payload` to `target` on behalf of an owed reply: the mail's
    /// reply target is pinned to `reply_to`, the caller still waiting, and its
    /// lineage is `root`, the chain the owed reply's hold keeps open (a fresh
    /// chain when `root` is [`MailId::NONE`]). The push's settlement count is
    /// taken eagerly, so the hold may be released as soon as this returns.
    ///
    /// The body of [`TaskDone::hand_off`](crate::actor::native::TaskDone::hand_off),
    /// which owns the hold and the reply target this reads.
    pub(crate) fn push_handed_off<K: Kind>(&self, target: ErasedActorRef, payload: &K, root: MailId, reply_to: Source) {
        let bytes = payload.encode_into_bytes();
        let root = (root != MailId::NONE).then_some(root);
        let _ = self.binding.push_envelope_buffered_with_reply_to(
            target.id().0,
            K::ID.0,
            &bytes,
            1,
            None,
            root,
            Some(reply_to),
        );
    }

    /// Deliver a proven boundary bundle item on a fresh causal chain, as
    /// [`Self::send_envelope_detached_to`] does, and return the minted
    /// [`MailId`] — the root of that chain, which a settlement subscription
    /// can wait on.
    ///
    /// Its consumer is `aether.render`'s `CaptureFrame`: each pre-mail's id
    /// feeds the settlement bridge that gates the capture, and each
    /// after-mail is released through it once the frame is read back.
    #[must_use]
    pub fn deliver_detached(&self, item: BoundaryMail) -> MailId {
        let BoundaryMail { recipient, kind, payload } = item;
        self.binding.push_envelope_buffered(recipient.id().0, kind.0, &payload, 1, None, None)
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
            recipient.id().0,
            kind.0,
            &payload,
            1,
            self.outbound_parent(),
            self.outbound_root(),
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
    pub fn forward_to<K: Kind>(&self, target: &ErasedActorRef, payload: &K) {
        let bytes = payload.encode_into_bytes();
        self.binding.push_envelope_buffered_with_reply_to(
            target.id().0,
            K::ID.0,
            &bytes,
            1,
            self.outbound_parent(),
            self.outbound_root(),
            Some(self.source),
        );
    }
}

// The per-stage capability trait impls (`MailSender` / `OutboundReply`).
// `send` / `send_many` inherit this handler's
// in-flight lineage (ADR-0080 §7); `send_detached` /
// `send_detached_to` explicitly suppress it. `shutdown` / `monitor`
// are inherent methods on `NativeCtx` that reach into the
// substrate-internal spawner + actor registry.

impl<M: ReplyMode, A> MailSender for NativeCtx<'_, A, M> {
    fn send<R, K>(&mut self, payload: &K)
    where
        R: Singleton + CallerAddressable + HandlesKind<K>,
        K: Kind,
    {
        let bytes = payload.encode_into_bytes();
        self.binding.push_envelope_buffered(
            R::resolve(self.binding.scope_mailbox(<<R as Addressable>::Resolver as CallerScoped>::SCOPE), ()).0,
            K::ID.0,
            &bytes,
            1,
            self.outbound_parent(),
            self.outbound_root(),
        );
    }

    fn send_many<R, K>(&mut self, payloads: &[K])
    where
        R: Singleton + CallerAddressable + HandlesKind<K>,
        K: Kind + bytemuck::NoUninit,
    {
        let bytes: &[u8] = bytemuck::cast_slice(payloads);
        // Batch count rides as `u32` on the wire (matches the FFI ABI);
        // realistic mail batches stay well below `u32::MAX`.
        #[allow(clippy::cast_possible_truncation)]
        let count = payloads.len() as u32;
        self.binding.push_envelope_buffered(
            R::resolve(self.binding.scope_mailbox(<<R as Addressable>::Resolver as CallerScoped>::SCOPE), ()).0,
            K::ID.0,
            bytes,
            count,
            self.outbound_parent(),
            self.outbound_root(),
        );
    }

    fn prev_correlation(&self) -> u64 {
        self.binding.prev_correlation()
    }

    fn send_detached<R, K>(&mut self, payload: &K)
    where
        R: Singleton + CallerAddressable + HandlesKind<K>,
        K: Kind,
    {
        let bytes = payload.encode_into_bytes();
        // ADR-0080 §7: suppress the in-flight lineage so the recipient
        // starts a fresh causal chain.
        self.binding.push_envelope_buffered(
            R::resolve(self.binding.scope_mailbox(<<R as Addressable>::Resolver as CallerScoped>::SCOPE), ()).0,
            K::ID.0,
            &bytes,
            1,
            None,
            None,
        );
    }

    // By-id detached send — the by-name body with the caller's id, `None` /
    // `None` lineage minting a fresh root (ADR-0080 §7).
    fn send_detached_to<K: Kind>(&mut self, target: ErasedActorRef, payload: &K) {
        let bytes = payload.encode_into_bytes();
        self.binding.push_envelope_buffered(target.id().0, K::ID.0, &bytes, 1, None, None);
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

    fn reply<K: Kind>(&mut self, payload: &K) {
        // ADR-0080 §5/§6 (#1695): a synchronous reply joins the handler's
        // causal chain — inherit this ctx's `root` + `parent` so the
        // reply's `Sent` lands in the caller's chain.
        self.binding.send_reply_for_handler(self.source, payload, self.in_flight_root, self.outbound_parent());
    }

    fn reply_to<K: Kind>(&mut self, sender: Source, payload: &K) {
        self.binding.send_reply_for_handler(sender, payload, self.in_flight_root, self.outbound_parent());
    }
}

// ADR-0134: the emit surface is the multi class's, implemented only for
// the `Multi<K>` mode. Each `emit` is `send_detached_to` at the proven
// `ctx.sender()` and starts a fresh detached chain (`None` / `None`
// lineage), so an emission does not hold the request chain open. A
// sourceless dispatch (broadcast / substrate-generated mail, no
// `SourceAddr::Component`) has no routable target, so the emission
// warn-drops.
impl<K: Kind, A> Emit<K> for NativeCtx<'_, A, Multi<K>> {
    fn emit(&mut self, payload: &K) {
        let Some(target) = self.sender() else {
            tracing::warn!(
                kind = <K as Kind>::NAME,
                "multi handler emit dropped: the dispatch carries no routable \
                 source (broadcast / substrate-origin mail)",
            );
            return;
        };
        self.send_detached_to(target, payload);
    }
}
