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

use aether_actor::{HandsOff, HeldReply, ReplyMode};
use aether_data::wire::{self, Decoder, Encoder, WireDecode, WireEncode};
use aether_data::{ActorMail, CastEligible, LabelNode, MailId, Schema, SchemaType, Source};

use super::blocking::{DeferredReply, DispatchId, IntoDeferredReply};
use crate::actor::native::binding::NativeBinding;
use crate::actor::native::ctx::NativeCtx;

/// The answer a closing actor sends for one held ledger entry: the entry's
/// reply kind's [`HeldReply::unanswered`], sent to the entry's reply target
/// under the root its hold keeps open (ADR-0243 §1). `hold` stores the
/// monomorphized [`answer_unanswered`] in the entry, so nothing is encoded
/// until the actor closes.
pub(crate) type AnswerUnanswered = fn(&NativeBinding, Source, Option<MailId>);

/// Send `R::unanswered()` to `reply_to` through the binding reply path
/// [`Held::answer`] takes, so the caller's correlation is echoed and the
/// reply's `Sent` counts against `root`.
pub(crate) fn answer_unanswered<R: HeldReply>(binding: &NativeBinding, reply_to: Source, root: Option<MailId>) {
    binding.send_reply_for_handler(reply_to, &R::unanswered(), root, None);
}

/// A reply of kind `R` this actor still owes its caller (ADR-0243 §1).
///
/// A move-only ticket to one held entry in the actor's in-flight ledger,
/// which keeps the caller's settlement hold and reply target.
/// [`Self::answer`] sends the one terminal `R` and releases the hold, and
/// [`Self::hand_off`] moves the answer to another actor.
/// Dropping it unanswered releases the hold and then panics outside an
/// unwind, as [`DeferredReply`] does. When the actor closes first while the
/// engine keeps running, the close tail answers the entry with
/// [`HeldReply::unanswered`] and then releases its hold, so the caller still
/// receives an `R`; an engine teardown releases it silently, because every
/// requester is closing too. Either happens before the actor's state drops,
/// so a ticket parked in that state finds its entry gone and drops
/// silently. [`NativeCtx::hold`] requires `R: HeldReply` for that answer. It
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

    /// Hand the owed reply to `target`, which then answers in its own name
    /// (ADR-0243 §9): send `payload` to it with the captured caller as its
    /// reply target and the held root as its lineage, then release the hold.
    /// The send takes its settlement count before the release, so the
    /// caller's chain stays open until `target` answers.
    ///
    /// This is the one way a debt leaves its actor. The caller hears from
    /// `target`, stamped as the reply's sender, and keeps that sender as its
    /// reference (ADR-0230 §3); no verb lets one actor reply *as* another.
    /// `target` is a proven reference whose row for `K` replies exactly `R`
    /// ([`HandsOff`]): an [`ActorRef<T>`](aether_actor::ActorRef) of an actor
    /// whose handler for `K` returns `R`, or a
    /// [`ProtocolRef<P>`](aether_actor::ProtocolRef) whose row for `K` is
    /// `Row<K, R>`, such as the control reference a guest birth completes
    /// with. An [`ErasedActorRef`](aether_actor::ErasedActorRef) proves no
    /// row, so handing to one does not compile (#6895):
    ///
    /// ```compile_fail,E0277
    /// use aether_actor::{ErasedActorRef, Single};
    /// use aether_kinds::{Ping, Pong};
    /// use aether_substrate::actor::native::{Held, NativeCtx};
    ///
    /// fn hand<A>(ctx: &mut NativeCtx<'_, A, Single>, held: Held<Pong>, target: ErasedActorRef) {
    ///     held.hand_off(ctx, target, &Ping::default());
    /// }
    /// ```
    ///
    /// # Panics
    /// Panics when `ctx` belongs to another actor (ADR-0243 §5).
    pub fn hand_off<K: ActorMail, I, A, M: ReplyMode>(
        self,
        ctx: &mut NativeCtx<'_, A, M>,
        target: impl HandsOff<K, R, I>,
        payload: &K,
    ) {
        let (id, ledger) = self.disarm();
        ctx.hand_off_held(id, &ledger, target.erased(), payload);
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
            .expect("a Held staged after actor close answered its ledger entry");
        DeferredReply::new(hold, reply_to)
    }
}

impl<R: ActorMail> Drop for Held<R> {
    /// Fails fast when the entry is still held. An entry that is gone (actor
    /// close answered it) or parked (a stored context's encode took the ticket,
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
mod tests {
    use std::any::Any;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::{Arc, mpsc};

