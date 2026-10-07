//! The inbound frame this ctx is dispatching.
//!
//! One envelope in one place (#1757 / ADR-0094): the dispatcher moves it in
//! and takes it back at its settlement tail unless a handler first retained
//! it, so a double-settle is structurally unrepresentable. Around it sit the
//! frame's read-only views — the in-flight lineage, the reply target, the
//! correlation the inbound answers — and the chain a hold taken from this
//! context gates.

use std::fmt::Display;
use std::sync::Arc;

use aether_actor::__macro_internals::SenderRefused;
use aether_actor::{CastTarget, ErasedActorRef, OutboundReply, PathRefused, ProtocolRef, ReplyMode, Unchecked};
use aether_data::wire::{self, DecodeCtx};
use aether_data::{ActorMail, Kind, KindId, MailId, RequestId};
use aether_kinds::DecodeRefused;

use crate::actor::native::envelope::Envelope;
use crate::chassis::inbox::InboundMail;
use crate::mail::attachments::{AttachedEntries, EncodedMail};
use crate::mail::{Source, SourceAddr};
use crate::runtime::trace::SettlementHold;

use super::NativeCtx;

impl<M: ReplyMode, A> NativeCtx<'_, A, M> {
    /// #1757 / ADR-0106: retain this handler's inbound mail as an
    /// [`InboundMail`] guard to defer its reply past the handler's return
    /// — the by-construction replacement for a hand-rolled
    /// [`SettlementHold`]. Moving the single dispatched envelope out of
    /// the ctx means the dispatcher's settlement tail sees `None` and does
    /// not discharge: the returned guard now owns the obligation, and its
    /// `Drop` records the inbound's `Finished` after any `reply` it sent
    /// (ADR-0080 §6), settling the chain exactly once. The guard is
    /// `Send`, so the deferred reply may be sent from a worker thread (the
    /// desktop capture readback is the motivating consumer).
    ///
    /// # Panics
    /// Panics if there is no dispatched envelope to take — a call from an
    /// init / close-hook / chassis-root ctx (those carry none), or a
    /// second `take_inbound` after the first already moved it out.
    /// Fail-fast per ADR-0063: a correct handler takes its inbound at most
    /// once.
    #[must_use]
    pub fn take_inbound(&mut self) -> InboundMail {
        let env =
            self.inbound.take().expect("take_inbound: no dispatched envelope (init/close-hook ctx, or already taken)");
        InboundMail::from_dispatched(
            env,
            Arc::clone(self.binding.mailer()),
            self.binding.self_mailbox(),
            self.binding.reply_lineage(),
        )
    }

    /// #1757: hand the dispatched envelope back to the dispatcher's
    /// settlement tail. `Some` on the normal path (the dispatcher records
    /// `Finished` + `discharge`s the single owner); `None` when a handler
    /// retained the guard via [`Self::take_inbound`], whose own un-fired
    /// `record_finished` then closes the chain when it drops. Called once,
    /// just before the ctx's handler-end flush, so an armed inbound is
    /// never dropped inside the ctx (which would trip the ADR-0094 guard).
    pub(crate) fn take_raw_inbound(&mut self) -> Option<Envelope> {
        self.inbound.take()
    }

    /// #1774: read-only borrow of the dispatched envelope. The cold
    /// fallback/warn miss path in `typed_then_fallback_or_warn` clones
    /// from here so the full envelope is available to `#[fallback]` and
    /// the warn without paying the per-dispatch allocation the hot path
    /// never needs. `None` for ctxs that carry no inbound (init /
    /// close-hook / chassis-root / cap-test fixtures).
    pub(crate) fn inbound(&self) -> Option<&Envelope> {
        self.inbound.as_ref()
    }

    /// Decode `payload` as `K` through `K::decode_with`, against a context
    /// built from what this ctx already holds (ADR-0231 §3, ADR-0238
    /// decision 3):
    ///
    /// - the inbound envelope's attachments (none without an inbound): each
    ///   tag-1 `Blob` field resolves to the entry its hash names and yields a
    ///   `Shared` value over it, and a hash no attachment carries refuses;
    /// - the mail registry `resolve` reads: each `ProtocolPath` field is
    ///   proven against the contract the `Live` or `Dropped` route at its
    ///   path published, and a path no route has stood at, one still
    ///   starting, or one whose route does not cover the protocol, refuses.
    ///   A closed actor's path decodes, so the handler's own `resolve`
    ///   answers it.
    ///
    /// A refusal is logged once at warn, naming the kind and the error, and
    /// returned: the handler does not run. The `#[actor]` typed arms and
    /// native handler-set arms call it and hand a refusal to
    /// `__refuse_inbound` (a row that replies) or
    /// `__refuse_inbound_unanswered` (a silent or unchecked row); a
    /// hand decoder of an `&Envelope` is not affected. It decodes only the
    /// mail being handled, so it grants nothing the handler does not already
    /// receive.
    ///
    /// # Errors
    ///
    /// The [`wire::Error`] `K::decode_with` refused with.
    #[doc(hidden)]
    pub fn __decode_inbound<K: Kind>(&self, payload: &[u8]) -> Result<K, wire::Error> {
        let entries = self.inbound.as_ref().map_or(&[][..], Envelope::attachments);
        let mut blobs = AttachedEntries(entries);
        let mut ctx = DecodeCtx::empty().blobs(&mut blobs).routes(&**self.binding.mailer().registry());

        K::decode_with(payload, &mut ctx).inspect_err(|error| {
            tracing::warn!(target: "aether_substrate::mail", kind = K::NAME, %error, "decode refused");
        })
    }

    /// Drop a refused decode of a `K` payload: the typed arm's miss, so the
    /// handler does not run and no reply is sent. The reply target hears the
    /// refusal only when it opts in (see `answer_decode_refusal`); no other
    /// sender does.
    #[cold]
    #[doc(hidden)]
    #[must_use]
    pub fn __refuse_inbound_unanswered<K: Kind>(&self, error: &wire::Error) -> Option<()> {
        self.answer_decode_refusal(K::ID, error);
        None
    }

    /// Answer a refusal of a `kind` payload made before its handler ran, a
    /// refused decode or a refused sender, to the reply target with a
    /// [`DecodeRefused`] naming the kind and `error`, when the target opts in
    /// (see `NativeBinding::refusal_listener`). The notice goes through the
    /// binding's reply path, so it joins the in-flight chain and is handled
    /// before that chain's `Settled`, and this actor is its sender.
    fn answer_decode_refusal(&self, kind: KindId, error: &impl Display) {
        if self.binding.refusal_listener(self.source).is_none() {
            return;
        }

        let notice = DecodeRefused { kind, error: error.to_string() };
        let payload = EncodedMail { bytes: notice.encode_into_bytes(), attachments: None };
        self.binding.send_reply_envelope_for_handler(
            self.source,
            DecodeRefused::ID,
            payload,
            self.in_flight_root,
            self.outbound_parent(),
        );
    }

    /// The mail's sender as the protocol `P` its handler requires
    /// (ADR-0231 §11), by the guard cast [`Self::cast`] runs.
    fn prove_sender<P: CastTarget>(&self) -> Option<ProtocolRef<P>> {
        self.sender().and_then(|sender| self.cast::<P>(sender))
    }

    /// Why the sender of a `K` did not cast to `P`, logged once as an error
    /// in this actor's log. Reads the sender's path and the rows it
    /// published; both are paid only here, after the cast refused.
    #[cold]
    fn refused_sender<P: CastTarget, K: Kind>(&self) -> SenderRefused {
        let sender = self.sender();
        let rows = sender.and_then(|sender| self.binding.mailer().registry().published_rows_at(sender.id()));
        let refused = SenderRefused::of::<P>(sender.map(|sender| self.binding.actor_path(sender)), rows.as_deref());

        tracing::error!(
            target: "aether_substrate::mail",
            kind = K::NAME,
            refusal = %refused,
            "handler did not run: its sender is refused",
        );
        refused
    }

    /// Prove a tell's sender as `P` before its handler runs (ADR-0231 §11),
    /// or refuse the mail: the `#[actor]` arm of a `#[handler::tell]` that
    /// takes `sender: ProtocolRef<P>` calls it and passes the proven
    /// reference as the handler's fourth argument. Hidden macro plumbing,
    /// like [`Self::__decode_inbound`]; no handler calls it.
    ///
    /// # Errors
    ///
    /// The [`SenderRefused`] when the sender does not cast to `P`, or the
    /// mail has none. The refusal is logged, the reply target hears it only
    /// when it opts in (see `NativeBinding::refusal_listener`), and the handler
    /// does not run.
    #[doc(hidden)]
    pub fn __sender_or_refuse<P: CastTarget, K: Kind>(&self) -> Result<ProtocolRef<P>, SenderRefused> {
        if let Some(proven) = self.prove_sender::<P>() {
            return Ok(proven);
        }

        let refused = self.refused_sender::<P, K>();
        self.answer_decode_refusal(K::ID, &refused);
        Err(refused)
    }

    /// ADR-0080 §5: the [`MailId`] of the mail currently being
    /// dispatched. Read by outbound `send` paths to stamp
    /// `parent_mail` on child mail. `None` when the ctx was
    /// built without an inbound (close hook, init, chassis-pushed).
    #[must_use]
    pub fn in_flight_mail_id(&self) -> Option<MailId> {
        self.in_flight_mail_id
    }

    /// ADR-0080 §5: the root [`MailId`] of the causal chain this
    /// handler is running in. Read by outbound `send` paths to inherit
    /// `root` on child mail so descendants share the chain. A birth's
    /// `wire` ctx carries its wire root here (ADR-0244). The
    /// chassis-root case (no inbound) leaves this `None` and
    /// `NativeBinding::push_envelope_buffered` mints a fresh root.
    #[must_use]
    pub fn in_flight_root(&self) -> Option<MailId> {
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

    /// The envelope sender as a proven [`ErasedActorRef`]: mints the dispatch
    /// source the host stamped, once one published-route read finds a route
    /// record standing there, so the reference always names a path. This is the
    /// *immediate* sender (one hop, the addressing layer's `Source`), not
    /// the chain origin — the origin lives in the tracing layer (`root` /
    /// `parent_mail`, ADR-0080). `None` for mail with no local sender
    /// (broadcast, substrate-generated, hub-bubbled). One piece of
    /// host-generated mail does carry a sender: an
    /// [`aether_kinds::MonitorNotice`] is stamped with the departed actor, so
    /// a watcher reads which actor it lost from here. Needs no actor type,
    /// so it exists on the erased ctx too.
    ///
    /// A reply carries no reply target of its own (its source is
    /// `SourceAddr::None` with the answered correlation), so its sender is the
    /// actor that replied: the replier minted the reply's mail id in its own
    /// id space, and that id's sender half is the stamp. This is how a load
    /// requester keeps the loaded actor, whose trampoline sends the
    /// successful `LoadResult` itself (ADR-0230 §3). A reply sent with no
    /// handler chain (`Mailer::send_reply_unchained`) carries no mail id and
    /// so no sender.
    ///
    /// Also `None` for a stamped position that holds no route record, such as
    /// the chassis sentinel or a position forged through the public mail
    /// entry points. The read is one lock-free load of the published route
    /// view and one hash probe, paid only when a handler asks.
    #[must_use]
    pub fn sender(&self) -> Option<ErasedActorRef> {
        match self.source.addr {
            SourceAddr::Component(id) => self.binding.stamped_sender(id),
            SourceAddr::None if self.in_reply_to().is_some() => {
                self.in_flight_mail_id.and_then(|id| self.binding.stamped_sender(id.sender))
            }
            _ => None,
        }
    }

    /// Correlation id of the request this inbound reply answers.
    #[must_use]
    pub fn in_reply_to(&self) -> Option<RequestId> {
        if matches!(self.source.addr, SourceAddr::None) && self.source.correlation_id != Source::NO_CORRELATION {
            Some(RequestId(self.source.correlation_id))
        } else {
            None
        }
    }

    /// Recover and remove the typed context for the request this inbound reply
    /// answers. Returns `None` for ordinary mail, unmatched replies, a wrong
    /// context kind, or a decode failure.
    ///
    /// A wrong-kind take leaves the context stored, so a handler that serves
    /// several context kinds tries each type in turn. A decode failure
    /// consumes it.
    ///
    /// A context holding a `Held` is claimed live (ADR-0243 §4): the taken
    /// `Held` answers the caller it was armed for. A reply whose handler
    /// leaves such a context untaken fails fast once the handler returns,
    /// naming the context kind (ADR-0243 §7).
    pub fn take_context<C: Kind>(&mut self) -> Option<C> {
        let request = self.in_reply_to()?;
        self.binding.take_request_context(request)
    }
    /// Acquire a [`SettlementHold`] on the current in-flight root
    /// (ADR-0080 §12). Use to keep a chain open across deferred work that
    /// later moves it into [`Self::dispatch_blocking_resumed`]. A bounded
    /// queue buffering an over-limit request holds its reply with
    /// [`Self::hold`] instead (ADR-0243 §3).
    ///
    /// A `wire` ctx dispatches no inbound. A handler-staged birth's holds the
    /// chain that caused the birth (ADR-0168 §1), though its ctx also carries
    /// a wire root, which is what puts a birth-completing effect inside the
    /// staging caller's `Settled`. A chainless birth's — a chassis boot's or
    /// an embedder spawn's — holds its wire root (ADR-0244), so work `wire`
    /// starts on its own chain is inside the root the birth's caller awaits,
    /// and must finish for that root to settle; long-lived work opens a
    /// detached chain.
    ///
    /// `None` when neither is present: the work has no causing chain, so
    /// there is nothing to keep open and no guard to hand back (ADR-0168 §2).
    /// Deferred work started from such a context is unobservable through
    /// settlement, which is a property worth reading at the call site
    /// rather than a guard that gates nothing.
    #[must_use]
    pub fn acquire_settlement_hold(&self) -> Option<SettlementHold> {
        self.held_chain().map(|root| self.binding.mailer().acquire_settlement_hold(root))
    }

    /// The chain a hold taken from this context gates: whatever caused this
    /// context to exist when something did, and otherwise the in-flight
    /// root — the inbound's while a handler is dispatching, or a chainless
    /// birth's wire root while its `wire` runs (ADR-0244). Only a
    /// handler-staged birth's `wire` ctx carries both, and the causing chain
    /// wins there, so a birth-completing effect held from `wire` stays inside
    /// the staging caller's `Settled` while the wire root gathers only the
    /// actor's own startup sends (ADR-0244 §2). A ctx with an inbound never
    /// has a causing chain.
    fn held_chain(&self) -> Option<MailId> {
        self.causing_chain.or(self.in_flight_root)
    }
}

