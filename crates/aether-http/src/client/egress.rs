//! Per-sender bounded async egress dispatch (ADR-0158).
//!
//! The `aether.http` client no longer runs one fetch at a time on the
//! dispatcher thread. [`PerSenderEgress`] composes a per-sender `(in_flight,
//! pending)` table over staged blocking work (`NativeCtx::hold` /
//! `stage_blocking`, ADR-0243 §9, the same "bound-and-hold machinery"
//! `TaskQueue` uses), adding the two things a single flat queue cannot
//! express: a **per-sender** budget (fairness — one noisy sender cannot
//! starve its peers) and a **global** ceiling (protection — a fan-out of
//! distinct senders cannot exhaust the host's worker-thread and socket
//! budget). A fetch starts only when it clears both bounds; otherwise it
//! queues, holding its chain from accept.
//!
//! Every fetch holds its reply with `ctx.hold` and stages its task with
//! `ctx.stage_blocking` in its own request's turn, so the task takes that
//! request's chain whether it starts at once or when a slot frees, exactly
//! like `TaskQueue::submit` (iamacoffeepot/aether#1031). The dispatcher
//! keeps each running fetch's `Held` and sender key under its task's
//! request, and the cap's `#[handler(task)]` completion finds them by the
//! request its wake is correlated to, answers the fetch, and frees the
//! right sender's slot.
//!
//! Entries reclaim on idle (ADR-0158 §5): a per-sender entry is created
//! lazily on a sender's first submit and removed the moment it drains fully
//! idle (`in_flight == 0` and `pending` empty). A held reply exists only
//! while a request is in flight or buffered pending a slot, so an entry that
//! holds anything is never idle — idle-reclamation can never drop a hold on
//! the floor. Before the dispatcher drops with the actor's state, an actor
//! close while the engine keeps running answers every held reply with its
//! `R::unanswered()`, an engine teardown settles them silently, and either
//! releases every unstarted task (ADR-0243 §1).

use std::collections::{HashMap, VecDeque};

use aether_actor::{ErasedActorRef, HeldReply, ReplyMode};
use aether_data::{ActorMail, RequestId};
use aether_substrate::actor::native::{Held, NativeCtx, Pending, StagedTask, TaskDone};

/// A queued fetch: its task, staged in its own request's turn, the reply it
/// owes, and the work its task runs once started. Built and run on the
/// actor thread (the actor IS the mutual exclusion), but `Send` so the
/// embedding cap can hold it in its `NativeActor` state.
struct Queued<R: ActorMail> {
    task: StagedTask<R>,
    held: Held<R>,
    work: Box<dyn FnOnce() -> R + Send>,
}

/// One sender's egress state: how many of its fetches are running, and the
/// FIFO of its requests waiting for a slot.
struct SenderEntry<R: ActorMail> {
    in_flight: usize,
    pending: VecDeque<Queued<R>>,
}

impl<R: ActorMail> Default for SenderEntry<R> {
    fn default() -> Self {
        Self { in_flight: 0, pending: VecDeque::new() }
    }
}

/// Per-sender bounded async egress dispatcher (ADR-0158), answering each
/// fetch with one `R`. Lives in the cap's plain (lock-free) actor state;
/// every method runs on the single-threaded dispatcher, so the actor IS the
/// mutual exclusion — no `Semaphore`, no `Mutex`.
pub struct PerSenderEgress<R: ActorMail> {
    per_sender_max: usize,
    global_max: usize,
    /// The per-sender table (ADR-0158 §2), keyed by the proven envelope
    /// sender `ctx.sender()` (ADR-0230). A local component keys on its own
    /// proof; MCP sessions, remote engines, and substrate-internal pushes have
    /// no local sender and share the `None` bucket.
    senders: HashMap<Option<ErasedActorRef>, SenderEntry<R>>,
    /// Round-robin cursor over the senders that currently have pending work.
    /// A key is present iff its entry holds ≥1 pending request; admission
    /// rotates across it so a freed global slot does not always favor the
    /// sender whose completion freed it (ADR-0158 §3 drain fairness).
    waiting: VecDeque<Option<ErasedActorRef>>,
    /// Every running fetch's reply and sender key, keyed by its task's
    /// request. Its size is the global in-flight count.
    running: HashMap<RequestId, (Held<R>, Option<ErasedActorRef>)>,
}

