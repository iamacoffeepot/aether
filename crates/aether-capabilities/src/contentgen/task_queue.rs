//! Cap-level rate-limit + queue helper over the ADR-0093 hold-until-
//! resolve dispatch primitive (`NativeCtx::dispatch_blocking`).
//!
//! The content-gen caps make long-tail blocking provider calls
//! (multi-second image gen, the `claude` subprocess) that must not block
//! the single-threaded actor's mail intake. The substrate's
//! `dispatch_blocking` primitive (ADR-0093) owns the spawn + the hold
//! lifecycle + the completion routing; this helper adds only the one
//! thing the framework deliberately doesn't centralise: the per-cap
//! concurrency bound + pending queue that rate-limits the paid provider
//! endpoints (ADR-0050 §2).
//!
//! Under the bound, [`TaskQueue::submit`] hands the work straight to
//! `ctx.dispatch_blocking`. Over the bound, it captures the chain context
//! *now* — a [`SettlementHold`](aether_substrate::runtime::trace::SettlementHold)
//! on the current root plus the originating reply target — and buffers a
//! thunk that, when a slot later frees, replays the work via
//! `ctx.dispatch_blocking_resumed(hold, reply_to, work)` so the deferred
//! request keeps *its own* chain held and replies to *its own* caller
//! (iamacoffeepot/aether#1031). [`TaskQueue::on_complete`], called from
//! the cap's `#[handler(task)]` after `resolve`, frees the slot and hands
//! it straight to the next buffered task.
//!
//! Everything `InFlightDispatch` used to own beyond the slot count + the
//! pending queue — the `request_id` correlation map, the hold accounting,
//! the raw worker spawn — now lives in the substrate's in-flight ledger
//! (ADR-0093 §2). This helper owns only the slot count, the pending
//! queue, and the hold-handoff.

use std::collections::VecDeque;

use aether_data::Kind;
use aether_substrate::actor::native::NativeCtx;

/// Default per-cap concurrency bound when a cap doesn't override it.
/// Doubles as rate-limit throttling for the paid provider endpoints
/// (ADR-0050 §2) — at most this many provider calls run concurrently;
/// the rest queue.
pub const DEFAULT_MAX_IN_FLIGHT: usize = 4;

