//! Per-sender bounded async egress dispatch (ADR-0158).
//!
//! The `aether.http` client no longer runs one fetch at a time on the
//! dispatcher thread. [`PerSenderEgress`] composes a per-sender `(in_flight,
//! pending)` table over the ADR-0093 hold-until-resolve dispatch primitives
//! (`NativeCtx::dispatch_blocking_with` / `dispatch_blocking_resumed_with`,
//! the same "bound-and-hold machinery" `TaskQueue` uses), adding the two
//! things a single flat queue cannot express: a **per-sender** budget
//! (fairness — one noisy sender cannot starve its peers) and a **global**
//! ceiling (protection — a fan-out of distinct senders cannot exhaust the
//! host's worker-thread and socket budget). A fetch dispatches only when it
//! clears both bounds; otherwise it queues, holding its chain from accept.
//!
//! A queued fetch captures its chain context *now* — a `SettlementHold` on
//! the current root plus the originating reply target — and buffers a thunk
//! that replays the work via `dispatch_blocking_resumed_with` when a slot
//! frees, exactly like `TaskQueue::submit` (iamacoffeepot/aether#1031). The
//! sender's `MailboxId` rides through as the dispatch context, so the cap's
//! `#[handler(task)]` completion reads it off the `TaskDone` and frees the
//! right sender's slot.
//!
//! Entries reclaim on idle (ADR-0158 §5): a per-sender entry is created
//! lazily on a sender's first submit and removed the moment it drains fully
//! idle (`in_flight == 0` and `pending` empty). A `SettlementHold` exists
//! only while a request is in flight or buffered pending a slot, so an entry
//! that holds anything is never idle — idle-reclamation can never drop a hold
//! on the floor.

use std::collections::{HashMap, VecDeque};

use aether_actor::ReplyMode;
use aether_data::{Kind, MailboxId, Source, SourceAddr};
use aether_substrate::actor::native::NativeCtx;

