//! ADR-0243 typed held replies: the [`Held<R>`] ticket to a reply this actor
//! owes, answered later by a handler turn no worker drives.
//!
//! [`NativeCtx::hold`] arms one entry in the actor's in-flight ledger, the
//! table offload dispatch already fills, and returns the pair. The handler
//! returns the [`Pending<R>`](super::blocking::Pending) receipt, which sets
//! its row, and keeps the [`Held<R>`] ticket in state or on a successor. The
//! ticket answers exactly one `R`, from any handler on the same actor.
//!
//! The ledger owns the obligation: the entry holds the caller's settlement
//! hold and reply target, and the ticket names the entry. The ticket keeps a
//! weak link back to the ledger because its `Drop` and
//! [`IntoDeferredReply::into_deferred_reply`] get no ctx, the precedent
//! `DeferredCompletion` set.
//!
//! A ticket also rides a request context (ADR-0243 §4). Its codec writes and
//! reads only the ticket, through [`Encoder::held`] and
//! [`Decoder::claim_held`]: the request-context table's encoder parks the
//! entry, and its decode claims the entry back and hands over the weak ledger
//! link. Every other encoder and decode refuses, so a stray codec path never
//! defuses or claims a debt.

use std::fmt;
use std::marker::PhantomData;
use std::mem::{self, ManuallyDrop};
use std::sync::Weak;
use std::thread;

use aether_actor::ReplyMode;
use aether_data::wire::{self, Decoder, Encoder, WireDecode, WireEncode};
use aether_data::{ActorMail, CastEligible, LabelNode, Schema, SchemaType};

use super::blocking::{DeferredReply, DispatchId, IntoDeferredReply};
use crate::actor::native::binding::NativeBinding;
use crate::actor::native::ctx::NativeCtx;

/// A reply of kind `R` this actor still owes its caller (ADR-0243 §1).
///
/// A move-only ticket to one held entry in the actor's in-flight ledger,
/// which keeps the caller's settlement hold and reply target.
/// [`Self::answer`] sends the one terminal `R` and releases the hold.
/// Dropping it unanswered releases the hold and then panics outside an
/// unwind, as [`DeferredReply`] does. Actor close is the one silent
/// discharge: the close tail settles the ledger before the actor's state
/// drops, so a ticket parked in that state finds its entry gone. It
/// implements [`IntoDeferredReply`], so every staging surface that takes a
/// deferred reply takes it unchanged.
///
/// A request context may carry it (ADR-0243 §4). `Held<R>` has actor reach:
/// it implements neither `CrossesActors` nor `CrossesWire`, so a kind holding
/// one declares and stores as a context, while it is never mail:
///
/// ```
/// use aether_kinds::Pong;
/// use aether_substrate::actor::native::Held;
///
/// #[aether_data::kind(name = "doc.held.waiting")]
/// struct Waiting {
///     held: Held<Pong>,
///     tag: u32,
/// }
/// ```
///
/// ```compile_fail,E0277
/// use aether_kinds::Pong;
/// use aether_substrate::actor::native::Held;
///
/// #[aether_data::kind(name = "doc.held.waiting")]
/// struct Waiting {
///     held: Held<Pong>,
///     tag: u32,
/// }
///
/// fn mail<K: aether_data::ActorMail>() {}
/// mail::<Waiting>();
/// ```
#[must_use = "answer the held reply or stage it on a successor; dropping it fails fast"]
pub struct Held<R: ActorMail> {
    id: DispatchId,
    ledger: Weak<NativeBinding>,
    /// `fn() -> R` so `Held<R>` is `Send` regardless of `R`: it owns no
    /// `R`, it only names the reply kind.
    _reply: PhantomData<fn() -> R>,
}

impl<R: ActorMail> Held<R> {
    pub(crate) fn new(id: DispatchId, ledger: Weak<NativeBinding>) -> Self {
        Self { id, ledger, _reply: PhantomData }
    }

    /// The [`DispatchId`] of the ledger entry this ticket names.
    #[must_use]
    pub fn dispatch_id(&self) -> DispatchId {
        self.id
    }

    /// Send `reply` to the waiting caller as the terminal answer, then
    /// release the settlement hold. Consuming, so a held reply answers once.
    /// `ctx` may be any handler's ctx on the actor that armed the ticket,
    /// and the reply still goes to the captured caller with its
    /// correlation.
    ///
    /// # Panics
    /// Panics when `ctx` belongs to another actor (ADR-0243 §5).
    pub fn answer<M: ReplyMode, A>(self, ctx: &mut NativeCtx<'_, A, M>, reply: &R) {
        let (id, ledger) = self.disarm();
        ctx.answer_held(id, &ledger, reply);
    }

    /// The ledger this ticket's entry lives in, for a consuming path that
    /// checks the ticket belongs to its ctx's actor before claiming it.
    pub(crate) fn ledger(&self) -> &Weak<NativeBinding> {
        &self.ledger
    }

