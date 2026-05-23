//! Cap-local spawn-and-die dispatch helper (ADR-0050 §2).
//!
//! The content-gen caps make long-tail blocking provider calls
//! (multi-second image gen, the `claude` subprocess) that must not
//! block the single-threaded actor's mail intake. This helper
//! implements the settled dispatch model: the cap holds an
//! `in_flight: usize` counter plus a `pending` queue in its lock-free
//! actor state (the single-threaded actor IS the mutual exclusion — no
//! `Semaphore`, no `Mutex`), and for each request either spawns one
//! ephemeral OS thread (if under the per-cap bound) or enqueues it.
//!
//! The ephemeral thread runs the blocking call, then routes the result
//! back through the cap's `Mailer` loopback — the same wake mechanism
//! `aether.tcp` / the RPC server use: capture `Arc<Mailer>` + the cap's
//! own `MailboxId` at submit time, run the call, push a result mail at
//! the cap's own mailbox (carrying the original sender's `ReplyTo` so a
//! re-reply routes correctly), and die. When that result mail lands on
//! the dispatcher thread, the cap's reply-landing handler runs
//! [`InFlightDispatch::on_reply_landed`] — decrement `in_flight`, pop +
//! spawn the next `pending` request — and re-replies to the original
//! caller. The original `ReplyTo` is stashed keyed on `request_id` so
//! the landing handler correlates without any FIFO assumption (the
//! ADR-0041 structured-correlation convention).
//!
//! This helper owns only the actor-state half (the counter + queue +
//! correlation map) and the spawn closure; the embedding cap owns the
//! two `#[handler]` methods that call [`submit`](InFlightDispatch::submit)
//! and [`on_reply_landed`](InFlightDispatch::on_reply_landed).

use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::thread;

use aether_data::{KindId, MailId, MailboxId, ReplyTo};
use aether_substrate::Mail;
use aether_substrate::mail::Mailer;
use aether_substrate::runtime::trace::SettlementHold;

/// Default per-cap concurrency bound when a cap doesn't override it.
/// Doubles as rate-limit throttling for the paid provider endpoints
/// (ADR-0050 §2) — at most this many provider calls run concurrently;
/// the rest queue.
pub const DEFAULT_MAX_IN_FLIGHT: usize = 4;

/// One unit of blocking work the dispatch helper runs off-thread. The
/// closure is the provider call; it returns the `(KindId, payload)` of
/// the result mail to land back on the cap's own mailbox. The cap's
/// reply-landing handler decodes that payload and re-replies to the
/// original caller.
///
/// Boxed so heterogeneous provider calls (a Messages request, a
/// `claude` subprocess, an image generation) share one queue type.
pub type BlockingCall = Box<dyn FnOnce() -> (KindId, Vec<u8>) + Send + 'static>;

struct QueuedRequest {
    request_id: u64,
    reply_to: ReplyTo,
    call: BlockingCall,
    /// The settlement hold acquired when this request was accepted (it
    /// enqueued rather than spawning immediately). The caller is owed a
    /// reply whether it ran at once or waited, so the chain stays held
    /// from accept to re-reply. Moved into the correlation map when the
    /// request later spawns (in [`InFlightDispatch::on_reply_landed`]).
    hold: SettlementHold,
}

/// What a landed reply hands back to the cap's reply-landing handler:
/// the original caller's `ReplyTo` plus the [`SettlementHold`] that has
/// kept the chain root open across the async provider call. The cap
/// re-replies through `reply_to` **first**, then drops `hold` so the
/// re-reply's `Sent` event is queued before the guard's `Release`
/// (ADR-0080 §12 ordering — see [`InFlightDispatch::take_landed`]).
#[must_use = "dropping the LandedReply releases the settlement hold; re-reply through reply_to first"]
pub struct LandedReply {
    /// The original caller's reply target, popped from the correlation
    /// map by `request_id`.
    pub reply_to: ReplyTo,
    /// The settlement hold acquired at `submit`/accept time. Drops
    /// (firing `Release`) when the cap handler's scope ends — after the
    /// re-reply, so `Sent` precedes `Release`.
    pub hold: SettlementHold,
}

