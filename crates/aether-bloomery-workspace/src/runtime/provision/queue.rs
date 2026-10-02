//! Budget-fit admission with backfill over ADR-0093's hold-until-resolve
//! dispatch (ADR-0237 decision 9, as amended 2026-10-01).
//!
//! [`Admission`] makes every decision without a context. Runs wait in
//! arrival order. The front run starts as soon as its amounts fit the free
//! budget. While it waits it holds a reservation: the earliest time enough
//! cores and memory free up for it, found by releasing the running runs in
//! the order their deadlines end. A run behind it starts first only when it
//! fits the free budget now and its own deadline ends by that reservation,
//! so a backfilled run never delays the front, and a stream of short runs
//! cannot starve it. A run's deadline is its estimate (a key never seen has
//! the default deadline), and the actor kills a run at its deadline, so the
//! estimate admission plans with is also the longest a run holds its budget.
//! Each run's cores are chosen as it starts ([`super::cores`]). Nothing is
//! ever dropped or refused for load, and since every allotment is clamped to
//! the whole budget, the front always fits once the host is idle.
//!
//! [`RunQueue`] carries those decisions out on the actor thread, the way
//! `aether-http`'s per-sender egress queue does: every run holds its reply
//! (`ctx.hold`) and stages its task (`ctx.stage_blocking`) in its own
//! request's turn, so its caller's chain stays held from accept to reply
//! (ADR-0243 §9). Its task starts when the run is admitted, now or from a
//! later completion's turn, with work built from the allotment admission
//! gave it and a storage session opened over the source the run was received
//! with, and the queue answers the held reply from the task's output. No
//! dispatcher thread ever waits.

use std::collections::{HashMap, VecDeque};
use std::num::NonZeroU32;
use std::time::{Duration, Instant};

use aether_actor::ProtocolRef;
use aether_bloomery_kinds::ArtifactStorage;
use aether_data::RequestId;
use aether_substrate::actor::native::{Held, NativeCtx, Pending, StagedTask, TaskDone};

use super::budget::Budget;
use super::estimate::{Amounts, Estimates};
use super::key::RunKey;
use crate::runtime::run::{Allotment, FAR_FUTURE, Observed, Ran, Runner};
use crate::runtime::storage::{StorageDesk, StorageSession};
use crate::{RunRequest, RunResult, WorkspaceCapability};

/// An admitted run's key and what it was given, kept while it runs so the
/// completion can learn from it and release it.
#[derive(Debug, Clone)]
pub struct Admitted {
    pub(super) key: RunKey,
    pub(super) allotment: Allotment,
    /// Which running run this is, among those [`Admission`] holds.
    ticket: u64,
}

/// What a running run holds of the budget, and when its deadline ends.
#[derive(Debug, Clone, Copy)]
struct Holding {
    ticket: u64,
    cores: u32,
    memory_bytes: u64,
    ends: Instant,
}

/// The cores and memory free by some time, as running runs end.
#[derive(Debug, Clone, Copy)]
struct Freed {
    cores: u32,
    memory_bytes: u64,
    by: Instant,
}

impl Freed {
    /// Whether `cores` and `memory_bytes` fit what is free.
    fn fits(self, cores: NonZeroU32, memory_bytes: u64) -> bool {
        let enough_cores = self.cores >= cores.get();
        let enough_memory = self.memory_bytes >= memory_bytes;
        enough_cores && enough_memory
    }

    /// What is free once `holding` ends too.
    fn after(self, holding: &Holding) -> Self {
        Self {
            cores: self.cores.saturating_add(holding.cores),
            memory_bytes: self.memory_bytes.saturating_add(holding.memory_bytes),
            by: self.by.max(holding.ends),
        }
    }
}

/// The waiting runs in arrival order, the running runs' holdings, the
/// budget, and the estimates. `W` is what a waiting run carries until it is
/// admitted.
#[derive(Debug)]
pub struct Admission<W> {
    budget: Budget,
    estimates: Estimates,
    waiting: VecDeque<(RunKey, W)>,
    running: Vec<Holding>,
    next_ticket: u64,
}

impl<W> Admission<W> {
    pub fn new(budget: Budget, estimates: Estimates) -> Self {
        Self { budget, estimates, waiting: VecDeque::new(), running: Vec::new(), next_ticket: 0 }
    }

