//! How mail leaves this ctx.
//!
//! Several surfaces over one buffered push. Every held-reference verb takes a
//! [`Target`], a proof checked for the kind it sends (ADR-0230, ADR-0231 §4),
//! and `fanout` multicasts one encoding to a runtime recipient set of such
//! proofs. A boundary item, proven by
//! [`NativeCtx::accept_bundle`](super::NativeCtx::accept_bundle) or, for a
//! wire `Call`, [`NativeCtx::accept_call`](super::NativeCtx::accept_call),
//! leaves only through `deliver_detached` or `deliver_forwarded`. The call a
//! handler is serving is forwarded, reply target and chain intact, by
//! `deliver_forwarded` for a bundle item and by `forward_to` for a typed
//! payload to a proof. A declared dependency is mailed by type through the
//! flat `send`, `send_with_context` and `send_detached`, which compile only
//! on a ctx whose actor declares the target (ADR-0232 §1). The
//! per-stage capability traits carry the typed vocabulary FFI guests share:
//! [`MailSender`] on every mode and [`OutboundReply`] on [`Unchecked`] only, so
//! a handler whose class disagrees with what it does fails to unify rather
//! than lying in its manifest.
//!
//! Every typed verb encodes through the envelope encoder (ADR-0238 decision
//! 3): each `Blob` field is shared through the engine store and its entry
//! rides the envelope, so an in-process recipient reads the same bytes.
//! `send_encoded_detached_to` and the deferred envelope reply carry
//! pre-encoded bytes, which may already hold tag-1 fields when a handler
//! forwards what it received. While the handled mail has attachments, each
//! such hash resolves against them and the entry is attached, and a hash they
//! do not carry refuses the send (resolve on send, ADR-0238 decision 3). A
//! handler whose mail has no attachments holds no blob, so its pre-encoded
//! sends go out unwalked.

use aether_actor::{
    Anyone, CallerAddressable, DependencyResolver, DependsOn, ErasedActorRef, MailSender, OutboundReply, ReplyMode, SendableTo,
    Singleton, Target, Unchecked,
};
use aether_data::{ActorMail, Encoded, Kind, KindId, MailId, RequestId};