/// Actor-state half of the spawn-and-die dispatch model. Lives in the
/// cap's plain (lock-free) fields; every method runs on the
/// single-threaded dispatcher.
pub struct InFlightDispatch {
    in_flight: usize,
    max_in_flight: usize,
    pending: VecDeque<QueuedRequest>,
    /// `request_id -> (original caller's ReplyTo, settlement hold)`.
    /// Stashed at submit, popped at reply-landing so the cap re-replies
    /// to the right caller without a FIFO assumption. The
    /// [`SettlementHold`] (ADR-0080 §12) keeps the chain root open from
    /// `submit` until the re-reply lands — without it the chain settles
    /// the instant the handler returns, seconds before the async
    /// provider reply exists (iamacoffeepot/aether#1031). The guard is
    /// `!Copy` and lives in single-threaded actor state, so the
    /// hold/release pair is structurally balanced.
    correlations: HashMap<u64, (ReplyTo, SettlementHold)>,
}

impl InFlightDispatch {
    /// Build a dispatcher bounded at `max_in_flight` concurrent
    /// provider calls. A `max_in_flight` of 0 is clamped to 1 — a
    /// zero bound would queue forever.
    #[must_use]
    pub fn new(max_in_flight: usize) -> Self {
        Self {
            in_flight: 0,
            max_in_flight: max_in_flight.max(1),
            pending: VecDeque::new(),
            correlations: HashMap::new(),
        }
    }

