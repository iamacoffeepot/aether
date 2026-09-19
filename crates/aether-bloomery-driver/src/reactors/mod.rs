//! Reactor routing state: the per-seq phase, the restart phase, and the page buffer (ADR-0226 decision 5).
//!
//! [`Routing`] holds the routing half's `Heads` at the routing cursor `R`,
//! the reactor-set cache, the single outstanding routing page read, the
//! single parked `WatchHead`, barrier waiters, and the in-progress seq or
//! restart work. The per-seq lockstep (`N = R + 1`) lives in the sibling
//! modules as `impl ProgramCore` blocks over this one field.

mod activate;
mod batch;
mod deliver;
mod follow;
mod intents;
mod members;
mod restart;

use std::collections::{BTreeMap, VecDeque};

use aether_bloomery_kinds::{Digest, DriverRecord, Head, JournalEntry, OpaqueBytes, ReactorSet};
use aether_bloomery_view::Heads;

use crate::core::{
    ArtifactTicket, CallerId, EvaluateTicket, EventsTicket, PlannedRecord, StatusTicket, WarmTicket, WatchTicket,
};

/// Purpose of the one outstanding routing page read.
#[derive(Debug)]
pub enum RoutingRead {
    /// Steady entry fetch.
    Steady {
        /// Boundary the read started after.
        after: u64,
    },
    /// Warm paging for one activation.
    Warm {
        /// Boundary the read started after.
        after: u64,
        /// Digest being warmed.
        digest: Digest,
        /// Head the activation serves.
        head: Head<OpaqueBytes>,
        /// Trigger seq whose batch will carry the activation.
        trigger: u64,
        /// First seq the instance will evaluate live.
        live_from: u64,
    },
    /// Owed catch-up paging for one activation, folded from seq 1.
    CatchUp {
        /// Boundary the read started after.
        after: u64,
        /// Digest being caught up.
        digest: Digest,
        /// Head the activation serves.
        head: Head<OpaqueBytes>,
        /// Trigger seq whose batch will carry the activation.
        trigger: u64,
        /// First owed seq, delivered live.
        live_from: u64,
    },
    /// Restart fold-only replay through `W`.
    RestartFold {
        /// Boundary the read started after.
        after: u64,
        /// Restart point.
        watermark: u64,
    },
    /// Restart warm of one digest through `W`.
    RestartWarm {
        /// Boundary the read started after.
        after: u64,
        /// Digest being warmed.
        digest: Digest,
        /// Restart point.
        watermark: u64,
    },
}

/// One committed routing batch awaiting read-back.
#[derive(Debug)]
pub struct CommittedRouting {
    /// Trigger seq whose records this batch carries.
    pub trigger: u64,
    /// Final records in append order, after swap resolution.
    pub records: Vec<DriverRecord>,
    /// First seq the batch occupies.
    pub start: u64,
    /// Distinct live digests that evaluated the trigger, in digest order.
    pub live: Vec<Digest>,
}

/// Context for one outstanding live or catch-up evaluation.
#[derive(Debug)]
pub struct EvaluateContext {
    /// Digest being evaluated.
    pub digest: Digest,
    /// Trigger seq whose batch will carry the reply's records.
    pub trigger: u64,
    /// Event seq delivered (`N` for live, `k` for catch-up).
    pub cause: u64,
    /// Head served for catch-up, or `None` for live fan-out.
    pub head: Option<Head<OpaqueBytes>>,
}

/// Context for one outstanding warmup batch.
#[derive(Debug)]
pub struct WarmContext {
    /// Digest being warmed.
    pub digest: Digest,
    /// Head the activation serves, or `None` for restart warm.
    pub head: Option<Head<OpaqueBytes>>,
    /// Trigger seq whose batch will carry the activation.
    pub trigger: u64,
    /// First seq the instance will evaluate live.
    pub live_from: u64,
    /// First seq in the sent batch.
    pub first: u64,
    /// Last seq in the sent batch.
    pub last: u64,
}

