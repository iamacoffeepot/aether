//! ADR-0106: the framework drain that backs every `Receiver<Envelope>` in
//! the substrate (the universal `SettlingInbox`).
//!
//! A mailbox claimed via [`ChassisCtx::claim_mailbox`](crate::chassis::ctx::ChassisCtx::claim_mailbox)
//! carries a [`SettlingInbox`] — the receiver plus the `Arc<Mailer>`,
//! the claim's [`MailboxId`], and a drain-owned reply-id counter.
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
//! The **dispatcher face** (`recv_blocking` / `try_recv`) yields the raw
//! [`Envelope`] so the native actor dispatcher
//! ([`NativeBinding`](crate::actor::native::NativeBinding)) keeps its
//! existing explicit `record_finished` + `discharge` tail unchanged.
//! Teardown still settles: `Drop` drains whatever is still queued and
//! settles each guard, so an armed envelope left in the dispatcher's inbox
//! at binding teardown settles rather than leaking (#1716).
//!
//! Settling on scope exit rather than on payload access is load-bearing:
//! ADR-0080 §6 requires a reply's `Sent` to be recorded before the
//! inbound's `Finished`, so a consume-time discharge would close the
//! caller's chain before the reply joins it. [`InboundMail::reply`]
//! routes through [`Mailer::send_reply_with_lineage`] with the inbound's
//! chain and a drain-owned reply-id counter, so a claimed-mailbox
//! consumer never reaches the bare, lineage-less `send_reply`.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use aether_data::{Kind, KindId, MailId, MailboxId, Source};

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

/// The sealed inbound surface that backs every `Receiver<Envelope>` in
/// the substrate (ADR-0106 + #1716).
///
/// Owns the inbox receiver, an `Arc<Mailer>` (for settlement discharge
/// and lineage-joined replies), the claim's [`MailboxId`], and a
/// reply-id counter.
///
/// **Sink face** — for out-of-crate caps and the desktop window driver:
/// [`Self::try_next`], [`Self::recv_timeout`], [`Self::drain`]. Each
/// yields an [`InboundMail`] guard that settles on `Drop`.
///
/// **Dispatcher face** (`pub(crate)`) — for the native actor dispatcher
/// ([`NativeBinding`](crate::actor::native::NativeBinding)):
/// `recv_blocking` / `try_recv`. Each yields a raw [`Envelope`] so the
/// dispatcher keeps its explicit `record_finished` + `discharge` tail.
///
/// Dropping a `SettlingInbox` drains whatever is still queued and lets
/// each guard settle, so a teardown that abandons mail (the #1704 shape:
/// a queued reply envelope dropped on driver teardown, or an armed
/// envelope in the dispatcher's inbox at binding teardown — #1716)
/// becomes a settled drain instead of an armed drop.
pub struct SettlingInbox {
    id: MailboxId,
    receiver: mpsc::Receiver<Envelope>,
    mailer: Arc<Mailer>,
    reply_counter: ReplyLineage,
}

impl fmt::Debug for SettlingInbox {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SettlingInbox")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl SettlingInbox {
    /// Build a `SettlingInbox` over `receiver` with a fresh reply-id
    /// counter. Production callers receive one already built on the
    /// [`MailboxClaim`](crate::chassis::ctx::MailboxClaim) returned by
    /// `claim_mailbox`; the explicit constructor is `pub` so out-of-crate
    /// tests can pair a channel with a registered inbox handler and drive
    /// the guard directly.
    #[must_use]
    pub fn new(id: MailboxId, receiver: mpsc::Receiver<Envelope>, mailer: Arc<Mailer>) -> Self {
        Self {
            id,
            receiver,
            mailer,
            reply_counter: ReplyLineage::new(),
        }
    }

    /// Build a `SettlingInbox` over `receiver`, sharing the given
    /// `reply_lineage` counter. Used by
    /// [`NativeBinding::install_inbox`](crate::actor::native::NativeBinding::install_inbox)
    /// so the dispatcher's inbox and the binding's reply allocator draw
    /// from one coherent disjoint id space.
    pub(crate) fn new_with_lineage(
        id: MailboxId,
        receiver: mpsc::Receiver<Envelope>,
        mailer: Arc<Mailer>,
        reply_lineage: ReplyLineage,
    ) -> Self {
        Self {
            id,
            receiver,
            mailer,
            reply_counter: reply_lineage,
        }
    }

    /// The claimed mailbox's id.
    #[must_use]
    pub fn id(&self) -> MailboxId {
        self.id
    }

