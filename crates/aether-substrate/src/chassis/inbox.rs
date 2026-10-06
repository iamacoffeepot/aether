//! ADR-0106: the framework drain that backs every inbox channel in the
//! substrate (the universal `SettlingInbox`).
//!
//! An inbox channel is opened only by `inbox_channel`, which returns the
//! two sealed halves: an `InboxReceiver`, which a [`SettlingInbox`] is
//! built from and which owns the channel's only strong sender, and an
//! `InboxFeed`, the weak handle a registry inbox handler sends through.
//! Nothing outside this module can name the sender, so nothing can keep one
//! alive past the inbox.
//!
//! A mailbox claimed via [`ChassisCtx::claim_mailbox`](crate::chassis::ctx::ChassisCtx::claim_mailbox)
//! carries a [`SettlingInbox`] — the channel's receiver and strong sender
//! plus the `Arc<Mailer>`, the claim's [`MailboxId`], and a drain-owned
//! reply-id counter.
//! The only way to reach an inbound envelope on the **sink face** is one
//! of [`SettlingInbox::try_next`], [`SettlingInbox::recv_timeout`], or
//! [`SettlingInbox::drain`], each of which wraps the envelope in an
//! [`InboundMail`] guard. The guard's `Drop` records `Finished` and
//! disarms the ADR-0094 obligation in one motion, so every consumer arm
//! — match, decode error, unrecognised kind, early return, panic-unwind,
//! teardown of the drain itself — settles, because settlement is what
//! falling out of scope *does*. This mirrors `DispatcherSlot::dispatch_one`'s
//! unconditional discharge tail (the standard actor path), so a leak at
//! this seam is unrepresentable in consumer code rather than detected
//! after the fact (the recurring #846 / #1325 / #1704 class).
//!
//! The **dispatcher face** (`try_recv`) yields the raw
//! [`Envelope`] so the native actor dispatcher
//! ([`NativeBinding`](crate::actor::native::NativeBinding)) keeps its
//! existing explicit `record_finished` + `discharge` tail unchanged.
//!
//! Teardown settles every envelope the channel ever accepted (#1716,
//! #7460). An inbox stops accepting mail only by being dropped, and its
//! field order does it in two steps: the strong sender is released first,
//! so no later `InboxFeed::send` is accepted (it hands the envelope back
//! and the relay settles it), and then the queue drains with a blocking
//! receive, which returns only once the queue is empty and no send is
//! still in progress, because a send in progress holds the sender it
//! upgraded. A send that began before the drop is therefore settled by the
//! drain, and one that began after it is settled at the relay. The wait is
//! bounded by one in-progress `send` call: a feed holds an upgraded sender
//! for nothing else, and no other strong sender exists.
//!
//! There is no closed-but-alive inbox. Mail that reaches an actor between
//! its close tail and the freeing of its slot is accepted, never
//! dispatched (a finalized slot's cycle returns without draining), and
//! settled by this drain. The drain does not log what it settles: a pool
//! worker's thread-local deque can hold the last reference to a slot, so
//! the drain can run inside a thread-local destructor at thread exit,
//! where a log event reaches thread-local state that is already gone and
//! aborts the process.
//!
//! Settling on scope exit rather than on payload access is load-bearing:
//! ADR-0080 §6 requires a reply's `Sent` to be recorded before the
//! inbound's `Finished`, so a consume-time discharge would close the
//! caller's chain before the reply joins it. [`InboundMail::reply`]
//! routes through [`Mailer::send_reply`] with the inbound's
//! chain and a drain-owned reply-id counter, so a claimed-mailbox
//! consumer never reaches the bare, lineage-less `send_reply`.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Weak};
use std::time::Duration;

use aether_data::{Kind, KindId, MailId, MailboxId, Source, SourceAddr};

use crate::actor::native::envelope::Envelope;
use crate::mail::mailer::Mailer;

/// Monotonic counter for the reply-lineage id space (ADR-0080 §5 / #1701).
///
/// Sits in the top half of the `u64` space, disjoint from the `send`
/// correlation counters that start at `0`, so a reply id minted here
/// never collides with a `send` id. Both the native actor binding
/// ([`NativeBinding`](crate::actor::native::NativeBinding)) and the
/// [`SettlingInbox`] sink face mint from this type, so the one
/// `BASE` value (`1 << 63`) is the only copy in the substrate.
///
/// Cloning produces a second handle to the **same** counter (Arc clone),
/// so a [`SettlingInbox`] shared with its host
/// [`NativeBinding`](crate::actor::native::NativeBinding) mints reply ids
/// in one coherent disjoint space.
#[derive(Clone)]
pub(crate) struct ReplyLineage(Arc<AtomicU64>);

