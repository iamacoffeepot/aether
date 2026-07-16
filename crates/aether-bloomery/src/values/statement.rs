//! Statements and their provenance (ADR-0149 §The value vocabulary).
//!
//! A statement is an artifact carrying words plus one of three provenance
//! claims. Only an *author signature* can become instruction; an
//! *observation attestation* is context, never command; a *stage receipt*
//! records that a configured agent profile ran one process over exact
//! inputs and produced exact outputs.

use alloc::string::String;
use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use crate::digest::{ContentAddressed, Digest};
use crate::ids::StageId;
use crate::sign::{KeyProvider, SignatureEnvelope};

/// An artifact carrying words plus exactly one provenance claim.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Statement {
    /// The asserted bytes — the exact bytes the provenance claim is over.
    pub words: Vec<u8>,
    /// How these words are grounded.
    pub provenance: Provenance,
    /// The parents in the derivation DAG this statement builds on.
    pub parents: Vec<Digest>,
}

impl Statement {
    /// Can these words become *instruction*? Only an author signature
    /// carries that authority (ADR-0149 §The value vocabulary).
    #[must_use]
    pub const fn is_instruction_capable(&self) -> bool {
        matches!(self.provenance, Provenance::AuthorSignature(_))
    }

    /// Verify the statement's provenance. An author signature verifies its
    /// envelope over the exact `words`; the two non-author claims carry no
    /// signature and so never verify as *authority* — they are context, not
    /// command, and this returns `false` for them.
    #[must_use]
    pub fn verify_authority(&self, keys: &dyn KeyProvider) -> bool {
        match &self.provenance {
            Provenance::AuthorSignature(envelope) => keys.verify(envelope, &self.words),
            Provenance::ObservationAttestation(_) | Provenance::StageReceipt(_) => false,
        }
    }
}

impl ContentAddressed for Statement {
    const DOMAIN: &'static str = "aether.bloomery.statement";
}

/// One of the three provenance claims a [`Statement`] can carry.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Provenance {
    /// A person asserted these exact bytes for this purpose — the only claim
    /// that can become instruction.
    AuthorSignature(SignatureEnvelope),
    /// An adapter saw these bytes elsewhere — context, never command.
    ObservationAttestation(Observation),
    /// A configured agent profile ran one process over exact inputs and
    /// produced exact outputs.
    StageReceipt(StageReceipt),
}

/// Where an adapter saw the observed bytes. An observation carries no
/// authority — it becomes intent only when a person adopts its exact digest
/// in a native signed statement (ADR-0149 §The boundary, second amendment).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Observation {
    /// A stable, human-readable source label (e.g. the adapter and the
    /// external object it mirrored).
    pub source: String,
}

/// A record that one stage binding ran: the profile, the exact inputs it
/// consumed, and the exact outputs it produced (ADR-0149 §The line).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct StageReceipt {
    /// The stage that ran.
    pub stage: StageId,
    /// The exact [`AgentProfile`] that ran, attested by digest — the value
    /// that makes "a configured agent profile ran one process" verifiable: a
    /// reader recomputes the profile's address to confirm the configuration.
    /// *Who* ran is not stored: the `iama-{stage}` worker identity is derived
    /// from [`stage`](Self::stage) via [`StageId::worker_identity`].
    ///
    /// [`AgentProfile`]: crate::values::AgentProfile
    pub profile: Digest,
    /// The exact inputs consumed, by digest.
    pub inputs: Vec<Digest>,
    /// The exact outputs produced, by digest.
    pub outputs: Vec<Digest>,
}