    fn wrap(&self, env: Envelope) -> InboundMail {
        InboundMail {
            env,
            mailer: Arc::clone(&self.mailer),
            self_mailbox: self.id,
            reply_counter: self.reply_counter.clone(),
        }
    }

    /// Take the next queued envelope without blocking, wrapped in an
    /// [`InboundMail`] guard. `None` when the inbox is empty (or the
    /// senders have all disconnected). The returned guard settles its
    /// inbound on `Drop` — fall out of scope on any arm and the bracket
    /// is discharged.
    #[must_use]
    pub fn try_next(&self) -> Option<InboundMail> {
        self.receiver.try_recv().ok().map(|env| self.wrap(env))
    }

    /// Block up to `timeout` for the next envelope, wrapped in an
    /// [`InboundMail`] guard. `None` on timeout or disconnect. Used by
    /// the desktop driver's synchronous lifecycle-reply gate.
    #[must_use]
    pub fn recv_timeout(&self, timeout: Duration) -> Option<InboundMail> {
        self.receiver
            .recv_timeout(timeout)
            .ok()
            .map(|env| self.wrap(env))
    }

    /// Drain every currently-queued envelope, invoking `on_mail` for each
    /// as an [`InboundMail`] guard that settles when `on_mail` returns
    /// (the closure may move it onward, but on the common path it just
    /// reads the fields it needs and drops). Returns when the inbox is
    /// empty. Use [`Self::try_next`] when the per-mail body needs the
    /// surrounding `&mut self` (the closure here cannot also borrow it).
    pub fn drain(&self, mut on_mail: impl FnMut(InboundMail)) {
        while let Ok(env) = self.receiver.try_recv() {
            on_mail(self.wrap(env));
        }
    }

    /// Dispatcher face: block until the next envelope arrives, yielding
    /// the raw [`Envelope`]. Returns `None` on channel disconnect. The
    /// native actor dispatcher uses this to keep its explicit
    /// `record_finished` + `discharge` tail unchanged.
    pub(crate) fn recv_blocking(&self) -> Option<Envelope> {
        self.receiver.recv().ok()
    }

    /// Dispatcher face: take the next queued envelope without blocking,
    /// yielding the raw [`Envelope`]. Returns `None` when the inbox is
    /// empty or disconnected.
    pub(crate) fn try_recv(&self) -> Option<Envelope> {
        self.receiver.try_recv().ok()
    }
}

