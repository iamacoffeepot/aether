//! The bounded queue `aether.fs.load` runs on: a load reads on a worker
//! thread that holds no chain, and its completion delivers `Loaded` to the
//! requester on a fresh one.
//!
//! [`TaskQueue`](aether_substrate::actor::native::TaskQueue) is the held
//! sibling the frame-bound `read` uses: it holds each request's reply and
//! chain until the work answers it. A load owes its requester nothing once
//! it is accepted, so this queue holds no reply and no chain. It starts each
//! load with `dispatch_blocking_resumed_with(None, ..)`, the hold-free worker
//! the bloomery driver's tick wait uses, so the caller's chain settles as
//! soon as the acceptance is handled. The requester's typed reference rides
//! the dispatch as its completion context, so the queue keeps no table of
//! who asked.
//!
//! Past the bound, loads wait in arrival order. None is ever dropped while
//! the actor lives; an actor close releases the waiting ones, since nothing
//! is owed for them.

use std::collections::VecDeque;

use aether_actor::{ProtocolRef, ReplyMode, Subscriber};
use aether_substrate::actor::native::{NativeCtx, TaskDone};

use super::super::Loaded;

/// The requester a load delivers to: its sender, typed at receipt as a
/// silent handler of `Loaded`.
pub type LoadSubscriber = ProtocolRef<Subscriber<Loaded>>;

/// A load's read, run on the worker thread once its load starts.
type Work = Box<dyn FnOnce() -> Loaded + Send>;

/// A load waiting for a free slot.
struct Waiting {
    subscriber: LoadSubscriber,
    work: Work,
}

/// Bounded background loads. Lives in the actor's plain state; every method
/// runs on the actor's single-threaded dispatch.
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

    /// Start `work` for `subscriber` now when a slot is free, or queue it
    /// behind the loads already waiting.
    pub fn submit<A, M, F>(&mut self, ctx: &mut NativeCtx<'_, A, M>, subscriber: LoadSubscriber, work: F)
    where
        M: ReplyMode,
        F: FnOnce() -> Loaded + Send + 'static,
    {
        let slot_free = self.running < self.max;
        if slot_free {
            self.start(ctx, subscriber, Box::new(work));
        } else {
            self.waiting.push_back(Waiting { subscriber, work: Box::new(work) });
        }
    }

    /// The load completion's body: send the finished load's `Loaded` to its
    /// requester on a fresh chain, then start the next waiting load in the
    /// freed slot.
    pub fn complete<A, M: ReplyMode>(
        &mut self,
        ctx: &mut NativeCtx<'_, A, M>,
        done: &TaskDone<Loaded, LoadSubscriber>,
    ) {
        let _root = ctx.send_detached_to(*done.context(), done.output());
        self.running -= 1;

        if let Some(Waiting { subscriber, work }) = self.waiting.pop_front() {
            self.start(ctx, subscriber, work);
        }
    }

    /// Spawn one load's worker holding no chain, with its requester as the
    /// completion context.
    fn start<A, M: ReplyMode>(&mut self, ctx: &mut NativeCtx<'_, A, M>, subscriber: LoadSubscriber, work: Work) {
        let reply_to = ctx.reply_target();
        let _dispatch = ctx.dispatch_blocking_resumed_with(None, reply_to, subscriber, work);
        self.running += 1;
    }
}
