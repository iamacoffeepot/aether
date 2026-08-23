//! Narrowing a composition to the candidates that actually collide (ADR-0210).
//!
//! There is one way candidates are merged: a composition over a parent set. The
//! bloom's fold is the composition whose parents are every live member; when
//! that one refuses and the failure is accounted for by a subset of the
//! candidates in it, the composition of exactly that subset is what repairs it.
//! Same record, same lane, same bound rule, same session rule — the arity is
//! what varies.
//!
//! Narrowing is what makes the difference worth having. Repairing a two-file
//! disagreement at arity N puts the whole bloom's tree under one lane and hands
//! it every member's surface; repairing it at arity two hands one lane exactly
//! the two candidates that have to coexist and exactly the paths their two
//! approvals already cover.
//!
//! What this deliberately does *not* emit is the point. No cursor move for the
//! member whose Verify produced the verdict, no revoked resolution, and no
//! dispatch naming a parent. The parents are finished; making them coexist is
//! the composition's job, at whatever arity the failure calls for.

use alloc::vec::Vec;

use super::attempt::{DispatchTargets, SealedLine, move_effects_with_candidate};
use super::composition::composition_progress;
use super::{BloomRecord, BloomStatus, Decision, Decisions, NarrowCompositionError, Outcome, Snapshot};
use crate::digest::Digest;
use crate::ids::{BloomId, StageId, WorkpieceId};
use crate::values::{CandidateRef, CompositionParents, Evidence, VerifyFailureSet, Wedge};

/// The repair allowance a narrowed composition gets: the sealed catalog's
/// `Refine` budget — the same allowance the whole-bloom composition's weave
/// repair takes, because it is the same repair at a different arity.
fn repair_budget(record: &BloomRecord) -> u32 {
    record.stage_catalog.retry_budget_of(StageId::Refine).unwrap_or(1)
}

