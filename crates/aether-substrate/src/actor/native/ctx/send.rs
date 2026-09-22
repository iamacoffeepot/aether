//! How mail leaves this ctx.
//!
//! Three surfaces over one buffered push. The untyped `send_envelope_*`
//! family carries already-encoded `(kind, bytes)` for endpoints that hold no
//! compile-time types — addressed by proof where the caller holds one
//! (ADR-0230), including the wire recipient the RPC server proves at
//! receipt, and by position for callers not yet migrated — and `fanout`
//! multicasts one encoding to a runtime recipient set of proofs. The
//! per-stage capability traits carry the typed vocabulary FFI guests share:
//! [`MailSender`] on every mode, [`OutboundReply`] on [`Manual`] only, and
//! [`Emit`] on [`Multi<K>`] only, so a handler whose class disagrees with
//! what it does fails to unify rather than lying in its manifest.

use aether_actor::{
    Addressable, AnyActorRef, CallerAddressable, CallerScoped, Emit, HandlesKind, MailSender, Manual, Multi,
    OutboundReply, ReplyMode, Singleton,
};
use aether_data::{Kind, KindId, MailId, MailboxId, mailbox_id_from_path};

use crate::mail::Source;

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
    /// the same [`NativeBinding::send_reply_for_handler`](crate::actor::native::binding::NativeBinding::send_reply_for_handler) path as
    /// [`OutboundReply::reply`].
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
    /// [`AnyActorRef`] the publisher already holds, proven when the
    /// subscription was accepted (ADR-0230), not a position handed over at
    /// the fan-out. The empty recipient set is a fast no-op — encoding only
    /// runs when there's at least one consumer.
    ///
    /// Issue iamacoffeepot/aether#723.
    pub fn fanout<K: Kind>(&mut self, recipients: impl IntoIterator<Item = AnyActorRef>, payload: &K) {
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

    /// Untyped sibling of [`NativeActorMailbox::send_tracked`](crate::actor::native::mailbox::NativeActorMailbox::send_tracked): dispatch
    /// an already-encoded mail payload with runtime `recipient` /
    /// `kind` ids and return the minted [`MailId`] for settlement
    /// subscription.
    ///
    /// Issue 750: the typed `send_tracked` path is gated on
    /// `R: HandlesKind<K>`, which requires the kind and receiver to be
    /// known at compile site. Endpoints that route mail with runtime
    /// ids have neither — they hold a `MailboxId` + `KindId` + opaque
    /// payload bytes. This method is the escape hatch: skips the
    /// type-system check, dispatches the raw bytes through the same
    /// lineage-aware path the typed helpers go through.
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
    ///
    /// This stays the runtime-*position* door: the recipient arrived on
    /// the wire or came back from the registry, and nothing has proven it
    /// (ADR-0230). A caller that already holds a proof takes
    /// [`Self::send_envelope_tracked_to`] instead, and this signature
    /// narrows when its last positional caller migrates.
    #[must_use]
    pub fn send_envelope_tracked(&self, recipient: MailboxId, kind: KindId, bytes: &[u8]) -> MailId {
        self.binding.push_envelope_buffered(recipient.0, kind.0, bytes, 1, self.outbound_parent(), self.outbound_root())
    }

    /// [`Self::send_envelope_tracked`] for a caller that holds a proof:
    /// the ADR-0230 form of the untyped dispatch, taking the [`AnyActorRef`]
    /// rather than the position under it.
    ///
    /// A capability fanning out pre-encoded bytes to its own subscriber
    /// table is the shape this exists for — the rows are already proofs, so
    /// unwrapping one back to a position at the moment of the send is
    /// exactly what the stored-state rule removes. Its first consumer is
    /// `SyntheticWindowCapability::on_inject`, which replays an injected
    /// event to the window subscribers; its second is `aether-lifecycle`'s
    /// `broadcast_to_subscribers` (#6302), which pushes each stage payload
    /// to the proofs the cap's subscriber table holds.
    ///
    /// Differs from [`Self::fanout`] only in what it carries: `fanout`
    /// encodes one typed `K` and pushes it to many recipients, while this
    /// takes `(KindId, &[u8])` already encoded and dispatches one.
    #[must_use]
    pub fn send_envelope_tracked_to(&self, target: AnyActorRef, kind: KindId, bytes: &[u8]) -> MailId {
        self.binding.push_envelope_buffered(
            target.id().0,
            kind.0,
            bytes,
            1,
            self.outbound_parent(),
            self.outbound_root(),
        )
    }

    /// Re-dispatch variant of [`Self::send_envelope_tracked`] that pins the
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
    /// [`Self::send_envelope_detached_to`] dispatching directly at the
    /// receiver) never reach this method — the default `reply_to` lands
    /// at the dispatcher which is also the call-correlation owner.
    #[must_use]
    pub fn send_envelope_tracked_with_reply_to(
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

    /// Like [`Self::send_envelope_tracked`] but always starts a fresh
    /// causal chain — ignores the ctx's in-flight lineage and passes
    /// `parent_mail = None, inherited_root = None` to the dispatch
    /// path. The returned [`MailId`] is the root of the new chain, so
    /// subscribing to its settlement via
    /// `SettlementRegistry::subscribe_settlement_mail` fires when the
    /// dispatch's entire descendant subtree drains.
    ///
    /// Use this when the cap is acting on an external event (file
    /// watcher, timer) rather than forwarding a mail that was already in
    /// flight; [`Self::send_envelope_detached_to`] carries the full
    /// motivation.
    ///
    /// This stays the runtime-*position* door: the recipient came back
    /// from the registry or a stored table, and nothing has proven it
    /// (ADR-0230). A caller that already holds a proof takes
    /// [`Self::send_envelope_detached_to`] instead, and this signature
    /// narrows when its last positional caller migrates.
    #[must_use]
    pub fn send_envelope_detached(&self, recipient: MailboxId, kind: KindId, bytes: &[u8]) -> MailId {
        self.binding.push_envelope_buffered(recipient.0, kind.0, bytes, 1, None, None)
    }

    /// [`Self::send_envelope_detached`] for a caller that holds a proof:
    /// the ADR-0230 form of the fresh-chain untyped dispatch, taking the
    /// [`AnyActorRef`] rather than the position under it.
    ///
    /// Use this when the cap is acting on an external event (wire-borne
    /// RPC call, file watcher, timer) rather than forwarding a mail that
    /// was already in flight. Its first consumer is
    /// `RpcServerState::handle_call`, which proves the wire `Call`'s
    /// recipient once at receipt and sends through the proof: the inbound
    /// that wakes the cap is an internal wake mail causally unrelated to
    /// the wire-borne `Call` — inheriting its chain would attribute the
    /// dispatch to the wrong root and `subscribe_settlement_mail` would
    /// never fire (descendants don't settle individually; only the chain
    /// root does).
    #[must_use]
    pub fn send_envelope_detached_to(&self, target: AnyActorRef, kind: KindId, bytes: &[u8]) -> MailId {
        self.binding.push_envelope_buffered(target.id().0, kind.0, bytes, 1, None, None)
    }
}

// The per-stage capability trait impls (`MailSender` / `OutboundReply`).
// `send` / `send_many` / `send_to_named` inherit this handler's
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
            R::resolve(self.binding.scope_mailbox(<<R as Addressable>::Resolver as CallerScoped>::SCOPE).0, ()).0,
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
            R::resolve(self.binding.scope_mailbox(<<R as Addressable>::Resolver as CallerScoped>::SCOPE).0, ()).0,
            K::ID.0,
            bytes,
            count,
            self.outbound_parent(),
            self.outbound_root(),
        );
    }

    // Runtime-name send escape hatch (the `Resolver::send_to_named` contract):
    // the recipient name is supplied at runtime, no compile-time `R` to resolve.
    #[allow(clippy::disallowed_methods)]
    // the runtime-name routing path itself — resolves the written name by the same
    // ADR-0099 §4 parse → fold the registry does, so a lineage address routes
    fn send_to_named<K: Kind>(&mut self, name: &str, payload: &K) {
        let bytes = payload.encode_into_bytes();
        self.binding.push_envelope_buffered(
            mailbox_id_from_path(name).0,
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

    fn send_detached<R, K>(&mut self, payload: &K)
    where
        R: Singleton + CallerAddressable + HandlesKind<K>,
        K: Kind,
    {
        let bytes = payload.encode_into_bytes();
        // ADR-0080 §7: suppress the in-flight lineage so the recipient
        // starts a fresh causal chain.
        self.binding.push_envelope_buffered(
            R::resolve(self.binding.scope_mailbox(<<R as Addressable>::Resolver as CallerScoped>::SCOPE).0, ()).0,
            K::ID.0,
            &bytes,
            1,
            None,
            None,
        );
    }

    // By-id detached send — the by-name body with the caller's id, `None` /
    // `None` lineage minting a fresh root (ADR-0080 §7).
    fn send_detached_to<K: Kind>(&mut self, target: AnyActorRef, payload: &K) {
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
