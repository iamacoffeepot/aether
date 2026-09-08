//! Verify proofs and their reuse: the memo that lets a verify position pass on
//! a tree its gates have already proven (#4891).
//!
//! A verify verdict is a fact about *content*, not about the position that
//! collected it — the mechanical fan-out reads a checked-out tree and nothing
//! else. So a green verdict over tree `T` under gate set `G` answers every later
//! verify of `T` under `G`, and re-running the fan-out buys nothing but the
//! wall-clock it costs. The case that happens in practice is a repair lap that
//! changed nothing the tree records: an amended commit message leaves the tree
//! it started from, and the member's own gates have already judged it.
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
use crate::values::VerifyFailureSet;
use crate::values::{
    Evidence, NetworkProfile, PipelineManifest, VERIFY_BASE_COMMAND, VERIFY_CHECK_COMMAND, VERIFY_LANE_IMAGE,
    VERIFY_LANE_NETWORK, VERIFY_MEMBER_COMMAND,
};

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
/// The three verify positions of the line resolve to three identities, one each:
/// [`Self::member`] over one candidate, [`Self::fold`] over the woven tree, and
/// [`Self::base`] over a sealed base. A proof crosses between two positions only
/// when they share an identity, and none of them do — so the fold asks its own
/// question over the fold, whatever a member proved about the candidate inside
/// it.
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
    /// The gate set the compiled fold verify lane runs — [`Self::fold_of`]
    /// over [`PipelineManifest::compiled`].
    ///
    /// Callers holding a sealed bloom read [`Self::fold_of`] against that
    /// bloom's record. This projection is what a caller with no manifest
    /// has, and what a pre-manifest bloom's compiled fallback produces.
    #[must_use]
    pub fn fold() -> Self {
        Self::fold_of(&PipelineManifest::compiled())
    }

    /// The gate set `manifest` declares for the fold (`verify.check`).
    ///
    /// Verifiers come from `[verifiers.runs]` for that command. Image and
    /// network stay compiled: the manifest declares vocabulary, never
    /// confinement.
    #[must_use]
    pub fn fold_of(manifest: &PipelineManifest) -> Self {
        Self::of(manifest, VERIFY_CHECK_COMMAND)
    }

    /// The gate set the compiled member verify lane runs — [`Self::member_of`]
    /// over [`PipelineManifest::compiled`].
    ///
    /// [`VerifyFailure::Docs`] is absent from that compiled run list, and that
    /// absence is the whole of what separates this vocabulary from
    /// [`Self::fold`]'s. Documentation correctness is a whole-workspace
    /// property — an intra-doc link resolves across crates, so a member's
    /// closure can neither break it alone nor prove it alone. A distinct
    /// command as well as a distinct verifier list, because the worker has to
    /// dispatch a different fan-out and the digest has to move for both
    /// reasons independently.
    #[must_use]
    pub fn member() -> Self {
        Self::member_of(&PipelineManifest::compiled())
    }

    /// The gate set `manifest` declares for the member position (`verify.member`).
    ///
    /// The per-position fan-out is data: a manifest that omits an identity
    /// from this command's run yields a member set without it, even when the
    /// fold run still names it.
    #[must_use]
    pub fn member_of(manifest: &PipelineManifest) -> Self {
        Self::of(manifest, VERIFY_MEMBER_COMMAND)
    }

    /// The gate set the compiled whole-workspace base verify runs —
    /// [`Self::base_of`] over [`PipelineManifest::compiled`].
    ///
    /// Identical verifier list, image, and network as [`Self::fold`] in the
    /// compiled vocabulary — [`VerifyFailure::Docs`] included, which is what
    /// lets a landing mint a base receipt from a fold proof at all. The
    /// differing command is deliberate: a closure-narrowed proof must not
    /// satisfy the whole-workspace base question.
    #[must_use]
    pub fn base() -> Self {
        Self::base_of(&PipelineManifest::compiled())
    }

    /// The gate set `manifest` declares for whole-workspace base verify
    /// (`verify.base`).
    #[must_use]
    pub fn base_of(manifest: &PipelineManifest) -> Self {
        Self::of(manifest, VERIFY_BASE_COMMAND)
    }

    /// The gate set the compiled verify position `stage` runs, or `None` when
    /// `stage` is not a verify position at all.
    ///
    /// The compiled projection of [`Self::for_stage_of`]. Production filing
    /// and lookup go through the bloom's sealed manifest.
    #[must_use]
    pub fn for_stage(stage: StageId) -> Option<Self> {
        Self::for_stage_of(stage, &PipelineManifest::compiled())
    }

    /// The gate set `manifest` declares for verify position `stage`, or `None`
    /// when `stage` is not a verify position at all.
    ///
    /// The one mapping from position to identity, so a proof is filed and looked
    /// up under the same key by construction rather than by two call sites
    /// agreeing. A stage that dispatches no verify has no gate set to name, and
    /// answering with one would let a proof be filed for gates that never ran.
    #[must_use]
    pub fn for_stage_of(stage: StageId, manifest: &PipelineManifest) -> Option<Self> {
        match stage {
            StageId::Verify => Some(Self::member_of(manifest)),
            StageId::AggregateVerify => Some(Self::fold_of(manifest)),
            StageId::BaseVerify => Some(Self::base_of(manifest)),
            _ => None,
        }
    }

    /// One position's gate set from `manifest`'s `[verifiers.runs]` for
    /// `command`.
    ///
    /// Identities the vocabulary does not intern are dropped rather than
    /// invented: a run list cannot extend the repair loop past what the
    /// bloom sealed. Image and network are the coordinator's compiled
    /// confinement, never a field the tree named.
    fn of(manifest: &PipelineManifest, command: &str) -> Self {
        Self {
            verifiers: manifest
                .verifiers
                .runs
                .get(command)
                .into_iter()
                .flatten()
                .filter_map(|name| manifest.intern(name))
                .collect(),
            command: String::from(command),
            image: String::from(VERIFY_LANE_IMAGE),
            network: VERIFY_LANE_NETWORK,
        }
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
    use crate::values::stage::dispatched_command;
    use crate::values::{
        Evidence, EvidenceKind, NetworkProfile, PipelineManifest, VERIFY_BASE_COMMAND, VERIFY_CHECK_COMMAND,
        VERIFY_MEMBER_COMMAND, VerifyFailure, VerifyFailureSet,
    };

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
        let fold = VerifyGateSet::fold();

        for changed in [
            VerifyGateSet { verifiers: once(VerifyFailure::Fmt).collect(), ..fold.clone() },
            VerifyGateSet { image: "iama/verify:2".into(), ..fold.clone() },
            VerifyGateSet { network: NetworkProfile::Restricted, ..fold.clone() },
            VerifyGateSet { command: "verify.partial".into(), ..fold.clone() },
        ] {
            assert_ne!(changed.digest(), fold.digest(), "{changed:?} must not share the compiled lane's identity");
        }
    }

    #[test]
    fn a_proof_is_filed_under_the_tree_its_evidence_names() {
        // Tripwire: the key is derived from the evidence rather than recorded
        // beside it, so a proof can never be filed under a tree its verdict did
        // not judge.
        let proof = VerifyProof {
            gate_set: VerifyGateSet::member().digest(),
            stage: StageId::Verify,
            evidence: Evidence { subject: digest(7), kind: EvidenceKind::VerificationResult, detail: digest(9) },
        };

        assert_eq!(proof.verified().tree, digest(7));
        assert_eq!(proof.verified().gate_set, VerifyGateSet::member().digest());
    }

    #[test]
    fn the_base_gate_set_is_a_distinct_identity() {
        // Tripwire: a closure-narrowed proof must not satisfy the
        // whole-workspace base question. The command is the half of the key
        // that makes them distinct.
        assert_ne!(VerifyGateSet::base().digest(), VerifyGateSet::fold().digest());
        assert_eq!(VerifyGateSet::base().command, VERIFY_BASE_COMMAND);
        assert_eq!(VerifyGateSet::base().verifiers, VerifyGateSet::fold().verifiers);
    }

    #[test]
    fn the_member_gate_set_is_the_fold_set_less_documentation() {
        // Tripwire: the member position stopped running `verify.docs`, and the
        // one thing that keeps that honest is its gate set saying so. Both
        // directions are the invariant. A member set that regained the identity
        // would claim a gate the fan-out no longer dispatches; a fold or base
        // set that lost it would let a landing mint a whole-workspace receipt
        // out of a chain where nothing ever built the documentation — and
        // `verify.docs` is precisely the gate no member's closure can answer
        // for, because an intra-doc link resolves across crates.
        //
        // The masks are computed from the bit each identity carries, so they
        // drift the moment either vocabulary does.
        let member = VerifyGateSet::member();

        assert!(!member.verifiers.contains(VerifyFailure::Docs), "the member position does not run documentation");
        assert!(VerifyGateSet::fold().verifiers.contains(VerifyFailure::Docs), "the fold does");
        assert!(VerifyGateSet::base().verifiers.contains(VerifyFailure::Docs), "and so does the base");
        assert_eq!(member.verifiers.to_mask(), "02f7");
        assert_eq!(VerifyGateSet::fold().verifiers.to_mask(), "02ff");
        assert_ne!(member.digest(), VerifyGateSet::fold().digest());
        assert_ne!(member.digest(), VerifyGateSet::base().digest());
    }

    #[test]
    fn a_gate_set_names_the_command_its_position_dispatches() {
        // Tripwire: the gate set's `command` is half the memo key and the
        // dispatched command is what actually runs, so a drift between them
        // files a proof under gates no worker ever ran. They are two matches on
        // `StageId` in two modules, which is exactly the pair that can drift.
        for &stage in StageId::ALL {
            match VerifyGateSet::for_stage(stage) {
                Some(gates) => assert_eq!(
                    Some(gates.command.as_str()),
                    dispatched_command(stage),
                    "{stage:?} keys its proofs under a command its dispatch does not name",
                ),
                None => assert!(
                    dispatched_command(stage).is_none_or(|command| !command.starts_with("verify.")),
                    "{stage:?} dispatches a verify with no gate set to file its proof under",
                ),
            }
        }
        assert_eq!(VerifyGateSet::member().command, VERIFY_MEMBER_COMMAND);
    }

    #[test]
    fn compiled_gate_sets_keep_the_memo_key() {
        // Tripwire: the verify-proof memo is keyed on VerifiedTree, whose
        // gate-set half is this digest. Reading [verifiers.runs] must produce
        // byte-identical gate sets or every stored proof misses and the ledger
        // re-proves. These literals were printed by CI on origin/main at
        // 129bcf361 (GitHub Actions run 34263194750), before this slice inverted
        // the constructors; they are not recomputed from live code.
        let compiled = PipelineManifest::compiled();
        assert_eq!(
            VerifyGateSet::fold_of(&compiled).digest(),
            Digest::pinned("0104f14726e7c5ced359beb39daa5ec92f684676eaaa4e875b3eb39ac53fb1bc"),
        );
        assert_eq!(
            VerifyGateSet::member_of(&compiled).digest(),
            Digest::pinned("c80ad4b275af605c6aec748e7c530debc65642ef750ee5cf6cbe79523744960d"),
        );
        assert_eq!(
            VerifyGateSet::base_of(&compiled).digest(),
            Digest::pinned("b9ccc1951502b07e3c04bf48afa2b59255abadf260882cb20709b1ace869e2d7"),
        );
    }

    #[test]
    fn a_member_run_that_omits_docs_does_not_take_them_from_the_fold() {
        // Tripwire: the per-position fan-out is data. A hardcoded
        // `member = fold − docs` would keep `verify.dup` even when the
        // member run omitted it, and would drop `verify.docs` even when a
        // future manifest put it back. The member set is exactly the interned
        // member run list; the fold set is independently the interned fold
        // run list.
        let mut manifest = PipelineManifest::compiled();
        let member_runs = [
            VerifyFailure::Preflight,
            VerifyFailure::Fmt,
            VerifyFailure::Clippy,
            VerifyFailure::Test,
            VerifyFailure::Deps,
            VerifyFailure::Suppress,
            VerifyFailure::Lock,
        ];
        manifest.verifiers.runs.insert(
            String::from(VERIFY_MEMBER_COMMAND),
            member_runs.iter().map(|identity| String::from(identity.as_str())).collect(),
        );
        manifest.verifiers.runs.insert(
            String::from(VERIFY_CHECK_COMMAND),
            [
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
            .iter()
            .map(|identity| String::from(identity.as_str()))
            .collect(),
        );

        let member = VerifyGateSet::member_of(&manifest);
        let fold = VerifyGateSet::fold_of(&manifest);

        assert!(!member.verifiers.contains(VerifyFailure::Docs), "the member run omitted documentation");
        assert!(
            !member.verifiers.contains(VerifyFailure::Dup),
            "and omitted dup — that omission is data, not fold−docs"
        );
        assert!(fold.verifiers.contains(VerifyFailure::Docs), "the fold run still names documentation");
        assert!(fold.verifiers.contains(VerifyFailure::Dup), "and still names dup");
        assert_eq!(member.verifiers, member_runs.into_iter().collect::<VerifyFailureSet>());
        assert_ne!(member.digest(), VerifyGateSet::member().digest(), "a custom run list is a different identity");
    }

    #[test]
    fn the_compiled_lane_does_not_run_containment() {
        // Tripwire: `VerifyGateSet::fold().digest()` is half of every
        // `VerifiedTree` memo key. Containment is not a `verify.check` member;
        // collecting it from `VerifyFailure::ALL` would re-key and re-prove the
        // verification ledger for a gate the lane never runs.
        let fold = VerifyGateSet::fold();
        assert!(
            !fold.verifiers.contains(VerifyFailure::Containment),
            "containment must not sit in the compiled lane's gate set"
        );
        assert!(!VerifyGateSet::member().verifiers.contains(VerifyFailure::Containment), "nor in the member's");
    }
}
