//! The inbound frame this ctx is dispatching.
//!
//! One envelope in one place (#1757 / ADR-0094): the dispatcher moves it in
//! and takes it back at its settlement tail unless a handler first retained
//! it, so a double-settle is structurally unrepresentable. Around it sit the
//! frame's read-only views — the in-flight lineage, the reply target, the
//! correlation the inbound answers — and the chain a hold taken from this
//! context gates.

use std::sync::Arc;

use aether_actor::{ErasedActorRef, ReplyMode};
use aether_data::{Kind, MailId, RequestId};

use crate::actor::native::envelope::Envelope;
use crate::chassis::inbox::InboundMail;
use crate::mail::registry::Registry;
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
    /// `root` on child mail so descendants share the chain. The
    /// chassis-root case (no inbound) leaves this `None` and
    /// `NativeBinding::push_envelope_buffered` mints a fresh root.
    #[must_use]
    pub fn in_flight_root(&self) -> Option<MailId> {
        self.in_flight_root
    }

    /// The reply target for the mail currently being dispatched.
    /// Useful when a handler wants to inspect the originator (audit
    /// trails, multi-tenant routing) without going through
    /// [`OutboundReply::reply`](aether_actor::OutboundReply::reply). `target == SourceAddr::None` means the
    /// inbound was broadcast or peer-component mail with no reply
    /// destination.
    #[must_use]
    pub fn reply_target(&self) -> Source {
        self.source
    }

    /// The envelope sender as a proven [`ErasedActorRef`]: mints the dispatch
    /// source the host stamped, with no registry read. This is the
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
    #[must_use]
    pub fn sender(&self) -> Option<ErasedActorRef> {
        match self.source.addr {
            SourceAddr::Component(id) => Some(Registry::structural_erased(id)),
            SourceAddr::None if self.in_reply_to().is_some() => {
                self.in_flight_mail_id.map(|id| Registry::structural_erased(id.sender))
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
    pub fn take_context<C: Kind>(&mut self) -> Option<C> {
        let request = self.in_reply_to()?;
        self.binding.take_request_context(request)
    }
    /// Acquire a [`SettlementHold`] on the current in-flight root
    /// (ADR-0080 §12). Use to keep a chain open across deferred work —
    /// e.g. a `TaskQueue` buffering an over-limit request holds it until
    /// a slot frees, then moves it into
    /// [`Self::dispatch_blocking_resumed`].
    ///
    /// A `wire` ctx dispatches no inbound, so it has no in-flight root; it
    /// holds the chain that caused the birth instead (ADR-0168 §1), which is
    /// what puts a birth-completing effect inside the staging caller's
    /// `Settled`.
    ///
    /// `None` when neither is present: the work has no causing chain, so
    /// there is nothing to keep open and no guard to hand back (ADR-0168 §2).
    /// Deferred work started from such a context is unobservable through
    /// settlement, which is a property worth reading at the call site
    /// rather than a guard that gates nothing.
    #[must_use]
    pub fn acquire_settlement_hold(&self) -> Option<SettlementHold> {
        self.held_chain().map(|root| self.mailer().acquire_settlement_hold(root))
    }

    /// The chain a hold taken from this context gates: the in-flight root
    /// while a handler is dispatching, and otherwise whatever caused this
    /// context to exist. Exactly one of the two is ever set — a ctx with an
    /// inbound is never a `wire` ctx — so the precedence is a formality that
    /// keeps the rule readable rather than a real disambiguation.
    fn held_chain(&self) -> Option<MailId> {
        self.in_flight_root.or(self.causing_chain)
    }
}
