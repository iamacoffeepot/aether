//! The verify memo: filing green verdicts, and passing by identity on one
//! already filed (#4891).
//!
//! Two decisions, one rule. A verify verdict judges a tree and nothing else, so
//! the reducer files every green one under
//! [`VerifiedTree`](crate::VerifiedTree) — the tree, plus the identity of the
//! gates that judged it — and any later verify aimed at that same key passes on
//! the record instead of dispatching. The lookup itself lives on the record
//! ([`BloomRecord::verify_proof_for`]), so every position asks the question the
//! same way; what lives here is the minting of a proof and the receipt a hit
//! leaves behind.
//!
//! The two callers are the two verify positions of the line. The member
//! `Verify` reaches it through the passing verdict's
//! [`Fact::Integrate`](crate::Fact::Integrate) and through the dispatch its
//! cursor move would have made; `AggregateVerify` reaches it through the fold's
//! resolve and its own completion. A fold of one member is byte-identical to
//! that member's candidate, and a repair lap that only amended a commit message
//! leaves the tree it started from — both are hits, and both would otherwise
//! re-pay a full mechanical run for a verdict the journal already holds.

use super::Decision;
use crate::ids::{BloomId, StageId};
use crate::values::{Evidence, EvidenceKind, VerifyGateSet, VerifyProof, VerifyReuse};

/// File `evidence` as `stage`'s green proof of the tree it names, or `None` when
/// it is not a verification verdict at all.
///
/// The kind check is what keeps the memo honest at its source: only a
/// verification result attests that the gates ran and passed. A claim carrying
/// any other evidence — an inherited one, a hand-built fixture — records
/// nothing, and the tree it names stays unproven rather than acquiring a proof
/// from a verdict that never judged it.
pub(super) fn proof_of(bloom: BloomId, stage: StageId, evidence: &Evidence) -> Option<Decision> {
    (evidence.kind == EvidenceKind::VerificationResult).then(|| Decision::RecordVerifyProof {
        bloom,
        proof: VerifyProof { gate_set: VerifyGateSet::lane().digest(), stage, evidence: evidence.clone() },
    })
}

/// The receipt for `stage` passing by identity on `proof`.
pub(super) fn reuse_of(bloom: BloomId, stage: StageId, proof: &VerifyProof) -> Decision {
    Decision::RecordVerifyReuse { bloom, reuse: VerifyReuse { stage, proof: proof.clone() } }
}
