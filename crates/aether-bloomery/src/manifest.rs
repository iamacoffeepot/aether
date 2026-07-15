//! Prompt-manifest assembly, fail-closed (ADR-0149 §The value vocabulary).
//!
//! Every model call consumes a prompt manifest listing each slot by artifact
//! digest, role, and parent closure. Assembly is **fail-closed**: an
//! instruction-capable slot that does not trace to a signed statement or a
//! versioned policy artifact rejects the attempt *before dispatch*. Unlike
//! the signature mechanism (stubbed in v1, [`crate::sign`]), this closure is
//! structural and enforced from day one — it is the load-bearing guard that
//! keeps a model from being instructed by ungrounded bytes.

use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use crate::digest::Digest;
use crate::sign::KeyProvider;
use crate::values::Statement;

/// One slot of a prompt manifest: an artifact, the role it plays in the
/// prompt, and the parent closure it traces to.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Slot {
    /// The artifact filling this slot, by digest.
    pub artifact: Digest,
    /// The role the artifact plays.
    pub role: SlotRole,
    /// The parent-closure digests this slot's artifact derives from.
    pub parent_closure: Vec<Digest>,
}

/// The role a slot's artifact plays in the assembled prompt.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum SlotRole {
    /// Instruction — command the model acts on. Only this role is subject to
    /// the fail-closed provenance closure.
    Instruction,
    /// Context — material the model may read but is not instructed by.
    Context,
    /// Reference — a named prior artifact, non-instructional.
    Reference,
}

/// A fully assembled, closure-checked prompt manifest.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct PromptManifest {
    /// The slots, in prompt order.
    pub slots: Vec<Slot>,
}

/// Resolves an artifact digest to the provenance that grounds it. The
/// concrete index is the host's; the trait is the pure contract manifest
/// assembly calls through.
pub trait ProvenanceIndex {
    /// The statement backing `digest`, if the digest names one.
    fn statement(&self, digest: &Digest) -> Option<&Statement>;

    /// Is `digest` a versioned policy artifact? Policy is the second
    /// admissible ground for an instruction slot (ADR-0149 §The value
    /// vocabulary).
    fn is_versioned_policy(&self, digest: &Digest) -> bool;
}

/// Why manifest assembly refused. Every variant names the exact slot that
/// failed the closure, so the refusal is auditable.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum ClosureViolation {
    /// An instruction slot traced to neither a signed statement nor a
    /// versioned policy artifact.
    UngroundedInstruction {
        /// The offending slot's artifact digest.
        slot: Digest,
    },
    /// An instruction slot traced to a statement that is not
    /// instruction-capable (its provenance is not an author signature).
    NonAuthorInstruction {
        /// The offending slot's artifact digest.
        slot: Digest,
    },
    /// An instruction slot's statement is instruction-capable but its
    /// signature did not verify.
    UnverifiedSignature {
        /// The offending slot's artifact digest.
        slot: Digest,
    },
}

/// Assemble a prompt manifest, enforcing the fail-closed closure over every
/// instruction slot.
///
/// An instruction slot is admissible only when it traces to *either* a
/// versioned policy artifact *or* an author-signed, instruction-capable
/// statement whose signature verifies against `keys`. Context and reference
/// slots carry no such requirement. The first violation short-circuits — the
/// attempt is refused before any dispatch.
///
/// # Errors
///
/// Returns the first [`ClosureViolation`] an instruction slot triggers.
pub fn assemble_manifest(
    slots: Vec<Slot>,
    index: &dyn ProvenanceIndex,
    keys: &dyn KeyProvider,
) -> Result<PromptManifest, ClosureViolation> {
    for slot in &slots {
        if slot.role != SlotRole::Instruction {
            continue;
        }
        if index.is_versioned_policy(&slot.artifact) {
            continue;
        }
        match index.statement(&slot.artifact) {
            None => return Err(ClosureViolation::UngroundedInstruction { slot: slot.artifact }),
            Some(statement) if !statement.is_instruction_capable() => {
                return Err(ClosureViolation::NonAuthorInstruction { slot: slot.artifact });
            }
            Some(statement) if !statement.verify_authority(keys) => {
                return Err(ClosureViolation::UnverifiedSignature { slot: slot.artifact });
            }
            Some(_) => {}
        }
    }
    Ok(PromptManifest { slots })
}
