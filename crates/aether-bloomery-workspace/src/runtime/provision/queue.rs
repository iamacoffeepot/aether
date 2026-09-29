//! Budget-fit FIFO admission over ADR-0093's hold-until-resolve dispatch.
//!
//! [`Admission`] makes every decision without a context: a run starts only
//! when nothing waits ahead of it and its amounts fit the free budget;
//! otherwise it joins the back of the queue. A completion releases the
//! finished run's cores and memory and admits from the front while the
//! front fits, recomputing the front's amounts from the current estimate.
//! Nothing is ever dropped or refused for load, and since every allotment is
//! clamped to the whole budget, the front always fits once the host is idle.
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

use aether_actor::ProtocolRef;
use aether_bloomery_kinds::ArtifactStorage;
use aether_data::RequestId;
use aether_substrate::actor::native::{Held, NativeCtx, Pending, StagedTask, TaskDone};

use super::budget::Budget;
use super::estimate::{Amounts, Estimates};
use super::key::RunKey;
use crate::runtime::run::{Allotment, Observed, Ran, Runner};
use crate::runtime::storage::{StorageDesk, StorageSession};
use crate::{RunRequest, RunResult, WorkspaceCapability};

/// An admitted run's key and what it was given, kept while it runs so the
/// completion can learn from it and release it.
#[derive(Debug, Clone)]
pub struct Admitted {
    pub(super) key: RunKey,
    pub(super) allotment: Allotment,
}

/// The FIFO of waiting runs, the budget, and the estimates. `W` is what a
/// waiting run carries until it is admitted.
#[derive(Debug)]
pub struct Admission<W> {
    budget: Budget,
    estimates: Estimates,
    waiting: VecDeque<(RunKey, W)>,
}

impl<W> Admission<W> {
    pub fn new(budget: Budget, estimates: Estimates) -> Self {
        Self { budget, estimates, waiting: VecDeque::new() }
    }

    /// Admit a run under `key` now, or `None` when a run already waits or
    /// its amounts do not fit; the caller then queues it.
    pub fn admit_now(&mut self, key: RunKey) -> Option<Admitted> {
        if !self.waiting.is_empty() {
            return None;
        }
        self.budget.take(&self.estimates.amounts(&key)).map(|allotment| Admitted { key, allotment })
    }

    /// Put a run at the back of the line.
    pub fn enqueue(&mut self, key: RunKey, waiting: W) {
        self.waiting.push_back((key, waiting));
    }

    /// Learn from a finished run and return its cores and memory.
    pub fn finish(&mut self, admitted: &Admitted, result: &RunResult, observed: &Observed) {
        self.estimates.observe(&admitted.key, &admitted.allotment, result, observed);
        self.budget.release(&admitted.allotment);
    }

    /// Admit the front run when its amounts, computed now, fit the free
    /// budget. Never looks past the front: a run behind one that does not
    /// fit waits too.
    pub fn next(&mut self) -> Option<(Admitted, W)> {
        let (key, waiting) = self.waiting.pop_front()?;
        if let Some(allotment) = self.budget.take(&self.estimates.amounts(&key)) {
            Some((Admitted { key, allotment }, waiting))
        } else {
            self.waiting.push_front((key, waiting));
            None
        }
    }

    /// How many runs wait.
    pub fn waiting(&self) -> usize {
        self.waiting.len()
    }

    /// What a run under `key` would be given now.
    pub fn amounts(&self, key: &RunKey) -> Amounts {
        self.estimates.amounts(key)
    }
}

/// A run waiting for the budget: its request and the source it reads and
/// stages through, proven live at receipt, its task, staged in its own
/// request's turn, and the reply it owes its caller.
struct Waiting {
    source: ProtocolRef<ArtifactStorage>,
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

    /// Accept `run` over `source` in its own turn: hold its reply and stage
    /// its task, then start the task now when the run is admitted, opening
    /// its storage session at `desk`, or queue it.
    pub fn submit(
        &mut self,
        ctx: &mut NativeCtx<'_, WorkspaceCapability>,
        desk: &mut StorageDesk,
        source: ProtocolRef<ArtifactStorage>,
        run: RunRequest,
    ) -> Pending<RunResult> {
        let key = RunKey::of(&run);
        let (pending, held) = ctx.hold::<RunResult>();
        let task = ctx.stage_blocking::<Ran>();
        let waiting = Waiting { source, run, task, held };
        if let Some(admitted) = self.admission.admit_now(key) {
            self.start(ctx, desk, admitted, waiting);
            return pending;
        }

        let amounts = self.admission.amounts(&key);
        self.admission.enqueue(key, waiting);
        tracing::info!(
            target: "aether_bloomery_workspace",
            %key,
            cores = amounts.cores.get(),
            memory_bytes = amounts.memory_bytes.get(),
            deadline_millis = amounts.deadline.as_millis(),
            waiting = self.admission.waiting(),
            "run queued for the budget",
        );
        pending
    }

    /// A run finished: learn from it, release its budget, answer its
    /// caller, then admit from the front while the front fits.
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
        while let Some((admitted, waiting)) = self.admission.next() {
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
        let Waiting { source, run, task, held } = waiting;
        self.log_admitted(&admitted);
        let session = desk.open(source, ctx.blob_check_in());
        let request = task.start(ctx, self.work(&admitted, run, session));
        self.running.insert(request, (held, admitted));
    }

    /// The worker's whole job for one admitted run.
    fn work(
        &self,
        admitted: &Admitted,
        run: RunRequest,
        session: StorageSession,
    ) -> impl FnOnce() -> Ran + Send + 'static {
        let runner = self.runner.clone();
        let allotment = admitted.allotment.clone();
        move || runner.answer(&run, &allotment, session)
    }

    fn log_admitted(&self, admitted: &Admitted) {
        let Admitted { key, allotment } = admitted;
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
