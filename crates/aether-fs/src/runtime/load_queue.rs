//! The bounded queue `aether.fs.load` runs on: each load is owed through a
//! `Held<Loaded>` from `ctx.defer`, which holds no chain, and reads on a
//! worker that answers that debt late.
//!
//! [`TaskQueue`](aether_substrate::actor::native::TaskQueue) is the held
//! sibling the frame-bound `read` uses: its `ctx.hold` keeps each request's
//! chain open until the work answers it. A load must not hold the caller's
//! chain, so `on_load` owes its reply with `ctx.defer` and hands the `Held`
//! here, in the request's own turn. A started load's worker attaches to that
//! debt with `dispatch_blocking_held_with`, so its completion's `resolve`
//! answers the request's own caller, by its correlation and with the context
//! the caller bound, joining no chain. A queued load keeps its `Held` until a
//! slot frees, so it is never dropped while the actor lives: an unanswered
//! `Held` fails fast if dropped, and an actor close answers it
//! `Loaded::unanswered()`.

use std::collections::VecDeque;

use aether_actor::{Anyone, ReplyMode, Single};
use aether_substrate::actor::native::{Held, NativeCtx, TaskDone};

use super::super::Loaded;

/// A load's read, run on the worker thread once its load starts.
type Work = Box<dyn FnOnce() -> Loaded + Send>;

/// A load waiting for a free slot: the reply it owes and its read.
struct Waiting {
    held: Held<Loaded>,
    work: Work,
}

/// Bounded loads answered late. Lives in the actor's plain state; every
/// method runs on the actor's single-threaded dispatch.
pub struct LoadQueue {
    max: usize,
    running: usize,
    waiting: VecDeque<Waiting>,
}

impl LoadQueue {
    /// A queue running at most `max` loads at once. A `max` of 0 is clamped
    /// to 1, as `TaskQueue::new` clamps it: a zero bound would queue forever.
    pub fn new(max: usize) -> Self {
        Self { max: max.max(1), running: 0, waiting: VecDeque::new() }
    }

    /// Accept a load in the request's own turn: start `work` against `held`
    /// now when a slot is free, or queue both behind the loads already
    /// waiting.
    pub fn submit<A, S, M, F>(&mut self, ctx: &mut NativeCtx<'_, A, S, M>, held: Held<Loaded>, work: F)
    where
        M: ReplyMode,
        F: FnOnce() -> Loaded + Send + 'static,
    {
        let slot_free = self.running < self.max;
        if slot_free {
            self.start(ctx, held, Box::new(work));
        } else {
            self.waiting.push_back(Waiting { held, work: Box::new(work) });
        }
    }

    /// The load completion's body: answer the finished load's caller, then
    /// start the next waiting load in the freed slot.
    pub fn complete<A>(&mut self, ctx: &mut NativeCtx<'_, A, Anyone, Single>, done: TaskDone<Loaded>) {
        done.resolve(ctx);
        self.running -= 1;

        if let Some(Waiting { held, work }) = self.waiting.pop_front() {
            self.start(ctx, held, work);
        }
    }

    /// Spawn one load's worker, attached to the debt `held` names, which
    /// holds no chain.
    fn start<A, S, M: ReplyMode>(&mut self, ctx: &mut NativeCtx<'_, A, S, M>, held: Held<Loaded>, work: Work) {
        let _dispatch = ctx.dispatch_blocking_held_with(held, (), work);
        self.running += 1;
    }
}