    use aether_actor::{ActorRef, ErasedActorRef};
    use aether_data::{Kind, MailId};

    use super::*;
    use crate::NativeInitCtx;
    use crate::actor::native::{NativeActor, Pending};
    use crate::chassis::builder::ReplyTarget;
    use crate::chassis::error::BootError;
    use crate::mail::mailer::Mailer;
    use crate::mail::registry::{InboxHandler, OwnedDispatch, Registry};
    use crate::testing::{PumpedDriver, boot_bare_test_chassis, fresh_substrate, registered_ref};

    #[aether_data::kind(name = "test.held.answer", copy, partial_eq)]
    struct Answer {
        value: u64,
    }

    // A sentinel: these tests never close the actor holding an `Answer`
    // while the engine keeps running.
    impl HeldReply for Answer {
        fn unanswered() -> Self {
            Self { value: u64::MAX }
        }
    }

    #[aether_data::kind(name = "test.held.hold")]
    struct HoldReq;

    #[aether_data::kind(name = "test.held.release", copy)]
    struct Release {
        value: u64,
    }

    #[aether_data::kind(name = "test.held.hold_twice")]
    struct HoldTwice;

    /// A pumped root that holds its reply in one turn and answers it from a
    /// later one.
    #[derive(Default)]
    struct HeldProbe {
        /// Set by `on_hold` and `on_hold_twice`, taken by `on_release`.
        held: Option<Held<Answer>>,
    }

    #[aether_actor::actor(singleton, root)]
    impl NativeActor for HeldProbe {
        const NAMESPACE: &'static str = "test.held.probe";
        type Config = ();

        fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self::default())
        }

        #[handler::request]
        fn on_hold(&mut self, ctx: &mut NativeCtx<'_>, _hold: HoldReq) -> Pending<Answer> {
            let (pending, held) = ctx.hold::<Answer>();
            self.held = Some(held);
            pending
        }

        #[handler::tell]
        fn on_release(&mut self, ctx: &mut NativeCtx<'_>, release: Release) {
            self.held.take().expect("a hold is waiting").answer(ctx, &Answer { value: release.value });
        }