    /// Give up the ticket without claiming its entry and return the entry's
    /// id: the entry stays owed, and the caller attaches what answers it.
    /// The ticket's unanswered-drop check never runs.
    pub(crate) fn into_ticket(self) -> DispatchId {
        self.disarm().0
    }

    /// Take the ticket apart so its `Drop` never runs after a consuming path
    /// claims the entry.
    fn disarm(self) -> (DispatchId, Weak<NativeBinding>) {
        let mut this = ManuallyDrop::new(self);
        (this.id, mem::take(&mut this.ledger))
    }
}

impl<R: ActorMail> IntoDeferredReply for Held<R> {
    /// Claim the held entry and move its hold and reply target into a bare
    /// [`DeferredReply`]. No `Release` is emitted.
    ///
    /// # Panics
    /// Panics when the entry is no longer held, which only actor close
    /// causes, and a closed actor stages nothing.
    fn into_deferred_reply(self) -> DeferredReply {
        let (id, ledger) = self.disarm();
        let (hold, reply_to) = ledger
            .upgrade()
            .and_then(|binding| binding.dispatch_claim_held(id))
            .expect("a Held staged after actor close settled its ledger entry");
        DeferredReply::new(hold, reply_to)
    }
}

impl<R: ActorMail> Drop for Held<R> {
    /// Fails fast when the entry is still held. An entry that is gone (actor
    /// close settled it) or parked (a stored context's encode took the ticket,
    /// ADR-0243 §4) claims nothing, so this drop stays silent: the parked
    /// entry is the context's to claim back.
    fn drop(&mut self) {
        let Some(binding) = self.ledger.upgrade() else {
            return;
        };
        let Some((hold, _reply_to)) = binding.dispatch_claim_held(self.id) else {
            return;
        };
        drop(hold);
        // Fails fast outside an unwind. A panic already unwinding past this
        // debt is the one the aborter should see; a second panic here would
        // abort the process instead.
        assert!(
            thread::panicking(),
            "Held dropped without an answer or successor staging (the hold was released, but the owed reply was lost)"
        );
    }
}

/// The ticket's schema names the reply kind it answers, so a context holding
/// `Held<A>` has another kind id from one holding `Held<B>` (ADR-0243 §4).
impl<R: ActorMail> Schema for Held<R> {
    const SCHEMA: SchemaType = SchemaType::Ticket { reply: R::ID };
    const LABEL: Option<&'static str> = None;
    const LABEL_NODE: LabelNode = LabelNode::Anonymous;
}

impl<R: ActorMail> CastEligible for Held<R> {
    const ELIGIBLE: bool = false;
}

impl<R: ActorMail> WireEncode for Held<R> {
    /// Refuses: a plain buffer grants no ledger to park the ticket in.
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), wire::Error> {
        self.encode_to(out)
    }

    /// Hand the ticket to [`Encoder::held`], which only the request-context
    /// table's encoder grants: it parks the entry and writes the ticket.
    fn encode_to<E: Encoder + ?Sized>(&self, enc: &mut E) -> Result<(), wire::Error> {
        enc.held(self.id.0, R::ID)
    }
}

impl<'de, R: ActorMail> WireDecode<'de> for Held<R> {
    /// Refuses: a bare cursor grants no ledger to claim the ticket from.
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, wire::Error> {
        Self::decode_from(cursor)
    }

    /// Read the ticket and claim its entry back through
    /// [`Decoder::claim_held`]; the claim carries the weak ledger link the
    /// live ticket keeps.
    fn decode_from<D: Decoder<'de> + ?Sized>(dec: &mut D) -> Result<Self, wire::Error> {
        let ticket = u64::decode(dec.cursor())?;
        let ledger = dec
            .claim_held(ticket, R::ID)?
            .downcast::<Weak<NativeBinding>>()
            .map_err(|_| wire::Error::HeldUnclaimed { ticket, reply: R::ID })?;
        Ok(Self::new(DispatchId(ticket), ledger))
    }
}

impl<R: ActorMail> fmt::Debug for Held<R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Held").field("id", &self.id).field("reply", &R::NAME).finish_non_exhaustive()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test-setup unwraps: fixture construction panic on failure is the assertion")]
mod tests {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::{Arc, mpsc};
    use std::time::Duration;

    use aether_data::{CrossesActors, Kind, KindId, MailId, MailboxId, Source, SourceAddr};

    use super::*;
    use crate::mail::registry::{InboxHandler, OwnedDispatch};
    use crate::testing::{bare_substrate, boot_authority};