use crate::actor::native::binding::OutboundSend;
use crate::actor::native::envelope::Envelope;
use crate::mail::attachments::{Attachments, EncodedMail, encode_envelope, resolve_on_send};
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
    /// is sent, since no typed bound checks the raw kind here. The bytes'
    /// tag-1 fields resolve against the handled mail's attachments as
    /// [`Self::send_encoded_detached_to`] resolves them, and a hash they do
    /// not carry is refused the same way.
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
        let Some(attachments) = self.resolve_forward(kind, bytes) else {
            return;
        };
        let payload = EncodedMail { bytes: bytes.to_vec(), attachments };
        self.binding.send_reply_envelope_for_handler(sender, kind, payload, root, parent);
    }

    /// Lineage-aware multicast: encode `payload` once, then push one copy
    /// to every `recipient`. The inbound `(mail_id, root)` from this ctx
    /// propagate as `parent_mail` + `inherited_root`, so each fanned-out
    /// copy lands in the same causal chain as the inbound that triggered
    /// the fanout — every subscriber-bound copy gets its own fresh
    /// `MailId` keyed under the same parent edge.
    ///
    /// Recipients are not known to share a receiver type at the compile
    /// site — subscribers register at runtime — so this takes a runtime set
    /// rather than the typed `R: Singleton + HandlesKind<K>` shape of the
    /// flat [`Self::send`]. Each recipient is a held proof checked for `K`
    /// the way [`Self::send_to`] checks one: a publisher's subscriber set is
    /// `ProtocolRef<Subscriber<K>>`s, proven when each subscription was
    /// accepted (ADR-0230, ADR-0231 §8), so a fan-out of any other kind does
    /// not compile. The empty recipient set is a fast no-op — encoding only
    /// runs when there's at least one consumer.
    ///
    /// Issue iamacoffeepot/aether#723.
    pub fn fanout<K: ActorMail, I, T: Target<K, I>>(&mut self, recipients: impl IntoIterator<Item = T>, payload: &K)
    where
        T::Sender: aether_actor::CoveredBy<A>,
    {
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
            recipient: first.erased().id().0,
            kind,
            bytes: &encoded.bytes,
            attachments,
            count: 1,
            parent_mail: parent,
            inherited_root: root,
        });
        for recipient in recipients {
            self.binding.push_envelope_buffered(OutboundSend {
                recipient: recipient.erased().id().0,
                kind,
                bytes: &encoded.bytes,
                attachments,
                count: 1,
                parent_mail: parent,
                inherited_root: root,
            });
        }
    }

    /// Resolve the tag-1 fields of `bytes`, a `kind` mail, and push them to
    /// `target` under `(parent, root)`: the push behind
    /// [`Self::send_encoded_detached_to`].
    fn push_encoded(
        &self,
        target: ErasedActorRef,
        kind: KindId,
        bytes: &[u8],
        parent: Option<MailId>,
        root: Option<MailId>,
    ) -> Option<MailId> {
        let attachments = self.resolve_forward(kind, bytes)?;
        Some(self.binding.push_envelope_buffered(OutboundSend {
            recipient: target.id().0,
            kind: kind.0,
            bytes,
            attachments: attachments.as_deref().unwrap_or_default(),
            count: 1,
            parent_mail: parent,
            inherited_root: root,
        }))
    }

    /// Dispatch a payload encoded elsewhere to the actor `target` proves on a
    /// fresh causal chain, ignoring this handler's in-flight lineage, and
    /// return the minted [`MailId`], the new chain's root.
    ///
    /// The kind comes from `K` and the bytes from [`Encoded<K>`], whose only
    /// producer is an encode of a `K`, so the pair cannot disagree. The
    /// target is kind-checked through [`Target`] as [`Self::send_to`]'s is: a
    /// [`ProtocolRef<P>`](aether_actor::ProtocolRef) compiles only for a kind
    /// `P` lists, an unchecked row included. `K: ActorMail` keeps engine-only mail
    /// out at compile time, so no runtime refusal repeats it. The bytes'
    /// tag-1 fields resolve against the handled mail's attachments, and a hash
    /// they do not carry, or bytes that do not match `K`'s schema, is refused
    /// with a warning and `None`.
    ///
    /// It serves a sender that encodes off the thread that sends: the HTTP
    /// server's reader encodes each buffered request and its dispatch shard
    /// sends the bytes to the route holder (ADR-0135 §2).
    #[must_use]
    pub fn send_encoded_detached_to<K: ActorMail, I, T: Target<K, I>>(
        &self,
        target: T,
        payload: &Encoded<K>,
    ) -> Option<MailId>
    where
        T::Sender: aether_actor::CoveredBy<A>,
    {
        self.push_encoded(target.erased(), K::ID, payload.as_bytes(), None, None)
    }

    /// Send `payload` through the held reference `target`, inheriting this
    /// handler's causal chain (ADR-0080 §7, ADR-0232 §1).
    ///
    /// An [`ActorRef<R>`](aether_actor::ActorRef) target is kind-checked, so
    /// the send compiles only when `R` handles `K`, and a
    /// [`ProtocolRef<P>`](aether_actor::ProtocolRef) target is checked
    /// against `P`'s rows, so it compiles only when `P` lists `K`; the row's
    /// index `I` is inferred. An [`ErasedActorRef`] is not a target: a holder
    /// casts it once to a protocol where it arrives (ADR-0231 §4).
    ///
    /// Its consumer is the fleet server's `TerminateEngine` forward to the
    /// proxy its spawn proved.
    pub fn send_to<K: ActorMail, I, T: Target<K, I>>(&mut self, target: T, payload: &K)
    where
        T::Sender: aether_actor::CoveredBy<A>,
    {
        let _ = self.push_to(target.erased(), payload, self.outbound_parent(), self.outbound_root());
    }

    /// Send `payload` through the held reference `target` on a fresh causal
    /// chain and return the emitted mail's [`MailId`], which is also the new
    /// chain's root. The detached mail has no parent, and replies are
    /// addressed to this sender.
    ///
    /// The target is kind-checked as [`Self::send_to`]'s is: an
    /// [`ActorRef<R>`](aether_actor::ActorRef) accepts only kinds `R`
    /// handles, while a [`ProtocolRef<P>`](aether_actor::ProtocolRef) accepts
    /// only kinds listed by `P`, including an explicitly unchecked row. The row
    /// index `I` is inferred and the send performs only the existing encode
    /// and buffered push.
    ///
    /// ```
    /// use aether_actor::{Erased, Unchecked, ProtocolRef, Undeclared, protocol};
    /// use aether_kinds::Ping;
    /// use aether_substrate::actor::native::NativeCtx;
    ///
    /// #[protocol]
    /// trait Pings {
    ///     fn ping(mail: Ping) -> Undeclared;
    /// }
    ///
    /// fn detached(ctx: &mut NativeCtx<'_, Erased, Unchecked>, target: ProtocolRef<Pings>, mail: &Ping) {
    ///     let _mail_id = ctx.send_detached_to(target, mail);
    /// }
    /// ```
    pub fn send_detached_to<K: ActorMail, I, T: Target<K, I>>(&mut self, target: T, payload: &K) -> MailId
    where
        T::Sender: aether_actor::CoveredBy<A>,
    {
        self.push_to(target.erased(), payload, None, None)
    }

    /// Send `payload` through the held reference `target` and move `context`
    /// into the request-context table under the minted correlation, for the
    /// reply handler to take back with
    /// [`Self::take_context`](super::NativeCtx::take_context).
    ///
    /// It inherits this handler's causal chain as [`Self::send_to`] does and
    /// returns the minted [`MailId`]. The target is kind-checked the same way:
    /// an [`ActorRef<R>`](aether_actor::ActorRef) only for the kinds `R`
    /// handles, a [`ProtocolRef<P>`](aether_actor::ProtocolRef) only for the
    /// kinds `P` lists.
    ///
    /// Its consumers are the bloomery driver's journal reads and appends,
    /// through its typed journal reference, and its four bundle-root sends
    /// (`Invoke`, `Warm`, `Evaluate`, and `StatusQuery`, through the role
    /// protocol it cast the load reply's stamped sender to).
    #[must_use]
    pub fn send_to_with_context<K: ActorMail, C: Kind, I, T: Target<K, I>>(
        &mut self,
        target: T,
        payload: &K,
        context: C,
    ) -> MailId
    where
        T::Sender: aether_actor::CoveredBy<A>,
    {
        let mail_id = self.push_to(target.erased(), payload, self.outbound_parent(), self.outbound_root());
        self.park_context(mail_id, context);
        mail_id
    }

    /// Send `payload` through the held reference `target` on a fresh causal
    /// chain and move `context` into the request-context table under the
    /// minted correlation, for the reply handler to take back with
    /// [`Self::take_context`](super::NativeCtx::take_context). The returned
    /// [`MailId`] is the root of the new chain.
    ///
    /// The detached sibling of [`Self::send_to_with_context`], for a request
    /// the running chain did not cause and whose recipient may park the reply
    /// (ADR-0080 §7): inheriting would hold the running chain open for as
    /// long as the recipient waits. The reply roots in the recipient's tree
    /// and still correlates home through the stored context. The target is
    /// kind-checked as [`Self::send_to_with_context`]'s is.
    ///
    /// Its consumer is the bloomery driver's `WatchHead`, the long poll the
    /// journal owner parks until the head moves.
    #[must_use]
    pub fn send_detached_to_with_context<K: ActorMail, C: Kind, I, T: Target<K, I>>(
        &mut self,
        target: T,
        payload: &K,
        context: C,
    ) -> MailId
    where
        T::Sender: aether_actor::CoveredBy<A>,
    {
        let mail_id = self.push_to(target.erased(), payload, None, None);
        self.park_context(mail_id, context);
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
    pub fn send<R: Singleton + CallerAddressable>(&mut self, payload: &(impl SendableTo<R> + aether_actor::SentBy<A, R>))
    where
        A: DependsOn<R>,
        R::Resolver: DependencyResolver,
    {
        let _ = self.push_to(self.actor_ref::<R>().erase(), payload, self.outbound_parent(), self.outbound_root());
    }

    /// Send `payload` to the declared dependency `R` as [`Self::send`] does
    /// and move `context` into the request-context table under the minted
    /// correlation, for the reply handler to take back with
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
        payload: &(impl SendableTo<R> + aether_actor::SentBy<A, R>),
        context: impl Kind,
    ) -> MailId
    where
        A: DependsOn<R>,
        R::Resolver: DependencyResolver,
    {
        let mail_id =
            self.push_to(self.actor_ref::<R>().erase(), payload, self.outbound_parent(), self.outbound_root());
        self.park_context(mail_id, context);
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
    pub fn send_detached<R: Singleton + CallerAddressable>(&mut self, payload: &(impl SendableTo<R> + aether_actor::SentBy<A, R>))
    where
        A: DependsOn<R>,
        R::Resolver: DependencyResolver,
    {
        let _ = self.push_to(self.actor_ref::<R>().erase(), payload, None, None);
    }

    /// Resolve on send for a raw verb (ADR-0238 decision 3): the entries the
    /// tag-1 fields of `bytes`, a `kind` mail, name, found among the handled
    /// mail's attachments. `Some(None)`, with nothing walked, when the handled
    /// mail has none. `None`, after a warning naming the kind and the hash or
    /// the malformation, when the verb must refuse.
    fn resolve_forward(&self, kind: KindId, bytes: &[u8]) -> Option<Attachments> {
        let held = self.inbound().map(Envelope::attachments).unwrap_or_default();
        if held.is_empty() {
            return Some(None);
        }
        let own = |hash| held.iter().find(|entry| entry.hash() == hash).cloned();
        match resolve_on_send(self.binding.mailer().registry(), kind, bytes, own) {
            Ok(attachments) => Some(attachments),
            Err(error) => {
                tracing::warn!(target: "aether_substrate::mail", kind = %kind, %error, "raw send refused at the sender");
                None
            }
        }
    }

    /// Encode `payload` for an in-process send: each `Blob` field shared
    /// through the engine store and attached (ADR-0238 decision 3).
    fn encode_in_process<K: Kind>(&self, payload: &K) -> EncodedMail {
        encode_envelope(self.binding.mailer().blob_store(), payload)
    }

    /// Move `context` into the request-context table under `mail_id`'s
    /// correlation (ADR-0243 §4): the sender keeps no copy it could answer
    /// after the reply's take, and each `Held` it carries parks in this
    /// actor's in-flight ledger until the reply's take claims it back.
    fn park_context(&self, mail_id: MailId, context: impl Kind) {
        self.binding.store_request_context(RequestId(mail_id.correlation_id), context);
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

    /// Deliver a proven boundary item on a fresh causal chain, ignoring this
    /// handler's in-flight lineage, and return the minted [`MailId`] — the root of that chain, which a settlement subscription
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
    /// The target is kind-checked for the forwarded kind through [`Target`].
    /// A concrete actor reference therefore accepts only kinds that actor
    /// handles, while a protocol reference accepts only its listed rows. The
    /// target's reply row is deliberately unchecked: a relay preserves the
    /// inbound reply destination, and the forwarding handler's unchecked row
    /// declares no reply shape (ADR-0231 §9).
    ///
    /// Its consumer is the `aether.window` root's forward of a per-window
    /// command to the sole live window.
    /// An unchecked protocol row is a valid relay target:
    ///
    /// ```
    /// use aether_actor::{Erased, Unchecked, ProtocolRef, Undeclared, protocol};
    /// use aether_kinds::Ping;
    /// use aether_substrate::actor::native::NativeCtx;
    ///
    /// #[protocol]
    /// trait Pings {
    ///     fn ping(mail: Ping) -> Undeclared;
    /// }
    ///
    /// fn forward(ctx: &NativeCtx<'_, Erased, Unchecked>, target: ProtocolRef<Pings>, mail: &Ping) {
    ///     ctx.forward_to(target, mail);
    /// }
    /// ```
    ///
    /// A kind outside the target's protocol is rejected at compile time:
    ///
    /// ```compile_fail,E0277
    /// use aether_actor::{Erased, Unchecked, ProtocolRef, Undeclared, protocol};
    /// use aether_kinds::{Ping, Pong};
    /// use aether_substrate::actor::native::NativeCtx;
    ///
    /// #[protocol]
    /// trait Pings {
    ///     fn ping(mail: Ping) -> Undeclared;
    /// }
    ///
    /// fn wrong(ctx: &NativeCtx<'_, Erased, Unchecked>, target: ProtocolRef<Pings>, mail: &Pong) {
    ///     ctx.forward_to(target, mail);
    /// }
    /// ```
    pub fn forward_to<K: ActorMail, I>(&self, target: impl Target<K, I>, payload: &K) {
        let encoded = self.encode_in_process(payload);
        self.binding.push_envelope_buffered_with_reply_to(
            OutboundSend {
                recipient: target.erased().id().0,
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

/// The raw-kind reply's ADR-0233 door: `true`, after a warning, when `kind` is
/// engine-only mail an actor may not originate.
fn refuse_engine_only(kind: KindId) -> bool {
    let refused = is_engine_only(kind);
    if refused {
        tracing::warn!(target: "aether_substrate::mail", kind = %kind, "actor-originated engine-only mail refused");
    }
    refused
}

// The per-stage capability trait impls (`MailSender` / `OutboundReply`).
// The shared detached send takes the same `Target` the inherent verb does;
// native delegates it to the inherent verb and discards the returned id.
// `shutdown` / `monitor` are inherent methods on `NativeCtx` that reach into
// the substrate-internal spawner + actor registry.

impl<M: ReplyMode, A> MailSender for NativeCtx<'_, A, M> {
    fn prev_correlation(&self) -> u64 {
        self.binding.prev_correlation()
    }

    fn send_detached_to<K: ActorMail, I>(&mut self, target: impl Target<K, I, Sender = Anyone>, payload: &K) {
        let _ = NativeCtx::send_detached_to(self, target, payload);
    }
}

// ADR-0112: the reply surface is per-mode. `Unchecked` carries it (a
// unchecked-class handler issues its own replies); `Single` deliberately
// does not, so a `-> ()` single handler is provably silent and a stray
// single-ctx `ctx.reply` is a compile error rather than a manifest lie.
impl<A> OutboundReply for NativeCtx<'_, A, Unchecked> {
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