impl<R: HeldReply + Send + 'static> PerSenderEgress<R> {
    /// Build a dispatcher bounded at `per_sender_max` concurrent fetches per
    /// sender and `global_max` across all senders. Each `0` clamps to 1 —
    /// following `TaskQueue::new`'s clamp, a zero bound would queue forever
    /// (ADR-0158 §4).
    #[must_use]
    pub fn new(per_sender_max: usize, global_max: usize) -> Self {
        Self {
            per_sender_max: per_sender_max.max(1),
            global_max: global_max.max(1),
            senders: HashMap::new(),
            waiting: VecDeque::new(),
            running: HashMap::new(),
        }
    }

    /// Accept a fetch from `sender` in its own turn: hold its reply and stage
    /// its task on its chain. If the sender is under its per-sender budget
    /// **and** the global ceiling has room, start the task now; otherwise
    /// queue it, and a later completion starts it when a slot frees
    /// (ADR-0158 §2). The returned receipt declares the handler's
    /// `-> Pending<R>` row.
    ///
    /// # Panics
    /// Takes this dispatch's one [`NativeCtx::hold`], so a handler that
    /// already holds a reply panics.
    pub fn submit<F, A, M>(
        &mut self,
        ctx: &mut NativeCtx<'_, A, M>,
        sender: Option<ErasedActorRef>,
        work: F,
    ) -> Pending<R>
    where
        F: FnOnce() -> R + Send + 'static,
        M: ReplyMode,
    {
        let (pending, held) = ctx.hold::<R>();
        let task = ctx.stage_blocking::<R>();
        let global_room = self.running.len() < self.global_max;
        let entry = self.senders.entry(sender).or_default();

        if entry.in_flight < self.per_sender_max && global_room {
            entry.in_flight += 1;
            self.running.insert(task.start(ctx, work), (held, sender));
            return pending;
        }

        let was_empty = entry.pending.is_empty();
        entry.pending.push_back(Queued { task, held, work: Box::new(work) });
        if was_empty {
            self.waiting.push_back(sender);
        }
        pending
    }

    /// The cap's `#[handler(task)]` body: answer the finished fetch with its
    /// output, free its sender's slot and one global slot, admit the next
    /// waiting request (rotating fairly across senders), then reclaim the
    /// completing sender's entry if it drained fully idle.
    ///
    /// # Panics
    /// Panics when `ctx` is not dispatching the completion of a fetch this
    /// dispatcher started.
    pub fn complete<A>(&mut self, ctx: &mut NativeCtx<'_, A>, done: TaskDone<R>) {
        let (held, sender) = ctx
            .in_reply_to()
            .and_then(|request| self.running.remove(&request))
            .expect("an egress completion names a fetch the dispatcher started");
        held.answer(ctx, &done.into_output());
        if let Some(entry) = self.senders.get_mut(&sender) {
            entry.in_flight = entry.in_flight.saturating_sub(1);
        }

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
    /// slot, so at most one request can newly start. Rotate across the
    /// waiting senders to find the first that is under its per-sender budget,
    /// so a freed global slot is shared rather than recaptured by the busiest
    /// sender (ADR-0158 §3). The admitted task's chain is still its own
    /// request's, fixed when it was staged.
    fn admit_next<A>(&mut self, ctx: &NativeCtx<'_, A>) {
        if self.running.len() >= self.global_max {
            return;
        }

        // Scan the rotation at most once: keys blocked by their own
        // per-sender cap move to the back (still waiting), the first
        // admittable one starts and stops the scan.
        for _ in 0..self.waiting.len() {
            let Some(key) = self.waiting.pop_front() else {
                return;
            };
            let entry = self.senders.get_mut(&key).expect("a waiting key has a live entry");

            if entry.in_flight < self.per_sender_max {
                let Queued { task, held, work } = entry.pending.pop_front().expect("a waiting key has a pending fetch");
                entry.in_flight += 1;
                if !entry.pending.is_empty() {
                    self.waiting.push_back(key);
                }
                self.running.insert(task.start(ctx, work), (held, key));
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
impl<R: ActorMail> PerSenderEgress<R> {
    /// Total fetches running across all senders.
    fn global_in_flight(&self) -> usize {
        self.running.len()
    }

    /// A `sender`'s running-fetch count, `0` if it has no live entry.
    fn in_flight_for(&self, sender: Option<ErasedActorRef>) -> usize {
        self.senders.get(&sender).map_or(0, |e| e.in_flight)
    }

    /// A `sender`'s queued-fetch count, `0` if it has no live entry.
    fn pending_for(&self, sender: Option<ErasedActorRef>) -> usize {
        self.senders.get(&sender).map_or(0, |e| e.pending.len())
    }

    /// How many senders currently have a live entry (in flight or queued) —
    /// the table's size, which the bounds keep proportional to live work
    /// rather than cumulative request volume (ADR-0158 §5).
    fn tracked_senders(&self) -> usize {
        self.senders.len()
    }
}

/// The dispatcher driven through a real chassis: every fetch reaches a probe
/// actor through the production dispatcher, and every completion through the
/// wake its worker pushes. Each worker announces its start and then waits at
/// a gate the test opens, so the test decides when each fetch finishes.
#[cfg(test)]
mod tests {
    use super::PerSenderEgress;
    use aether_actor::{ErasedActorRef, HeldReply};
    use aether_data::{Kind, Source, SourceAddr};
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx, Pending, TaskDone};
    use aether_substrate::chassis::builder::PassiveChassis;
    use aether_substrate::chassis::error::BootError;
    use aether_substrate::mail::MailRef;
    use aether_substrate::mail::registry::{DispatchParts, MailboxEntry, OwnedDispatch, Registry};
    use aether_substrate::testing::{TestChassis, bare_substrate, boot_test_chassis_with, registered_ref};
    use std::collections::HashSet;
    use std::sync::mpsc::{self, Receiver, Sender};
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::Duration;

    /// How long a wait that must succeed may take before the test fails.
    const PATIENCE: Duration = Duration::from_secs(5);

    /// How long a test watches for something that must not happen.
    const QUIET: Duration = Duration::from_millis(200);

    #[aether_data::kind(name = "test.egress.fetch", copy)]
    struct Fetch {
        gate: u32,
    }

    #[aether_data::kind(name = "test.egress.fetched", copy)]
    struct Fetched {
        gate: u32,
    }

    // A sentinel: no test here closes the dispatcher while it owes a
    // `Fetched`.
    impl HeldReply for Fetched {
        fn unanswered() -> Self {
            Self { gate: u32::MAX }
        }
    }

    /// Report the dispatcher's bookkeeping, for the census's own sender.
    #[aether_data::kind(name = "test.egress.census", copy)]
    struct Census;

    /// The dispatcher's bookkeeping as one sender sees it.
    #[derive(Debug, PartialEq, Eq)]
    struct Counts {
        global_in_flight: usize,
        in_flight: usize,
        pending: usize,
        tracked_senders: usize,
    }

    /// Gates the probe's workers wait at, opened by number.
    #[derive(Clone, Default)]
    struct Gates(Arc<(Mutex<HashSet<u32>>, Condvar)>);

    impl Gates {
        fn open(&self, gate: u32) {
            let (open, opened) = &*self.0;
            open.lock().expect("gates lock").insert(gate);
            opened.notify_all();
        }

        fn pass(&self, gate: u32) {
            let (open, opened) = &*self.0;
            let open = open.lock().expect("gates lock");
            drop(opened.wait_while(open, |open| !open.contains(&gate)).expect("gates lock"));
        }
    }

    #[derive(Clone)]
    struct ProbeParams {
        per_sender_max: usize,
        global_max: usize,
        gates: Gates,
        started: Sender<u32>,
        counts: Sender<Counts>,
    }

    /// A cap whose fetches run through one [`PerSenderEgress`].
    struct EgressProbe {
        egress: PerSenderEgress<Fetched>,
        params: ProbeParams,
    }

    #[aether_actor::actor(root)]
    impl NativeActor for EgressProbe {
        type Config = ();
        type Params = ProbeParams;
        const NAMESPACE: &'static str = "test.egress.probe";

        fn init((): (), params: Self::Params, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self { egress: PerSenderEgress::new(params.per_sender_max, params.global_max), params })
        }

        #[aether_actor::handler::single]
        fn on_fetch(&mut self, ctx: &mut NativeCtx<'_>, fetch: Fetch) -> Pending<Fetched> {
            let sender = ctx.sender();
            let ProbeParams { gates, started, .. } = self.params.clone();
            self.egress.submit(ctx, sender, move || {
                let _ = started.send(fetch.gate);
                gates.pass(fetch.gate);
                Fetched { gate: fetch.gate }
            })
        }

        #[aether_actor::handler::single]
        fn on_census(&mut self, ctx: &mut NativeCtx<'_>, _census: Census) {
            let sender = ctx.sender();
            let _ = self.params.counts.send(Counts {
                global_in_flight: self.egress.global_in_flight(),
                in_flight: self.egress.in_flight_for(sender),
                pending: self.egress.pending_for(sender),
                tracked_senders: self.egress.tracked_senders(),
            });
        }

        #[aether_actor::handler(task)]
        fn on_fetched(&mut self, ctx: &mut NativeCtx<'_>, done: TaskDone<Fetched>) {
            self.egress.complete(ctx, done);
        }
    }

    /// A booted probe and the channels it reports through.
    struct Probe {
        registry: Arc<Registry>,
        chassis: PassiveChassis<TestChassis>,
        gates: Gates,
        started: Receiver<u32>,
        counts: Receiver<Counts>,
    }

    impl Probe {
        fn boot(per_sender_max: usize, global_max: usize) -> Self {
            let (registry, mailer) = bare_substrate();
            let gates = Gates::default();
            let (started_tx, started) = mpsc::channel();
            let (counts_tx, counts) = mpsc::channel();
            let params = ProbeParams {
                per_sender_max,
                global_max,
                gates: gates.clone(),
                started: started_tx,
                counts: counts_tx,
            };
            let chassis = boot_test_chassis_with::<EgressProbe>(&registry, &mailer, (), params);
            Self { registry, chassis, gates, started, counts }
        }

        /// Register a sender whose replies land on the returned channel.
        fn sender(&self, name: &str) -> (ErasedActorRef, Receiver<u32>) {
            let (tx, rx) = mpsc::channel();
            let sender = registered_ref(
                &self.registry,
                name,
                Arc::new(move |dispatch: OwnedDispatch| {
                    dispatch.discharge();
                    if let Some(Fetched { gate }) = Fetched::decode_from_bytes(dispatch.payload.bytes()) {
                        let _ = tx.send(gate);
                    }
                }),
            );
            (sender, rx)
        }

        /// Push `mail` to the probe as `sender` sends it.
        fn push<K: Kind>(&self, sender: ErasedActorRef, mail: &K) {
            let probe = self.chassis.actor_ref::<EgressProbe>().erase();
            let MailboxEntry::Inbox { handler, .. } = self.registry.entry(probe).expect("the probe is registered")
            else {
                panic!("expected the probe's inbox");
            };
            let source = Source::with_correlation(SourceAddr::Component(sender.id()), 1);
            let parts =
                DispatchParts { sender: source, ..DispatchParts::new(K::ID, MailRef::from(mail.encode_into_bytes())) };
            handler.enqueue(OwnedDispatch::disarmed(parts, probe));
        }

        fn fetch(&self, sender: ErasedActorRef, gate: u32) {
            self.push(sender, &Fetch { gate });
        }

        /// The dispatcher's bookkeeping as `sender` sees it, once every mail
        /// pushed before this call is handled.
        fn census(&self, sender: ErasedActorRef) -> Counts {
            self.push(sender, &Census);
            self.counts.recv_timeout(PATIENCE).expect("the probe answers a census")
        }

        /// The gates of the next `count` fetches to start, in any order.
        fn started(&self, count: usize) -> HashSet<u32> {
            (0..count).map(|_| self.started.recv_timeout(PATIENCE).expect("a fetch starts")).collect()
        }

        fn assert_none_starts(&self) {
            assert!(self.started.recv_timeout(QUIET).is_err(), "no further fetch starts");
        }
    }

    #[test]
    fn new_clamps_zero_bounds_to_one() {
        let q = PerSenderEgress::<Fetched>::new(0, 0);
        assert_eq!(q.per_sender_max, 1, "a zero per-sender bound clamps to 1");
        assert_eq!(q.global_max, 1, "a zero global ceiling clamps to 1");
    }

    /// Catches a per-sender budget that admits past its bound, and a
    /// completion that frees the slot without starting the sender's queued
    /// fetch.
    #[test]
    fn over_per_sender_budget_queues_until_a_slot_frees() {
        let probe = Probe::boot(2, 32);
        let (sender, replies) = probe.sender("test.egress.per_sender.sender");
        for gate in 1..=3 {
            probe.fetch(sender, gate);
        }
        assert_eq!(probe.started(2), HashSet::from([1, 2]), "two start under the per-sender budget of 2");
        probe.assert_none_starts();
        assert_eq!(
            probe.census(sender),
            Counts { global_in_flight: 2, in_flight: 2, pending: 1, tracked_senders: 1 },
            "the third fetch queued",
        );

        probe.gates.open(1);
        assert_eq!(replies.recv_timeout(PATIENCE), Ok(1), "the finished fetch answers its sender");
        assert_eq!(probe.started(1), HashSet::from([3]), "the freed slot starts the queued fetch");
        assert_eq!(
            probe.census(sender),
            Counts { global_in_flight: 2, in_flight: 2, pending: 0, tracked_senders: 1 },
            "one freed, one started: still 2 in flight",
        );
    }

    /// Catches one sender's surplus delaying another sender's fetch
    /// (ADR-0158 §2 fairness).
    #[test]
    fn per_sender_isolation() {
        let probe = Probe::boot(2, 32);
        let (a, _a_replies) = probe.sender("test.egress.isolation.a");
        let (b, _b_replies) = probe.sender("test.egress.isolation.b");
        for gate in 1..=3 {
            probe.fetch(a, gate);
        }
        probe.fetch(b, 10);

        assert_eq!(probe.started(3), HashSet::from([1, 2, 10]), "B starts at once while A's surplus waits");
        probe.assert_none_starts();
        assert_eq!(probe.census(a).pending, 1, "A's surplus queued");
        assert_eq!(probe.census(b).pending, 0);
    }

    /// Catches a global ceiling that a sender under its own budget can pass
    /// (ADR-0158 §3 protection).
    #[test]
    fn global_ceiling_gates_under_per_sender_budget() {
        let probe = Probe::boot(4, 2);
        let (a, _a_replies) = probe.sender("test.egress.global.a");
        let (b, _b_replies) = probe.sender("test.egress.global.b");
        probe.fetch(a, 1);
        probe.fetch(b, 2);
        probe.fetch(a, 3);

        assert_eq!(probe.started(2), HashSet::from([1, 2]));
        probe.assert_none_starts();
        assert_eq!(
            probe.census(a),
            Counts { global_in_flight: 2, in_flight: 1, pending: 1, tracked_senders: 2 },
            "A is under its own budget but waits on the ceiling",
        );

        probe.gates.open(2);
        assert_eq!(probe.started(1), HashSet::from([3]), "B's finish frees the ceiling slot A waits on");
    }

    /// Catches a drain that always readmits the sender whose completion freed
    /// the slot, or that spends one freed slot twice (ADR-0158 §3 drain
    /// fairness).
    #[test]
    fn drain_rotates_across_senders_at_the_ceiling() {
        let probe = Probe::boot(4, 2);
        let (a, _a_replies) = probe.sender("test.egress.rotate.a");
        let (b, _b_replies) = probe.sender("test.egress.rotate.b");
        probe.fetch(a, 1);
        probe.fetch(b, 2);
        probe.fetch(a, 3);
        probe.fetch(b, 4);
        assert_eq!(probe.started(2), HashSet::from([1, 2]), "the ceiling of 2 is full");

        probe.gates.open(1);
        assert_eq!(probe.started(1), HashSet::from([3]), "A, first in the rotation, takes the freed slot");
        probe.assert_none_starts();

        probe.gates.open(2);
        assert_eq!(probe.started(1), HashSet::from([4]), "B's queued fetch starts when B frees a slot");
    }

    /// Catches an entry that outlives its sender's last fetch, so the table
    /// grows with cumulative volume instead of live senders (ADR-0158 §5).
    #[test]
    fn idle_entry_reclaims() {
        let probe = Probe::boot(2, 32);
        let (sender, replies) = probe.sender("test.egress.reclaim.sender");
        probe.fetch(sender, 1);
        probe.started(1);
        assert_eq!(probe.census(sender).tracked_senders, 1, "the entry is created lazily on first submit");

        probe.gates.open(1);
        assert_eq!(replies.recv_timeout(PATIENCE), Ok(1));
        assert_eq!(
            probe.census(sender),
            Counts { global_in_flight: 0, in_flight: 0, pending: 0, tracked_senders: 0 },
            "the entry is removed once it drains idle",
        );
    }

    /// Catches a queued fetch started on the chain of the completion that
    /// freed its slot: the answered fetch's chain would then stay open until
    /// the queued fetch finished (ADR-0158 §8, ADR-0243 §9).
    #[test]
    fn a_queued_fetch_holds_only_its_own_chain() {
        let probe = Probe::boot(1, 32);
        let actor = probe.chassis.actor_ref::<EgressProbe>();
        let (_, first_settled) = probe.chassis.send_tracked(actor, &Fetch { gate: 1 }, None);
        let (_, second_settled) = probe.chassis.send_tracked(actor, &Fetch { gate: 2 }, None);
        assert_eq!(probe.started(1), HashSet::from([1]));

        probe.gates.open(1);
        assert_eq!(probe.started(1), HashSet::from([2]), "the first completion starts the queued fetch");
        first_settled.recv_timeout(PATIENCE).expect("the first fetch settles once it is answered");
        assert!(second_settled.recv_timeout(QUIET).is_err(), "the queued fetch holds its own chain from accept");

        probe.gates.open(2);
        second_settled.recv_timeout(PATIENCE).expect("the queued fetch settles once it is answered");
    }
}