    #[repr(C)]
    #[derive(
        Copy, Clone, Debug, PartialEq, Eq, bytemuck::Pod, bytemuck::Zeroable, serde::Serialize, serde::Deserialize,
    )]
    struct Answer {
        value: u64,
    }

    impl Kind for Answer {
        const NAME: &'static str = "test.held.answer";
        const ID: KindId = KindId(0xD15B_0CC1_0000_0002);
        aether_data::pod_kind_codec!();
    }

    impl ActorMail for Answer {}
    impl CrossesActors for Answer {}

    fn forward_to(tx: mpsc::Sender<OwnedDispatch>) -> Arc<dyn InboxHandler> {
        Arc::new(move |dispatch: OwnedDispatch| {
            dispatch.discharge();
            let _ = tx.send(dispatch);
        })
    }

    fn root_id(cid: u64) -> MailId {
        MailId { sender: MailboxId(0xAB), correlation_id: cid }
    }

    /// Catches `answer` replying to the answering ctx's `reply_target()`
    /// instead of the target the hold captured, or releasing before `Sent`.
    #[test]
    fn answer_from_a_later_ctx_echoes_the_captured_correlation_and_releases() {
        let (registry, mailer) = bare_substrate();
        let counter = Arc::clone(mailer.trace_handle().settlement_counter());
        let (reply_tx, reply_rx) = mpsc::channel::<OwnedDispatch>();
        let caller = registry.register_inbox(&boot_authority(), "test.held.answer.caller", forward_to(reply_tx));
        let binding = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), MailboxId(0)));
        let root = root_id(1);

        let held = {
            let mut ctx =
                NativeCtx::new(&binding, Source::with_correlation(SourceAddr::Component(caller), 77), None, Some(root));
            let (_pending, held) = ctx.hold::<Answer>();
            held
        };
        assert_eq!(counter.held_open(root), 1, "the held entry keeps the caller's chain open");

        let mut later = NativeCtx::new(&binding, Source::with_correlation(SourceAddr::None, 99), None, None);
        held.answer(&mut later, &Answer { value: 5 });
        assert_eq!(counter.held_open(root), 0, "answer releases the hold");

        let reply = reply_rx.recv_timeout(Duration::from_secs(2)).expect("the answer reaches the captured caller");
        assert_eq!(reply.sender.correlation_id, 77, "the captured correlation is echoed, not the answering ctx's");
        assert_eq!(Answer::decode_from_bytes(reply.payload.bytes()).unwrap(), Answer { value: 5 });
    }

    /// Catches a silent leak: an unanswered ticket must release its hold,
    /// remove its entry, and fail fast.
    #[test]
    fn unanswered_drop_panics_and_releases_the_hold() {
        let (_registry, mailer) = bare_substrate();
        let counter = Arc::clone(mailer.trace_handle().settlement_counter());
        let binding = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), MailboxId(0)));
        let root = root_id(2);

        let held = NativeCtx::new(&binding, Source::NONE, None, Some(root)).hold::<Answer>().1;
        let id = held.dispatch_id();

        let payload = catch_unwind(AssertUnwindSafe(|| drop(held))).expect_err("an unanswered Held fails fast");
        let message =
            payload.downcast_ref::<&str>().copied().or_else(|| payload.downcast_ref::<String>().map(String::as_str));
        assert!(
            message.is_some_and(|message| message.starts_with("Held dropped without an answer")),
            "the panic names the lost reply"
        );
        assert_eq!(counter.held_open(root), 0, "the dropped ticket released its hold");
        assert_eq!(binding.dispatch_state_of(id), None, "the dropped ticket removed its entry");
    }

    /// Catches a double release: staging moves the one hold into the
    /// successor debt and removes the entry, so neither the ticket's drop
    /// nor actor close releases it again.
    #[test]
    fn into_deferred_reply_moves_the_hold_and_removes_the_entry() {
        let (_registry, mailer) = bare_substrate();
        let counter = Arc::clone(mailer.trace_handle().settlement_counter());
        let binding = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), MailboxId(0)));
        let root = root_id(3);

        let held = NativeCtx::new(&binding, Source::NONE, None, Some(root)).hold::<Answer>().1;
        let id = held.dispatch_id();

        let owed = held.into_deferred_reply();
        assert_eq!(counter.held_open(root), 1, "staging keeps the chain held");
        assert_eq!(binding.dispatch_state_of(id), None, "staging removed the entry");

        binding.settle_held_for_actor_close();
        assert_eq!(counter.held_open(root), 1, "actor close finds no entry to release twice");
        owed.abandon_for_actor_close();
        assert_eq!(counter.held_open(root), 0, "the successor debt owns the one hold");
    }

    #[test]
    #[should_panic(expected = "a second NativeCtx::hold in one dispatch")]
    fn second_hold_in_one_dispatch_panics() {
        let (_registry, mailer) = bare_substrate();
        let binding = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), MailboxId(0)));
        let mut ctx = NativeCtx::new(&binding, Source::NONE, None, Some(root_id(4)));

        let (_pending, _first) = ctx.hold::<Answer>();
        let _second = ctx.hold::<Answer>();
    }
}
