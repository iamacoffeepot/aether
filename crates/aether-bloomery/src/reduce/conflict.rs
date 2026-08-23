//! The conflict workpiece (ADR-0210): the synthetic subject minted when two
//! candidates that each verified green alone refuse to build together.
//!
//! ADR-0191 gave the fold a subject — the composition — for defects discovered
//! in the woven tree as a whole. A collision between exactly two candidates is
//! narrower than that and needs a narrower owner: the weave is fine, the two
//! intents are fine, and only their coexistence is not. Charging it to the
//! composition would put the whole bloom's tree under repair for a two-file
//! disagreement; charging it to whichever member happened to be verified on the
//! fold puts a lane in front of code it never wrote, which is the refusal the
//! estate spent three hours on.
//!
//! So the collision gets its own subject. It takes the same maps a member takes
//! — a cursor in [`BloomRecord::progress`], a wedge in [`BloomRecord::wedged`],
//! a dispatch slot — keyed by a [`WorkpieceId::conflict`] id that names both
//! parents. It has no commission and needs no approval: its bound is the union
//! of what its two parents were already approved at, so it reaches no path an
//! approval does not already cover.
//!
//! What this deliberately does *not* emit is the point. No cursor move for the
//! member whose Verify produced the verdict, no revoked resolution, and no
//! dispatch naming either parent. The parents are finished; the collision is
//! somebody else's job.

use alloc::vec::Vec;

use super::attempt::{DispatchTargets, SealedLine, move_effects_with_candidate};
use super::composition::composition_progress;
use super::{BloomRecord, BloomStatus, ConflictAttributedError, Decision, Decisions, Outcome, Snapshot};
use crate::digest::Digest;
use crate::ids::{BloomId, StageId, WorkpieceId};
use crate::values::{CandidateRef, ConflictAttribution, Evidence, VerifyFailureSet, Wedge};

/// The repair allowance one collision gets: the sealed catalog's `Construct`
/// budget.
///
/// `Construct` rather than `Refine`, because the conflict workpiece authors a
/// candidate from nothing rather than repairing one of its own — its first lap
/// is a first attempt at the only thing it will ever do. The composition's
/// weave repair takes the `Refine` budget for the mirror-image reason.
fn repair_budget(record: &BloomRecord) -> u32 {
    record.stage_catalog.retry_budget_of(StageId::Construct).unwrap_or(1)
}

/// The line a conflict workpiece dispatches under: the bloom's own
/// configuration and sealed catalog, over the base both parents built onto.
///
/// Bloom-wide rather than layered with either parent's registry. Layering one
/// parent's would give that parent's model override standing over a subject
/// whose whole objective is to favour neither.
fn conflict_line(record: &BloomRecord) -> SealedLine<'_> {
    SealedLine {
        configs: record.spec.configs().clone(),
        catalog: &record.stage_catalog,
        base: record.spec.base(),
        held: record.operator_hold.is_some(),
        base_proven: record.base_proven,
    }
}

