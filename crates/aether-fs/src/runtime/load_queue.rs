//! The bounded queue `aether.fs.load` runs on: a load reads on a worker
//! thread that holds no chain, and its completion answers the request late,
//! outside the caller's chain.
//!
//! [`TaskQueue`](aether_substrate::actor::native::TaskQueue) is the held
//! sibling the frame-bound `read` uses: it holds each request's reply and
//! chain until the work answers it. A load must not hold the caller's chain,
//! so this queue captures only each request's reply target, in the request's
//! own turn, and starts its work with `dispatch_blocking_resumed_with(None,
//! ..)`: the hold-free worker the bloomery driver's tick wait uses. The
//! caller's chain settles as soon as `on_load` returns. The completion
//! resolves the reply through the captured target with no root, so the reply
//! reaches the caller's response handler by its correlation, with the context
//! the caller bound, and joins no chain.
//!
//! Past the bound, loads wait in arrival order. None is ever dropped while
//! the actor lives.

use std::collections::VecDeque;

use aether_actor::{ReplyMode, Single};
use aether_substrate::Source;
use aether_substrate::actor::native::{NativeCtx, TaskDone};

use super::super::Loaded;

/// A load's read, run on the worker thread once its load starts.
type Work = Box<dyn FnOnce() -> Loaded + Send>;

/// A load waiting for a free slot: the caller it answers and its read.
struct Waiting {
    reply_to: Source,
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

    /// Accept a load in the request's own turn: capture the caller it
    /// answers, then start `work` now when a slot is free, or queue it behind
    /// the loads already waiting. Takes no settlement hold.
    pub fn submit<A, M, F>(&mut self, ctx: &mut NativeCtx<'_, A, M>, work: F)
    where
        M: ReplyMode,
        F: FnOnce() -> Loaded + Send + 'static,
    {
        let reply_to = ctx.reply_target();
        let slot_free = self.running < self.max;
        if slot_free {
            self.start(ctx, reply_to, Box::new(work));
        } else {
            self.waiting.push_back(Waiting { reply_to, work: Box::new(work) });
        }
    }

    /// The load completion's body: answer the finished load's caller, then
    /// start the next waiting load in the freed slot.
    pub fn complete<A>(&mut self, ctx: &mut NativeCtx<'_, A, Single>, done: TaskDone<Loaded>) {
        done.resolve(ctx);
        self.running -= 1;

        if let Some(Waiting { reply_to, work }) = self.waiting.pop_front() {
            self.start(ctx, reply_to, work);
        }
    }

    /// Spawn one load's worker, holding no chain and answering `reply_to`.
    fn start<A, M: ReplyMode>(&mut self, ctx: &mut NativeCtx<'_, A, M>, reply_to: Source, work: Work) {
        let _dispatch = ctx.dispatch_blocking_resumed_with(None, reply_to, (), work);
        self.running += 1;
    }
}