        #[handler::request]
        fn on_hold_twice(&mut self, ctx: &mut NativeCtx<'_>, _hold: HoldTwice) -> Pending<Answer> {
            let (pending, first) = ctx.hold::<Answer>();
            self.held = Some(first);
            let _second = ctx.hold::<Answer>();
            pending
        }
    }

    /// A booted [`HeldProbe`] and the mailer its chains settle through.
    struct Rig {
        driver: PumpedDriver<HeldProbe>,
        registry: Arc<Registry>,
        mailer: Arc<Mailer>,
    }

    impl Rig {
        fn boot() -> Self {
            let (registry, mailer) = fresh_substrate();
            let driver = PumpedDriver::boot(boot_bare_test_chassis(&registry, &mailer), (), ());

            Self { driver, registry, mailer }
        }

        fn probe(&self) -> ActorRef<HeldProbe> {
            self.driver.chassis().actor_ref::<HeldProbe>()
        }

        /// A caller registered under `name` that forwards each reply it
        /// receives for the test to read, then finishes it, so the chain the
        /// reply joined settles only once the reply is readable.
        fn caller(&self, name: &str) -> (ErasedActorRef, mpsc::Receiver<OwnedDispatch>) {
            let (tx, rx) = mpsc::channel::<OwnedDispatch>();
            let mailer = Arc::clone(&self.mailer);
            let sink: Arc<dyn InboxHandler> = Arc::new(move |dispatch: OwnedDispatch| {
                let (mail_id, root) = (dispatch.mail_id, dispatch.root);
                dispatch.discharge();
                let _ = tx.send(dispatch);
                mailer.record_finished(mail_id, root);
            });

            (registered_ref(&self.registry, name, sink), rx)
        }

        /// Send [`HoldReq`] as a root answered to `caller` under correlation
        /// 77, and pump until the probe holds it.
        fn hold(&mut self, caller: ErasedActorRef) -> MailId {
            let root = self.driver.send_tracked(
                self.probe(),
                &HoldReq,
                Some(ReplyTarget::Actor { to: caller, correlation: 77 }),
            );
            self.driver.pump_until("the probe holds the request", |probe| probe.held.is_some());
            root
        }

        /// Move the held ticket out of the probe's state.
        fn take_held(&mut self) -> Held<Answer> {
            self.driver.host_turn(|probe, _ctx| probe.held.take()).flatten().expect("the probe holds a ticket")
        }

        fn held_open(&self, root: MailId) -> u32 {
            self.mailer.trace_handle().settlement_counter().held_open(root)
        }
    }

    /// The panic message `payload` carries.
    fn panic_message(payload: &(dyn Any + Send)) -> Option<&str> {
        payload.downcast_ref::<&str>().copied().or_else(|| payload.downcast_ref::<String>().map(String::as_str))
    }

    /// Catches `answer` replying to the answering turn's reply target
    /// instead of the target the hold captured, or releasing before `Sent`.
    #[test]
    fn answer_from_a_later_turn_echoes_the_captured_correlation_and_releases() {
        let mut rig = Rig::boot();
        let (caller, replies) = rig.caller("test.held.answer.caller");
        let (other, other_replies) = rig.caller("test.held.answer.other");

        let root = rig.hold(caller);
        assert_eq!(rig.held_open(root), 1, "the held entry keeps the caller's chain open");

        let release = rig.driver.send_tracked(
            rig.probe(),
            &Release { value: 5 },
            Some(ReplyTarget::Actor { to: other, correlation: 99 }),
        );
        rig.driver.settle(&[root, release]);
        assert_eq!(rig.held_open(root), 0, "answer releases the hold");

        let reply = replies.try_recv().expect("the answer reaches the captured caller");
        assert_eq!(reply.sender.correlation_id, 77, "the captured correlation is echoed, not the answering turn's");
        assert_eq!(Answer::decode_from_bytes(reply.payload.bytes()), Some(Answer { value: 5 }));
        assert!(other_replies.try_recv().is_err(), "the answering turn's reply target hears nothing");
    }

    /// Catches a silent leak: an unanswered ticket must release its hold,
    /// remove its entry, and fail fast.
    #[test]
    fn unanswered_drop_panics_and_releases_the_hold() {
        let mut rig = Rig::boot();
        let (caller, replies) = rig.caller("test.held.unanswered.caller");

        let root = rig.hold(caller);
        let held = rig.take_held();
        let id = held.dispatch_id();
        let ledger = held.ledger().upgrade().expect("the probe's ledger is live");

        let payload = catch_unwind(AssertUnwindSafe(|| drop(held))).expect_err("an unanswered Held fails fast");
        assert!(
            panic_message(payload.as_ref())
                .is_some_and(|message| message.starts_with("Held dropped without an answer")),
            "the panic names the lost reply"
        );
        assert_eq!(rig.held_open(root), 0, "the dropped ticket released its hold");
        assert_eq!(ledger.dispatch_state_of(id), None, "the dropped ticket removed its entry");

        rig.driver.settle(&[root]);
        assert!(replies.try_recv().is_err(), "the lost reply is never sent");
    }

    /// Catches a double release: staging moves the one hold into the
    /// successor debt and removes the entry, so neither the ticket's drop
    /// nor actor close releases it again.
    #[test]
    fn into_deferred_reply_moves_the_hold_and_removes_the_entry() {
        let mut rig = Rig::boot();
        let (caller, _replies) = rig.caller("test.held.deferred.caller");

        let root = rig.hold(caller);
        let held = rig.take_held();
        let id = held.dispatch_id();
        let ledger = held.ledger().upgrade().expect("the probe's ledger is live");

        let owed = held.into_deferred_reply();
        assert_eq!(rig.held_open(root), 1, "staging keeps the chain held");
        assert_eq!(ledger.dispatch_state_of(id), None, "staging removed the entry");

        ledger.answer_held_for_actor_close();
        assert_eq!(rig.held_open(root), 1, "actor close finds no entry to release twice");
        owed.abandon_for_actor_close();
        assert_eq!(rig.held_open(root), 0, "the successor debt owns the one hold");
        rig.driver.settle(&[root]);
    }

    /// Catches a handler that holds two replies for one request: the second
    /// hold fails fast, naming the rule.
    #[test]
    fn second_hold_in_one_dispatch_panics() {
        let mut rig = Rig::boot();
        let root = rig.driver.send_tracked(rig.probe(), &HoldTwice, None);

        let payload =
            catch_unwind(AssertUnwindSafe(|| rig.driver.settle(&[root]))).expect_err("the second hold fails fast");
        assert!(
            panic_message(payload.as_ref())
                .is_some_and(|message| message.starts_with("a second NativeCtx::hold in one dispatch")),
            "the panic names the second hold"
        );
    }
}