/// Reduce a host attribution of a failing fold (ADR-0210).
///
/// The refusal ladder, in order: an unknown or non-`Sealed` bloom; an
/// attribution that does not name exactly two parents; a parent that is not a
/// member of this bloom or has been withdrawn; and the verified member turning
/// up as its own parent. Each of those is a reason to leave the verdict where
/// the host found it rather than mint a subject over a membership that cannot
/// support it.
///
/// Past the ladder the effects are: the verdict on the journal, then either the
/// conflict workpiece's cursor move plus its dispatch, or — when its budget is
/// already spent — its wedge. A second attribution of the same tree onto the
/// same pair files only the verdict: one refused fold buys one repair lap, and
/// a second dispatch would set two lanes on one seam.
pub(super) fn reduce_conflict_attributed(
    snapshot: &Snapshot,
    bloom: &BloomId,
    verified: &WorkpieceId,
    tree: Digest,
    head: Digest,
    evidence: &Evidence,
    attribution: &ConflictAttribution,
) -> Decisions {
    let Some(record) = snapshot.blooms.get(bloom) else {
        return Decisions::rejected(Outcome::ConflictRejected(ConflictAttributedError::UnknownOrInactiveBloom));
    };
    if record.status != BloomStatus::Sealed {
        return Decisions::rejected(Outcome::ConflictRejected(ConflictAttributedError::UnknownOrInactiveBloom));
    }
    let Some(workpiece) = attribution.workpiece() else {
        return Decisions::rejected(Outcome::ConflictRejected(ConflictAttributedError::NotTwoParents(
            attribution.parents.len(),
        )));
    };
    for parent in &attribution.parents {
        if !record.spec.members().iter().any(|member| member.workpiece == *parent) {
            return Decisions::rejected(Outcome::ConflictRejected(ConflictAttributedError::NotAMember(parent.clone())));
        }
        if record.withdrawn.contains_key(parent) {
            return Decisions::rejected(Outcome::ConflictRejected(ConflictAttributedError::ParentWithdrawn(
                parent.clone(),
            )));
        }
        if parent == verified {
            return Decisions::rejected(Outcome::ConflictRejected(ConflictAttributedError::VerifiedIsParent(
                parent.clone(),
            )));
        }
    }

    let mut effects: Vec<Decision> =
        alloc::vec![Decision::RecordEvidence { bloom: *bloom, evidence: evidence.clone() }];

    let cursor = record.progress.get(&workpiece).copied();
    if cursor.is_some_and(|progress| progress.candidate.is_some_and(|current| current.tree == tree)) {
        return Decisions { outcome: Outcome::ConflictRepairInFlight { bloom: *bloom, workpiece }, effects };
    }

    let attempt = cursor.map_or(0, |progress| progress.attempts) + 1;
    if attempt > repair_budget(record) {
        effects.push(Decision::RecordWedge {
            bloom: *bloom,
            workpiece: workpiece.clone(),
            wedge: Wedge {
                stage: StageId::Construct,
                evidence: evidence.detail,
                repeated_verifiers: VerifyFailureSet::EMPTY,
            },
        });
        return Decisions {
            outcome: Outcome::ConflictWedged { bloom: *bloom, workpiece, question: evidence.detail },
            effects,
        };
    }

    let subject = CandidateRef { tree, checkout: head };
    effects.extend(move_effects_with_candidate(
        *bloom,
        &workpiece,
        record.spec.base(),
        composition_progress(StageId::Construct, attempt, subject),
        DispatchTargets { subject: subject.tree, checkout: subject.checkout },
        Some(subject.tree),
        conflict_line(record),
    ));

    Decisions {
        outcome: Outcome::ConflictMinted {
            bloom: *bloom,
            workpiece,
            parents: attribution.parents.clone(),
            bound: attribution.bound.clone(),
            attempt,
        },
        effects,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use alloc::string::{String, ToString as _};
    use alloc::vec;
    use alloc::vec::Vec;

    use super::reduce_conflict_attributed;
    use crate::digest::Digest;
    use crate::ids::{BloomId, IdempotencyKey, StageId, WorkpieceId};
    use crate::reduce::{ConflictAttributedError, Decision, Event, Fact, Outcome, Snapshot, reduce};
    use crate::testing::{draft, membership};
    use crate::values::{ConflictAttribution, Evidence, EvidenceKind, ResolvedConfigs, SpendWindow};

    const FIRST: &str = "wp-0";
    const SECOND: &str = "wp-1";
    const VERIFIED: &str = "wp-2";

    fn digest(seed: u8) -> Digest {
        Digest::from_bytes([seed; 32])
    }

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    fn attribution(parents: &[&str]) -> ConflictAttribution {
        ConflictAttribution {
            parents: parents.iter().map(|name| WorkpieceId((*name).to_string())).collect(),
            paths: strings(&["xtask/src/transform/verify/mod.rs"]),
            bound: strings(&["crates/example/**", "xtask/**"]),
        }
    }

    /// A sealed three-member bloom, each member at its entry cursor.
    fn sealed() -> (Snapshot, BloomId) {
        let spec = draft(0, vec![membership(FIRST, 1), membership(SECOND, 2), membership(VERIFIED, 3)]).seal();
        let bloom = spec.id();
        let snapshot = Snapshot::new(digest(0)).with_green_base(digest(0));
        let seal = Event { idempotency_key: IdempotencyKey("seal".into()), fact: Fact::Seal(spec) };
        let snapshot = snapshot.apply(
            &seal,
            &reduce(&snapshot, &seal, &ResolvedConfigs::default(), &SpendWindow::default()),
            &ResolvedConfigs::default(),
        );
        (snapshot, bloom)
    }

    fn verdict() -> Evidence {
        Evidence { kind: EvidenceKind::VerificationResult, subject: digest(0xF0), detail: digest(0xE0) }
    }

    #[test]
    fn a_minted_conflict_dispatches_itself_and_moves_no_member() {
        // Tripwire: the entire defect. Every lever the reducer holds is
        // member-shaped, so a fold verdict lands on whichever member produced
        // it — a member that wrote none of the failing file, will decline the
        // repair, and stops the bloom while it declines.
        let (snapshot, bloom) = sealed();
        let verified = WorkpieceId(VERIFIED.to_string());

        let decisions = reduce_conflict_attributed(
            &snapshot,
            &bloom,
            &verified,
            digest(0xF0),
            digest(0xF1),
            &verdict(),
            &attribution(&[FIRST, SECOND]),
        );

        let Outcome::ConflictMinted { workpiece, parents, bound, attempt, .. } = &decisions.outcome else {
            panic!("a two-parent attribution mints a subject: {:?}", decisions.outcome);
        };
        assert!(workpiece.is_conflict());
        assert_eq!(workpiece.conflict_parents().unwrap().0.0, FIRST);
        assert_eq!(parents.len(), 2);
        assert_eq!(bound, &strings(&["crates/example/**", "xtask/**"]));
        assert_eq!(*attempt, 1);

        let moved: Vec<&WorkpieceId> = decisions
            .effects
            .iter()
            .filter_map(|effect| match effect {
                Decision::AdvanceStage { workpiece, .. } | Decision::DispatchAttempt { workpiece, .. } => {
                    Some(workpiece)
                }
                _ => None,
            })
            .collect();
        assert!(!moved.is_empty(), "the minted subject dispatches: {:?}", decisions.effects);
        assert!(
            moved.iter().all(|target| target.is_conflict()),
            "no member's cursor moves for a collision it did not cause: {moved:?}",
        );
    }

    #[test]
    fn the_repair_enters_at_construct_against_the_refused_tree() {
        // The subject is the fold that refused, not either parent's candidate:
        // the objective is that both intents coexist *on that tree*, so a lap
        // aimed anywhere else would be reconciling something nobody refused.
        let (snapshot, bloom) = sealed();
        let decisions = reduce_conflict_attributed(
            &snapshot,
            &bloom,
            &WorkpieceId(VERIFIED.to_string()),
            digest(0xF0),
            digest(0xF1),
            &verdict(),
            &attribution(&[FIRST, SECOND]),
        );

        let dispatched = decisions
            .effects
            .iter()
            .find_map(|effect| match effect {
                Decision::DispatchAttempt { stage, transformation, .. } => Some((*stage, transformation.clone())),
                _ => None,
            })
            .expect("the mint dispatches");

        assert_eq!(dispatched.0, StageId::Construct);
        assert_eq!(dispatched.1.checkout, digest(0xF1), "the lap checks out the commit carrying the refused tree");
    }

    #[test]
    fn a_second_verdict_on_the_same_tree_buys_no_second_lane() {
        // Two gates can refuse one fold, and both verdicts are real. A second
        // dispatch would double-spend the budget and set two lanes writing one
        // seam.
        let (snapshot, bloom) = sealed();
        let verified = WorkpieceId(VERIFIED.to_string());
        let first = reduce_conflict_attributed(
            &snapshot,
            &bloom,
            &verified,
            digest(0xF0),
            digest(0xF1),
            &verdict(),
            &attribution(&[FIRST, SECOND]),
        );
        let advance = first
            .effects
            .iter()
            .find_map(|effect| match effect {
                Decision::AdvanceStage { workpiece, progress, .. } => Some((workpiece.clone(), *progress)),
                _ => None,
            })
            .expect("the mint advances its own cursor");

        let mut after = snapshot;
        after.blooms.get_mut(&bloom).expect("the bloom is sealed").progress.insert(advance.0, advance.1);

        let second = reduce_conflict_attributed(
            &after,
            &bloom,
            &verified,
            digest(0xF0),
            digest(0xF1),
            &verdict(),
            &attribution(&[FIRST, SECOND]),
        );

        assert!(matches!(second.outcome, Outcome::ConflictRepairInFlight { .. }), "{:?}", second.outcome);
        assert!(
            !second.effects.iter().any(|effect| matches!(effect, Decision::DispatchAttempt { .. })),
            "the second verdict files itself and dispatches nothing: {:?}",
            second.effects,
        );
    }

    #[test]
    fn an_attribution_naming_the_verified_member_is_refused() {
        // The guard against the mint becoming a laundry: a member that wrote
        // the failing file owns that finding at its own Verify.
        let (snapshot, bloom) = sealed();
        let decisions = reduce_conflict_attributed(
            &snapshot,
            &bloom,
            &WorkpieceId(FIRST.to_string()),
            digest(0xF0),
            digest(0xF1),
            &verdict(),
            &attribution(&[FIRST, SECOND]),
        );

        assert_eq!(
            decisions.outcome,
            Outcome::ConflictRejected(ConflictAttributedError::VerifiedIsParent(WorkpieceId(FIRST.to_string()))),
        );
        assert!(decisions.effects.is_empty(), "a refused attribution changes nothing");
    }

    #[test]
    fn an_attribution_naming_a_stranger_or_the_wrong_count_is_refused() {
        let (snapshot, bloom) = sealed();
        let verified = WorkpieceId(VERIFIED.to_string());

        assert_eq!(
            reduce_conflict_attributed(
                &snapshot,
                &bloom,
                &verified,
                digest(0xF0),
                digest(0xF1),
                &verdict(),
                &attribution(&[FIRST, "wp-elsewhere"]),
            )
            .outcome,
            Outcome::ConflictRejected(ConflictAttributedError::NotAMember(WorkpieceId("wp-elsewhere".to_string()))),
        );

        assert_eq!(
            reduce_conflict_attributed(
                &snapshot,
                &bloom,
                &verified,
                digest(0xF0),
                digest(0xF1),
                &verdict(),
                &attribution(&[FIRST]),
            )
            .outcome,
            Outcome::ConflictRejected(ConflictAttributedError::NotTwoParents(1)),
        );
    }
}
