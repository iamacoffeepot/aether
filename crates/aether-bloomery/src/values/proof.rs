//! Verify proofs and their reuse: the memo that lets a verify position pass on
//! a tree its gates have already proven (#4891).
//!
//! A verify verdict is a fact about *content*, not about the position that
//! collected it — the mechanical fan-out reads a checked-out tree and nothing
//! else. So a green verdict over tree `T` under gate set `G` answers every later
//! verify of `T` under `G`, and re-running the fan-out buys nothing but the
//! wall-clock it costs. The two places that happens in practice are a
//! single-member bloom, whose fold is byte-identical to the candidate its member
//! already verified, and a repair lap that changed nothing the tree records (an
//! amended commit message leaves the same tree).
//!
//! What makes the reuse safe is the key. [`VerifiedTree`] pairs the tree with
//! the identity of the gates that proved it, so a proof recorded under one
//! verify vocabulary cannot satisfy another: a changed identity yields a
//! different [`VerifyGateSet::digest`], the lookup misses, and the gates run.
//! This is proof reuse by content identity, never a compiler cache — nothing is
//! shared between two trees that differ, because the key names the tree.

use alloc::string::String;

use serde::{Deserialize, Serialize};

use crate::digest::{ContentAddressed, Digest, digest_of};
use crate::ids::StageId;
use crate::values::{
    Evidence, NetworkProfile, VERIFY_BASE_COMMAND, VERIFY_CHECK_COMMAND, VERIFY_LANE_IMAGE, VERIFY_LANE_NETWORK,
};
use crate::values::{VerifyFailure, VerifyFailureSet};

/// The identity of the gate set one verify position runs (ADR-0178): the
/// verifier vocabulary, plus the lane that executes it.
///
/// The vocabulary is the whole of what a verify *proves* — each identity is one
/// gate the fan-out runs — and the lane is how it is executed, which is what
/// makes a verdict reproducible at all. A ninth verifier identity, a re-pointed
/// image, or a lane that gains egress all move the digest, so every proof
/// recorded under the old identity stops matching and the gates run again.
/// Nothing here is a wall-clock limit: a limit bounds the run without changing
/// what a pass proves.
///
/// Both verify positions — the member `Verify` over one candidate and
/// `AggregateVerify` over the fold — dispatch the same `verify.check` fan-out,
/// so they resolve to the same identity and a proof crosses between them. That
/// is the point: the fold of a single member *is* the candidate that member
/// already proved.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct VerifyGateSet {
    /// The verifier identities the fan-out runs, as the canonical ADR-0178 set.
    pub verifiers: VerifyFailureSet,
    /// The typed command the lane dispatches.
    pub command: String,
    /// The execution image the lane runs in.
    pub image: String,
    /// The network posture the lane runs under.
    pub network: NetworkProfile,
}

impl ContentAddressed for VerifyGateSet {
    const DOMAIN: &'static str = "aether.bloomery.verify_gate_set";
}

impl VerifyGateSet {
    /// The gate set the compiled verify lane runs — the calibration every
    /// verify position of this binary dispatches, named the way
    /// [`StageCatalog::line`](crate::StageCatalog::line) names the compiled
    /// line.
    ///
    /// Compiled rather than sealed because the vocabulary and the lane are both
    /// compiled today: the catalog authors a stage's profile, budget, and limit,
    /// none of which is a gate. A memo is journal-derived, so a binary that
    /// changes either half replays the old proofs under a digest that no longer
    /// matches and re-proves what it can no longer read as proven — the refusal
    /// is the ordinary consequence of keying on identity, not a migration step.
    /// When the gate set becomes an explicitly declared value it lands in this
    /// type and the memo keeps keying on exactly the same digest.
    ///
    /// The verifier set is the nine identities the compiled `verify.check`
    /// fan-out runs, listed by hand rather than derived from
    /// [`VerifyFailure::ALL`]. Containment is coordinator-side and never a lane
    /// member; picking it up from the vocabulary would re-key every stored
    /// [`VerifiedTree`] proof memo. A future identity the lane *does* run must
    /// be added here by hand — as [`VerifyFailure::Lock`] was (#5309), which
    /// re-keys every stored memo exactly as the doc above says it should,
    /// because the lane's gate set genuinely changed.
    #[must_use]
    pub fn lane() -> Self {
        Self {
            verifiers: [
                VerifyFailure::Preflight,
                VerifyFailure::Fmt,
                VerifyFailure::Clippy,
                VerifyFailure::Docs,
                VerifyFailure::Test,
                VerifyFailure::Dup,
                VerifyFailure::Deps,
                VerifyFailure::Suppress,
                VerifyFailure::Lock,
            ]
            .into_iter()
            .collect(),
            command: String::from(VERIFY_CHECK_COMMAND),
            image: String::from(VERIFY_LANE_IMAGE),
            network: VERIFY_LANE_NETWORK,
        }
    }

    /// The gate set the compiled whole-workspace base verify runs.
    ///
    /// Identical eight-plus-one verifier list, image, and network as
    /// [`Self::lane`]. The differing command is deliberate: a closure-narrowed
    /// member proof must not satisfy the whole-workspace base question, and the
    /// two proofs cannot be confused in one map.
    #[must_use]
    pub fn base() -> Self {
        Self { command: String::from(VERIFY_BASE_COMMAND), ..Self::lane() }
    }