impl ReplyLineage {
    /// The starting value: the top half of the `u64` space, above the
    /// `send` correlation counter (which starts at `0`). Mirrors the
    /// wasm trampoline's `ComponentCtx::reply_lineage_counter` base so
    /// the native and guest reply paths derive reply ids the same way. A
    /// run would need `2^63` sends to reach this base, so the two spaces
    /// stay disjoint in practice.
    pub(crate) const BASE: u64 = 1 << 63;

    /// Construct a fresh counter starting at [`Self::BASE`].
    pub(crate) fn new() -> Self {
        Self(Arc::new(AtomicU64::new(Self::BASE)))
    }

    /// Mint the next reply id, advancing the counter by one.
    pub(crate) fn mint(&self) -> u64 {
        self.0.fetch_add(1, Ordering::AcqRel)
    }
}

/// Open an inbox channel and return its two sealed halves: the
/// [`InboxReceiver`] a [`SettlingInbox`] is built from, and the
/// [`InboxFeed`] a registry inbox handler sends through.
///
/// This is the only place an inbox channel is opened, so the strong sender
/// inside the returned [`InboxReceiver`] is the only one that outlives a
/// single [`InboxFeed::send`] call.
pub(crate) fn inbox_channel() -> (InboxReceiver, InboxFeed) {
    let (sender, receiver) = mpsc::channel::<Envelope>();
    let sender = Arc::new(sender);
    let feed = InboxFeed { sender: Arc::downgrade(&sender) };
    (InboxReceiver { sender, receiver }, feed)
}

/// The receiving half of an inbox channel before it is bound to a mailbox:
/// the receiver and the channel's only strong sender.
///
/// It exists because a claim learns its [`MailboxId`] from the registration
/// that needs the handler, which needs the [`InboxFeed`]; each claim binds
/// it into a [`SettlingInbox`] before it returns. The fields are private
/// and have no accessor, so the sender cannot leave this module. An
/// `InboxReceiver` dropped unbound has no mailer to settle queued mail
/// through, so every path that publishes the feed's handler goes on to
/// bind it; one is dropped unbound only when its handler was never
/// published.
pub(crate) struct InboxReceiver {
    sender: Arc<mpsc::Sender<Envelope>>,
    receiver: mpsc::Receiver<Envelope>,
}

/// The sending half of an inbox channel: a weak handle to the strong
/// sender its [`SettlingInbox`] owns. Cloning yields a second handle to the
/// same inbox.
#[derive(Clone)]
pub(crate) struct InboxFeed {
    sender: Weak<mpsc::Sender<Envelope>>,
}

/// What [`InboxFeed::send`] did with an envelope. Each refusal hands the
/// envelope back so the caller settles it.
///
/// An enum of its own rather than a `Result`: a refusal carries the whole
/// envelope, and the delivered path moves nothing.
#[must_use = "a refused envelope is still armed: the caller settles it"]
pub(crate) enum FeedOutcome {
    /// The envelope is on the inbox's queue.
    Queued,
    /// Refused: the inbox is gone (its drop released the strong sender).
    Closed(Envelope),
    /// Refused: the sender upgraded but the receiver was gone.
    ReceiverGone(Envelope),
}

impl FeedOutcome {
    /// Whether the envelope was queued.
    #[cfg(test)]
    pub(crate) fn is_queued(&self) -> bool {
        matches!(self, Self::Queued)
    }
}

impl InboxFeed {
    /// Queue `env` on the inbox, or hand it back.
    ///
    /// The upgraded sender is released before this returns, so a caller
    /// that wakes the inbox's owner afterwards holds no sender while it
    /// does: an owner whose last reference the wake path releases can drop
    /// its inbox on this thread without waiting on this thread's own send.
    pub(crate) fn send(&self, env: Envelope) -> FeedOutcome {
        let Some(sender) = self.sender.upgrade() else {
            return FeedOutcome::Closed(env);
        };
        match sender.send(env) {
            Ok(()) => FeedOutcome::Queued,
            Err(mpsc::SendError(env)) => FeedOutcome::ReceiverGone(env),
        }
    }
}

