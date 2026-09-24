//! Reactor routing state: the per-seq phase, the restart phase, and the page buffer (ADR-0226 decision 5).
//!
//! [`Routing`] holds the routing half's `Heads` at the routing cursor `R`,
//! the reactor-set cache, the single outstanding routing page read, the
//! single parked `WatchHead`, barrier waiters, and the in-progress seq or
//! restart work. The per-seq lockstep (`N = R + 1`) lives in the sibling
//! modules as `impl ProgramCore` blocks over this one field.

mod activate;
mod batch;
mod claim;
mod deliver;
mod follow;
mod instance;
mod intents;
mod members;
mod restart;

use std::collections::{BTreeMap, VecDeque};

use aether_bloomery_kinds::{
    ActivationRejected, Detail, Digest, DriverRecord, Head, JournalEntry, OpaqueBytes, ReactorName, ReactorSet, SetHead,
};
use aether_bloomery_view::Heads;

use self::instance::Instance;
use self::intents::PlannedIntent;
use crate::core::{CallerId, EvaluateTicket, EventsTicket, PlannedRecord, StatusTicket, WarmTicket, WatchTicket};

/// Member heads selected at one prefix, each with its bound digest.
pub type Selection = BTreeMap<Head<OpaqueBytes>, Digest>;

/// Live digests, each with the heads it serves in head order.
pub type Served = BTreeMap<Digest, Vec<Head<OpaqueBytes>>>;

/// What the one outstanding routing page read is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoutingRead {
    /// The next entries to route.
    Steady,
    /// A warm page for the current activation.
    Warm,
    /// A catch-up page for the current activation, folded from its live-from.
    CatchUp,
    /// A restart warm page for the digest warming.
    RestartWarm,
}

/// The one outstanding routing page read.
#[derive(Debug)]
pub struct PendingRead {
    /// Ticket the page arrives under.
    pub ticket: EventsTicket,
    /// Boundary the read starts after.
    pub after: u64,
    /// What the page is for.
    pub purpose: RoutingRead,
}

/// One committed routing batch awaiting read-back.
#[derive(Debug)]
pub struct CommittedRouting {
    /// Records in append order, after swap resolution.
    pub records: Vec<DriverRecord>,
    /// First seq the batch occupies.
    pub start: u64,
}

/// One outstanding `Event` delivery.
#[derive(Debug, Clone, Copy)]
pub enum Delivery {
    /// Live `Event(N)` to one digest.
    Live {
        /// Digest evaluating `N`.
        digest: Digest,
    },
    /// An owed `Event(cause)` to the current activation's digest.
    CatchUp {
        /// Seq delivered.
        cause: u64,
    },
}

/// One outstanding `Warm` batch.
#[derive(Debug, Clone, Copy)]
pub struct WarmBatch {
    /// Digest being warmed.
    pub digest: Digest,
    /// First seq in the batch.
    pub first: u64,
    /// Last seq in the batch.
    pub last: u64,
}

/// Where one planned record sorts in `N`'s batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PlanOrder {
    /// Live evaluation records, in ascending digest order then intent order.
    Live {
        /// Evaluated digest.
        digest: Digest,
        /// Intent index within the reply, or the poisoned follow-up index.
        index: usize,
    },
    /// Activation records, in head order.
    Activation {
        /// Index into the seq's activation queue.
        head_index: usize,
        /// The record's place within the head's activation.
        step: ActivationStep,
    },
}

/// A record's place within one head's activation: catch-up records, then its terminal record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ActivationStep {
    /// A catch-up reply's record.
    CatchUp {
        /// Owed seq that caused it.
        cause: u64,
        /// Intent index within the reply.
        index: usize,
    },
    /// The head's `Activated` or `ActivationRejected`.
    Terminal,
}

/// One `SetHead` awaiting its destination check.
#[derive(Debug, Clone)]
pub struct PendingDestination {
    /// Trigger or catch-up seq causing the move or its failure.
    pub cause: u64,
    /// Bundle whose rule returned the intent.
    pub bundle: Digest,
    /// Reactor that returned the intent.
    pub reactor: ReactorName,
    /// The move to attempt.
    pub set_head: SetHead,
}

/// Per-seq phase for `N = R + 1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeqPhase {
    /// Live `Event(N)` fan-out outstanding, collecting replies.
    Evaluating,
    /// Serial activation of changed heads.
    Activating,
    /// All replies collected, checking destinations and queueing the batch.
    Planning,
}