    /// Admit a run under `key` at `now`, or `None` when it does not fit, or
    /// when a run waits and this one would not end by the front's
    /// reservation; the caller then queues it.
    pub fn admit_now(&mut self, key: RunKey, now: Instant) -> Option<Admitted> {
        let Some(&(front, _)) = self.waiting.front() else {
            return self.take(key, NonZeroU32::MIN, now);
        };
        let sharing = self.sharing(1);
        let backfills = self.ends_by_reservation(&key, &front, now);
        if backfills {
            self.take(key, sharing, now)
        } else {
            None
        }
    }

    /// Put a run at the back of the line.
    pub fn enqueue(&mut self, key: RunKey, waiting: W) {
        self.waiting.push_back((key, waiting));
    }

    /// Learn from a finished run and return its cores and memory.
    pub fn finish(&mut self, admitted: &Admitted, result: &RunResult, observed: &Observed) {
        self.estimates.observe(&admitted.key, &admitted.allotment, result, observed);
        self.budget.release(&admitted.allotment);
        self.running.retain(|holding| holding.ticket != admitted.ticket);
    }

    /// Admit at `now` the front run when its amounts, computed now, fit the
    /// free budget; otherwise the first run behind it that fits and ends by
    /// the front's reservation. `None` when neither starts.
    pub fn next(&mut self, now: Instant) -> Option<(Admitted, W)> {
        let &(front, _) = self.waiting.front()?;
        let sharing = self.sharing(0);
        if let Some(admitted) = self.take(front, sharing, now) {
            let (_, waiting) = self.waiting.pop_front()?;
            return Some((admitted, waiting));
        }

        for index in 1..self.waiting.len() {
            let (key, _) = self.waiting[index];
            let backfills = self.ends_by_reservation(&key, &front, now);
            if backfills && let Some(admitted) = self.take(key, sharing, now) {
                let (_, waiting) = self.waiting.remove(index)?;
                return Some((admitted, waiting));
            }
        }
        None
    }

    /// How many runs wait.
    pub fn waiting(&self) -> usize {
        self.waiting.len()
    }

    /// What a run under `key` would be given now, before its cores are
    /// chosen.
    pub fn amounts(&self, key: &RunKey) -> Amounts {
        self.estimates.amounts(key)
    }

    /// Take the budget for a run under `key` that shares the free cores with
    /// `sharing` runs, itself included, and hold it from `now` until its
    /// deadline ends.
    fn take(&mut self, key: RunKey, sharing: NonZeroU32, now: Instant) -> Option<Admitted> {
        let amounts = self.estimates.amounts(&key);
        let allotment = self.budget.take(&amounts, sharing)?;
        let ticket = self.next_ticket;
        self.next_ticket += 1;
        self.running.push(Holding {
            ticket,
            cores: allotment.cpus.count().get(),
            memory_bytes: allotment.memory_bytes,
            ends: ends(now, allotment.deadline),
        });
        Some(Admitted { key, allotment, ticket })
    }

    /// The waiting runs plus `arriving`, as the count that shares the free
    /// cores.
    fn sharing(&self, arriving: usize) -> NonZeroU32 {
        let wanting = self.waiting.len().saturating_add(arriving);
        u32::try_from(wanting).ok().and_then(NonZeroU32::new).unwrap_or(NonZeroU32::MIN)
    }

    /// Whether a run under `key` started at `now` ends by the reservation of
    /// the front run, under `front`.
    fn ends_by_reservation(&self, key: &RunKey, front: &RunKey, now: Instant) -> bool {
        let ends = ends(now, self.estimates.amounts(key).deadline);
        ends <= self.reservation(front, now)
    }

    /// The earliest time, from `now`, that enough cores and memory are free
    /// for the front run under `front` to start: the running runs release
    /// their holdings in the order their deadlines end until the front's
    /// fewest cores and its memory fit.
    fn reservation(&self, front: &RunKey, now: Instant) -> Instant {
        let cores = self.budget.floor_cores();
        let memory_bytes = self.estimates.amounts(front).memory_bytes.get();
        let mut ending = self.running.clone();
        ending.sort_by_key(|holding| holding.ends);

        let mut freed =
            Freed { cores: self.budget.free_cores(), memory_bytes: self.budget.free_memory_bytes(), by: now };
        for holding in &ending {
            let starts = freed.fits(cores, memory_bytes);
            if starts {
                break;
            }
            freed = freed.after(holding);
        }
        freed.by
    }
}

/// When a run started at `now` with `deadline` ends; a deadline too large for
/// the clock ends in the far future, as the runner reads it.
fn ends(now: Instant, deadline: Duration) -> Instant {
    now.checked_add(deadline).unwrap_or(now + FAR_FUTURE)
}