/// Context for one outstanding status resync.
#[derive(Debug)]
pub struct StatusContext {
    /// Digest being queried.
    pub digest: Digest,
    /// Trigger seq awaiting the resync.
    pub trigger: u64,
}

/// Where one planned record sorts in `N`'s batch.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum PlanOrder {
    /// Live evaluation records, in ascending digest order then intent order.
    Live {
        /// Evaluated digest.
        digest: Digest,
        /// Intent index within the reply, or the poisoned follow-up index.
        index: usize,
    },
    /// Activation records, in head order then cause then intent order.
    Activation {
        /// Index into the seq's activation queue.
        head_index: usize,
        /// Cause (`k` for catch-up, `u64::MAX` for the terminal activation).
        cause: u64,
        /// Intent index within the reply, or `usize::MAX` for the terminal.
        index: usize,
    },
}

/// One `SetHead` awaiting its destination check, in plan order.
#[derive(Debug, Clone)]
pub struct PendingDestination {
    /// Order key for the resolved record.
    pub order: PlanOrder,
    /// Trigger or catch-up seq causing the move or its failure.
    pub cause: u64,
    /// Bundle whose rule returned the intent.
    pub bundle: Digest,
    /// Reactor that returned the intent.
    pub reactor: aether_bloomery_kinds::ReactorName,
    /// The move to attempt.
    pub set_head: aether_bloomery_kinds::SetHead,
}

/// Per-seq phase for `N = R + 1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeqPhase {
    /// Live `Event(N)` fan-out outstanding, collecting replies.
    Evaluating,
    /// Serial activation of changed heads.
    Activating,
    /// All replies collected, resolving destinations and queueing the batch.
    Planning,
}

/// One activation's phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivationPhase {
    /// Bundle artifact read or load outstanding.
    Loading,
    /// Paging the journal and sending `Warm` batches through `live_from - 1`.
    Warming,
    /// Delivering owed `Event`s `live_from..=trigger` one at a time.
    CatchingUp,
}

/// One head's activation in progress.
#[derive(Debug)]
pub struct ActivationWork {
    /// Head being activated.
    pub head: Head<OpaqueBytes>,
    /// Digest selected for the head.
    pub bundle: Digest,
    /// First seq the instance will evaluate live.
    pub live_from: u64,
    /// Trigger seq whose batch carries the activation.
    pub trigger: u64,
    /// Index into the seq's activation queue, for ordering.
    pub head_index: usize,
    /// Current phase.
    pub phase: ActivationPhase,
    /// Scratch `Heads` folded from seq 1 through the catch-up cursor.
    pub scratch: Heads,
    /// Next owed seq to deliver.
    pub next_k: u64,
    /// Current catch-up page, trimmed to the trigger.
    pub catchup_page: Vec<JournalEntry>,
    /// Next entry in `catchup_page` to fold.
    pub catchup_index: usize,
}

/// Work for `N = R + 1`.
#[derive(Debug)]
pub struct SeqWork {
    /// Trigger seq.
    pub n: u64,
    /// Journal entry `N`.
    pub entry: JournalEntry,
    /// Selection at prefix `N-1`.
    pub prev: BTreeMap<Head<OpaqueBytes>, Digest>,
    /// Selection at prefix `N`, once `N` is applied.
    pub curr: Option<BTreeMap<Head<OpaqueBytes>, Digest>>,
    /// Live digests to evaluate and the heads each serves, in head order.
    pub live: BTreeMap<Digest, Vec<Head<OpaqueBytes>>>,
    /// Current phase.
    pub phase: SeqPhase,
    /// Heads needing activation, in canonical head order.
    pub to_activate: Vec<(Head<OpaqueBytes>, Digest)>,
    /// Next activation index to start.
    pub activate_index: usize,
    /// Activation in progress, if any.
    pub activation: Option<ActivationWork>,
    /// Accumulated plan in append order.
    pub order: BTreeMap<PlanOrder, PlannedRecord>,
    /// `SetHead`s awaiting destination checks, in plan order.
    pub pending_setheads: VecDeque<PendingDestination>,
    /// Destination read outstanding, with its order key.
    pub current_destination: Option<(ArtifactTicket, PendingDestination)>,
}