/// The line a narrowed composition dispatches under: the bloom's own
/// configuration and sealed catalog, over the base its parents built onto.
///
/// The same line `composition_line` gives the whole-bloom instance, for the same
/// reason — one merge mechanism means one line.
///
/// Bloom-wide rather than layered with either parent's registry. Layering one
/// parent's would give that parent's model override standing over a subject
/// whose whole objective is to favour neither.
fn narrowed_line(record: &BloomRecord) -> SealedLine<'_> {
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
/// narrowed composition's cursor move plus its dispatch, or — when its budget is
/// already spent — its wedge. A second attribution of the same tree onto the
/// same pair files only the verdict: one refused fold buys one repair lap, and
/// a second dispatch would set two lanes on one seam.
pub(super) fn reduce_composition_narrowed(
    snapshot: &Snapshot,
    bloom: &BloomId,
    verified: &WorkpieceId,
    tree: Digest,
    head: Digest,
    evidence: &Evidence,
    attribution: &CompositionParents,
) -> Decisions {
    let Some(record) = snapshot.blooms.get(bloom) else {
        return Decisions::rejected(Outcome::NarrowCompositionRejected(NarrowCompositionError::UnknownOrInactiveBloom));
    };
    if record.status != BloomStatus::Sealed {
        return Decisions::rejected(Outcome::NarrowCompositionRejected(NarrowCompositionError::UnknownOrInactiveBloom));
    }
    let Some(workpiece) = attribution.workpiece() else {
        return Decisions::rejected(Outcome::NarrowCompositionRejected(NarrowCompositionError::NotTwoParents(
            attribution.parents.len(),
        )));
    };
    for parent in &attribution.parents {
        if !record.spec.members().iter().any(|member| member.workpiece == *parent) {
            return Decisions::rejected(Outcome::NarrowCompositionRejected(NarrowCompositionError::NotAMember(
                parent.clone(),
            )));
        }
        if record.withdrawn.contains_key(parent) {
            return Decisions::rejected(Outcome::NarrowCompositionRejected(NarrowCompositionError::ParentWithdrawn(
                parent.clone(),
            )));
        }
        if parent == verified {
            return Decisions::rejected(Outcome::NarrowCompositionRejected(NarrowCompositionError::VerifiedIsParent(
                parent.clone(),
            )));
        }
    }

    let mut effects: Vec<Decision> =
        alloc::vec![Decision::RecordEvidence { bloom: *bloom, evidence: evidence.clone() }];

    let cursor = record.progress.get(&workpiece).copied();
    if cursor.is_some_and(|progress| progress.candidate.is_some_and(|current| current.tree == tree)) {
        return Decisions { outcome: Outcome::CompositionRepairAlreadyInFlight { bloom: *bloom, workpiece }, effects };
    }

    let attempt = cursor.map_or(0, |progress| progress.attempts) + 1;
    if attempt > repair_budget(record) {
        effects.push(Decision::RecordWedge {
            bloom: *bloom,
            workpiece: workpiece.clone(),
            wedge: Wedge {
                stage: StageId::Refine,
                evidence: evidence.detail,
                repeated_verifiers: VerifyFailureSet::EMPTY,
            },
        });
        return Decisions {
            outcome: Outcome::NarrowCompositionWedged { bloom: *bloom, workpiece, question: evidence.detail },
            effects,
        };
    }

    let subject = CandidateRef { tree, checkout: head };
    effects.extend(move_effects_with_candidate(
        *bloom,
        &workpiece,
        record.spec.base(),
        composition_progress(StageId::Refine, attempt, subject),
        DispatchTargets { subject: subject.tree, checkout: subject.checkout },
        Some(subject.tree),
        narrowed_line(record),
    ));

    Decisions {
        outcome: Outcome::CompositionNarrowed {
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

    use super::reduce_composition_narrowed;
    use crate::digest::Digest;
    use crate::ids::{BloomId, IdempotencyKey, StageId, WorkpieceId};
    use crate::reduce::{Decision, Event, Fact, NarrowCompositionError, Outcome, Snapshot, reduce};
    use crate::testing::{draft, membership};
    use crate::values::{CompositionParents, Evidence, EvidenceKind, ResolvedConfigs, SpendWindow};

    const FIRST: &str = "wp-0";
    const SECOND: &str = "wp-1";
    const VERIFIED: &str = "wp-2";

    fn digest(seed: u8) -> Digest {
        Digest::from_bytes([seed; 32])
    }

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    fn attribution(parents: &[&str]) -> CompositionParents {
        CompositionParents {
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
    fn a_narrowed_composition_dispatches_itself_and_moves_no_member() {
        // Tripwire: the entire defect. Every lever the reducer holds is
        // member-shaped, so a fold verdict lands on whichever member produced
        // it — a member that wrote none of the failing file, will decline the
        // repair, and stops the bloom while it declines.
        let (snapshot, bloom) = sealed();
        let verified = WorkpieceId(VERIFIED.to_string());

        let decisions = reduce_composition_narrowed(
            &snapshot,
            &bloom,
            &verified,
            digest(0xF0),
            digest(0xF1),
            &verdict(),
            &attribution(&[FIRST, SECOND]),
        );

        let Outcome::CompositionNarrowed { workpiece, parents, bound, attempt, .. } = &decisions.outcome else {
            panic!("a two-parent narrowing mints a composition: {:?}", decisions.outcome);
        };
        assert!(workpiece.is_composition(), "a narrowed composition is still a composition: {workpiece:?}");
        assert_eq!(
            workpiece.composition_parents().expect("a narrowed composition names its parents in its id"),
            *parents,
            "the id carries the same parent set the outcome reports",
        );
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
            moved.iter().all(|target| target.is_composition()),
            "no member's cursor moves for a collision it did not cause: {moved:?}",
        );
    }

    #[test]
    fn the_repair_enters_at_the_same_stage_the_whole_bloom_repair_does() {
        // The subject is the fold that refused, not either parent's candidate:
        // the objective is that both intents coexist *on that tree*, so a lap
        // aimed anywhere else would be reconciling something nobody refused.
        let (snapshot, bloom) = sealed();
        let decisions = reduce_composition_narrowed(
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

        assert_eq!(dispatched.0, StageId::Refine, "one merge mechanism means one repair stage");
        assert_eq!(dispatched.1.checkout, digest(0xF1), "the lap checks out the commit carrying the refused tree");
    }

    #[test]
    fn a_second_verdict_on_the_same_tree_buys_no_second_lane() {
        // Two gates can refuse one fold, and both verdicts are real. A second
        // dispatch would double-spend the budget and set two lanes writing one
        // seam.
        let (snapshot, bloom) = sealed();
        let verified = WorkpieceId(VERIFIED.to_string());
        let first = reduce_composition_narrowed(
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

        let second = reduce_composition_narrowed(
            &after,
            &bloom,
            &verified,
            digest(0xF0),
            digest(0xF1),
            &verdict(),
            &attribution(&[FIRST, SECOND]),
        );

        assert!(matches!(second.outcome, Outcome::CompositionRepairAlreadyInFlight { .. }), "{:?}", second.outcome);
        assert!(
            !second.effects.iter().any(|effect| matches!(effect, Decision::DispatchAttempt { .. })),
            "the second verdict files itself and dispatches nothing: {:?}",
            second.effects,
        );
    }

    #[test]
    fn a_parent_set_naming_the_verified_member_is_refused() {
        // The guard against the mint becoming a laundry: a member that wrote
        // the failing file owns that finding at its own Verify.
        let (snapshot, bloom) = sealed();
        let decisions = reduce_composition_narrowed(
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
            Outcome::NarrowCompositionRejected(NarrowCompositionError::VerifiedIsParent(WorkpieceId(
                FIRST.to_string()
            ))),
        );
        assert!(decisions.effects.is_empty(), "a refused attribution changes nothing");
    }

    #[test]
    fn a_parent_set_naming_a_stranger_or_the_wrong_count_is_refused() {
        let (snapshot, bloom) = sealed();
        let verified = WorkpieceId(VERIFIED.to_string());

        assert_eq!(
            reduce_composition_narrowed(
                &snapshot,
                &bloom,
                &verified,
                digest(0xF0),
                digest(0xF1),
                &verdict(),
                &attribution(&[FIRST, "wp-elsewhere"]),
            )
            .outcome,
            Outcome::NarrowCompositionRejected(NarrowCompositionError::NotAMember(WorkpieceId(
                "wp-elsewhere".to_string()
            ))),
        );

        assert_eq!(
            reduce_composition_narrowed(
                &snapshot,
                &bloom,
                &verified,
                digest(0xF0),
                digest(0xF1),
                &verdict(),
                &attribution(&[FIRST]),
            )
            .outcome,
            Outcome::NarrowCompositionRejected(NarrowCompositionError::NotTwoParents(1)),
        );
    }
}
