//! Mail a native driver exchanges with a program bundle.

use alloc::vec::Vec;

use aether_data::KindId;

use crate::program::fault::Detail;
use crate::program::name::ProgramName;
use crate::program::refusal::Refusal;
use crate::{Digest, EncodedArtifact, artifact_digest};

/// One artifact from the input's transitive closure, without a stored digest.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Schema)]
pub struct ClosureArtifact {
    kind: KindId,
    bytes: Vec<u8>,
}

impl ClosureArtifact {
    /// Carry `bytes` under `kind`. The digest is computed, never stored.
    #[must_use]
    pub fn new(kind: KindId, bytes: Vec<u8>) -> Self {
        Self { kind, bytes }
    }

    /// Stored artifact kind; the blob's prefix.
    #[must_use]
    pub const fn kind(&self) -> KindId {
        self.kind
    }

    /// Encoded storage payload without its kind prefix.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Take the kind and payload without copying payload bytes.
    #[must_use]
    pub fn into_parts(self) -> (KindId, Vec<u8>) {
        (self.kind, self.bytes)
    }

    /// Digest of the kind-prefixed blob.
    #[must_use]
    pub fn digest(&self) -> Digest {
        artifact_digest(self.kind, &self.bytes)
    }
}

/// Ask a bundle to run one named program over an injected closure.
#[aether_data::kind(name = "aether.bloomery.program.invoke", eq, no_serde)]
pub struct Invoke {
    seq: u64,
    program: ProgramName,
    input: Digest,
    closure: Vec<ClosureArtifact>,
}

impl Invoke {
    /// Address program `program` at `input`, injecting `closure`.
    ///
    /// `seq` is the driver's `Requested` sequence.
    #[must_use]
    pub fn new(seq: u64, program: ProgramName, input: Digest, closure: Vec<ClosureArtifact>) -> Self {
        Self { seq, program, input, closure }
    }

    /// Driver's `Requested` sequence for this invocation.
    #[must_use]
    pub const fn seq(&self) -> u64 {
        self.seq
    }

    /// Program name unique within the bundle.
    #[must_use]
    pub const fn program(&self) -> &ProgramName {
        &self.program
    }

    /// Digest of the input artifact, which must appear in [`Self::closure`].
    #[must_use]
    pub const fn input(&self) -> Digest {
        self.input
    }

    /// Transitive closure the guest may read.
    #[must_use]
    pub fn closure(&self) -> &[ClosureArtifact] {
        &self.closure
    }

    /// Take the sequence, program, input, and closure.
    #[must_use]
    pub fn into_parts(self) -> (u64, ProgramName, Digest, Vec<ClosureArtifact>) {
        (self.seq, self.program, self.input, self.closure)
    }
}

/// Reply to one [`Invoke`]. Written by the bundle; the driver records the journal event.
#[aether_data::kind(name = "aether.bloomery.program.invoked", eq, no_serde)]
pub enum Invoked {
    /// The program ran and staged these artifacts, rooted at `result`.
    Completed { seq: u64, result: Digest, staged: Vec<EncodedArtifact> },
    /// The program returned a [`Refusal`].
    Refused { seq: u64, refusal: Refusal },
    /// The bundle refused the request as protocol, not as a program outcome.
    Rejected { seq: u64, reason: Detail },
}
