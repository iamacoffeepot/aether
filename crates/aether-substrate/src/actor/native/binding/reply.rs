//! The reply path native actors take (ADR-0080 §5) and the typed
//! request-context table replies are matched against (ADR-0139).

use std::sync::Arc;

use super::NativeBinding;
use super::offload::blocking::DispatchId;
use crate::mail::attachments::EncodedMail;
use crate::mail::{MailId, Source};
use aether_data::wire::{self, HeldClaim, HeldLedger};
use aether_data::{ActorMail, Kind, KindId, RequestId};
use aether_kinds::DecodeRefused;

use crate::mail::{MailboxId, SourceAddr};

impl NativeBinding {
    /// Reply path for native actors (ADR-0080 §5 / #1695). Mints the
    /// reply's lineage `MailId` from this actor's disjoint
    /// `reply_lineage` allocator and routes through
    /// [`Mailer::send_reply`](crate::mail::mailer::Mailer::send_reply), so the reply joins the
    /// caller's causal chain: it inherits the handler's `root` and
    /// `parent`, and its `Sent` is recorded against that root (keeping
    /// the §6 hold contract exact — a synchronous reply's `Sent`
    /// precedes the replying handler's `Finished`). An absent `root`
    /// (a reply from a ctx with no inbound chain) stamps no root and
    /// skips the producer hook.
    ///
    /// The per-handler [`super::ctx::NativeCtx`](crate::actor::native::ctx::NativeCtx) supplies `root` /
    /// `parent` from its in-flight context (`in_flight_root` /
    /// `outbound_parent`); the ADR-0093 deferred path supplies the
    /// `SettlementHold`'s root. Issue 665 retired the FFI-shaped
    /// `reply_mail` stub the prior `MailTransport` impl carried; this
    /// typed entry is the only reply API native actors reach for.
    pub(crate) fn send_reply_for_handler<K>(
        &self,
        sender: Source,
        payload: &K,
        root: Option<MailId>,
        parent: Option<MailId>,
    ) where
        K: ActorMail,
    {
        let correlation = self.reply_lineage.mint();
        let reply_id = MailId::new(self.self_mailbox(), correlation);
        self.mailer.send_reply(sender, payload, Some(reply_id), root, parent);
    }

    /// [`Self::send_reply_for_handler`] for an already-encoded reply of
    /// `kind`: the reply id is minted from the same `reply_lineage`
    /// allocator and the reply joins the caller's chain under `root` /
    /// `parent` the same way. `payload` carries the bytes and the entries
    /// their tag-1 fields name, which the ctx resolved against the handled
    /// mail (ADR-0238 decision 3). Its caller is
    /// [`DeferredReply::reply_envelope`](crate::actor::native::DeferredReply::reply_envelope),
    /// through the ctx's engine-only refusal and resolve.
    pub(crate) fn send_reply_envelope_for_handler(
        &self,
        sender: Source,
        kind: KindId,
        payload: EncodedMail,
        root: Option<MailId>,
        parent: Option<MailId>,
    ) {
        let correlation = self.reply_lineage.mint();
        let reply_id = MailId::new(self.self_mailbox(), correlation);
        self.mailer.send_reply_envelope(sender, kind, payload, Some(reply_id), root, parent);
    }

    /// The actor a refusal made before a handler ran is answered to
    /// (ADR-0231 §3, §11), or `None` when the refused mail's reply target
    /// `sender` does not opt in. It opts in when it is an actor that asked
    /// under a correlation and its published contract carries a
    /// [`DecodeRefused`] row.
    ///
    /// The opt-in is the target's own declared handler, which only the RPC
    /// server declares: a wire payload is untrusted and its caller cannot
    /// read actor logs. Every other sender is typed code, so a refusal it
    /// causes is a bug the refuser's log records, and it hears nothing.
    ///
    /// Its callers are the native ctx's decode and sender refusals and the
    /// wasm guest ctx's, for a guest arm that refused its sender.
    pub(crate) fn refusal_listener(&self, sender: Source) -> Option<MailboxId> {
        let SourceAddr::Component(target) = sender.addr else {
            return None;
        };
        if sender.correlation_id == Source::NO_CORRELATION {
            return None;
        }
        self.mailer
            .registry()
            .published_contract(target)
            .is_some_and(|contract| contract.handles(DecodeRefused::ID))
            .then_some(target)
    }