impl Drop for SettlingInbox {
    fn drop(&mut self) {
        // Settle anything still queued: each wrapped envelope's guard
        // records `Finished` + disarms on drop, so teardown is a settled
        // drain rather than an armed drop (#1704, #1716).
        while let Ok(env) = self.receiver.try_recv() {
            drop(self.wrap(env));
        }
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
pub struct InboundMail {
    env: Envelope,
    mailer: Arc<Mailer>,
    self_mailbox: MailboxId,
    reply_counter: ReplyLineage,
}

impl InboundMail {
    /// #1757: build a guard for a native dispatcher's *retained* inbound
    /// (via [`NativeCtx::take_inbound`](crate::actor::native::ctx::NativeCtx::take_inbound)).
    /// Mirrors [`SettlingInbox::wrap`], but the mailer / claimed mailbox /
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
        Self {
            env,
            mailer,
            self_mailbox,
            reply_counter,
        }
    }

    /// The mail's kind id.
    #[must_use]
    pub fn kind(&self) -> KindId {
        self.env.kind
    }

    /// The mail's registered kind name.
    #[must_use]
    pub fn kind_name(&self) -> &str {
        &self.env.kind_name
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

    /// The mail's producer-minted identity (ADR-0080 §1).
    #[must_use]
    pub fn mail_id(&self) -> MailId {
        self.env.mail_id
    }

    /// The root of the mail's causal chain (ADR-0080 §5).
    #[must_use]
    pub fn root(&self) -> MailId {
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
    /// [`Mailer::send_reply_with_lineage`]. Returns whether the reply
    /// was routed (`false` for a `SourceAddr::None` sender — nobody
    /// asked for a reply). The reply's `Sent` is recorded here, before
    /// this guard's `Drop` records the inbound's `Finished`, so the §6
    /// hold ordering holds by construction.
    pub fn reply<K: Kind>(&self, result: &K) -> bool {
        let correlation = self.reply_counter.mint();
        let reply_id = MailId::new(self.self_mailbox, correlation);
        // ADR-0080 §5: collapse a NONE parent to `None` (chassis-root /
        // lineage-less inbound), mirroring `NativeCtx::outbound_parent`.
        let parent = if self.env.mail_id == MailId::NONE {
            None
        } else {
            Some(self.env.mail_id)
        };
        self.mailer.send_reply_with_lineage(
            self.env.sender,
            result,
            reply_id,
            self.env.root,
            parent,
        )
    }
}

impl Drop for InboundMail {
    fn drop(&mut self) {
        // ADR-0080 §2 settlement discharge, then ADR-0094 guard disarm —
        // the same two-step the standard `dispatch_one` tail runs.
        // `record_finished` no-ops on `MailId::NONE`, so a lineage-less
        // inbound settles nothing (parity with the chassis-internal push
        // sentinel); the guard was minted disarmed for that case too.
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
#[allow(
    clippy::unwrap_used,
    reason = "test-setup unwraps: fixture construction panic on failure is the assertion"
)]
mod tests {
    use super::*;

    use crate::actor::native::binding::NativeBinding;
    use crate::actor::native::ctx::NativeCtx;
    use crate::chassis::settlement::SettlementRegistry;
    use crate::handle_store::HandleStore;
    use crate::mail::MailRef;
    use crate::mail::SourceAddr;
    use crate::mail::registry::{InboxHandler, OwnedDispatch, Registry};
    use aether_kinds::descriptors;
    use aether_kinds::{LifecycleAdvanceComplete, trace::Nanos};

    /// A mailer wired to a settlement registry on both seams (the chassis
    /// builder does both installs at boot), plus the registry handle so a
    /// test can register a reply-target inbox.
    fn test_env() -> (Arc<Registry>, Arc<Mailer>, Arc<SettlementRegistry>) {
        let registry = Arc::new(Registry::new());
        for d in descriptors::all() {
            let _ = registry.register_kind_with_descriptor(d);
        }
        let store = Arc::new(HandleStore::new(1024 * 1024));
        let mailer = Arc::new(Mailer::new(Arc::clone(&registry), store));
        let settlement = Arc::new(SettlementRegistry::new());
        mailer.install_settlement_registry(Arc::clone(&settlement));
        mailer
            .trace_handle()
            .install_settlement_registry(Arc::clone(&settlement));
        (registry, mailer, settlement)
    }

    /// An obligation-armed envelope addressed at `id` (armed iff
    /// `mail_id != NONE`, matching the production `route_mail` Inbox arm).
    fn armed_env(id: MailboxId, mail_id: MailId, root: MailId, sender: Source) -> Envelope {
        OwnedDispatch::armed(
            KindId(7),
            "test.inbox.kind".to_owned(),
            None,
            sender,
            MailRef::from(Vec::new()),
            1,
            mail_id,
            root,
            None,
            Nanos(0),
            0,
            id,
        )
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
            let (tx, rx) = mpsc::channel();
            let inbox = SettlingInbox::new(id, rx, Arc::clone(&mailer));
            let root = MailId::new(id, 1);
            mailer.record_sent_inflight(root);
            let settle = settlement.subscribe_settlement(root);
            tx.send(armed_env(id, MailId::new(id, 11), root, Source::NONE))
                .unwrap();
            let mail = inbox.try_next().expect("one queued");
            let _ = mail.payload();
            drop(mail);
            settle.recv().expect("consume arm settles the root");
        }

        // (2) unmatched drop — never touch the fields, just drop.
        {
            let (tx, rx) = mpsc::channel();
            let inbox = SettlingInbox::new(id, rx, Arc::clone(&mailer));
            let root = MailId::new(id, 2);
            mailer.record_sent_inflight(root);
            let settle = settlement.subscribe_settlement(root);
            tx.send(armed_env(id, MailId::new(id, 12), root, Source::NONE))
                .unwrap();
            drop(inbox.try_next().expect("one queued"));
            settle.recv().expect("unmatched-drop arm settles the root");
        }

        // (3) closure drain.
        {
            let (tx, rx) = mpsc::channel();
            let inbox = SettlingInbox::new(id, rx, Arc::clone(&mailer));
            let root = MailId::new(id, 3);
            mailer.record_sent_inflight(root);
            let settle = settlement.subscribe_settlement(root);
            tx.send(armed_env(id, MailId::new(id, 13), root, Source::NONE))
                .unwrap();
            inbox.drain(|_mail| {});
            settle.recv().expect("drain arm settles the root");
        }

        // (4) teardown — mail queued, SettlingInbox dropped.
        {
            let (tx, rx) = mpsc::channel();
            let inbox = SettlingInbox::new(id, rx, Arc::clone(&mailer));
            let root = MailId::new(id, 4);
            mailer.record_sent_inflight(root);
            let settle = settlement.subscribe_settlement(root);
            tx.send(armed_env(id, MailId::new(id, 14), root, Source::NONE))
                .unwrap();
            drop(inbox);
            settle
                .recv()
                .expect("teardown drain settles the queued root");
        }
    }

    /// A `MailId::NONE` inbound carries no settlement obligation: dropping
    /// its guard records no `Finished` (parity with `record_finished`'s
    /// NONE no-op) and the disarmed guard never panics.
    #[test]
    fn none_mail_id_is_a_noop() {
        let (_registry, mailer, settlement) = test_env();
        let id = MailboxId(0x107);
        let guard_root = MailId::new(id, 9);
        mailer.record_sent_inflight(guard_root);
        let guard_rx = settlement.subscribe_settlement(guard_root);

        let (tx, rx) = mpsc::channel();
        let inbox = SettlingInbox::new(id, rx, Arc::clone(&mailer));
        tx.send(armed_env(id, MailId::NONE, guard_root, Source::NONE))
            .unwrap();
        // Drop without reading — a NONE inbound must not settle anything.
        drop(inbox.try_next().expect("one queued"));
        assert!(
            guard_rx.try_recv().is_err(),
            "a NONE inbound discharges no root",
        );
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
            .try_register_inbox_with_id(reply_target, "test.inbox.reply_target", handler)
            .expect("register reply target");

        let root = MailId::new(id, 1);
        mailer.record_sent_inflight(root);
        let settle = settlement.subscribe_settlement(root);

        let (tx, rx) = mpsc::channel();
        let inbox = SettlingInbox::new(id, rx, Arc::clone(&mailer));
        let sender = Source::with_correlation(SourceAddr::Component(reply_target), 7);
        tx.send(armed_env(id, MailId::new(id, 21), root, sender))
            .unwrap();

        let mail = inbox.try_next().expect("one queued");
        assert!(
            mail.reply(&LifecycleAdvanceComplete {
                completed: 1,
                next: 42,
            }),
            "reply routed to the Component target",
        );
        // The reply's `Sent` is now on the root; it is not yet settled.
        assert!(
            settle.try_recv().is_err(),
            "reply Sent holds the chain open",
        );
        drop(mail);
        // The inbound's `Finished` landed, but the reply is still in flight.
        assert!(
            settle.try_recv().is_err(),
            "inbound Finished alone does not settle — the reply is still open",
        );

        // Finish the reply the way its eventual recipient's dispatcher
        // would; only now does the root settle.
        let reply_env = rrx.recv().expect("reply routed to the target inbox");
        let reply_id = reply_env.mail_id;
        reply_env.discharge();
        mailer.record_finished(reply_id, root);
        settle
            .recv()
            .expect("root settles after the reply finishes");
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
            .try_register_inbox_with_id(reply_target, "test.inbox.reply_id_target", handler)
            .expect("register reply target");

        let (tx, rx) = mpsc::channel();
        let inbox = SettlingInbox::new(id, rx, Arc::clone(&mailer));
        let sender = Source::with_correlation(SourceAddr::Component(reply_target), 1);
        // A lineage-less inbound (root NONE) still mints a high-space
        // reply id — the id space is the drain's, not the inbound's.
        tx.send(armed_env(id, MailId::NONE, MailId::NONE, sender))
            .unwrap();

        let mail = inbox.try_next().expect("one queued");
        mail.reply(&LifecycleAdvanceComplete {
            completed: 0,
            next: 0,
        });
        drop(mail);

        let reply_env = rrx.recv().expect("reply routed");
        assert!(
            reply_env.mail_id.correlation_id >= ReplyLineage::BASE,
            "reply id sits in the reply-lineage space",
        );
        assert_eq!(
            reply_env.mail_id.sender, id,
            "reply id is stamped with the claimed mailbox",
        );
        reply_env.discharge();
    }

    /// #1757: `NativeCtx::take_inbound` moves the *single* dispatched
    /// envelope out of the ctx, so the dispatcher's settlement tail
    /// (`take_raw_inbound`) sees `None` and does not also discharge it.
    /// One envelope in one place — the detector is `Option::is_some`, so a
    /// double-settle is structurally unrepresentable. A `MailId::NONE`
    /// inbound carries no obligation, so the guard's drop is a clean
    /// no-op; this asserts the ownership mechanics, not settlement.
    #[test]
    fn take_inbound_moves_the_single_envelope_out_of_the_ctx() {
        let (_registry, mailer, _settlement) = test_env();
        let id = MailboxId(0x1757_0001);
        let binding = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), id));
        let env = armed_env(id, MailId::NONE, MailId::NONE, Source::NONE);
        let mut ctx =
            NativeCtx::with_inbound(&binding, Source::NONE, MailId::NONE, MailId::NONE, env);