impl<A> NativeCtx<'_, A, Unchecked> {
    /// Settle a refused decode of a `K` request whose row replies `O`
    /// (ADR-0231 §3). When `error` is a typed-path refusal and `answer`
    /// yields a reply, which it does exactly when `K` carries a
    /// `ProtocolPath`, the reply goes to the sender through
    /// [`OutboundReply::reply`], joining the request's chain, and the arm
    /// reports handled: the requester hears the refusal as its typed reply,
    /// and no [`DecodeRefused`] follows, so an RPC caller gets exactly one
    /// answer. A `Pending<O>` row is answered the same way, at once.
    /// Otherwise the refusal is dropped as `__refuse_inbound_unanswered`
    /// drops it.
    #[cold]
    #[doc(hidden)]
    pub fn __refuse_inbound<K: Kind, O: ActorMail>(
        &mut self,
        error: &wire::Error,
        answer: impl FnOnce(PathRefused) -> Option<O>,
    ) -> Option<()> {
        if let Some(reply) = PathRefused::from_wire(error).and_then(answer) {
            OutboundReply::reply(self, &reply);
            return Some(());
        }
        self.__refuse_inbound_unanswered::<K>(error)
    }

    /// Prove a request's sender as `P` before its handler runs
    /// (ADR-0231 §11), or answer the request: the `#[actor]` arm of a
    /// `#[handler::request]` that takes `sender: ProtocolRef<P>` calls it and
    /// passes the proven reference as the handler's fourth argument.
    ///
    /// A refused sender is answered at once with the row's reply `O`, built
    /// from a [`PathRefused`] naming the sender's path, through
    /// [`OutboundReply::reply`], so the requester hears the refusal as its
    /// typed reply and no [`DecodeRefused`] follows. A `Pending<O>` row is
    /// answered the same way. `O: From<PathRefused>` is what makes every such
    /// request answerable, and a reply that lacks it fails to build at the
    /// handler's return type. Mail with no sender has no path to name, so it
    /// is refused as a tell's is.
    ///
    /// # Errors
    ///
    /// The [`SenderRefused`] when the sender does not cast to `P`, or the
    /// mail has none. The refusal is logged and the handler does not run.
    #[doc(hidden)]
    pub fn __sender_or_answer<P: CastTarget, K: Kind, O: ActorMail + From<PathRefused>>(
        &mut self,
    ) -> Result<ProtocolRef<P>, SenderRefused> {
        if let Some(proven) = self.prove_sender::<P>() {
            return Ok(proven);
        }

        let refused = self.refused_sender::<P, K>();
        match refused.path_refused() {
            Some(path) => OutboundReply::reply(self, &O::from(path)),
            None => self.answer_decode_refusal(K::ID, &refused),
        }
        Err(refused)
    }
}