    /// Store request context for a just-minted outbound request, warning
    /// with this actor's canonical name when the table passes a new
    /// high-water mark (ADR-0139 §4).
    ///
    /// The context moves in (ADR-0243 §4): each `Held` it carries parks in
    /// this actor's in-flight ledger as it encodes, and the value then drops
    /// with its tickets owned by the stored bytes. A no-correlation request
    /// stores nothing, so the context drops here as an ordinary value, outside
    /// the table lock, and a live `Held` inside fails fast rather than parking
    /// with no context to carry it.
    ///
    /// Lock order: `request_contexts` → `inflight`. Parking takes the ledger
    /// lock while the table lock is held; nothing takes the table lock while
    /// holding the ledger lock.
    ///
    /// # Panics
    /// Panics if the request-context mutex is poisoned, and when the context
    /// fails to encode.
    pub fn store_request_context<C: Kind>(self: &Arc<Self>, request: RequestId, context: C) {
        if request.0 == Source::NO_CORRELATION {
            tracing::warn!(kind = C::NAME, "request context not stored: request has no correlation id");
            drop(context);
            return;
        }

        let mut ledger = NativeParkLedger { binding: self, request, context_name: C::NAME };
        let high_water = {
            let mut table =
                self.request_contexts.lock().expect("request context table poisoned; fail-fast per ADR-0063");
            table.insert_with(request, context, &mut ledger);
            table.high_water()
        };
        if let Some(live) = high_water {
            let name = self
                .identity
                .runtime_identity()
                .map_or("<untyped test binding>", |identity| identity.canonical_name().as_str());
            tracing::warn!(
                actor = name,
                live,
                "request context table grew past its preallocated room; a reply that never arrives keeps its context",
            );
        }
    }

    /// Remove and decode request context for an inbound reply. Each `Held`
    /// the context carries is claimed back from this actor's in-flight
    /// ledger as it decodes, so it comes back live. A wrong-kind take leaves
    /// the entry stored and claims nothing.
    ///
    /// Lock order: `request_contexts` → `inflight`, as for
    /// [`Self::store_request_context`].
    ///
    /// # Panics
    /// Panics if the request-context mutex is poisoned.
    pub fn take_request_context<C: Kind>(self: &Arc<Self>, request: RequestId) -> Option<C> {
        let mut ledger = NativeParkLedger { binding: self, request, context_name: C::NAME };
        self.request_contexts
            .lock()
            .expect("request context table poisoned; fail-fast per ADR-0063")
            .take_with(request, &mut ledger)
    }
}

/// This actor's in-flight ledger as the [`HeldLedger`] one request context's
/// encode or decode is granted (ADR-0243 §4). Built per store or take and
/// holding nothing between calls: a park moves the ticket's entry to parked
/// under `request`, and a claim moves it back to held and hands the decode
/// the weak ledger link a live `Held` keeps.
struct NativeParkLedger<'b> {
    binding: &'b Arc<NativeBinding>,
    request: RequestId,
    context_name: &'static str,
}

