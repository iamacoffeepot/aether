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
//! `aether-http`'s per-sender egress queue does: an admitted run dispatches
//! through `dispatch_blocking_with`, and a queued one captures its
//! settlement hold and reply target at accept and dispatches later through
//! `dispatch_blocking_resumed_with`, so its caller's chain stays held from
//! accept to reply. No dispatcher thread ever waits.

use std::collections::VecDeque;

use aether_data::Source;
use aether_substrate::actor::native::{DispatchId, NativeCtx, Pending, TaskDone};
use aether_substrate::runtime::trace::SettlementHold;

use super::budget::Budget;
use super::estimate::{Amounts, Estimates};
use super::key::RunKey;
use crate::runtime::run::{Allotment, Observed, Ran, Runner};
use crate::{Run, RunResult, WorkspaceCapability};

/// A run's dispatch context: its key and what it was given, so the
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

/// A run waiting for the budget, with the chain it answers on.
struct Waiting {
    run: Run,
    hold: Option<SettlementHold>,
    reply_to: Source,
}

/// Provisioned runs: the runner each admitted run clones onto its worker,
/// and the admission it waits in.
pub struct RunQueue {
    runner: Runner,
    admission: Admission<Waiting>,
}

impl RunQueue {
    pub fn new(runner: Runner, budget: Budget, estimates: Estimates) -> Self {
        Self { runner, admission: Admission::new(budget, estimates) }
    }

    /// Accept `run`: dispatch it now when it is admitted, or queue it with
    /// its settlement hold and reply target.
    pub fn submit(&mut self, ctx: &mut NativeCtx<'_, WorkspaceCapability>, run: Run) -> Pending<RunResult> {
        let key = RunKey::of(&run);
        if let Some(admitted) = self.admission.admit_now(key) {
            self.log_admitted(&admitted);
            let work = self.work(&admitted, run);
            let id = ctx.dispatch_blocking_with(admitted, work);
            return ctx.pending(id);
        }

        let amounts = self.admission.amounts(&key);
        self.admission.enqueue(key, Waiting { run, hold: ctx.acquire_settlement_hold(), reply_to: ctx.reply_target() });
        tracing::info!(
            target: "aether_workspace",
            %key,
            cores = amounts.cores.get(),
            memory_bytes = amounts.memory_bytes.get(),
            deadline_millis = amounts.deadline.as_millis(),
            waiting = self.admission.waiting(),
            "run queued for the budget",
        );
        ctx.pending(DispatchId::NONE)
    }

    /// A run finished: learn from it, release its budget, reply to its
    /// caller, then admit from the front while the front fits.
    pub fn complete(&mut self, ctx: &mut NativeCtx<'_, WorkspaceCapability>, done: TaskDone<Ran, Admitted>) {
        self.admission.finish(done.context(), &done.output().result, &done.output().observed);
        done.resolve_with(ctx, |ran, _| ran.result.clone());
        while let Some((admitted, Waiting { run, hold, reply_to })) = self.admission.next() {
            self.log_admitted(&admitted);
            let work = self.work(&admitted, run);
            ctx.dispatch_blocking_resumed_with(hold, reply_to, admitted, work);
        }
    }

    /// The worker's whole job for one admitted run.
    fn work(&self, admitted: &Admitted, run: Run) -> impl FnOnce() -> Ran + Send + 'static {
        let runner = self.runner.clone();
        let allotment = admitted.allotment.clone();
        move || runner.answer(&run, &allotment)
    }

    fn log_admitted(&self, admitted: &Admitted) {
        let Admitted { key, allotment } = admitted;
        tracing::info!(
            target: "aether_workspace",
            %key,
            cpus = %allotment.cpus,
            memory_bytes = allotment.memory_bytes,
            deadline_millis = allotment.deadline.as_millis(),
            waiting = self.admission.waiting(),
            "run admitted",
        );
    }
}
