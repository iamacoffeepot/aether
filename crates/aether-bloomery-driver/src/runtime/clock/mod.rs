//! The driver's clock (ADR-0245): `clock.until` requests armed on a heap of
//! due times and fired by an explicit tick.
//!
//! `clock.until` is a driver-native program. A request for it is recorded
//! under the reserved [`CLOCK_BUNDLE`] like any other `Requested`, but it
//! never enters a digest queue: the driver checks its name and input, arms
//! it here, and frees the request at once. [`ProgramCore::tick`] is the only
//! reader of time: the shell feeds it the clock, and every armed timer due
//! by then fires in one paged batch of `Transition`s, each stamped no
//! earlier than its due time. The journal is the timer table, so the heap
//! holds only what is armed in this life; a restart re-arms every
//! outstanding clock request from the fold.
//!
//! [`ProgramCore::tick`]: crate::ProgramCore::tick

mod arm;
mod fire;

use std::cmp::Reverse;
use std::collections::{BTreeSet, BinaryHeap};

use aether_bloomery_kinds::{CLOCK, CLOCK_BUNDLE, Head};
use aether_bloomery_program::Heads;
use aether_data::{Digest, OpaqueBytes};

/// The bundle digest `program` names at `heads`.
///
/// The reserved [`CLOCK`] head resolves to [`CLOCK_BUNDLE`] before any
/// binding is consulted, so a binding recorded under that name is never
/// read. Every other head resolves through its binding, or not at all.
pub fn program_bundle(heads: &Heads, program: &Head<OpaqueBytes>) -> Option<Digest> {
    if *program == CLOCK {
        return Some(CLOCK_BUNDLE);
    }
    heads.get(program).map(|bound| bound.digest())
}

/// Whether `bundle` is the reserved clock identity.
pub fn is_clock(bundle: Digest) -> bool {
    bundle == CLOCK_BUNDLE
}

/// One armed timer: its due time, then its request seq, so the heap pops in
/// due order and equal due times in request order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Due {
    /// The journal time the timer fires at.
    pub due_millis: u64,
    /// The `Requested` seq of the clock request.
    pub seq: u64,
}

/// The armed timers and whether a tick is outstanding.
///
/// A timer costs one heap entry and one set entry, so thousands of them cost
/// kilobytes. Their number is bounded by the outstanding requests the journal
/// holds, with no separate cap.
#[derive(Debug, Default)]
pub struct Timers {
    heap: BinaryHeap<Reverse<Due>>,
    armed: BTreeSet<u64>,
    tick_outstanding: bool,
}

impl Timers {
    /// Arm one timer. Returns `true` when no tick is outstanding, so the
    /// caller asks the shell for one; the tick is then counted outstanding.
    pub(crate) fn arm(&mut self, due: Due) -> bool {
        self.heap.push(Reverse(due));
        self.armed.insert(due.seq);
        self.claim_tick()
    }

    /// Whether the clock request at `seq` is armed and waiting on time.
    pub(crate) fn is_armed(&self, seq: u64) -> bool {
        self.armed.contains(&seq)
    }

    /// Take the outstanding tick, then pop every timer due by `now_millis`,
    /// in due order.
    pub(crate) fn pop_due(&mut self, now_millis: u64) -> Vec<Due> {
        self.tick_outstanding = false;
        let mut fired = Vec::new();
        while let Some(Reverse(due)) = self.heap.peek().copied()
            && due.due_millis <= now_millis
        {
            self.heap.pop();
            self.armed.remove(&due.seq);
            fired.push(due);
        }
        fired
    }

    /// Count a tick outstanding while any timer is armed. Returns `true`
    /// when the caller must ask the shell for it.
    pub(crate) fn claim_tick(&mut self) -> bool {
        if self.tick_outstanding || self.heap.is_empty() {
            return false;
        }
        self.tick_outstanding = true;
        true
    }
}