impl HeldLedger for NativeParkLedger<'_> {
    fn park(&mut self, ticket: u64, reply: KindId) -> Result<(), wire::Error> {
        self.binding.dispatch_park(DispatchId(ticket), self.request, reply, self.context_name)
    }

    fn claim(&mut self, ticket: u64, reply: KindId) -> Result<HeldClaim, wire::Error> {
        self.binding.dispatch_unpark(DispatchId(ticket), self.request, reply)?;
        Ok(HeldClaim(Box::new(Arc::downgrade(self.binding))))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use aether_actor::{ErasedActorRef, MailSender, OutboundReply, Unchecked};
    use aether_kinds::Tick;

    use super::*;
    use crate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    use crate::chassis::builder::ReplyTarget;
    use crate::chassis::error::BootError;
    use crate::chassis::inbox::ReplyLineage;
    use crate::mail::mailer::Mailer;
    use crate::mail::registry::{InboxHandler, OwnedDispatch, Registry};
    use crate::testing::{PumpedDriver, boot_bare_test_chassis, fresh_substrate, registered_ref};

    /// Asks [`Replier`] to reply `replies` times from one unchecked turn.
    #[aether_data::kind(name = "test.binding.reply.ask", copy)]
    struct Ask {
        replies: u32,
    }

    /// A pumped root whose unchecked handler replies by hand through
    /// `ctx.reply`, the verb that reaches `send_reply_for_handler`.
    struct Replier {
        /// The replies its turns have sent.
        replied: u32,
    }

    #[aether_actor::actor(singleton, root)]
    impl NativeActor for Replier {
        const NAMESPACE: &'static str = "test.binding.reply.replier";
        type Config = ();

        fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self { replied: 0 })
        }

        #[handler::unchecked(reason = "test: replies more than once")]
        fn on_ask(&mut self, ctx: &mut NativeCtx<'_, Self, Unchecked>, ask: Ask) {
            for _ in 0..ask.replies {
                ctx.reply(&Tick::default());
                self.replied += 1;
            }
        }
    }

    /// A booted [`Replier`] and a caller that forwards each reply for the
    /// test to read, then finishes it, so the chain the reply joined settles
    /// only once the reply is readable.
    struct Rig {
        driver: PumpedDriver<Replier>,
        caller: ErasedActorRef,
        replies: mpsc::Receiver<OwnedDispatch>,
    }

    impl Rig {
        fn boot() -> Self {
            let (registry, mailer) = fresh_substrate();
            let (caller, replies) = finishing_caller(&registry, &mailer);
            let driver = PumpedDriver::boot(boot_bare_test_chassis(&registry, &mailer), (), ());

            Self { driver, caller, replies }
        }

        /// Ask for `replies` replies answered to the caller under correlation
        /// 55, and wait for the chain to settle.
        fn ask(&mut self, replies: u32) -> MailId {
            let reply = Some(ReplyTarget::Actor { to: self.caller, correlation: 55 });
            self.driver.send_and_settle(self.driver.chassis().actor_ref::<Replier>(), &Ask { replies }, reply)
        }

        /// The send correlation the replier's binding last minted, read on a
        /// ctx the runtime builds.
        fn prev_correlation(&mut self) -> u64 {
            self.driver.host_turn(|_replier, ctx| MailSender::prev_correlation(ctx)).expect("the replier is live")
        }
    }

    fn finishing_caller(registry: &Registry, mailer: &Arc<Mailer>) -> (ErasedActorRef, mpsc::Receiver<OwnedDispatch>) {
        let (tx, rx) = mpsc::channel::<OwnedDispatch>();
        let mailer = Arc::clone(mailer);
        let sink: Arc<dyn InboxHandler> = Arc::new(move |dispatch: OwnedDispatch| {
            let (mail_id, root) = (dispatch.mail_id, dispatch.root);
            dispatch.discharge();
            let _ = tx.send(dispatch);
            mailer.record_finished(mail_id, root);
        });

        (registered_ref(registry, "test.binding.reply.caller", sink), rx)
    }

    /// #1695 / ADR-0080 §5/§6: a synchronous `ctx.reply` from a handler
    /// with an in-flight chain stamps the reply mail with the caller's
    /// `root` + the handled mail as `parent`, mints the reply id in the
    /// replier's id space, and records the reply's `Sent` on that root —
    /// so the chain stays live until the reply's `Finished`. The reply
    /// joins the caller's chain instead of opening a lineage-less one.
    #[test]
    fn ctx_reply_joins_caller_chain() {
        let mut rig = Rig::boot();
        let root = rig.ask(1);

        let reply = rig.replies.try_recv().expect("the reply lands on the caller before its chain settles");
        assert_eq!(reply.root, Some(root), "reply inherits the caller's root");
        assert_eq!(reply.parent_mail, Some(root), "reply's parent is the handled request");
        assert_eq!(reply.sender.correlation_id, 55, "the caller's correlation is echoed onto the reply");
        let reply_id = reply.mail_id.expect("reply carries a real mail id");
        assert_eq!(
            reply_id.sender,
            rig.driver.chassis().actor_ref::<Replier>().id(),
            "reply id is minted in the replier's id space"
        );
    }

    /// #1695: minting a reply's lineage id draws from the disjoint
    /// reply-lineage counter, so a reply never advances the `send`
    /// correlation `prev_correlation` reports (symmetric with the wasm
    /// trampoline's separate reply counter).
    #[test]
    fn reply_does_not_advance_send_correlation() {
        let mut rig = Rig::boot();
        let before = rig.prev_correlation();

        rig.ask(2);

        assert_eq!(rig.driver.read_state(|replier| replier.replied), Some(2), "the turn sent both replies");
        assert_eq!(rig.replies.try_iter().count(), 2, "both replies land on the caller");
        assert_eq!(rig.prev_correlation(), before, "replies must not advance the send correlation counter");
    }

    /// Step 3: reply ids minted via `send_reply_for_handler` still sit in
    /// the disjoint reply-lineage space ([`ReplyLineage::BASE`]), and minting
    /// a reply does not advance `prev_correlation` (the send counter).
    #[test]
    fn reply_mints_in_disjoint_space_and_does_not_advance_send_correlation() {
        let mut rig = Rig::boot();
        let before = rig.prev_correlation();

        rig.ask(1);

        let reply = rig.replies.try_recv().expect("the reply lands on the caller");
        assert!(
            reply.mail_id.is_some_and(|id| id.correlation_id >= ReplyLineage::BASE),
            "reply id sits in the disjoint reply-lineage space",
        );
        assert_eq!(rig.prev_correlation(), before, "minting a reply must not advance the send correlation counter");
    }
}