/// A buffered dispatch thunk: replays an over-bound request via
/// `dispatch_blocking_resumed` when a slot frees. Built on the actor
/// thread and run on the actor thread, so the actor IS the mutual
/// exclusion — but the thunk is `Send` so the embedding cap (a
/// `NativeActor`, which is `Send + 'static`) can hold the queue in its
/// state. Everything the thunk closes over (`work`, the captured
/// `SettlementHold`, the `Source`) is already `Send`.
type PendingDispatch = Box<dyn FnOnce(&mut NativeCtx<'_>) + Send>;

/// Cap-level rate-limit + queue over the substrate's hold-until-resolve
/// dispatch (ADR-0093). Lives in the cap's plain (lock-free) actor state;
/// every method runs on the single-threaded dispatcher (the actor IS the
/// mutual exclusion — no `Semaphore`, no `Mutex`).
pub struct TaskQueue {
    max: usize,
    in_flight: usize,
    pending: VecDeque<PendingDispatch>,
}

impl TaskQueue {
    /// Build a queue bounded at `max` concurrent provider calls. A `max`
    /// of 0 is clamped to 1 — a zero bound would queue forever.
    #[must_use]
    pub fn new(max: usize) -> Self {
        Self {
            max: max.max(1),
            in_flight: 0,
            pending: VecDeque::new(),
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

    /// Accept a unit of blocking work. Under the bound, dispatch it now
    /// via [`NativeCtx::dispatch_blocking`] (which acquires the hold +
    /// reply target from `ctx`). Over the bound, capture the chain
    /// context *now* — a [`SettlementHold`](aether_substrate::runtime::trace::SettlementHold)
    /// on the current root plus this handler's reply target — and buffer
    /// a thunk that replays the work via
    /// [`NativeCtx::dispatch_blocking_resumed`] when a slot later frees,
    /// so the deferred dispatch keeps *this* chain held and replies to
    /// *this* caller (iamacoffeepot/aether#1031).
    pub fn submit<O, F>(&mut self, ctx: &mut NativeCtx<'_>, work: F)
    where
        O: Kind + serde::Serialize + Send + 'static,
        F: FnOnce() -> O + Send + 'static,
    {
        if self.in_flight < self.max {
            ctx.dispatch_blocking(work);
            self.in_flight += 1;
        } else {
            // Capture the hold + reply target at accept time so the
            // buffered request stays held from accept -> its eventual
            // re-reply, exactly like the immediate path's `Finished` is
            // preceded by `HoldOpen`.
            let hold = ctx.acquire_settlement_hold();
            let reply_to = ctx.reply_target();
            self.pending
                .push_back(Box::new(move |ctx: &mut NativeCtx<'_>| {
                    ctx.dispatch_blocking_resumed(hold, reply_to, work);
                }));
        }
    }

    /// Call from the cap's `#[handler(task)]` after `resolve`. Frees the
    /// completed slot, handing it straight to the next buffered task if
    /// there is one (so `in_flight` is unchanged on a drain) or
    /// decrementing the count when the queue is empty.
    pub fn on_complete(&mut self, ctx: &mut NativeCtx<'_>) {
        match self.pending.pop_front() {
            Some(next) => next(ctx),
            None => self.in_flight = self.in_flight.saturating_sub(1),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::TaskQueue;
    use aether_data::{Kind, KindId, MailId, MailboxId, Source, SourceAddr, mailbox_id_from_name};
    use aether_substrate::actor::native::binding::NativeBinding;
    use aether_substrate::actor::native::ctx::NativeCtx;
    use std::sync::Arc;

    use crate::test_chassis::fresh_substrate;

    /// A `#[repr(C)]` `Pod` reply kind the worker produces and `resolve`
    /// re-replies. Hand-rolled `Kind` (cast-shape) so the tests don't
    /// depend on the kind inventory.
    #[repr(C)]
    #[derive(
        Copy,
        Clone,
        Debug,
        PartialEq,
        Eq,
        bytemuck::Pod,
        bytemuck::Zeroable,
        serde::Serialize,
        serde::Deserialize,
    )]
    struct Answer {
        value: u64,
    }

    impl Kind for Answer {
        const NAME: &'static str = "test.task_queue.answer";
        const ID: KindId = KindId(0xD15B_0CC2_0000_0001);
        aether_data::pod_kind_codec!();
    }

    /// A synthetic chain root for a request — the value the cap handler
    /// would read from `ctx.in_flight_root()`. Distinct per `cid` so a
    /// multi-request test keeps each chain's hold accounting separate.
    fn root_id(cid: u64) -> MailId {
        MailId {
            sender: MailboxId(1),
            correlation_id: cid,
        }
    }

    fn session_reply_to(corr: u64) -> Source {
        Source::with_correlation(
            SourceAddr::Session(aether_data::SessionToken(aether_data::Uuid::nil())),
            corr,
        )
    }

    #[test]
    fn new_clamps_zero_bound_to_one() {
        let q = TaskQueue::new(0);
        assert_eq!(q.in_flight(), 0);
        assert_eq!(
            q.max, 1,
            "a zero bound clamps to 1 so the first submit dispatches"
        );
    }

    /// Under the bound `submit` dispatches immediately (in-flight bumps,
    /// nothing queues); over the bound it buffers (in-flight pinned at the
    /// bound, the surplus lands in `pending`).
    #[test]
    fn submit_under_bound_dispatches_and_overflow_queues() {
        let (registry, mailer) = fresh_substrate();
        let actor_mailbox = mailbox_id_from_name("test.task_queue.actor");
        // Register a sink for the worker's completion-wake push so it
        // routes to a real inbox rather than warn-dropping.
        registry.register_inbox("test.task_queue.actor", Arc::new(|_d| {}));
        let binding = Arc::new(NativeBinding::new_for_test(
            Arc::clone(&mailer),
            actor_mailbox,
        ));

        let mut q = TaskQueue::new(2);
        // Three submits against a bound of 2: two dispatch, one queues.
        for cid in 1..=3 {
            let mut ctx =
                NativeCtx::new(&binding, session_reply_to(cid), MailId::NONE, root_id(cid));
            q.submit(&mut ctx, move || Answer { value: cid });
        }
        assert_eq!(q.in_flight(), 2, "two dispatched under the bound of 2");
        assert_eq!(q.pending(), 1, "the third request queued");
    }

    /// The queued request's chain is held from accept (its
    /// `acquire_settlement_hold` at `submit` time), and `on_complete`
    /// drains it: a buffered request dispatches when a slot frees, with
    /// `in_flight` unchanged.
    #[test]
    fn on_complete_drains_pending_and_holds_queued_chain() {
        let (registry, mailer) = fresh_substrate();
        let counter = Arc::clone(mailer.trace_handle().settlement_counter());
        let actor_mailbox = mailbox_id_from_name("test.task_queue.actor2");
        registry.register_inbox("test.task_queue.actor2", Arc::new(|_d| {}));
        let binding = Arc::new(NativeBinding::new_for_test(
            Arc::clone(&mailer),
            actor_mailbox,
        ));

        // Bound of 1 so the second submit queues.
        let mut q = TaskQueue::new(1);
        let root_a = root_id(1);
        let root_b = root_id(2);
        {
            let mut ctx = NativeCtx::new(&binding, session_reply_to(1), MailId::NONE, root_a);
            q.submit(&mut ctx, || Answer { value: 1 });
        }
        {
            let mut ctx = NativeCtx::new(&binding, session_reply_to(2), MailId::NONE, root_b);
            q.submit(&mut ctx, || Answer { value: 2 });
        }
        assert_eq!(q.in_flight(), 1);
        assert_eq!(q.pending(), 1, "the second request queued");
        assert_eq!(
            counter.held_open(root_b),
            1,
            "the queued request holds its chain from accept (iamacoffeepot/aether#1031)"
        );

        // First completion: drains the queued request, dispatching it.
        // `in_flight` stays 1 (one freed, one dispatched), pending empties.
        {
            let mut ctx = NativeCtx::new(&binding, Source::NONE, MailId::NONE, MailId::NONE);
            q.on_complete(&mut ctx);
        }
        assert_eq!(
            q.in_flight(),
            1,
            "one freed, one drained -> still 1 in flight"
        );
        assert_eq!(q.pending(), 0);
        assert_eq!(
            counter.held_open(root_b),
            1,
            "the drained request's chain stays held until its own completion resolves"
        );

        // Second completion: nothing queued, so the slot frees.
        {
            let mut ctx = NativeCtx::new(&binding, Source::NONE, MailId::NONE, MailId::NONE);
            q.on_complete(&mut ctx);
        }
        assert_eq!(
            q.in_flight(),
            0,
            "in-flight returns to 0 once the queue is empty"
        );
    }
}
