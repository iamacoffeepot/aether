//! Cap-level rate-limit + queue helper over staged blocking work
//! (ADR-0093, ADR-0243 §9).
//!
//! The content-gen caps make long-tail blocking provider calls
//! (multi-second image gen, the `claude` subprocess) that must not block
//! the single-threaded actor's mail intake. The substrate owns the worker
//! spawn, the settlement hold, and the completion routing; this helper adds
//! only the one thing the framework deliberately doesn't centralise: the
//! per-cap concurrency bound + waiting queue that rate-limits the paid
//! provider endpoints (ADR-0050 §2).
//!
//! [`TaskQueue::submit`] runs in the request's own turn. It holds the
//! request's reply with `ctx.hold`, keeps the [`Held<R>`] ticket, and stages
//! the work with `ctx.stage_blocking`, so the task takes the request's chain
//! there, whether it starts at once or waits for a slot. A staged task owes
//! no reply: the queue answers from its own table. [`TaskQueue::complete`],
//! the whole body of the cap's `#[handler(task)]`, finds the finished task's
//! `Held` by the request its completion is correlated to, answers it with
//! the worker's output, and starts the next waiting task, whose chain is
//! still the waiting request's own. So a request's chain settles when that
//! request is answered, never when another request's work finishes.
//!
//! Before the queue drops with the actor's state, an actor close while the
//! engine keeps running answers every held reply with its `R::unanswered()`,
//! an engine teardown settles them silently, and either releases every
//! unstarted task (ADR-0243 §1).

use std::collections::{HashMap, VecDeque};

use aether_actor::{HeldReply, ReplyMode};
use aether_data::{ActorMail, RequestId};

use crate::actor::native::NativeCtx;
use crate::actor::native::offload::blocking::{Pending, TaskDone};
use crate::actor::native::offload::held::Held;
use crate::actor::native::offload::staged_task::StagedTask;

/// Default per-cap concurrency bound when a cap doesn't override it.
/// Doubles as rate-limit throttling for the paid provider endpoints
/// (ADR-0050 §2) — at most this many provider calls run concurrently;
/// the rest queue.
pub const DEFAULT_MAX_IN_FLIGHT: usize = 4;

/// A waiting request's work, run on the worker thread when its task starts.
/// `Send` so the embedding cap (a `NativeActor`, which is `Send + 'static`)
/// can hold the queue in its state.
type Work<R> = Box<dyn FnOnce() -> R + Send>;

/// A request waiting for a slot: its task, staged in its own turn, the reply
/// it is owed, and the work its task runs once started.
struct Waiting<R: ActorMail> {
    task: StagedTask<R>,
    held: Held<R>,
    work: Work<R>,
}

/// Cap-level rate-limit + queue over staged blocking work, answering each
/// request with one `R`. Lives in the cap's plain (lock-free) actor state;
/// every method runs on the single-threaded dispatcher (the actor IS the
/// mutual exclusion — no `Semaphore`, no `Mutex`).
pub struct TaskQueue<R: ActorMail> {
    max: usize,
    /// The reply each started task answers, keyed by the task's request.
    running: HashMap<RequestId, Held<R>>,
    waiting: VecDeque<Waiting<R>>,
}

impl<R: HeldReply + Send + 'static> TaskQueue<R> {
    /// Build a queue bounded at `max` concurrent provider calls. A `max`
    /// of 0 is clamped to 1 — a zero bound would queue forever.
    #[must_use]
    pub fn new(max: usize) -> Self {
        Self { max: max.max(1), running: HashMap::new(), waiting: VecDeque::new() }
    }

    /// How many provider calls are running right now. Exposed for the
    /// cap's `engine_logs` tracing and for tests.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.running.len()
    }

    /// How many requests are waiting for a free in-flight slot.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.waiting.len()
    }

    /// Accept a unit of blocking work in the request's own turn: hold its
    /// reply, stage its task on its chain, and start the task now when a
    /// slot is free, or when [`Self::complete`] frees one. The returned
    /// receipt declares the handler's `-> Pending<R>` row.
    ///
    /// # Panics
    /// Takes this dispatch's one [`NativeCtx::hold`], so a handler that
    /// already holds a reply panics.
    pub fn submit<F, A, M>(&mut self, ctx: &mut NativeCtx<'_, A, M>, work: F) -> Pending<R>
    where
        F: FnOnce() -> R + Send + 'static,
        M: ReplyMode,
    {
        let (pending, held) = ctx.hold::<R>();
        let task = ctx.stage_blocking::<R>();
        if self.running.len() < self.max {
            self.running.insert(task.start(ctx, work), held);
        } else {
            self.waiting.push_back(Waiting { task, held, work: Box::new(work) });
        }
        pending
    }

    /// The cap's `#[handler(task)]` body: answer the finished task's request
    /// with its output, then start the next waiting task in the freed slot.
    ///
    /// # Panics
    /// Panics when `ctx` is not dispatching the completion of a task this
    /// queue started.
    pub fn complete<A>(&mut self, ctx: &mut NativeCtx<'_, A>, done: TaskDone<R>) {
        let held = ctx
            .in_reply_to()
            .and_then(|request| self.running.remove(&request))
            .expect("a TaskQueue completion names a task the queue started");
        held.answer(ctx, &done.into_output());

        if let Some(Waiting { task, held, work }) = self.waiting.pop_front() {
            self.running.insert(task.start(ctx, work), held);
        }
    }
}