    /// This gate set's content-addressed identity — the half of a
    /// [`VerifiedTree`] key that is not the tree.
    #[must_use]
    pub fn digest(&self) -> Digest {
        digest_of(self)
    }
}

/// The memo key: one tree, and the gate set that proved it.
///
/// Both halves are load-bearing. The tree alone would let a proof answer for
/// content it never saw; the gate set alone would let one vocabulary's verdict
/// satisfy another's. Keyed together, a hit means exactly "these gates have
/// already run over this content and passed".
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
pub struct VerifiedTree {
    /// The tree the verdict judged.
    pub tree: Digest,
    /// The [`VerifyGateSet::digest`] of the gates that judged it.
    pub gate_set: Digest,
}

/// One recorded green verify verdict — what a later verify of the same tree
/// under the same gates passes by identity on.
///
/// Holds the verdict's own [`Evidence`] rather than a copy of its parts, so a
/// reuse re-presents the exact evidence that was admitted: it already binds the
/// tree ([`Evidence::validates`]), and no second construction can drift from
/// what the lane actually returned.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct VerifyProof {
    /// The [`VerifyGateSet::digest`] the verdict was collected under.
    pub gate_set: Digest,
    /// The verify position that collected it — the member `Verify`, or
    /// `AggregateVerify` over a fold.
    pub stage: StageId,
    /// The green verdict, bound to the tree it judged.
    pub evidence: Evidence,
}

impl VerifyProof {
    /// The key this proof is filed under: its evidence's subject tree, paired
    /// with the gate set that proved it.
    #[must_use]
    pub fn verified(&self) -> VerifiedTree {
        VerifiedTree { tree: self.evidence.subject, gate_set: self.gate_set }
    }
}

/// A verify position that passed by identity, and the proof it reused — the
/// receipt a memo hit leaves behind.
///
/// Recorded because a landing that was proven by identity has to say so. Without
/// it the journal shows a stage that produced a verdict nothing dispatched, and
/// a reader cannot tell a reused proof from a gate that was quietly skipped.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct VerifyReuse {
    /// The verify position that passed without dispatching.
    pub stage: StageId,
    /// The proof it stood on.
    pub proof: VerifyProof,
}

#[cfg(test)]
mod tests {
    use core::iter::once;

    use super::{VerifyGateSet, VerifyProof};
    use crate::digest::Digest;
    use crate::ids::StageId;
    use crate::values::{Evidence, EvidenceKind, NetworkProfile, VERIFY_BASE_COMMAND, VerifyFailure};

    fn digest(seed: u8) -> Digest {
        Digest::from_bytes([seed; 32])
    }

    #[test]
    fn a_changed_gate_set_is_a_changed_identity() {
        // Tripwire: the whole safety of the memo is that a proof cannot answer
        // for a gate set it was not collected under. Each axis is checked
        // separately because each is a different way the gates can change —
        // dropping a verifier, re-pointing the image, opening the lane's egress
        // — and a digest that ignored any one of them would let that change
        // through as a reuse.
        let lane = VerifyGateSet::lane();

        for changed in [
            VerifyGateSet { verifiers: once(VerifyFailure::Fmt).collect(), ..lane.clone() },
            VerifyGateSet { image: "iama/verify:2".into(), ..lane.clone() },
            VerifyGateSet { network: NetworkProfile::Restricted, ..lane.clone() },
            VerifyGateSet { command: "verify.partial".into(), ..lane.clone() },
        ] {
            assert_ne!(changed.digest(), lane.digest(), "{changed:?} must not share the compiled lane's identity");
        }
    }

    #[test]
    fn a_proof_is_filed_under_the_tree_its_evidence_names() {
        // Tripwire: the key is derived from the evidence rather than recorded
        // beside it, so a proof can never be filed under a tree its verdict did
        // not judge.
        let proof = VerifyProof {
            gate_set: VerifyGateSet::lane().digest(),
            stage: StageId::Verify,
            evidence: Evidence { subject: digest(7), kind: EvidenceKind::VerificationResult, detail: digest(9) },
        };

        assert_eq!(proof.verified().tree, digest(7));
        assert_eq!(proof.verified().gate_set, VerifyGateSet::lane().digest());
    }

    #[test]
    fn the_base_gate_set_is_a_distinct_identity() {
        // Tripwire: a closure-narrowed member proof must not satisfy the
        // whole-workspace base question. The command is the half of the key
        // that makes them distinct.
        assert_ne!(VerifyGateSet::base().digest(), VerifyGateSet::lane().digest());
        assert_eq!(VerifyGateSet::base().command, VERIFY_BASE_COMMAND);
        assert_eq!(VerifyGateSet::base().verifiers, VerifyGateSet::lane().verifiers);
    }

    #[test]
    fn the_compiled_lane_does_not_run_containment() {
        // Tripwire: `VerifyGateSet::lane().digest()` is half of every
        // `VerifiedTree` memo key. Containment is not a `verify.check` member;
        // collecting it from `VerifyFailure::ALL` would re-key and re-prove the
        // verification ledger for a gate the lane never runs.
        let lane = VerifyGateSet::lane();
        assert!(
            !lane.verifiers.contains(VerifyFailure::Containment),
            "containment must not sit in the compiled lane's gate set"
        );
        assert_eq!(lane.verifiers.to_mask(), "02ff");
    }
}