/// The sealed inbound surface that backs every inbox channel in the
/// substrate (ADR-0106 + #1716).
///
/// Owns the inbox receiver, the channel's only strong sender, an
/// `Arc<Mailer>` (for settlement discharge and lineage-joined replies),
/// the claim's [`MailboxId`], and a reply-id counter.
///
/// **Sink face** — for out-of-crate caps and the desktop window driver:
/// [`Self::try_next`], [`Self::recv_timeout`], [`Self::drain`]. Each
/// yields an [`InboundMail`] guard that settles on `Drop`.
///
/// **Dispatcher face** (`pub(crate)`) — for the native actor dispatcher
/// ([`NativeBinding`](crate::actor::native::NativeBinding)):
/// `try_recv`, which yields a raw [`Envelope`] so the dispatcher keeps
/// its explicit `record_finished` + `discharge` tail.
///
/// Dropping a `SettlingInbox` releases its sender and then drains whatever
/// was queued, waiting for any send still in progress, and lets each guard
/// settle. A teardown that abandons mail (the #1704 shape: a queued reply
/// envelope dropped on driver teardown, or an armed envelope in the
/// dispatcher's inbox at binding teardown — #1716), or that races a send
/// (#7460), becomes a settled drain instead of an armed drop.
pub struct SettlingInbox {
    /// The channel's only strong sender. Field order is load-bearing: this
    /// field is declared, and so dropped, before `queue`, whose `Drop` is
    /// the blocking drain. Releasing the sender first is what lets that
    /// drain end; moved below `queue`, the drain would wait on the inbox's
    /// own sender forever.
    sender: Arc<mpsc::Sender<Envelope>>,
    queue: InboxQueue,
}

/// The receiving side of a [`SettlingInbox`]: the receiver and what it
/// takes to settle an envelope. A type of its own so that its `Drop`, the
/// final drain, runs after the inbox's sender field has been dropped.
struct InboxQueue {
    id: MailboxId,
    receiver: mpsc::Receiver<Envelope>,
    mailer: Arc<Mailer>,
    reply_counter: ReplyLineage,
}

impl InboxQueue {
    fn wrap(&self, env: Envelope) -> InboundMail {
        InboundMail {
            env,
            mailer: Arc::clone(&self.mailer),
            self_mailbox: self.id,
            reply_counter: self.reply_counter.clone(),
        }
    }
}

impl Drop for InboxQueue {
    fn drop(&mut self) {
        // The inbox's strong sender is already gone (field order on
        // `SettlingInbox`), so no later feed upgrade succeeds and the only
        // senders left are the ones sends already in progress hold. The
        // blocking receive returns `Err` only once those are released and
        // the queue is empty, so a send that raced this drop is settled
        // here instead of being destroyed armed by the receiver's own drop
        // (#7460). Each wrapped envelope's guard records `Finished` +
        // disarms on drop, so teardown is a settled drain (#1704, #1716).
        //
        // Nothing is logged here: this can run inside a thread-local
        // destructor at worker exit (see the module docs), where a log
        // event aborts the process.
        while let Ok(env) = self.receiver.recv() {
            drop(self.wrap(env));
        }
    }
}

impl fmt::Debug for SettlingInbox {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SettlingInbox").field("id", &self.queue.id).finish_non_exhaustive()
    }
}

impl SettlingInbox {
    /// Bind `receiver` to the mailbox at `id`, with a fresh reply-id
    /// counter. Production callers outside the crate receive one already
    /// built on the [`MailboxClaim`](crate::chassis::ctx::MailboxClaim)
    /// returned by `claim_mailbox`; this form serves substrate-internal
    /// inboxes that hold no proof yet: a claim, and a Starting route before
    /// its owner promotes it.
    pub(crate) fn new_at(id: MailboxId, receiver: InboxReceiver, mailer: Arc<Mailer>) -> Self {
        Self::new_with_lineage(id, receiver, mailer, ReplyLineage::new())
    }

    /// Bind `receiver` to the mailbox at `id`, sharing the given
    /// `reply_lineage` counter. Used by
    /// [`NativeBinding::install_inbox`](crate::actor::native::NativeBinding::install_inbox)
    /// so the dispatcher's inbox and the binding's reply allocator draw
    /// from one coherent disjoint id space.
    pub(crate) fn new_with_lineage(
        id: MailboxId,
        receiver: InboxReceiver,
        mailer: Arc<Mailer>,
        reply_lineage: ReplyLineage,
    ) -> Self {
        let InboxReceiver { sender, receiver } = receiver;
        Self { sender, queue: InboxQueue { id, receiver, mailer, reply_counter: reply_lineage } }
    }

    /// Queue `env` through the inbox's own sender: mail that must be in the
    /// queue before the actor is live (`after_init`, bootstrap, and parked
    /// mail), so the spawn and activation paths hold no sender.
    ///
    /// The send cannot fail: an inbox always holds its sender, and the
    /// receiver is this inbox's own field. A refused send is therefore a
    /// broken invariant (ADR-0063): a debug build panics, and a release
    /// build logs the error and settles the envelope so its chain still
    /// closes. The settle comes first so the panic never unwinds past an
    /// armed envelope.
    pub(crate) fn preload(&self, env: Envelope) {
        if let Err(mpsc::SendError(env)) = self.sender.send(env) {
            tracing::error!(
                target: "aether_substrate::capability",
                mailbox = %self.queue.id,
                kind = %env.kind,
                "preloaded mail refused by the inbox's own receiver — mail discarded",
            );
            drop(self.queue.wrap(env));
            debug_assert!(false, "SettlingInbox::preload: the inbox's own receiver refused the send");
        }
    }