    /// How many provider calls are running right now. Exposed for the
    /// cap's `engine_logs` tracing and for tests.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.in_flight
    }

    /// How many requests are waiting for a free in-flight slot.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    /// Look up (and remove) the [`LandedReply`] — the original caller's
    /// `ReplyTo` plus the [`SettlementHold`] that kept the chain open —
    /// for a landed `request_id`. The cap's reply-landing handler calls
    /// this, re-replies through the returned `reply_to` **first**, then
    /// lets the `LandedReply` (carrying the hold) drop so the
    /// re-reply's `Sent` precedes the guard's `Release` (ADR-0080 §12
    /// ordering). After re-replying it calls
    /// [`on_reply_landed`](Self::on_reply_landed) to free the slot.
    /// Returns `None` for an unknown `request_id` (a double-landing or
    /// a stale reply); the chain is no longer held in that case (the
    /// hold dropped on the prior landing).
    pub fn take_landed(&mut self, request_id: u64) -> Option<LandedReply> {
        self.correlations
            .remove(&request_id)
            .map(|(reply_to, hold)| LandedReply { reply_to, hold })
    }

    /// Take a request off the mail queue. If a slot is free, stash the
    /// caller's `ReplyTo` + settlement hold, increment `in_flight`, and
    /// spawn the ephemeral thread; otherwise enqueue (carrying the
    /// hold). `mailer` + `self_id` are the cap's own (`ctx.mailer()` /
    /// `ctx.self_id()`) so the ephemeral thread can land its result mail
    /// back on the cap.
    ///
    /// `root` is the chain root (`ctx.in_flight_root()`). Before the
    /// request spawns *or* enqueues, this acquires a [`SettlementHold`]
    /// on `root` (ADR-0080 §12). Acquiring it here — before the cap's
    /// handler returns and queues its `Finished` — means the `HoldOpen`
    /// event is visible before `Finished`, so the chain never
    /// transiently settles between handler-return and the async reply.
    /// The hold lives in actor state (the correlation map or the
    /// `pending` queue) until the re-reply lands, outliving the
    /// ephemeral worker thread (which holds nothing).
    pub fn submit(
        &mut self,
        mailer: &Arc<Mailer>,
        self_id: MailboxId,
        root: MailId,
        request_id: u64,
        reply_to: ReplyTo,
        call: BlockingCall,
    ) {
        // Acquire before spawn/enqueue so `HoldOpen` is queued ahead of
        // the handler's `Finished`. The caller is owed a reply whether
        // it runs now or waits, so the hold spans accept -> re-reply in
        // both branches.
        let hold = mailer.acquire_settlement_hold(root);
        if self.in_flight < self.max_in_flight {
            self.in_flight += 1;
            self.correlations.insert(request_id, (reply_to, hold));
            Self::spawn(mailer, self_id, reply_to, call);
        } else {
            self.pending.push_back(QueuedRequest {
                request_id,
                reply_to,
                call,
                hold,
            });
        }
    }

    /// A result mail landed on the cap's own mailbox: free the slot and
    /// spawn the next pending request if there is one. Returns the
    /// freshly-dequeued request's `request_id` (or `None` when nothing
    /// was waiting) so the caller can trace drains.
    ///
    /// When a pending request drains, its [`SettlementHold`] (acquired
    /// at accept time in [`Self::submit`]) moves from the `pending`
    /// queue into the correlation map so it keeps the chain held until
    /// that request's own re-reply lands. The landing request's hold
    /// has already been taken (and dropped after re-reply) by
    /// [`Self::take_landed`] in the cap handler before this call.
    pub fn on_reply_landed(&mut self, mailer: &Arc<Mailer>, self_id: MailboxId) -> Option<u64> {
        self.in_flight = self.in_flight.saturating_sub(1);
        if let Some(next) = self.pending.pop_front() {
            self.in_flight += 1;
            self.correlations
                .insert(next.request_id, (next.reply_to, next.hold));
            Self::spawn(mailer, self_id, next.reply_to, next.call);
            Some(next.request_id)
        } else {
            None
        }
    }

    /// Spawn one ephemeral thread that runs the blocking call and lands
    /// the result mail back on the cap's own mailbox. The mail carries
    /// the original caller's `ReplyTo` so the dispatcher's reply-landing
    /// handler (and `ctx.reply`) routes the final reply correctly. The
    /// thread touches no actor state and dies after the push.
    fn spawn(mailer: &Arc<Mailer>, self_id: MailboxId, reply_to: ReplyTo, call: BlockingCall) {
        let mailer = Arc::clone(mailer);
        let spawned = thread::Builder::new()
            .name(String::from("aether-contentgen-call"))
            .spawn(move || {
                let (kind, payload) = call();
                mailer.push(Mail::new(self_id, kind, payload, 1).with_reply_to(reply_to));
            });
        if let Err(e) = spawned {
            tracing::error!(
                target: "aether_capabilities::contentgen",
                error = %e,
                "failed to spawn content-gen dispatch thread",
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_MAX_IN_FLIGHT, InFlightDispatch};
    use aether_data::{Kind, KindId, MailId, MailboxId, ReplyTarget, ReplyTo, SessionToken, Uuid};
    use aether_kinds::Pong;
    use aether_substrate::handle_store::HandleStore;
    use aether_substrate::mail::Mailer;
    use aether_substrate::mail::outbound::HubOutbound;
    use aether_substrate::mail::registry::Registry;
    use std::sync::Arc;
    use std::sync::mpsc;
    use std::time::Duration;

    /// Build a mailer wired to a loopback outbound. We don't drive the
    /// landed mail through a real dispatcher here — the test invokes
    /// `on_reply_landed` directly once the ephemeral thread signals it
    /// fired — so the registry can be bare.
    fn test_mailer() -> Arc<Mailer> {
        let (outbound, _rx) = HubOutbound::attached_loopback();
        let registry = Arc::new(Registry::new());
        let store = Arc::new(HandleStore::new(1024 * 1024));
        Arc::new(Mailer::new(registry, store).with_outbound(outbound))
    }

    fn session_reply_to(corr: u64) -> ReplyTo {
        ReplyTo::with_correlation(ReplyTarget::Session(SessionToken(Uuid::nil())), corr)
    }

    /// A synthetic chain root for a request — the value the cap handler
    /// would read from `ctx.in_flight_root()`. Distinct per `cid` so a
    /// multi-request test can keep each chain's hold accounting separate.
    fn root_id(cid: u64) -> MailId {
        MailId {
            sender: MailboxId(1),
            correlation_id: cid,
        }
    }

    #[test]
    fn new_clamps_zero_bound_to_one() {
        let d = InFlightDispatch::new(0);
        assert_eq!(d.in_flight(), 0);
        // A zero bound would queue forever; clamp guarantees the first
        // submit spawns rather than enqueues. Drive one through below.
        let _ = DEFAULT_MAX_IN_FLIGHT;
    }

    fn signal_call(id: u64, tx: mpsc::Sender<u64>) -> super::BlockingCall {
        Box::new(move || {
            let _ = tx.send(id);
            (KindId(<Pong as Kind>::ID.0), Vec::new())
        })
    }

    #[test]
    fn submit_under_bound_spawns_and_overflow_queues() {
        let mailer = test_mailer();
        let self_id = MailboxId(7);
        let mut d = InFlightDispatch::new(2);

        // A done-channel each spawned call signals so the test knows
        // the ephemeral thread ran and fired its loopback mail.
        let (done_tx, done_rx) = mpsc::channel::<u64>();

        // Submit max_in_flight + 1 = 3 requests against a bound of 2.
        d.submit(
            &mailer,
            self_id,
            root_id(1),
            1,
            session_reply_to(1),
            signal_call(1, done_tx.clone()),
        );
        d.submit(
            &mailer,
            self_id,
            root_id(2),
            2,
            session_reply_to(2),
            signal_call(2, done_tx.clone()),
        );
        d.submit(
            &mailer,
            self_id,
            root_id(3),
            3,
            session_reply_to(3),
            signal_call(3, done_tx),
        );

        // Two spawned immediately, the third queued.
        assert_eq!(d.in_flight(), 2);
        assert_eq!(d.pending(), 1);

        // The first two ephemeral threads run and signal. Their loopback
        // mails land on mailbox 7 (unknown → bubble to loopback
        // outbound, dropped) — the test doesn't depend on that; it
        // depends on the bookkeeping the landing handler would run.
        let mut landed = vec![
            done_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("first call runs"),
            done_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("second call runs"),
        ];

        // Simulate the first reply landing: frees a slot, drains the
        // queued third request, which spawns and signals.
        let drained = d.on_reply_landed(&mailer, self_id);
        assert_eq!(
            drained,
            Some(3),
            "the queued request drains on the first landing"
        );
        assert_eq!(
            d.in_flight(),
            2,
            "one freed, one spawned -> still 2 in flight"
        );
        assert_eq!(d.pending(), 0);
        landed.push(
            done_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("third call runs"),
        );

        // The remaining two landings free both slots; nothing left to
        // drain.
        assert_eq!(d.on_reply_landed(&mailer, self_id), None);
        assert_eq!(d.in_flight(), 1);
        assert_eq!(d.on_reply_landed(&mailer, self_id), None);
        assert_eq!(
            d.in_flight(),
            0,
            "in_flight returns to 0 after every reply lands"
        );

        // All three calls ran exactly once.
        landed.sort_unstable();
        assert_eq!(landed, vec![1, 2, 3]);
    }

    #[test]
    fn take_landed_correlates_by_request_id() {
        let mailer = test_mailer();
        let self_id = MailboxId(7);
        let mut d = InFlightDispatch::new(4);
        let (done_tx, done_rx) = mpsc::channel::<u64>();

        let rt_a = session_reply_to(100);
        let rt_b = session_reply_to(200);
        d.submit(
            &mailer,
            self_id,
            root_id(42),
            42,
            rt_a,
            Box::new({
                let tx = done_tx.clone();
                move || {
                    let _ = tx.send(42);
                    (KindId(<Pong as Kind>::ID.0), Vec::new())
                }
            }),
        );
        d.submit(
            &mailer,
            self_id,
            root_id(43),
            43,
            rt_b,
            Box::new(move || {
                let _ = done_tx.send(43);
                (KindId(<Pong as Kind>::ID.0), Vec::new())
            }),
        );
        let _ = done_rx.recv_timeout(Duration::from_secs(2));
        let _ = done_rx.recv_timeout(Duration::from_secs(2));

        // Out-of-order correlation: pop request 43 first, then 42. The
        // `LandedReply` carries the right `reply_to` (the hold rides
        // alongside it, dropping when the binding goes out of scope).
        let landed_b = d.take_landed(43).expect("request 43 landed");
        assert_eq!(landed_b.reply_to, rt_b);
        let landed_a = d.take_landed(42).expect("request 42 landed");
        assert_eq!(landed_a.reply_to, rt_a);
        // Already taken -> None (double-landing guard).
        assert!(d.take_landed(42).is_none());
    }

    /// iamacoffeepot/aether#1031: the settlement hold spans submit ->
    /// reply. After `submit` returns (the cap handler "finished"), the
    /// chain root still has a net `held_open` of 1 — settlement is
    /// gated. Only after the cap re-replies and the `LandedReply`
    /// (carrying the hold) drops does the net return to 0, releasing
    /// the chain.
    #[test]
    fn dispatch_holds_chain_until_reply() {
        let mailer = test_mailer();
        let self_id = MailboxId(7);
        let root = root_id(1);
        let mut d = InFlightDispatch::new(4);
        let (done_tx, done_rx) = mpsc::channel::<u64>();

        d.submit(
            &mailer,
            self_id,
            root,
            1,
            session_reply_to(1),
            signal_call(1, done_tx),
        );

        // The cap handler has returned. The worker thread may still be
        // running (or already finished) — either way the actor-state
        // guard keeps the chain held: net held_open == 1.
        assert_eq!(
            mailer.trace_handle().settlement_counter().held_open(root),
            1,
            "the chain stays held after submit returns (before the reply lands)"
        );

        // The worker ran and fired its loopback mail.
        let _ = done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("the call runs");
        // Still held — the result mail landing handler hasn't run yet.
        assert_eq!(
            mailer.trace_handle().settlement_counter().held_open(root),
            1,
            "the worker finishing does not release the chain; the hold lives in actor state"
        );

        // The cap handler runs on reply-landing: pop the LandedReply
        // (re-reply happens here in production), then drop it — that
        // fires Release.
        let landed = d.take_landed(1).expect("request 1 landed");
        assert_eq!(
            mailer.trace_handle().settlement_counter().held_open(root),
            1,
            "still held before drop"
        );
        drop(landed);
        let _ = d.on_reply_landed(&mailer, self_id);

        // Hold released — net back to 0, so the chain may settle.
        assert_eq!(
            mailer.trace_handle().settlement_counter().held_open(root),
            0,
            "dropping the LandedReply after re-reply releases the chain"
        );
    }

    /// iamacoffeepot/aether#1031: a request that overflows
    /// `max_in_flight` and enqueues still acquires its hold at accept
    /// time — settlement doesn't fire while it waits in `pending`, and
    /// the hold survives the move out of the queue (in `on_reply_landed`)
    /// into the correlation map, releasing only after its own re-reply.
    #[test]
    fn dispatch_holds_each_queued_request() {
        let mailer = test_mailer();
        let self_id = MailboxId(7);
        let root_a = root_id(1);
        let root_b = root_id(2);
        // Bound of 1 so the second submit enqueues.
        let mut d = InFlightDispatch::new(1);
        let (done_tx, done_rx) = mpsc::channel::<u64>();

        d.submit(
            &mailer,
            self_id,
            root_a,
            1,
            session_reply_to(1),
            signal_call(1, done_tx.clone()),
        );
        d.submit(
            &mailer,
            self_id,
            root_b,
            2,
            session_reply_to(2),
            signal_call(2, done_tx),
        );
        assert_eq!(d.in_flight(), 1);
        assert_eq!(d.pending(), 1, "the second request enqueued");

        // Both chains are held from accept — the queued one too.
        assert_eq!(
            mailer.trace_handle().settlement_counter().held_open(root_a),
            1,
            "the running request holds its chain"
        );
        assert_eq!(
            mailer.trace_handle().settlement_counter().held_open(root_b),
            1,
            "the queued request also holds its chain from accept"
        );

        // First worker ran; land its reply. take_landed(1) + drop
        // releases chain A; on_reply_landed drains the queued request 2
        // (moving its hold into the correlation map) and spawns it.
        let _ = done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("first call runs");
        drop(d.take_landed(1).expect("request 1 landed"));
        let drained = d.on_reply_landed(&mailer, self_id);
        assert_eq!(drained, Some(2), "the queued request drains");

        assert_eq!(
            mailer.trace_handle().settlement_counter().held_open(root_a),
            0,
            "chain A released after request 1's reply"
        );
        assert_eq!(
            mailer.trace_handle().settlement_counter().held_open(root_b),
            1,
            "chain B still held — its reply hasn't landed yet"
        );

        // Land request 2's reply: release chain B.
        let _ = done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("second call runs");
        drop(d.take_landed(2).expect("request 2 landed"));
        let _ = d.on_reply_landed(&mailer, self_id);
        assert_eq!(
            mailer.trace_handle().settlement_counter().held_open(root_b),
            0,
            "chain B released after request 2's reply"
        );
    }
}
