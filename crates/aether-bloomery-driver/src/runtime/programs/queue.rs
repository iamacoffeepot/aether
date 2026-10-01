//! One digest's program request queue: the active requests' steps plus a waiting FIFO.

use std::collections::{BTreeMap, VecDeque};

use aether_bloomery_kinds::{ClosureArtifact, Program};

/// One active request's progress through the pipeline.
#[derive(Debug)]
pub enum Step {
    /// Waiting on the digest's shared read.
    Declaring,
    /// Closure read in flight.
    ReadingClosure {
        /// The request's declaration, resolved by the name check.
        declaration: Program,
    },
    /// Waiting on the digest's shared load.
    Loading {
        /// The request's declaration, resolved by the name check.
        declaration: Program,
        /// The request's fetched closure, held for its `Invoke`.
        closure: Vec<ClosureArtifact>,
    },
    /// `Invoke` in flight.
    Invoking {
        /// The request's declaration, resolved by the name check.
        declaration: Program,
    },
}

/// One digest's active requests and waiting FIFO.
///
/// Up to the driver's invocation limit are active at once, each from its
/// section check to its outcome; the rest wait in FIFO order.
#[derive(Debug, Default)]
pub struct DigestQueue {
    /// Each active request's progress, keyed by its `Requested` seq.
    pub active: BTreeMap<u64, Step>,
    /// Seqs waiting their turn, in FIFO order.
    pub waiting: VecDeque<u64>,
}

impl DigestQueue {
    /// The step of `seq`, when it is active on this digest.
    pub fn step(&self, seq: u64) -> Option<&Step> {
        self.active.get(&seq)
    }

    /// The step of `seq`, when it is active on this digest.
    pub fn step_mut(&mut self, seq: u64) -> Option<&mut Step> {
        self.active.get_mut(&seq)
    }
}
