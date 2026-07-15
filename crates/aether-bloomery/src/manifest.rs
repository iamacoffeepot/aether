//! Prompt-manifest assembly, fail-closed (ADR-0149 §The value vocabulary).
//!
//! Every model call consumes a prompt manifest listing each slot by artifact
//! digest, role, and parent closure. Assembly is **fail-closed**: an
//! instruction-capable slot whose derivation closure does not trace to a
//! signed statement or a versioned policy artifact rejects the attempt
//! *before dispatch*. Unlike the signature mechanism (stubbed in v1,
//! [`crate::sign`]), this closure is structural and enforced from day one — it
//! is the load-bearing guard that keeps a model from being instructed by
//! ungrounded bytes.
//!
//! # The closure walk
//!
//! Grounding is not depth-0: an instruction artifact that *derives from* a
//! signed statement (a scoper output, a refined candidate) is admitted, not
//! rejected. [`assemble_manifest`] walks each instruction slot's derivation
//! closure — starting at the slot artifact and expanding through
//! [`ProvenanceIndex::parents`] — and admits on the first node that is a
//! versioned policy or a signed, instruction-capable, verifying statement.
//! The walk is **iterative and budget-capped** (an explicit work-stack plus a
//! visited-set, per the repo recursion rule): a cyclic parent graph is bounded
//! by the visited-set, and an over-long one by [`MANIFEST_CLOSURE_BUDGET`],
//! each refusing rather than overflowing the stack.
//!
//! The authoritative parent source is [`ProvenanceIndex::parents`] alone —
//! never the caller-declared [`Slot::parent_closure`], which the walk ignores
//! for grounding. On admit, the assembler *authors* the closure it actually
//! traced back into the output slot's `parent_closure`, so the field is an
//! audit record of what was verified, not an unchecked caller claim. A forged
//! closure can therefore never ground a slot.

use alloc::collections::BTreeSet;
use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use crate::digest::Digest;
use crate::sign::KeyProvider;
use crate::values::Statement;

/// The maximum number of derivation nodes [`assemble_manifest`]'s closure walk
/// visits before refusing an instruction slot with
/// [`ClosureViolation::ClosureBudgetExceeded`]. Derivation closures are shallow
/// in practice; a walk that exceeds this is a pathological or adversarial
/// parent graph, refused rather than allowed to run unbounded.
pub const MANIFEST_CLOSURE_BUDGET: usize = 1024;

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

    /// The artifact's *recorded* derivation parents, or `None` when the index
    /// does not know the artifact. This is the only authoritative source the
    /// closure walk trusts — the caller-declared [`Slot::parent_closure`] is
    /// never trusted for grounding. A known artifact with no parents returns
    /// `Some(empty)`; an unknown one returns `None` (a broken derivation edge).
    fn parents(&self, digest: &Digest) -> Option<Vec<Digest>>;
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
    /// An instruction slot's derivation closure exceeded
    /// [`MANIFEST_CLOSURE_BUDGET`] nodes before reaching a ground — a
    /// pathological or adversarial parent graph, refused rather than walked
    /// unbounded.
    ClosureBudgetExceeded {
        /// The offending slot's artifact digest.
        slot: Digest,
    },
}

/// How far into the closure the walk got before failing, so a refusal reports
/// the *most-advanced* reason reached (a closure that touches an
/// instruction-capable-but-unverified statement is a more informative refusal
/// than one that touches only ungrounded bytes). `Ord` ranks least- to
/// most-advanced.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Advance {
    /// Reached no statement or policy at all.
    Ungrounded,
    /// Reached a statement that is not instruction-capable.
    NonAuthor,
    /// Reached an instruction-capable statement whose signature did not verify.
    Unverified,
}

impl Advance {
    /// The closure violation this failure level names, for the given slot.
    fn violation(self, slot: Digest) -> ClosureViolation {
        match self {
            Self::Ungrounded => ClosureViolation::UngroundedInstruction { slot },
            Self::NonAuthor => ClosureViolation::NonAuthorInstruction { slot },
            Self::Unverified => ClosureViolation::UnverifiedSignature { slot },
        }
    }
}