/// A run waiting for the budget: its request and the source it reads and
/// stages through, proven live at receipt, its task, staged in its own
/// request's turn, and the reply it owes its caller.
struct Waiting {
    source: ProtocolRef<ArtifactStorage>,
    /// The source's path text: the unit a warm layer is kept for.
    unit: String,
    run: RunRequest,
    task: StagedTask<Ran>,
    held: Held<RunResult>,
}

/// Provisioned runs: the runner each admitted run clones onto its worker,
/// the admission it waits in, and the reply and allotment of each running
/// run, keyed by its task's request.
pub struct RunQueue {
    runner: Runner,
    admission: Admission<Waiting>,
    running: HashMap<RequestId, (Held<RunResult>, Admitted)>,
}

impl RunQueue {
    pub fn new(runner: Runner, budget: Budget, estimates: Estimates) -> Self {
        Self { runner, admission: Admission::new(budget, estimates), running: HashMap::new() }
    }

    /// Accept `run` over `source`, whose path text is `unit`, in its own
    /// turn: hold its reply and stage its task, then start the task now when
    /// the run is admitted, opening its storage session at `desk`, or queue
    /// it.
    pub fn submit(
        &mut self,
        ctx: &mut NativeCtx<'_, WorkspaceCapability>,
        desk: &mut StorageDesk,
        source: ProtocolRef<ArtifactStorage>,
        unit: String,
        run: RunRequest,
    ) -> Pending<RunResult> {
        let key = RunKey::of(&run);
        let (pending, held) = ctx.hold::<RunResult>();
        let task = ctx.stage_blocking::<Ran>();
        let waiting = Waiting { source, unit, run, task, held };
        if let Some(admitted) = self.admission.admit_now(key, Instant::now()) {
            self.start(ctx, desk, admitted, waiting);
            return pending;
        }

        let amounts = self.admission.amounts(&key);
        self.admission.enqueue(key, waiting);
        tracing::info!(
            target: "aether_bloomery_workspace",
            %key,
            memory_bytes = amounts.memory_bytes.get(),
            deadline_millis = amounts.deadline.as_millis(),
            waiting = self.admission.waiting(),
            "run queued for the budget",
        );
        pending
    }

    /// A run finished: learn from it, release its budget, answer its
    /// caller, then admit the front, or runs that backfill past it, until
    /// nothing more starts.
    ///
    /// # Panics
    /// Panics when `ctx` is not dispatching the completion of a run this
    /// queue started.
    pub fn complete(
        &mut self,
        ctx: &mut NativeCtx<'_, WorkspaceCapability>,
        desk: &mut StorageDesk,
        done: TaskDone<Ran>,
    ) {
        let (held, admitted) = ctx
            .in_reply_to()
            .and_then(|request| self.running.remove(&request))
            .expect("a RunQueue completion names a run the queue started");
        let Ran { result, observed } = done.into_output();
        self.admission.finish(&admitted, &result, &observed);
        held.answer(ctx, &result);
        while let Some((admitted, waiting)) = self.admission.next(Instant::now()) {
            self.start(ctx, desk, admitted, waiting);
        }
    }

    /// Start an admitted run's staged task with work built from its
    /// allotment and a storage session over its source, and keep its reply
    /// and allotment until it completes.
    fn start(
        &mut self,
        ctx: &NativeCtx<'_, WorkspaceCapability>,
        desk: &mut StorageDesk,
        admitted: Admitted,
        waiting: Waiting,
    ) {
        let Waiting { source, unit, run, task, held } = waiting;
        self.log_admitted(&admitted);
        let session = desk.open(source, ctx.blob_check_in());
        let request = task.start(ctx, self.work(&admitted, unit, run, session));
        self.running.insert(request, (held, admitted));
    }

    /// The worker's whole job for one admitted run.
    fn work(
        &self,
        admitted: &Admitted,
        unit: String,
        run: RunRequest,
        session: StorageSession,
    ) -> impl FnOnce() -> Ran + Send + 'static {
        let runner = self.runner.clone();
        let Admitted { key, allotment, .. } = admitted.clone();
        move || runner.answer(&run, &unit, key, &allotment, session)
    }

    fn log_admitted(&self, admitted: &Admitted) {
        let Admitted { key, allotment, .. } = admitted;
        tracing::info!(
            target: "aether_bloomery_workspace",
            %key,
            cpus = %allotment.cpus,
            memory_bytes = allotment.memory_bytes,
            deadline_millis = allotment.deadline.as_millis(),
            waiting = self.admission.waiting(),
            "run admitted",
        );
    }
}
