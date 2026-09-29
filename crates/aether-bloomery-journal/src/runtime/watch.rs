//! Bounded table of held `WatchHead` replies (ADR-0226 decision 10,
//! ADR-0243).
//!
//! A watch whose `after` is already behind the head answers at once; the
//! journal actor never parks it. Every other watch enters this table until
//! a committed write moves the head past `after`. The table's bound
//! ([`crate::MAX_HEAD_WATCHERS`]) protects against an unbounded
//! backlog, since journal writes and watches are unauthenticated
//! (ADR-0226 Consequences).

use std::collections::BTreeMap;
use std::mem;

use aether_bloomery_kinds::WatchHeadResult;
use aether_substrate::actor::native::{Held, NativeCtx};

use crate::MAX_HEAD_WATCHERS;

/// Held `WatchHead` replies, keyed by the exclusive sequence boundary
/// they are waiting to pass.
///
/// Actor close answers the held entries with `WatchHeadResult::Err` before
/// the state drops (ADR-0243 §1), so a ticket still parked here at close
/// drops silently.
pub struct Watchers {
    by_after: BTreeMap<u64, Vec<Held<WatchHeadResult>>>,
    count: usize,
}

impl Watchers {
    /// Empty table.
    pub fn new() -> Self {
        Self { by_after: BTreeMap::new(), count: 0 }
    }

    /// Park the held reply `held` under `after`, refusing when the table
    /// already holds [`MAX_HEAD_WATCHERS`] entries.
    ///
    /// On refusal `held` is handed back so the caller can still answer it
    /// exactly once; the table never drops a held reply.
    pub fn park(&mut self, after: u64, held: Held<WatchHeadResult>) -> Result<(), Held<WatchHeadResult>> {
        if self.count >= MAX_HEAD_WATCHERS {
            return Err(held);
        }

        self.by_after.entry(after).or_default().push(held);
        self.count += 1;
        Ok(())
    }

    /// Answer every watch whose `after` the new `head` has passed with
    /// `Advanced { head }`, in ascending `after` order, and leave every
    /// watch with `after >= head` parked.
    ///
    /// `split_off` removes the passed prefix in one step; the reply loop
    /// that follows is iterative, with no recursion.
    pub fn wake<A>(&mut self, ctx: &mut NativeCtx<'_, A>, head: u64) {
        let remaining = self.by_after.split_off(&head);
        let passed = mem::replace(&mut self.by_after, remaining);

        for (_, held) in passed {
            for reply in held {
                self.count -= 1;
                reply.answer(ctx, &WatchHeadResult::Advanced { head });
            }
        }
    }
}