/// Assemble a prompt manifest, enforcing the fail-closed closure over every
/// instruction slot.
///
/// An instruction slot is admissible only when its derivation closure — walked
/// from the slot artifact through [`ProvenanceIndex::parents`] — reaches
/// *either* a versioned policy artifact *or* an author-signed,
/// instruction-capable statement whose signature verifies against `keys`. The
/// walk is iterative and budget-capped (see [`MANIFEST_CLOSURE_BUDGET`]).
/// Context and reference slots carry no such requirement. The first violating
/// slot short-circuits — the attempt is refused before any dispatch.
///
/// On admit, each instruction slot's `parent_closure` is *rewritten* to the
/// closure the walk actually traced (the caller-declared value is ignored), so
/// the manifest records what the assembler verified.
///
/// # Errors
///
/// Returns the first [`ClosureViolation`] an instruction slot triggers. A slot
/// whose closure is exhausted without a ground reports the most-advanced
/// failure it reached ([`ClosureViolation::UnverifiedSignature`] >
/// [`ClosureViolation::NonAuthorInstruction`] >
/// [`ClosureViolation::UngroundedInstruction`]); one whose closure exceeds the
/// budget reports [`ClosureViolation::ClosureBudgetExceeded`].
pub fn assemble_manifest(
    slots: Vec<Slot>,
    index: &dyn ProvenanceIndex,
    keys: &dyn KeyProvider,
) -> Result<PromptManifest, ClosureViolation> {
    let mut authored = Vec::with_capacity(slots.len());
    for slot in slots {
        if slot.role != SlotRole::Instruction {
            authored.push(slot);
            continue;
        }
        let closure = ground_instruction(slot.artifact, index, keys)?;
        authored.push(Slot { parent_closure: closure, ..slot });
    }
    Ok(PromptManifest { slots: authored })
}

/// Walk one instruction slot's derivation closure and return the traced
/// closure on admit, or the closure violation on refusal.
///
/// Iterative by construction (work-stack + visited-set, per the repo recursion
/// rule): a cyclic parent graph terminates via the visited-set, an over-long
/// one via [`MANIFEST_CLOSURE_BUDGET`]. The returned closure is the sorted set
/// of derivation ancestors the walk visited to reach the ground (empty for a
/// depth-0 ground, where the artifact is itself the policy or statement).
fn ground_instruction(
    artifact: Digest,
    index: &dyn ProvenanceIndex,
    keys: &dyn KeyProvider,
) -> Result<Vec<Digest>, ClosureViolation> {
    let mut stack = Vec::new();
    stack.push(artifact);
    let mut visited = BTreeSet::new();
    visited.insert(artifact);
    let mut traced = Vec::new();
    let mut most_advanced = Advance::Ungrounded;

    while let Some(node) = stack.pop() {
        if node != artifact {
            traced.push(node);
        }

        if index.is_versioned_policy(&node) {
            traced.sort_unstable();
            return Ok(traced);
        }
        if let Some(statement) = index.statement(&node) {
            if !statement.is_instruction_capable() {
                most_advanced = most_advanced.max(Advance::NonAuthor);
            } else if statement.verify_authority(keys) {
                traced.sort_unstable();
                return Ok(traced);
            } else {
                most_advanced = most_advanced.max(Advance::Unverified);
            }
        }

        if let Some(parents) = index.parents(&node) {
            for parent in parents {
                if visited.insert(parent) {
                    // Cap total distinct nodes at insertion, not just at pop, so
                    // a single high-fan-out node cannot grow the stack and
                    // visited-set unboundedly before the next pop is examined.
                    if visited.len() > MANIFEST_CLOSURE_BUDGET {
                        return Err(ClosureViolation::ClosureBudgetExceeded { slot: artifact });
                    }
                    stack.push(parent);
                }
            }
        }
    }

    Err(most_advanced.violation(artifact))
}