impl SeqWork {
    /// Empty work for `n`: no successor set, no activations, no plan.
    pub fn new(
        n: u64,
        entry: JournalEntry,
        prev: BTreeMap<Head<OpaqueBytes>, Digest>,
        live: BTreeMap<Digest, Vec<Head<OpaqueBytes>>>,
        phase: SeqPhase,
    ) -> Self {
        Self {
            n,
            entry,
            prev,
            curr: None,
            live,
            phase,
            to_activate: Vec::new(),
            activate_index: 0,
            activation: None,
            order: BTreeMap::new(),
            pending_setheads: VecDeque::new(),
            current_destination: None,
        }
    }
}

/// Restart phase through `W`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartPhase {
    /// Folding pages `1..=W` into routing `Heads` without delivering.
    Folding,
    /// Serially warming each selected digest through `W`.
    Warming,
}

/// Restart replay work through `W`.
#[derive(Debug)]
pub struct RestartWork {
    /// Restart point, the higher watermark.
    pub watermark: u64,
    /// Current phase.
    pub phase: RestartPhase,
    /// Distinct digests to warm, in first-head order, with served heads.
    pub to_warm: Vec<(Digest, Vec<Head<OpaqueBytes>>)>,
    /// Next warm index to start.
    pub warm_index: usize,
    /// Digest currently warming, if any.
    pub warming: Option<Digest>,
    /// Failed digests with served heads and reasons, for the restart batch.
    pub failures: Vec<(Digest, Vec<Head<OpaqueBytes>>, String)>,
}

/// Reactor routing state over the routing cursor `R`.
#[derive(Debug)]
pub struct Routing {
    /// Routing `Heads` at `R`, the last routed seq.
    pub heads: Heads,
    /// Reactor-set cache by digest: `None` selects nothing.
    pub sets: BTreeMap<Digest, Option<ReactorSet>>,
    /// Outstanding routing page read, if any.
    pub read_ticket: Option<EventsTicket>,
    /// Purpose of the outstanding routing read.
    pub read_purpose: Option<RoutingRead>,
    /// Buffered steady page, trimmed to the journal-view cursor.
    pub page: Vec<JournalEntry>,
    /// Outstanding `WatchHead`, if any.
    pub watch: Option<WatchTicket>,
    /// Barrier waiters: caller and `through`.
    pub awaiters: Vec<(CallerId, u64)>,
    /// Committed routing batch awaiting read-back, if any.
    pub committed: Option<CommittedRouting>,
    /// Final records for the in-flight routing append, if any.
    pub inflight_records: Option<Vec<DriverRecord>>,
    /// Live digests for the in-flight routing append, if any.
    pub inflight_live: Option<Vec<Digest>>,
    /// Outstanding live and catch-up evaluations.
    pub evaluates: BTreeMap<EvaluateTicket, EvaluateContext>,
    /// Outstanding warmup batches.
    pub warms: BTreeMap<WarmTicket, WarmContext>,
    /// Outstanding status resyncs.
    pub statuses: BTreeMap<StatusTicket, StatusContext>,
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
            read_ticket: None,
            read_purpose: None,
            page: Vec::new(),
            watch: None,
            awaiters: Vec::new(),
            committed: None,
            inflight_records: None,
            inflight_live: None,
            evaluates: BTreeMap::new(),
            warms: BTreeMap::new(),
            statuses: BTreeMap::new(),
            current: None,
            restart: None,
            started: false,
        }
    }

    /// Routing cursor `R`, the last routed seq.
    pub fn cursor(&self) -> u64 {
        self.heads.cursor().0
    }
}

impl Default for Routing {
    fn default() -> Self {
        Self::new()
    }
}