        let guard = ctx.take_inbound();
        assert!(
            ctx.take_raw_inbound().is_none(),
            "take_inbound moved the single envelope out — the tail sees None and never re-settles",
        );
        // NONE obligation: dropping the retained guard records nothing and
        // never panics (parity with `record_finished`'s NONE no-op).
        drop(guard);
    }

    /// #1757 / ADR-0094: single ownership must not weaken the leak
    /// detector. An armed inbound left in the ctx and dropped without
    /// being taken (the dispatcher forgot to settle it, or no handler
    /// retained it) still trips the obligation guard — settlement is not a
    /// silent self-disarming no-op. Debug-only (the guard is compiled out
    /// in release).
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "settlement-obligation leak")]
    fn undispatched_armed_inbound_panics_on_ctx_drop() {
        let (_registry, mailer, _settlement) = test_env();
        let id = MailboxId(0x1757_0002);
        let binding = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), id));
        let mail_id = MailId::new(id, 7);
        let env = armed_env(id, mail_id, mail_id, Source::NONE);
        let ctx = NativeCtx::with_inbound(&binding, Source::NONE, mail_id, mail_id, env);
        // Drop the ctx without taking the inbound — the single armed
        // envelope is dropped *inside* the ctx, so its ADR-0094 guard
        // panics rather than leaking.
        drop(ctx);
    }

    mod heavy {
        use super::*;
        use std::thread;

        /// #1757: the headline gate. A handler defers its reply by
        /// retaining the inbound guard (`take_inbound`), the dispatcher's
        /// tail sees `None` and does not settle, and the reply is sent
        /// across a worker thread. The chain settles **exactly once** —
        /// not prematurely (the retained guard holds the inbound's
        /// `Finished` until after the reply's `Sent`, ADR-0080 §6) and not
        /// twice (single ownership: only the retained guard ever settles
        /// this inbound). `mod heavy` because the cross-thread settlement
        /// needs timely progress.
        #[test]
        fn deferred_reply_across_thread_settles_exactly_once() {
            let (registry, mailer, settlement) = test_env();
            let id = MailboxId(0x1757_0010);
            let reply_target = MailboxId(0x1757_0011);

            // Register a reply target so the deferred reply routes
            // somewhere we can pick it up and finish it.
            let (rtx, rrx) = mpsc::channel::<Envelope>();
            let handler: Arc<dyn InboxHandler> = Arc::new(move |d: Envelope| {
                let _ = rtx.send(d);
            });
            let reply_target = registry
                .try_register_inbox_with_id(reply_target, "test.inbox.deferred_target", handler)
                .expect("register reply target");

            let root = MailId::new(id, 1);
            mailer.record_sent_inflight(root);
            let settle = settlement.subscribe_settlement(root);

            let binding = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), id));
            let sender = Source::with_correlation(SourceAddr::Component(reply_target), 7);
            let env = armed_env(id, MailId::new(id, 21), root, sender);
            let mut ctx = NativeCtx::with_inbound(&binding, sender, MailId::new(id, 21), root, env);

            // Handler defers: retain the guard, then the dispatcher tail
            // observes the inbound was taken (single ownership).
            let guard = ctx.take_inbound();
            assert!(
                ctx.take_raw_inbound().is_none(),
                "the handler retained the single envelope — the tail must not also settle it",
            );
            drop(ctx);

            // Reply + settle the inbound from a worker thread (the capture
            // readback shape). `InboundMail: Send`, so the guard crosses.
            #[allow(
                clippy::disallowed_methods,
                reason = "test infra thread below the actor/mail layer — models the off-thread deferred reply"
            )]
            let worker = thread::spawn(move || {
                assert!(
                    guard.reply(&LifecycleAdvanceComplete {
                        completed: 1,
                        next: 42,
                    }),
                    "deferred reply routed to the component target",
                );
                // Dropping the guard records the inbound's `Finished`
                // (after the reply's `Sent`) — the chain stays open until
                // the reply itself finishes.
                drop(guard);
            });
            worker.join().expect("worker thread");

            assert!(
                settle.try_recv().is_err(),
                "the reply's Sent holds the chain open — no premature settle",
            );

            // Finish the reply the way its eventual recipient's dispatcher
            // would; only now does the root settle — exactly once.
            let reply_env = rrx.recv().expect("reply routed to the target inbox");
            let reply_id = reply_env.mail_id;
            reply_env.discharge();
            mailer.record_finished(reply_id, root);
            settle
                .recv()
                .expect("root settles once the deferred reply finishes");
            assert!(
                settle.try_recv().is_err(),
                "the chain settles exactly once — no double-settle",
            );
        }
    }
}
