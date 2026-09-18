//! Per-digest bundle states and request queues (ADR-0226 decision 3).
//!
//! The table maps each bundle digest to a state — `Reading`, `Declared`,
//! `Loading`, `Ready`, or `Unavailable` — and a FIFO of request seqs. One
//! request per digest is active from its section check until its `Invoked`
//! reply or its pre-`Invoke` fault, so at most one `Invoke` is ever in
//! flight per root. Requests for different digests proceed concurrently.
//! Only the first request for a digest reads the bundle artifact; the
//! decoded declarations stay cached for the engine's life, and the wasm
//! bytes are held only until the load. A digest that fails to read, decode,
//! or load becomes `Unavailable`: every later request for it faults with
//! the recorded reason, and no read or load is issued again.

use std::collections::{BTreeMap, VecDeque};

use aether_bloomery_kinds::{ClosureArtifact, Detail, Digest, Program};
use aether_data::MailboxId;

/// Lifecycle state of one bundle digest.
#[derive(Debug)]
pub enum DigestState {
    /// A bundle artifact read is in flight for the active request.
    Reading,
    /// Section decoded; the wasm is held only until the load.
    Declared {
        /// Cached declarations.
        programs: Vec<Program>,
        /// Wasm bytes, moved into the load command.
        wasm: Vec<u8>,
    },
    /// A load is in flight for the active request.
    Loading {
        /// Cached declarations, kept while the wasm loads.
        programs: Vec<Program>,
    },
    /// Loaded; the root and declarations stay cached for the engine's life.
    Ready {
        /// Mailbox of the digest-named root.
        root: MailboxId,
        /// Cached declarations.
        programs: Vec<Program>,
    },
    /// Read, decode, or load failed; fault everything with the reason.
    Unavailable {
        /// The recorded failure.
        reason: Detail,
    },
}

/// The one request driving a digest, from its section check to its outcome.
#[derive(Debug)]
pub struct Active {
    /// The `Requested` seq.
    pub seq: u64,
    /// The request's declaration, resolved by the name check.
    pub declaration: Option<Program>,
    /// The request's fetched closure, held for its `Invoke`.
    pub closure: Option<Vec<ClosureArtifact>>,
}

/// One digest's state, active request, and waiting FIFO.
#[derive(Debug)]
pub struct DigestQueue {
    pub state: DigestState,
    pub active: Option<Active>,
    pub waiting: VecDeque<u64>,
}

impl DigestQueue {
    /// The active request's seq, if one is driving this digest.
    pub fn active_seq(&self) -> Option<u64> {
        self.active.as_ref().map(|active| active.seq)
    }

    /// Whether `seq` is already driving or waiting on this digest.
    pub fn tracks(&self, seq: u64) -> bool {
        self.active_seq() == Some(seq) || self.waiting.contains(&seq)
    }
}

/// Digest-keyed bundle states. The reactor role and its "a digest serves one
/// role" check arrive with reactor routing; this table serves programs.
#[derive(Debug, Default)]
pub struct BundleTable {
    queues: BTreeMap<Digest, DigestQueue>,
}

impl BundleTable {
    pub fn queue(&self, bundle: &Digest) -> Option<&DigestQueue> {
        self.queues.get(bundle)
    }

    pub fn queue_mut(&mut self, bundle: &Digest) -> Option<&mut DigestQueue> {
        self.queues.get_mut(bundle)
    }

    pub fn insert(&mut self, bundle: Digest, queue: DigestQueue) {
        self.queues.insert(bundle, queue);
    }
}