/// One activation's phase.
#[derive(Debug)]
pub enum ActivationPhase {
    /// Waiting on the digest's shared read or load, issued by either role.
    Loading,
    /// Paging the journal and sending `Warm` batches through `live_from - 1`.
    Warming,
    /// Delivering owed `Event`s `live_from..=N` one at a time.
    CatchingUp(CatchUp),
}

/// Owed catch-up in progress.
#[derive(Debug)]
pub struct CatchUp {
    /// `Heads` taken at `live_from - 1` from the journal view's head history,
    /// then folded through the last buffered entry, resolving each owed seq's
    /// `CallProgram`s.
    pub scratch: Heads,
    /// Next owed seq to deliver.
    pub next: u64,
    /// Buffered entries not yet folded, trimmed to `N`.
    pub page: VecDeque<JournalEntry>,
}

/// The head being activated; its queue index is the seq's `activate_index`.
#[derive(Debug)]
pub struct ActivationWork {
    /// Head being activated.
    pub head: Head<OpaqueBytes>,
    /// Digest selected for the head.
    pub bundle: Digest,
    /// First seq the instance will evaluate live.
    pub live_from: u64,
    /// Current phase.
    pub phase: ActivationPhase,
}

/// Work for `N = R + 1`.
#[derive(Debug)]
pub struct SeqWork {
    /// Trigger seq.
    pub n: u64,
    /// Journal entry `N`.
    pub entry: JournalEntry,
    /// Selection at prefix `N-1`.
    pub prev: Selection,
    /// Live digests evaluating `N` and the heads each serves.
    pub live: Served,
    /// Current phase.
    pub phase: SeqPhase,
    /// Heads needing activation, in canonical head order.
    pub to_activate: Vec<(Head<OpaqueBytes>, Digest)>,
    /// Index of the head being activated, or of the next one to start.
    pub activate_index: usize,
    /// Activation in progress, if any.
    pub activation: Option<ActivationWork>,
    /// Accumulated plan in append order.
    pub order: BTreeMap<PlanOrder, PlannedRecord>,
    /// `SetHead`s awaiting destination checks, in plan order.
    pub destinations: BTreeMap<PlanOrder, PendingDestination>,
    /// The `SetHead` whose destination read is outstanding.
    pub checking: Option<(PlanOrder, PendingDestination)>,
}

impl SeqWork {
    /// Work for `n` with its live fan-out already sent.
    pub fn new(n: u64, entry: JournalEntry, prev: Selection, live: Served) -> Self {
        Self {
            n,
            entry,
            prev,
            live,
            phase: SeqPhase::Evaluating,
            to_activate: Vec::new(),
            activate_index: 0,
            activation: None,
            order: BTreeMap::new(),
            destinations: BTreeMap::new(),
            checking: None,
        }
    }

    /// Plan one reply's intents, keying each by its index within the reply.
    pub fn plan_reply(&mut self, key: impl Fn(usize) -> PlanOrder, planned: Vec<PlannedIntent>) {
        for (index, intent) in planned.into_iter().enumerate() {
            match intent {
                PlannedIntent::Ready(record) => {
                    self.order.insert(key(index), PlannedRecord::Ready(record));
                }
                PlannedIntent::Destination(pending) => {
                    self.destinations.insert(key(index), pending);
                }
            }
        }
    }

    /// Plan one record at `step` of the head being activated.
    pub fn plan_activation(&mut self, step: ActivationStep, record: PlannedRecord) {
        self.order.insert(PlanOrder::Activation { head_index: self.activate_index, step }, record);
    }

    /// Plan the head's terminal record and move on to the next head.
    pub fn finish_head(&mut self, record: DriverRecord) {
        self.plan_activation(ActivationStep::Terminal, PlannedRecord::Ready(record));
        self.activation = None;
        self.activate_index += 1;
    }

    /// Reject the head at `activate_index`, dropping its catch-up records and
    /// queued destination checks: the interval stays owed, so a later
    /// activation evaluates it again.
    pub fn reject_head(&mut self, head: Head<OpaqueBytes>, bundle: Digest, reason: Detail) {
        let head_index = self.activate_index;
        let kept = |order: &PlanOrder| !matches!(order, PlanOrder::Activation { head_index: indexed, .. } if *indexed == head_index);
        self.order.retain(|order, _| kept(order));
        self.destinations.retain(|order, _| kept(order));
        let record =
            DriverRecord::ActivationRejected { cause: self.n, record: ActivationRejected { head, bundle, reason } };
        self.finish_head(record);
    }
}