    /// Re-home this inbox's reply-id minting onto `reply_lineage`,
    /// replacing the fresh counter [`Self::new_at`] gave it. Additive
    /// (ADR-0160 §1): a driver-as-actor mailbox is reserved at the Claim
    /// stage (ADR-0155 §4) through `claim_mailbox`, which builds the
    /// [`SettlingInbox`] with its own fresh [`ReplyLineage`]; when the
    /// pumped boot recovers that claim and installs the inbox on a
    /// [`NativeBinding`](crate::actor::native::NativeBinding), the two must draw reply ids from one coherent
    /// disjoint space — otherwise the inbox's counter and the binding's
    /// both start at [`ReplyLineage::BASE`] and collide (the issue 1695
    /// invariant). This mirrors what the standard
    /// [`NativeBinding::install_inbox`](crate::actor::native::NativeBinding::install_inbox)
    /// path gets for free by building its inbox through
    /// [`Self::new_with_lineage`].
    #[must_use]
    pub(crate) fn relineage(mut self, reply_lineage: ReplyLineage) -> Self {
        self.queue.reply_counter = reply_lineage;
        self
    }

    /// The reply target that routes a recipient's reply into this inbox,
    /// correlated by `correlation`. [`RootPusher::push_root`](crate::RootPusher::push_root)
    /// stamps it on a root whose reply the claimer drains here, so the
    /// claim's position never leaves the crate.
    pub(crate) fn reply_source(&self, correlation: u64) -> Source {
        Source::with_correlation(SourceAddr::Component(self.queue.id), correlation)
    }

    /// Take the next queued envelope without blocking, wrapped in an
    /// [`InboundMail`] guard. `None` when the inbox is empty (or the
    /// senders have all disconnected). The returned guard settles its
    /// inbound on `Drop` — fall out of scope on any arm and the bracket
    /// is discharged.
    #[must_use]
    pub fn try_next(&self) -> Option<InboundMail> {
        self.queue.receiver.try_recv().ok().map(|env| self.queue.wrap(env))
    }

    /// Block up to `timeout` for the next envelope, wrapped in an
    /// [`InboundMail`] guard. `None` on timeout or disconnect. Used by
    /// the desktop driver's synchronous lifecycle-reply gate.
    #[must_use]
    pub fn recv_timeout(&self, timeout: Duration) -> Option<InboundMail> {
        self.queue.receiver.recv_timeout(timeout).ok().map(|env| self.queue.wrap(env))
    }

    /// Drain every currently-queued envelope, invoking `on_mail` for each
    /// as an [`InboundMail`] guard that settles when `on_mail` returns
    /// (the closure may move it onward, but on the common path it just
    /// reads the fields it needs and drops). Returns when the inbox is
    /// empty. Use [`Self::try_next`] when the per-mail body needs the
    /// surrounding `&mut self` (the closure here cannot also borrow it).
    pub fn drain(&self, mut on_mail: impl FnMut(InboundMail)) {
        while let Ok(env) = self.queue.receiver.try_recv() {
            on_mail(self.queue.wrap(env));
        }
    }

    /// Dispatcher face: take the next queued envelope without blocking,
    /// yielding the raw [`Envelope`]. Returns `None` when the inbox is
    /// empty or disconnected.
    pub(crate) fn try_recv(&self) -> Option<Envelope> {
        self.queue.receiver.try_recv().ok()
    }
}

/// A single inbound envelope drained from a [`SettlingInbox`], with its
/// ADR-0080 §2 settlement bracket fused to the value's lifetime.
///
/// Exposes the envelope's fields by borrow and replies through
/// [`Self::reply`] (lineage-joined). On `Drop` it records
/// `Finished(mail_id, root)` and disarms the ADR-0094 obligation guard,
/// in that order — so a reply sent earlier in the same scope records its
/// `Sent` before this inbound's `Finished` (ADR-0080 §6).
///
/// Owns its envelope and an `Arc` clone of the drain's mailer + reply
/// counter rather than borrowing the [`SettlingInbox`], so a consumer can
/// hold the guard while still reaching the surrounding `&mut self` (the
/// desktop window driver dispatches each mail against `&mut App`).
///
/// # Distinct from `DeferredReply`
///
/// ADR-0080 keeps two counts per root, and this type and
/// [`DeferredReply`](crate::actor::native::DeferredReply) are the handles for
/// one each (iamacoffeepot/aether#4163). This one brackets an *envelope* and
/// moves `in_flight`; `DeferredReply` carries a *debt* — a `SettlementHold`
/// plus a reply target, no envelope — and moves `held_open`. Both are
/// outstanding at once on every deferred reply: the handler returns and its
/// inbound records `Finished`, while the hold keeps the chain open until the
/// answer goes out.
///
/// [`Self::reply`] is a capability this guard offers rather than an obligation
/// it carries, which is why it returns `bool` and why dropping unreplied is
/// ordinary: an envelope earns exactly one `Finished` whether or not anyone
/// asked for an answer. A `DeferredReply` exists only when someone is waiting,
/// so its drop asserts.
pub struct InboundMail {
    env: Envelope,
    mailer: Arc<Mailer>,
    self_mailbox: MailboxId,
    reply_counter: ReplyLineage,
}

