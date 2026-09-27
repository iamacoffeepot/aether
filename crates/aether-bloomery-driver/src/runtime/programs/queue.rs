//! One digest's program request queue: the active request's step plus its waiting FIFO.

use std::collections::VecDeque;

use aether_bloomery_kinds::{ClosureArtifact, Program};

/// The active request's progress through the pipeline.
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

/// The one request driving a digest, from its section check to its outcome.
#[derive(Debug)]
pub struct Active {
    /// The `Requested` seq.
    pub seq: u64,
    /// The request's progress.
    pub step: Step,
}

/// One digest's active request and waiting FIFO.
#[derive(Debug, Default)]
pub struct DigestQueue {
    /// The request driving this digest, if any.
    pub active: Option<Active>,
    /// Seqs waiting their turn, in FIFO order.
    pub waiting: VecDeque<u64>,
}

impl DigestQueue {
    /// The active request's step, when `seq` is the one driving this digest.
    pub fn step(&self, seq: u64) -> Option<&Step> {
        self.active.as_ref().filter(|active| active.seq == seq).map(|active| &active.step)
    }

    /// The active request's step, when `seq` is the one driving this digest.
    pub fn step_mut(&mut self, seq: u64) -> Option<&mut Step> {
        self.active.as_mut().filter(|active| active.seq == seq).map(|active| &mut active.step)
    }
}