/// Restart phase through `W`.
#[derive(Debug)]
pub enum RestartPhase {
    /// Taking routing `Heads` at `W` from the head history without delivering.
    Folding,
    /// Serially warming each selected digest through `W`.
    Warming {
        /// Digests still to warm, with the heads each serves.
        queued: VecDeque<(Digest, Vec<Head<OpaqueBytes>>)>,
        /// The digest loading or warming, with the heads it serves.
        warming: Option<(Digest, Vec<Head<OpaqueBytes>>)>,
    },
}

/// Restart replay work through `W`.
#[derive(Debug)]
pub struct RestartWork {
    /// Restart point, the higher watermark.
    pub watermark: u64,
    /// Current phase.
    pub phase: RestartPhase,
    /// Heads whose digest failed to load or warm, with the digest and reason.
    pub failures: Vec<(Head<OpaqueBytes>, Digest, Detail)>,
}

/// Reactor routing state over the routing cursor `R`.
#[derive(Debug)]
pub struct Routing {
    /// Routing `Heads` at `R`, the last routed seq.
    pub heads: Heads,
    /// Reactor-set cache by digest: `None` selects nothing.
    pub sets: BTreeMap<Digest, Option<ReactorSet>>,
    /// Outstanding routing page read, if any.
    pub read: Option<PendingRead>,
    /// Buffered steady entries after `R`, trimmed to the journal-view cursor.
    pub page: VecDeque<JournalEntry>,
    /// Outstanding `WatchHead`, if any.
    pub watch: Option<WatchTicket>,
    /// Barrier waiters: caller and `through`.
    pub awaiters: Vec<(CallerId, u64)>,
    /// Derived records of the in-flight routing append, if any.
    pub appending: Option<Vec<DriverRecord>>,
    /// Committed routing batch awaiting read-back, if any.
    pub committed: Option<CommittedRouting>,
    /// Outstanding live and catch-up deliveries.
    pub deliveries: BTreeMap<EvaluateTicket, Delivery>,
    /// Outstanding warmup batches.
    pub warms: BTreeMap<WarmTicket, WarmBatch>,
    /// Outstanding status resyncs, by the digest queried.
    pub statuses: BTreeMap<StatusTicket, Digest>,
    /// Reactor instances by digest, each live, poisoned, or untrusted.
    pub instances: BTreeMap<Digest, Instance>,
    /// In-progress seq work, if any.
    pub current: Option<SeqWork>,
    /// In-progress restart work, if any.
    pub restart: Option<RestartWork>,
    /// Whether restart replay finished and live routing may proceed.
    pub started: bool,
}

impl Routing {
    /// Empty routing: cursor `0`, no cache, no reads, no watch, unstarted.
    pub fn new() -> Self {
        Self {
            heads: Heads::new(),
            sets: BTreeMap::new(),
            read: None,
            page: VecDeque::new(),
            watch: None,
            awaiters: Vec::new(),
            appending: None,
            committed: None,
            deliveries: BTreeMap::new(),
            warms: BTreeMap::new(),
            statuses: BTreeMap::new(),
            instances: BTreeMap::new(),
            current: None,
            restart: None,
            started: false,
        }
    }

    /// Routing cursor `R`, the last routed seq.
    pub fn cursor(&self) -> u64 {
        self.heads.cursor().0
    }

    /// The activation in progress, if any.
    pub fn activation(&self) -> Option<&ActivationWork> {
        self.current.as_ref()?.activation.as_ref()
    }

    /// The owed catch-up in progress, with the digest it delivers to.
    pub fn catch_up(&self) -> Option<(Digest, &CatchUp)> {
        match self.activation()? {
            ActivationWork { bundle, phase: ActivationPhase::CatchingUp(catch_up), .. } => Some((*bundle, catch_up)),
            _ => None,
        }
    }

    /// The digest whose shared read or load routing waits on, if any: the
    /// activation's bundle while its phase is `Loading`, or the restart's
    /// warming digest.
    pub fn awaited(&self) -> Option<Digest> {
        let loading = self
            .activation()
            .filter(|activation| matches!(activation.phase, ActivationPhase::Loading))
            .map(|activation| activation.bundle);
        if loading.is_some() {
            return loading;
        }
        self.restart.as_ref().and_then(|restart| match &restart.phase {
            RestartPhase::Warming { warming, .. } => warming.as_ref().map(|(digest, _)| *digest),
            RestartPhase::Folding => None,
        })
    }
}