impl InboundMail {
    /// #1757: build a guard for a native dispatcher's *retained* inbound
    /// (via [`NativeCtx::take_inbound`](crate::actor::native::ctx::NativeCtx::take_inbound)).
    /// Mirrors a [`SettlingInbox`]'s own wrap, but the mailer / claimed mailbox /
    /// reply-lineage are passed explicitly because the native dispatcher
    /// owns those on its
    /// [`NativeBinding`](crate::actor::native::NativeBinding) rather than
    /// on a `SettlingInbox`. The returned guard settles the inbound's
    /// chain on `Drop` exactly as a sink-face guard does, so a deferred
    /// reply joins the same chain the dispatcher would otherwise have
    /// closed at its tail.
    pub(crate) fn from_dispatched(
        env: Envelope,
        mailer: Arc<Mailer>,
        self_mailbox: MailboxId,
        reply_counter: ReplyLineage,
    ) -> Self {
        Self { env, mailer, self_mailbox, reply_counter }
    }

    /// The mail's kind id.
    #[must_use]
    pub fn kind(&self) -> KindId {
        self.env.kind
    }

    /// The mail's immediate sender (reply target + correlation).
    #[must_use]
    pub fn sender(&self) -> Source {
        self.env.sender
    }

    /// The mail's encoded payload bytes.
    #[must_use]
    pub fn payload(&self) -> &[u8] {
        self.env.payload.bytes()
    }

    /// The mail's producer-minted identity (ADR-0080 §1), `None` for
    /// mail no producer stamped.
    #[must_use]
    pub fn mail_id(&self) -> Option<MailId> {
        self.env.mail_id
    }

    /// The root of the mail's causal chain (ADR-0080 §5), `None` for mail
    /// that carries no chain.
    #[must_use]
    pub fn root(&self) -> Option<MailId> {
        self.env.root
    }

    /// Borrow the underlying envelope — for the framework-built-in
    /// dispatch arms (`aether.log.tail` / `aether.trace.tail` /
    /// `aether.cost.tail`) that take `&Envelope`.
    #[must_use]
    pub fn envelope(&self) -> &Envelope {
        &self.env
    }

    /// Reply to the mail's sender, joining the inbound's causal chain
    /// (ADR-0080 §5/§6). Mints the reply id from the drain-owned
    /// reply-id counter in the disjoint reply-lineage space (`1 << 63`
    /// base) and stamps the inbound's `root` / parent, routing through
    /// [`Mailer::send_reply`]. Returns whether the reply
    /// was routed (`false` for a `SourceAddr::None` sender — nobody
    /// asked for a reply). The reply's `Sent` is recorded here, before
    /// this guard's `Drop` records the inbound's `Finished`, so the §6
    /// hold ordering holds by construction.
    pub fn reply<K: Kind>(&self, result: &K) -> bool {
        let correlation = self.reply_counter.mint();
        let reply_id = MailId::new(self.self_mailbox, correlation);
        // ADR-0080 §5: the inbound is the reply's parent, and a chassis-root
        // / lineage-less inbound gives it none, mirroring
        // `NativeCtx::outbound_parent`.
        self.mailer.send_reply(self.env.sender, result, Some(reply_id), self.env.root, self.env.mail_id)
    }
}

impl Drop for InboundMail {
    fn drop(&mut self) {
        // ADR-0080 §2 settlement discharge, then ADR-0094 guard disarm —
        // the same two-step the standard `dispatch_one` tail runs.
        // `record_finished` no-ops on an absent mail id, so a lineage-less
        // inbound settles nothing; the guard was minted disarmed for that
        // case too.
        self.mailer.record_finished(self.env.mail_id, self.env.root);
        self.env.discharge();
    }
}

