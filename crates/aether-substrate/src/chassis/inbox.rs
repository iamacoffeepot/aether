//! ADR-0106: the framework drain that seals the claimed-mailbox surface.
//!
//! A mailbox claimed via [`ChassisCtx::claim_mailbox`](crate::chassis::ctx::ChassisCtx::claim_mailbox)
//! no longer hands the capability a raw `mpsc::Receiver<Envelope>`. It
//! carries a [`ClaimedInbox`] — the receiver plus the `Arc<Mailer>` and
//! the claim's [`MailboxId`] — and the only way to reach an inbound
//! envelope is one of its drain methods, each of which yields an
//! [`InboundMail`] guard. The guard's `Drop` records `Finished` and
//! disarms the ADR-0094 obligation in one motion, so every consumer arm
//! — match, decode error, unrecognised kind, early return, panic-unwind,
//! teardown of the drain itself — settles, because settlement is what
//! falling out of scope *does*. This mirrors `DispatcherSlot::dispatch_one`'s
//! unconditional discharge tail (the standard actor path), so a leak at
//! this seam is unrepresentable in consumer code rather than detected
//! after the fact (the recurring #846 / #1325 / #1704 class).
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

/// Base of the drain's reply-lineage id space (ADR-0080 §5 / #1701).
/// Sits in the top half of the `u64` space, disjoint from the `send`
/// correlation counters that start at `0`, so a reply id minted here
/// never collides with a `send` id. Mirrors
/// [`NativeBinding`](crate::actor::native::NativeBinding)'s and the wasm
/// trampoline's reply-lineage base, so the claimed-mailbox reply path
/// derives reply ids the same way every other reply site does.
const REPLY_LINEAGE_BASE: u64 = 1 << 63;

/// The sealed inbound surface of a claimed mailbox (ADR-0106).
///
/// Owns the inbox receiver, an `Arc<Mailer>` (for settlement discharge
/// and lineage-joined replies), the claim's [`MailboxId`], and a
/// drain-owned reply-id counter. The raw receiver never escapes — every
/// inbound envelope is reached through [`Self::try_next`],
/// [`Self::recv_timeout`], or [`Self::drain`], each of which wraps the
/// envelope in an [`InboundMail`] guard that settles on `Drop`.
///
/// Dropping a `ClaimedInbox` drains whatever is still queued and lets
/// each guard settle, so a teardown that abandons mail (the #1704 shape:
/// a queued reply envelope dropped on driver teardown) becomes a settled
/// drain instead of an armed drop.
pub struct ClaimedInbox {
    id: MailboxId,
    receiver: mpsc::Receiver<Envelope>,
    mailer: Arc<Mailer>,
    reply_counter: Arc<AtomicU64>,
}

impl fmt::Debug for ClaimedInbox {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClaimedInbox")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl ClaimedInbox {
    /// Build a `ClaimedInbox` over `receiver`. Production callers receive
    /// one already built on the [`MailboxClaim`](crate::chassis::ctx::MailboxClaim)
    /// returned by `claim_mailbox`; the explicit constructor is `pub` so
    /// out-of-crate tests can pair a channel with a registered inbox
    /// handler and drive the guard directly.
    #[must_use]
    pub fn new(id: MailboxId, receiver: mpsc::Receiver<Envelope>, mailer: Arc<Mailer>) -> Self {
        Self {
            id,
            receiver,
            mailer,
            reply_counter: Arc::new(AtomicU64::new(REPLY_LINEAGE_BASE)),
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
            reply_counter: Arc::clone(&self.reply_counter),
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
}

impl Drop for ClaimedInbox {
    fn drop(&mut self) {
        // Settle anything still queued: each wrapped envelope's guard
        // records `Finished` + disarms on drop, so teardown is a settled
        // drain rather than an armed drop (#1704).
        while let Ok(env) = self.receiver.try_recv() {
            drop(self.wrap(env));
        }
    }
}

/// A single inbound envelope drained from a [`ClaimedInbox`], with its
/// ADR-0080 §2 settlement bracket fused to the value's lifetime.
///
/// Exposes the envelope's fields by borrow and replies through
/// [`Self::reply`] (lineage-joined). On `Drop` it records
/// `Finished(mail_id, root)` and disarms the ADR-0094 obligation guard,
/// in that order — so a reply sent earlier in the same scope records its
/// `Sent` before this inbound's `Finished` (ADR-0080 §6).
///
/// Owns its envelope and an `Arc` clone of the drain's mailer + reply
/// counter rather than borrowing the [`ClaimedInbox`], so a consumer can
/// hold the guard while still reaching the surrounding `&mut self` (the
/// desktop window driver dispatches each mail against `&mut App`).
pub struct InboundMail {
    env: Envelope,
    mailer: Arc<Mailer>,
    self_mailbox: MailboxId,
    reply_counter: Arc<AtomicU64>,
}

impl InboundMail {
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
    /// counter in the disjoint reply-lineage id space and stamps the
    /// inbound's `root` / parent, routing through
    /// [`Mailer::send_reply_with_lineage`]. Returns whether the reply
    /// was routed (`false` for a `SourceAddr::None` sender — nobody
    /// asked for a reply). The reply's `Sent` is recorded here, before
    /// this guard's `Drop` records the inbound's `Finished`, so the §6
    /// hold ordering holds by construction.
    pub fn reply<K: Kind>(&self, result: &K) -> bool {
        let correlation = self.reply_counter.fetch_add(1, Ordering::AcqRel);
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

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "test-setup unwraps: fixture construction panic on failure is the assertion"
)]
mod tests {
    use super::*;

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
    /// teardown of the `ClaimedInbox` itself with mail still queued.
    #[test]
    fn every_arm_settles() {
        let (_registry, mailer, settlement) = test_env();
        let id = MailboxId(0x106);

        // (1) consume — read the payload, then drop.
        {
            let (tx, rx) = mpsc::channel();
            let inbox = ClaimedInbox::new(id, rx, Arc::clone(&mailer));
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
            let inbox = ClaimedInbox::new(id, rx, Arc::clone(&mailer));
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
            let inbox = ClaimedInbox::new(id, rx, Arc::clone(&mailer));
            let root = MailId::new(id, 3);
            mailer.record_sent_inflight(root);
            let settle = settlement.subscribe_settlement(root);
            tx.send(armed_env(id, MailId::new(id, 13), root, Source::NONE))
                .unwrap();
            inbox.drain(|_mail| {});
            settle.recv().expect("drain arm settles the root");
        }

        // (4) teardown — mail queued, ClaimedInbox dropped.
        {
            let (tx, rx) = mpsc::channel();
            let inbox = ClaimedInbox::new(id, rx, Arc::clone(&mailer));
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
        let inbox = ClaimedInbox::new(id, rx, Arc::clone(&mailer));
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
        let inbox = ClaimedInbox::new(id, rx, Arc::clone(&mailer));
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
    /// reply-lineage id space (`1 << 63` base), stamped with the claimed
    /// mailbox as the sender.
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
        let inbox = ClaimedInbox::new(id, rx, Arc::clone(&mailer));
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
            reply_env.mail_id.correlation_id >= REPLY_LINEAGE_BASE,
            "reply id sits in the reply-lineage space",
        );
        assert_eq!(
            reply_env.mail_id.sender, id,
            "reply id is stamped with the claimed mailbox",
        );
        reply_env.discharge();
    }
}