/// A buffered fetch: replays an over-bound request via
/// `dispatch_blocking_resumed_with` when a slot frees. Built and run on the
/// actor thread (the actor IS the mutual exclusion), but `Send` so the
/// embedding cap can hold it in its `NativeActor` state. Everything it closes
/// over (the work closure, the captured `SettlementHold`, the reply `Source`,
/// the sender key) is already `Send`.
type PendingFetch = Box<dyn FnOnce(&mut NativeCtx<'_>) + Send>;

/// One sender's egress state: how many of its fetches are running, and the
/// FIFO of its requests waiting for a slot.
#[derive(Default)]
struct SenderEntry {
    in_flight: usize,
    pending: VecDeque<PendingFetch>,
}

/// Read the sender's `MailboxId` from a mail envelope's reply target — the
/// per-sender table key (ADR-0158 §2). A component sender comes through as
/// `SourceAddr::EngineMailbox { mailbox_id }` and keys on its own id; every
/// other source (MCP sessions, substrate-internal pushes) collapses to
/// `MailboxId(0)` and shares one bucket. Mirrors `aether_audio`'s
/// `sender_mailbox_id`, the same keying that isolates one sender's audio
/// state.
#[must_use]
pub fn sender_mailbox_id(sender: Source) -> MailboxId {
    match sender.addr {
        SourceAddr::EngineMailbox { mailbox_id, .. } => mailbox_id,
        _ => MailboxId(0),
    }
}

/// Per-sender bounded async egress dispatcher (ADR-0158). Lives in the cap's
/// plain (lock-free) actor state; every method runs on the single-threaded
/// dispatcher, so the actor IS the mutual exclusion — no `Semaphore`, no
/// `Mutex`.
pub struct PerSenderEgress {
    per_sender_max: usize,
    global_max: usize,
    global_in_flight: usize,
    senders: HashMap<MailboxId, SenderEntry>,
    /// Round-robin cursor over the senders that currently have pending work.
    /// A key is present iff its entry holds ≥1 pending request; admission
    /// rotates across it so a freed global slot does not always favor the
    /// sender whose completion freed it (ADR-0158 §3 drain fairness).
    waiting: VecDeque<MailboxId>,
}

impl PerSenderEgress {
    /// Build a dispatcher bounded at `per_sender_max` concurrent fetches per
    /// sender and `global_max` across all senders. Each `0` clamps to 1 —
    /// following `TaskQueue::new`'s clamp, a zero bound would queue forever
    /// (ADR-0158 §4).
    #[must_use]
    pub fn new(per_sender_max: usize, global_max: usize) -> Self {
        Self {
            per_sender_max: per_sender_max.max(1),
            global_max: global_max.max(1),
            global_in_flight: 0,
            senders: HashMap::new(),
            waiting: VecDeque::new(),
        }
    }

    /// Accept a fetch from `sender`. If the sender is under its per-sender
    /// budget **and** the global ceiling has room, dispatch `work` now via
    /// [`NativeCtx::dispatch_blocking_with`] (carrying `sender` as the
    /// completion context). Otherwise capture the chain context *now* — a
    /// `SettlementHold` on the current root plus this handler's reply target —
    /// and buffer a thunk that replays the work via
    /// [`NativeCtx::dispatch_blocking_resumed_with`] when a slot frees, so the
    /// queued fetch keeps *its own* chain held from accept through its
    /// eventual re-reply and replies to *its own* caller (ADR-0158 §2).
    pub fn submit<O, F, M>(&mut self, ctx: &mut NativeCtx<'_, M>, sender: MailboxId, work: F)
    where
        O: Kind + serde::Serialize + Send + 'static,
        F: FnOnce() -> O + Send + 'static,
        M: ReplyMode,
    {
        let global_room = self.global_in_flight < self.global_max;
        let per_sender_max = self.per_sender_max;
        let entry = self.senders.entry(sender).or_default();

        if entry.in_flight < per_sender_max && global_room {
            entry.in_flight += 1;
            self.global_in_flight += 1;
            ctx.dispatch_blocking_with(sender, work);
            return;
        }

        let hold = ctx.acquire_settlement_hold();
        let reply_to = ctx.reply_target();
        let was_empty = entry.pending.is_empty();
        entry.pending.push_back(Box::new(move |ctx: &mut NativeCtx<'_>| {
            ctx.dispatch_blocking_resumed_with::<O, _, _>(hold, reply_to, sender, work);
        }));
        if was_empty {
            self.waiting.push_back(sender);
        }
    }

    /// Call from the cap's `#[handler(task)]` after `resolve`, passing the
    /// completed fetch's `sender` (read off the `TaskDone` context). Frees the
    /// sender's slot and one global slot, admits the next waiting request
    /// (rotating fairly across senders), then reclaims the completing sender's
    /// entry if it drained fully idle.
    pub fn on_complete(&mut self, ctx: &mut NativeCtx<'_>, sender: MailboxId) {
        if let Some(entry) = self.senders.get_mut(&sender) {
            entry.in_flight = entry.in_flight.saturating_sub(1);
        }
        self.global_in_flight = self.global_in_flight.saturating_sub(1);

        self.admit_next(ctx);

        // Only the completing sender can be left idle here — admission never
        // idles anyone (it increments). Reclaim it if it now holds nothing.
        if let Some(entry) = self.senders.get(&sender)
            && entry.in_flight == 0
            && entry.pending.is_empty()
        {
            self.senders.remove(&sender);
        }
    }

    /// Admit at most one queued fetch — a completion frees exactly one global
    /// slot, so at most one request can newly dispatch. Rotate across the
    /// waiting senders to find the first that is under its per-sender budget,
    /// so a freed global slot is shared rather than recaptured by the busiest
    /// sender (ADR-0158 §3).
    fn admit_next(&mut self, ctx: &mut NativeCtx<'_>) {
        if self.global_in_flight >= self.global_max {
            return;
        }
        let per_sender_max = self.per_sender_max;

        // Scan the rotation at most once: keys blocked by their own
        // per-sender cap move to the back (still waiting), the first
        // admittable one dispatches and stops the scan.
        for _ in 0..self.waiting.len() {
            let Some(key) = self.waiting.pop_front() else {
                return;
            };
            let entry = self.senders.get_mut(&key).expect("a waiting key has a live entry");

            if entry.in_flight < per_sender_max {
                let thunk = entry.pending.pop_front().expect("a waiting key has a pending fetch");
                entry.in_flight += 1;
                let still_pending = !entry.pending.is_empty();
                self.global_in_flight += 1;
                if still_pending {
                    self.waiting.push_back(key);
                }
                thunk(ctx);
                return;
            }

            // At its per-sender cap: keep it waiting, rotated to the back.
            self.waiting.push_back(key);
        }
    }
}

/// Observational accessors used only by the unit tests to assert the
/// dispatcher's bookkeeping; the cap reads none of them in production.
#[cfg(test)]
impl PerSenderEgress {
    /// Total fetches running across all senders.
    fn global_in_flight(&self) -> usize {
        self.global_in_flight
    }

    /// A `sender`'s running-fetch count, `0` if it has no live entry.
    fn in_flight_for(&self, sender: MailboxId) -> usize {
        self.senders.get(&sender).map_or(0, |e| e.in_flight)
    }

    /// A `sender`'s queued-fetch count, `0` if it has no live entry.
    fn pending_for(&self, sender: MailboxId) -> usize {
        self.senders.get(&sender).map_or(0, |e| e.pending.len())
    }

    /// How many senders currently have a live entry (in flight or queued) —
    /// the table's size, which the bounds keep proportional to live work
    /// rather than cumulative request volume (ADR-0158 §5).
    fn tracked_senders(&self) -> usize {
        self.senders.len()
    }
}

#[cfg(test)]
mod tests {
    // The test harness derives its own actor mailbox id by name so the
    // worker's completion-wake push routes to a registered inbox rather than
    // warn-dropping — fixture id derivation, not sibling-cap addressing.
    #![allow(clippy::disallowed_methods)]

    use super::PerSenderEgress;
    use aether_data::{Kind, KindId, MailId, MailboxId, Source, SourceAddr, mailbox_id_from_name};
    use aether_substrate::actor::native::binding::NativeBinding;
    use aether_substrate::actor::native::ctx::NativeCtx;
    use aether_substrate::testing::{boot_authority, fresh_substrate};
    use std::sync::Arc;

    /// A `#[repr(C)]` `Pod` reply kind the worker produces. Hand-rolled `Kind`
    /// (cast-shape) so the tests don't depend on the HTTP kind inventory.
    #[repr(C)]
    #[derive(
        Copy, Clone, Debug, PartialEq, Eq, bytemuck::Pod, bytemuck::Zeroable, serde::Serialize, serde::Deserialize,
    )]
    struct Answer {
        value: u64,
    }

    impl Kind for Answer {
        const NAME: &'static str = "test.egress.answer";
        const ID: KindId = KindId(0xE9E5_0CC2_0000_0001);
        aether_data::pod_kind_codec!();
    }

    /// A distinct chain root per request so a multi-request test keeps each
    /// chain's hold accounting separate — the value a cap handler reads from
    /// `ctx.in_flight_root()`.
    fn root_id(cid: u64) -> MailId {
        MailId { sender: MailboxId(1), correlation_id: cid }
    }

    fn session_reply_to(corr: u64) -> Source {
        Source::with_correlation(SourceAddr::Session(aether_data::SessionToken(aether_data::Uuid::nil())), corr)
    }

    /// Boot a fresh substrate + a binding whose self-mailbox is registered,
    /// so worker completion-wake pushes route to a real inbox.
    fn harness(tag: &str) -> Arc<NativeBinding> {
        let (registry, mailer) = fresh_substrate();
        let mailbox = mailbox_id_from_name(tag);
        registry.register_inbox(&boot_authority(), tag, Arc::new(|_d| {}));
        Arc::new(NativeBinding::new_for_test(mailer, mailbox))
    }

    fn submit(q: &mut PerSenderEgress, binding: &Arc<NativeBinding>, sender: MailboxId, cid: u64) {
        let mut ctx = NativeCtx::new(binding, session_reply_to(cid), MailId::NONE, root_id(cid));
        q.submit(&mut ctx, sender, move || Answer { value: cid });
    }

    fn complete(q: &mut PerSenderEgress, binding: &Arc<NativeBinding>, sender: MailboxId) {
        let mut ctx = NativeCtx::new(binding, Source::NONE, MailId::NONE, MailId::NONE);
        q.on_complete(&mut ctx, sender);
    }

    #[test]
    fn new_clamps_zero_bounds_to_one() {
        let q = PerSenderEgress::new(0, 0);
        assert_eq!(q.per_sender_max, 1, "a zero per-sender bound clamps to 1");
        assert_eq!(q.global_max, 1, "a zero global ceiling clamps to 1");
    }

    /// Under the per-sender budget `submit` dispatches immediately; over it the
    /// surplus queues, with `in_flight` pinned at the budget.
    #[test]
    fn over_per_sender_budget_queues() {
        let binding = harness("test.egress.per_sender");
        let sender = MailboxId(7);
        let mut q = PerSenderEgress::new(2, 32);
        for cid in 1..=3 {
            submit(&mut q, &binding, sender, cid);
        }
        assert_eq!(q.in_flight_for(sender), 2, "two dispatched under the per-sender budget of 2");
        assert_eq!(q.pending_for(sender), 1, "the third queued");
        assert_eq!(q.global_in_flight(), 2);
    }

    /// A queued fetch dispatches when a slot frees — the drain path. `in_flight`
    /// is unchanged across the drain (one freed, one dispatched).
    #[test]
    fn on_complete_drains_the_queue() {
        let binding = harness("test.egress.drain");
        let sender = MailboxId(7);
        let mut q = PerSenderEgress::new(1, 32);
        submit(&mut q, &binding, sender, 1);
        submit(&mut q, &binding, sender, 2);
        assert_eq!(q.in_flight_for(sender), 1);
        assert_eq!(q.pending_for(sender), 1, "the second request queued behind the budget of 1");

        complete(&mut q, &binding, sender);
        assert_eq!(q.in_flight_for(sender), 1, "one freed, one drained -> still 1 in flight");
        assert_eq!(q.pending_for(sender), 0, "the queued request dispatched");
    }

    /// One sender saturating its budget does not delay another sender's fetch:
    /// B dispatches immediately while A's surplus queues (ADR-0158 §2 fairness).
    #[test]
    fn per_sender_isolation() {
        let binding = harness("test.egress.isolation");
        let a = MailboxId(1);
        let b = MailboxId(2);
        let mut q = PerSenderEgress::new(2, 32);
        // A fills and overruns its budget.
        for cid in 1..=3 {
            submit(&mut q, &binding, a, cid);
        }
        // B's request arrives while A is over budget.
        submit(&mut q, &binding, b, 10);

        assert_eq!(q.in_flight_for(a), 2, "A pinned at its budget");
        assert_eq!(q.pending_for(a), 1, "A's surplus queued");
        assert_eq!(q.in_flight_for(b), 1, "B dispatched immediately, unaffected by A");
        assert_eq!(q.pending_for(b), 0);
    }

    /// The global ceiling gates a fetch even when its sender is under its own
    /// budget: two senders each under a per-sender budget of 4 still queue once
    /// the global ceiling of 2 is reached (ADR-0158 §3 protection).
    #[test]
    fn global_ceiling_gates_under_per_sender_budget() {
        let binding = harness("test.egress.global");
        let a = MailboxId(1);
        let b = MailboxId(2);
        let mut q = PerSenderEgress::new(4, 2);
        submit(&mut q, &binding, a, 1);
        submit(&mut q, &binding, b, 2);
        // Global ceiling now full at 2; A is at 1/4 per-sender, but the ceiling
        // forces the next fetch to queue.
        submit(&mut q, &binding, a, 3);

        assert_eq!(q.global_in_flight(), 2, "global ceiling holds at 2");
        assert_eq!(q.in_flight_for(a), 1, "A under its own budget but ceiling-gated");
        assert_eq!(q.pending_for(a), 1, "A's second fetch queued on the global ceiling");
    }

    /// A completion at the global ceiling rotates admission across waiting
    /// senders rather than always readmitting the sender whose completion freed
    /// the slot (ADR-0158 §3 drain fairness).
    #[test]
    fn drain_rotates_across_senders_at_the_ceiling() {
        let binding = harness("test.egress.rotate");
        let a = MailboxId(1);
        let b = MailboxId(2);
        // Per-sender budget 4 (never the binding constraint here), global 2.
        let mut q = PerSenderEgress::new(4, 2);
        submit(&mut q, &binding, a, 1); // A in flight
        submit(&mut q, &binding, b, 2); // B in flight; ceiling full
        submit(&mut q, &binding, a, 3); // A queued (ceiling)
        submit(&mut q, &binding, b, 4); // B queued (ceiling)
        assert_eq!(q.pending_for(a), 1);
        assert_eq!(q.pending_for(b), 1);

        // A completes, freeing one global slot. Both A and B are under their
        // per-sender budgets, so fairness must admit A (the front of the
        // rotation) — but crucially it must NOT then also admit B: only one
        // global slot freed.
        complete(&mut q, &binding, a);
        assert_eq!(q.global_in_flight(), 2, "exactly one admitted; ceiling still full");
        assert_eq!(q.pending_for(a), 0, "A's queued fetch admitted");
        assert_eq!(q.pending_for(b), 1, "B still waits — the freed slot was not double-spent");

        // B completes next; its own queued fetch is the only admittable one.
        complete(&mut q, &binding, b);
        assert_eq!(q.pending_for(b), 0, "B's queued fetch admitted when B freed a slot");
    }

    /// An entry reclaims the moment it drains fully idle, so the table's size
    /// tracks live senders rather than cumulative volume (ADR-0158 §5).
    #[test]
    fn idle_entry_reclaims() {
        let binding = harness("test.egress.reclaim");
        let sender = MailboxId(7);
        let mut q = PerSenderEgress::new(2, 32);
        submit(&mut q, &binding, sender, 1);
        assert_eq!(q.tracked_senders(), 1, "the entry is created lazily on first submit");

        complete(&mut q, &binding, sender);
        assert_eq!(q.tracked_senders(), 0, "the entry is removed once it drains idle");
        assert_eq!(q.in_flight_for(sender), 0);
    }

    /// The load-bearing property (ADR-0158 §8): a queued fetch's chain is held
    /// from accept, and the drain keeps it held until its own reply — so
    /// `send_mail_traced` never observes the chain settle before the queued
    /// fetch dispatches. Mirrors `task_queue.rs`'s `held_open` assertion.
    #[test]
    fn queued_fetch_holds_its_chain_until_reply() {
        let (registry, mailer) = fresh_substrate();
        let counter = Arc::clone(mailer.trace_handle().settlement_counter());
        let mailbox = mailbox_id_from_name("test.egress.hold");
        registry.register_inbox(&boot_authority(), "test.egress.hold", Arc::new(|_d| {}));
        let binding = Arc::new(NativeBinding::new_for_test(mailer, mailbox));

        let sender = MailboxId(7);
        let mut q = PerSenderEgress::new(1, 32);
        let root_a = root_id(1);
        let root_b = root_id(2);
        {
            let mut ctx = NativeCtx::new(&binding, session_reply_to(1), MailId::NONE, root_a);
            q.submit(&mut ctx, sender, || Answer { value: 1 });
        }
        {
            let mut ctx = NativeCtx::new(&binding, session_reply_to(2), MailId::NONE, root_b);
            q.submit(&mut ctx, sender, || Answer { value: 2 });
        }
        assert_eq!(q.pending_for(sender), 1, "the second request queued behind the budget of 1");
        assert_eq!(counter.held_open(root_b), 1, "the queued fetch holds its own chain from accept (ADR-0158 §8)");

        // The first fetch completes and drains the queued one. Its chain must
        // stay held until its own completion resolves, not settle at the drain.
        complete(&mut q, &binding, sender);
        assert_eq!(q.pending_for(sender), 0);
        assert_eq!(counter.held_open(root_b), 1, "the drained fetch's chain stays held until its own reply resolves");
    }
}