/// #1757: a retained [`InboundMail`] crosses to a worker thread for a
/// deferred reply (the desktop capture readback path retains the guard in
/// one handler turn and `reply`s + drops it from the render thread), so it
/// must be `Send`. The compile is the assertion — if a future field makes
/// `InboundMail` `!Send`, this stops compiling rather than failing at the
/// `take_inbound` move site.
const _: fn() = || {
    fn assert_send<T: Send>() {}
    assert_send::<InboundMail>();
};

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test-setup unwraps: fixture construction panic on failure is the assertion")]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::thread;

    use super::*;
    use crate::testing::boot_authority;

    use crate::chassis::ctx::{MailboxWakeSlot, RelayOutcome, relay_or_transfer};
    use crate::chassis::settlement::SettlementRegistry;
    use crate::mail::MailRef;
    use crate::mail::SourceAddr;
    use crate::mail::registry::{DispatchParts, InboxHandler, OwnedDispatch, Registry};
    use aether_kinds::LifecycleAdvanceComplete;
    use aether_kinds::descriptors;

    /// A mailer wired to a settlement registry on both seams (the chassis
    /// builder does both installs at boot), plus the registry handle so a
    /// test can register a reply-target inbox.
    fn test_env() -> (Arc<Registry>, Arc<Mailer>, Arc<SettlementRegistry>) {
        let registry = Arc::new(Registry::new());
        for d in descriptors::all() {
            let _ = registry.register_kind_with_descriptor(&boot_authority(), d);
        }
        let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
        let settlement = Arc::new(SettlementRegistry::new());
        mailer.install_settlement_registry(Arc::clone(&settlement));
        mailer.trace_handle().install_settlement_registry(Arc::clone(&settlement));
        (registry, mailer, settlement)
    }

    /// An obligation-armed envelope addressed at `id` (armed iff
    /// `mail_id` is present, matching the production `route_mail` Inbox arm).
    fn armed_env(id: MailboxId, mail_id: Option<MailId>, root: Option<MailId>, sender: Source) -> Envelope {
        OwnedDispatch::armed(
            DispatchParts { sender, mail_id, root, ..DispatchParts::new(KindId(7), MailRef::from(Vec::new())) },
            id,
        )
    }

    /// Queue `env` on an open inbox through its feed, as a registry inbox
    /// handler does.
    fn queue(feed: &InboxFeed, env: Envelope) {
        assert!(feed.send(env).is_queued(), "an open inbox accepts the mail");
    }

    /// Every consumer arm settles the inbound: a payload-reading consume,
    /// an unmatched drop (fields never touched), a closure `drain`, and
    /// teardown of the `SettlingInbox` itself with mail still queued.
    #[test]
    fn every_arm_settles() {
        let (_registry, mailer, settlement) = test_env();
        let id = MailboxId(0x106);

        // (1) consume — read the payload, then drop.
        {
            let (receiver, feed) = inbox_channel();
            let inbox = SettlingInbox::new_at(id, receiver, Arc::clone(&mailer));
            let root = MailId::new(id, 1);
            mailer.record_sent_inflight(root);
            let settle = settlement.subscribe_settlement(root);
            queue(&feed, armed_env(id, Some(MailId::new(id, 11)), Some(root), Source::NONE));
            let mail = inbox.try_next().expect("one queued");
            let _ = mail.payload();
            drop(mail);
            settle.recv().expect("consume arm settles the root");
        }

        // (2) unmatched drop — never touch the fields, just drop.
        {
            let (receiver, feed) = inbox_channel();
            let inbox = SettlingInbox::new_at(id, receiver, Arc::clone(&mailer));
            let root = MailId::new(id, 2);
            mailer.record_sent_inflight(root);
            let settle = settlement.subscribe_settlement(root);
            queue(&feed, armed_env(id, Some(MailId::new(id, 12)), Some(root), Source::NONE));
            drop(inbox.try_next().expect("one queued"));
            settle.recv().expect("unmatched-drop arm settles the root");
        }

        // (3) closure drain.
        {
            let (receiver, feed) = inbox_channel();
            let inbox = SettlingInbox::new_at(id, receiver, Arc::clone(&mailer));
            let root = MailId::new(id, 3);
            mailer.record_sent_inflight(root);
            let settle = settlement.subscribe_settlement(root);
            queue(&feed, armed_env(id, Some(MailId::new(id, 13)), Some(root), Source::NONE));
            inbox.drain(|_mail| {});
            settle.recv().expect("drain arm settles the root");
        }

        // (4) teardown — mail queued, SettlingInbox dropped.
        {
            let (receiver, feed) = inbox_channel();
            let inbox = SettlingInbox::new_at(id, receiver, Arc::clone(&mailer));
            let root = MailId::new(id, 4);
            mailer.record_sent_inflight(root);
            let settle = settlement.subscribe_settlement(root);
            queue(&feed, armed_env(id, Some(MailId::new(id, 14)), Some(root), Source::NONE));
            drop(inbox);
            settle.recv().expect("teardown drain settles the queued root");
        }
    }

    /// An inbound with no mail id carries no settlement obligation:
    /// dropping its guard records no `Finished` (parity with
    /// `record_finished`'s absent-id no-op) and the disarmed guard never
    /// panics.
    #[test]
    fn absent_mail_id_is_a_noop() {
        let (_registry, mailer, settlement) = test_env();
        let id = MailboxId(0x107);
        let guard_root = MailId::new(id, 9);
        mailer.record_sent_inflight(guard_root);
        let guard_rx = settlement.subscribe_settlement(guard_root);

        let (receiver, feed) = inbox_channel();
        let inbox = SettlingInbox::new_at(id, receiver, Arc::clone(&mailer));
        queue(&feed, armed_env(id, None, Some(guard_root), Source::NONE));
        // Drop without reading — an inbound with no mail id must not settle anything.
        drop(inbox.try_next().expect("one queued"));
        assert!(guard_rx.try_recv().is_err(), "an inbound with no mail id discharges no root");
    }

    /// ADR-0080 §6: a reply's `Sent` is recorded before the inbound's
    /// `Finished`, so the caller's chain stays open until the reply's own
    /// `Finished` lands. Reply, then drop the guard, then settle the
    /// reply — only the last step closes the root.
    #[test]
    fn reply_sent_recorded_before_inbound_finished() {
        let (registry, mailer, settlement) = test_env();
        let id = MailboxId(0x108);
        let reply_target = MailboxId(0x109);

        // Register the reply target so the reply routes somewhere we can
        // pick it back up and finish it.
        let (rtx, rrx) = mpsc::channel::<Envelope>();
        let handler: Arc<dyn InboxHandler> = Arc::new(move |d: Envelope| {
            let _ = rtx.send(d);
        });
        let reply_target = registry
            .try_register_inbox_with_id(&boot_authority(), reply_target, "test.inbox.reply_target", handler)
            .expect("register reply target");

        let root = MailId::new(id, 1);
        mailer.record_sent_inflight(root);
        let settle = settlement.subscribe_settlement(root);

        let (receiver, feed) = inbox_channel();
        let inbox = SettlingInbox::new_at(id, receiver, Arc::clone(&mailer));
        let sender = Source::with_correlation(SourceAddr::Component(reply_target), 7);
        queue(&feed, armed_env(id, Some(MailId::new(id, 21)), Some(root), sender));

        let mail = inbox.try_next().expect("one queued");
        assert!(
            mail.reply(&LifecycleAdvanceComplete { completed: 1, next: 42 }),
            "reply routed to the Component target",
        );
        // The reply's `Sent` is now on the root; it is not yet settled.
        assert!(settle.try_recv().is_err(), "reply Sent holds the chain open");
        drop(mail);
        // The inbound's `Finished` landed, but the reply is still in flight.
        assert!(settle.try_recv().is_err(), "inbound Finished alone does not settle — the reply is still open");

        // Finish the reply the way its eventual recipient's dispatcher
        // would; only now does the root settle.
        let reply_env = rrx.recv().expect("reply routed to the target inbox");
        let reply_id = reply_env.mail_id;
        reply_env.discharge();
        mailer.record_finished(reply_id, Some(root));
        settle.recv().expect("root settles after the reply finishes");
    }

    /// `InboundMail::reply` mints its reply id in the disjoint
    /// reply-lineage id space ([`ReplyLineage::BASE`]), stamped with the
    /// claimed mailbox as the sender.
    #[test]
    fn reply_id_minted_in_reply_lineage_space() {
        let (registry, mailer, _settlement) = test_env();
        let id = MailboxId(0x10a);
        let reply_target = MailboxId(0x10b);

        let (rtx, rrx) = mpsc::channel::<Envelope>();
        let handler: Arc<dyn InboxHandler> = Arc::new(move |d: Envelope| {
            let _ = rtx.send(d);
        });
        let reply_target = registry
            .try_register_inbox_with_id(&boot_authority(), reply_target, "test.inbox.reply_id_target", handler)
            .expect("register reply target");

        let (receiver, feed) = inbox_channel();
        let inbox = SettlingInbox::new_at(id, receiver, Arc::clone(&mailer));
        let sender = Source::with_correlation(SourceAddr::Component(reply_target), 1);
        // A lineage-less inbound (no root) still mints a high-space
        // reply id — the id space is the drain's, not the inbound's.
        queue(&feed, armed_env(id, None, None, sender));

        let mail = inbox.try_next().expect("one queued");
        mail.reply(&LifecycleAdvanceComplete { completed: 0, next: 0 });
        drop(mail);

        let reply_env = rrx.recv().expect("reply routed");
        let reply_id = reply_env.mail_id.expect("a reply always carries its own id");
        assert!(reply_id.correlation_id >= ReplyLineage::BASE, "reply id sits in the reply-lineage space");
        assert_eq!(reply_id.sender, id, "reply id is stamped with the claimed mailbox");
        reply_env.discharge();
    }

    /// #7460: a send that began before the inbox closed is settled by the
    /// closing drain. Catches a final drain made non-blocking again, and the
    /// sender field moved below the queue so it is released after the drain
    /// instead of before it: either
    /// lets the drop finish while a relay still holds an upgraded sender,
    /// so the send lands on a channel nobody drains and the receiver's own
    /// drop destroys the envelope armed.
    #[test]
    fn a_send_in_progress_is_settled_by_the_closing_drain() {
        let (_registry, mailer, settlement) = test_env();
        let id = MailboxId(0x7460);
        let (receiver, feed) = inbox_channel();
        let inbox = SettlingInbox::new_at(id, receiver, Arc::clone(&mailer));

        let queued_root = MailId::new(id, 1);
        mailer.record_sent_inflight(queued_root);
        let queued_settled = settlement.subscribe_settlement(queued_root);
        queue(&feed, armed_env(id, Some(MailId::new(id, 11)), Some(queued_root), Source::NONE));

        let racing_root = MailId::new(id, 2);
        mailer.record_sent_inflight(racing_root);
        let racing_settled = settlement.subscribe_settlement(racing_root);
        let racing = armed_env(id, Some(MailId::new(id, 12)), Some(racing_root), Source::NONE);

        // A send in progress: the upgraded sender `InboxFeed::send` holds
        // between its upgrade and its return.
        let held = feed.sender.upgrade().expect("an open inbox's sender upgrades");

        thread::scope(|scope| {
            let dropping = scope.spawn(move || drop(inbox));

            // The queued mail settles only inside the drop's drain, which
            // runs after the close, so the drop is now waiting on `held`.
            queued_settled.recv().expect("the closing drain settles the queued mail");

            let sent = held.send(racing);
            if let Err(mpsc::SendError(refused)) = sent {
                refused.discharge();
                panic!("the closing drain returned while a send was still in progress");
            }
            drop(held);

            dropping.join().expect("the inbox drops without an obligation leak");
        });

        racing_settled.try_recv().expect("the closing drain settles the mail whose send was in progress");
    }

    /// #7460: once the inbox is dropped, the feed refuses and the relay
    /// settles the mail and reports its kind. Catches a feed that keeps the
    /// channel alive past its inbox, which would queue mail nothing ever
    /// drains, and a relay that disarms a refused mail without recording
    /// its `Finished`, which leaves its root in flight forever (#7116).
    #[test]
    fn a_send_after_the_inbox_dropped_is_refused_and_settled() {
        let (_registry, mailer, settlement) = test_env();
        let id = MailboxId(0x7461);
        let (receiver, feed) = inbox_channel();
        let inbox = SettlingInbox::new_at(id, receiver, Arc::clone(&mailer));

        let root = MailId::new(id, 1);
        mailer.record_sent_inflight(root);
        let settled = settlement.subscribe_settlement(root);
        drop(inbox);

        let late = armed_env(id, Some(MailId::new(id, 11)), Some(root), Source::NONE);
        let outcome = relay_or_transfer(late, &feed, &MailboxWakeSlot::default(), &Arc::downgrade(&mailer));

        let RelayOutcome::SenderGone { kind } = outcome else {
            panic!("a dropped inbox refuses the mail: {outcome:?}");
        };
        assert_eq!(kind, KindId(7), "the discarded mail's kind id rides the outcome");
        settled.try_recv().expect("the relay settles the refused mail");
    }

    /// #7460: the relay holds no sender while it wakes. Catches an upgraded
    /// sender held across the wake hook: a slot whose last reference the
    /// wake path releases drops its inbox on the relaying thread, and that
    /// inbox's blocking drain would then wait on the sender its own thread
    /// holds.
    #[test]
    fn the_wake_hook_runs_with_no_sender_held() {
        let (_registry, mailer, _settlement) = test_env();
        let id = MailboxId(0x7462);
        let (receiver, feed) = inbox_channel();
        let inbox = SettlingInbox::new_at(id, receiver, Arc::clone(&mailer));

        let senders_at_wake = Arc::new(AtomicUsize::new(0));
        let wake = MailboxWakeSlot::default();
        wake.set(Arc::new({
            let feed = feed.clone();
            let senders_at_wake = Arc::clone(&senders_at_wake);
            move || senders_at_wake.store(feed.sender.strong_count(), Ordering::SeqCst)
        }));

        let mail = armed_env(id, Some(MailId::new(id, 11)), None, Source::NONE);
        let outcome = relay_or_transfer(mail, &feed, &wake, &Arc::downgrade(&mailer));

        assert!(matches!(outcome, RelayOutcome::Delivered), "an open inbox takes the mail: {outcome:?}");
        assert_eq!(senders_at_wake.load(Ordering::SeqCst), 1, "only the inbox's own sender is held during the wake");
        drop(inbox);
    }
}
