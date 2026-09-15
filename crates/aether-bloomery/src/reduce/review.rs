//! The composition workpiece's `Review` (ADR-0191 §3): one verdict over the
//! composed head, bounded by the two-pass ceiling that parks the bloom to the
//! owner rather than buying a third roll (ADR-0153).
//!
//! Its subject is the weave — the reconcile-authored seam edits, the files more
//! than one member touched, and whether each member's work order is still
//! visibly satisfied in the composed tree. The member work orders and candidates
//! are reference input; the member diffs are not re-read. So a refusal repairs
//! the weave and never re-opens a member.

use alloc::vec::Vec;

use super::composition::{Refusal, finding_of, reweave};
use super::{
    AggregateReviewError, AggregateReviewFault, BloomRecord, BloomStatus, Decision, Decisions, FoldedIntegration,
    Outcome, Snapshot,
};
use crate::digest::Digest;
use crate::ids::{BloomId, StageId, WorkpieceId};
use crate::values::{Evidence, EvidenceKind, ResolvedBloom, VerifyGateSet, VerifyProof};

/// Candidate-review judgments per bloom (ADR-0153). Independent of the sealed
/// `AggregateReview` retry budget, which ADR-0176 assigns to the executor-fault
/// ledger.
const CANDIDATE_REVIEW_PASS_CEILING: u32 = 2;

/// The bloom record and the integration fold a fold-bound aggregate-review
/// result may act on, or the refusal it earns.
///
/// The three refusals every aggregate-review result makes, in one place: an
/// unknown or inactive bloom, no held integration, and evidence naming a tree
/// other than the held fold's. The last is the load-bearing one — a stale result
/// from a superseded fold must not act on a newer integration, whether it
/// carries a verdict or an executor fault.
fn held_fold_under_review<'a>(
    snapshot: &'a Snapshot,
    bloom: &BloomId,
    evidence: &Evidence,
) -> Result<(&'a BloomRecord, &'a FoldedIntegration), AggregateReviewError> {
    let record = snapshot
        .blooms
        .get(bloom)
        .filter(|record| record.status == BloomStatus::Sealed)
        .ok_or(AggregateReviewError::UnknownOrInactiveBloom)?;
    let integration = record.integration.as_ref().ok_or(AggregateReviewError::NoPendingIntegration)?;
    if !evidence.validates(&integration.tree) {
        return Err(AggregateReviewError::SubjectMismatch { expected: integration.tree, got: evidence.subject });
    }
    Ok((record, integration))
}

/// The effects a fold produces once *both* composite gates have passed on it:
/// the held fold is consumed, the bloom resolves onto it, and the land reactor
/// is handed the head to fast-forward mainline onto.
///
/// Named once because either gate can be the one that completes the join — the
/// two run concurrently against one fold — and a second copy would let the
/// mechanical gate and the critic resolve a bloom onto different values. What
/// the landing receives is the head of the tree that was judged; nothing is
/// re-folded or rebuilt on the way, so the artifact that lands is the artifact
/// the gates passed.
pub(super) fn resolution_effects(
    record: &BloomRecord,
    bloom: BloomId,
    integration: &FoldedIntegration,
) -> (ResolvedBloom, Vec<Decision>) {
    let resolved = ResolvedBloom {
        bloom,
        tree: integration.tree,
        head: integration.head,
        lineage: integration.lineage.clone(),
        resolution_claims: record.claims.values().cloned().collect::<Vec<_>>(),
    };

    // Resolution is land-readiness: the bloom now carries its one judged
    // artifact and a claim for every member, so the source-port CAS land can be
    // driven (ADR-0149 migration step 3). `new_head` is the integrated head
    // commit's digest (distinct from the artifact `tree`) the mainline advances
    // to; the reducer never does the I/O. The consumed fold is cleared — a
    // resolved bloom holds no pending gate run.
    let mut effects = Vec::new();
    if let Some(proof) = contextual_memo_proof(record, integration.tree) {
        effects.push(Decision::RecordVerifyProof { bloom, proof });
    }
    effects.extend(alloc::vec![
        Decision::RecordIntegration { bloom, integration: None },
        Decision::SetResolved { bloom, resolved: resolved.clone() },
        Decision::DispatchLand { bloom, expected_base: record.spec.base(), new_head: integration.head },
    ]);

    (resolved, effects)
}

/// The legacy aggregate-verify memo entry a contextual resolve files for the
/// tree it lands, or `None` when this resolve needs none.
///
/// A contextual bloom integrates through shared runs whose `PassedIn` receipts
/// never enter the memo, so without this the landing that follows finds no
/// `AggregateVerify` proof for the tree and files no base receipt — and the
/// next seal re-proves a head the landing already proved. The proof stands on
/// the green shared run whose candidate is the resolved tree, filed under the
/// fold gate set so `verify_proof_for(AggregateVerify, tree)` answers the way
/// a classic bloom's does. A resolve that already holds such a proof — every
/// classic bloom, whose `AggregateVerifyCompleted` filed it — mints nothing.
fn contextual_memo_proof(record: &BloomRecord, tree: Digest) -> Option<VerifyProof> {
    if record.verify_proof_for(StageId::AggregateVerify, tree).is_some() {
        return None;
    }
    let coordination = record.coordination.as_deref()?;
    // The same refusal [`super::coordination::contextual_aggregate_authority`]
    // makes, at the other seam a contextual receipt reaches the fold identity
    // through. A shared run is sealed under the composition position's gate
    // set, so minting a fold proof from one would file a green for
    // `verify.docs` under a run that never spawned it — and the landing reads
    // exactly this proof when it decides whether to mint a whole-workspace base
    // receipt. Nothing is filed instead, so the next seal pays an honest
    // `verify.base`.
    let aggregate = VerifyGateSet::for_stage_of(StageId::AggregateVerify, &record.pipeline_manifest)?.digest();
    if coordination.composition_contract.gate_set != aggregate {
        return None;
    }
    let receipt = coordination.contextual_proof_for_tree(tree)?;
    if receipt.kind != EvidenceKind::VerificationResult || !receipt.validates(&tree) {
        return None;
    }
    Some(VerifyProof {
        gate_set: VerifyGateSet::fold_of(&record.pipeline_manifest).digest(),
        stage: StageId::AggregateVerify,
        evidence: receipt.clone(),
    })
}

/// Reduce a composition-review verdict (ADR-0153, ADR-0191 §3). A passing
/// verdict records the critic's half of the composite-gate join; it resolves
/// the bloom — [`Decision::SetResolved`] plus the [`Decision::DispatchLand`]
/// the land reactor consumes — only when the mechanical gate has already passed
/// on the same fold, and otherwise leaves the bloom waiting on it. A failing
/// verdict files the finding on the composition's channel and dispatches the
/// weave repair against the composed tree; the fold stays held, because it is
/// the composition's candidate under repair rather than someone else's stale
/// artifact. The second failing verdict files its finding the same way and
/// parks the bloom to the owner — the two-pass ceiling; the machine never buys
/// a third roll, though an adopting answer lets the owner buy a fresh cycle.
///
/// Which of the two a verdict is, is now the *reviewer's* statement about its
/// findings rather than the mere existence of one (#4961). A review whose
/// findings are all judgment advisories reports as a pass and arrives here as
/// one, carrying [`EvidenceKind::ReviewAdvisory`] — so the observations are
/// filed on the composition's channel on the way to the landing, and a
/// subjective finding costs a bloom nothing.
///
/// A blocking verdict that implicates only members an operator withdrew reads
/// as the same kind of pass (#5327, bloom 0f16e207): those members produced no
/// claim and contributed no candidate, so the fold was never under the
/// obligations the critic judged it against, and neither a re-weave nor a
/// refusal is an honest answer to it.
pub(super) fn reduce_aggregate_review_completed(
    snapshot: &Snapshot,
    bloom: &BloomId,
    passed: bool,
    evidence: &Evidence,
    implicated: &[WorkpieceId],
) -> Decisions {
    let (record, integration) = match held_fold_under_review(snapshot, bloom, evidence) {
        Ok(held) => held,
        Err(refusal) => return Decisions::rejected(Outcome::AggregateReviewRejected(refusal)),
    };
    // The implication is a *label on the finding*, not a routing table: it names
    // which members' intent the verdict thinks the weave lost, so the recorded
    // finding points a reader (and any follow-up work filed from it) at the right
    // code. A named non-member is malformed and is validated before any effect
    // applies, so such a verdict changes nothing — checked ahead of the verdict
    // split because a passing verdict can carry an advisory finding now, and a
    // label the record cannot resolve is no better on that row than on a refusal's.
    // An empty implication is never expanded to every member: under ADR-0191 there
    // is nothing to over-route *to*, and a verdict about the weave as a whole is
    // exactly a finding that names nobody.
    if let Some(stranger) =
        implicated.iter().find(|wp| !record.spec.members().iter().any(|member| member.workpiece == **wp))
    {
        return Decisions::rejected(Outcome::AggregateReviewRejected(AggregateReviewError::NotAMember(
            stranger.clone(),
        )));
    }
    // A withdrawn member is a *member the composed tree does not carry*, which
    // is a different thing from a stranger, and used to earn the same refusal
    // (#5327). It cannot: a refusal makes the whole verdict change nothing, so
    // the critic's half of the composite gate never lands and the bloom sits on
    // a fold no verdict can complete. What a withdrawn implication actually
    // means is that the reviewer judged an obligation the fold was never under
    // — its member produced no claim and contributed no candidate — so the
    // finding is kept as an observation and dropped from the routing.
    //
    // The live remainder is what the verdict is about. If nothing is left, the
    // whole blocking verdict was about work that is provably absent, so it is
    // not a blocking verdict: it re-kinds to an advisory, files on the
    // composition's channel naming the departed members it judged, and counts
    // as the critic's pass. Nothing re-weaves — a repair lane pointed at a
    // withdrawn member's order can only re-author work an operator removed on
    // purpose (bloom 0f16e207).
    let live: Vec<WorkpieceId> = implicated.iter().filter(|wp| !record.withdrawn.contains_key(*wp)).cloned().collect();
    let about_departed_only = !passed && !implicated.is_empty() && live.is_empty();
    let evidence = &if about_departed_only {
        Evidence { kind: EvidenceKind::ReviewAdvisory, ..evidence.clone() }
    } else {
        evidence.clone()
    };
    let implicated = if about_departed_only {
        implicated
    } else {
        live.as_slice()
    };

    let rolls = record.aggregate_rolls + 1;
    let mut effects = alloc::vec![
        Decision::RecordEvidence { bloom: *bloom, evidence: evidence.clone() },
        Decision::RecordAggregateRoll { bloom: *bloom, rolls },
    ];
    if passed || about_departed_only {
        // A pass that still recorded judgment findings (#4961), or a blocking
        // verdict re-kinded above because every member it named had left. Either
        // way nothing here re-weaves, spends the repair budget, or delays the
        // landing — and the observations still land on the composition's own
        // channel, where an operator can adjudicate them and the study that
        // files fix-forward work can read them. Filed before the resolution
        // effects so the journal shows the finding under the verdict that
        // raised it.
        if evidence.kind == EvidenceKind::ReviewAdvisory {
            effects.push(finding_of(*bloom, integration.tree, evidence, implicated));
        }
        // The critic's half of the join. Filed whether or not it completes the
        // pair, so the mechanical gate's own arrival can read it.
        effects.push(Decision::RecordAggregateGatePass { bloom: *bloom, stage: StageId::AggregateReview });
        if !record.aggregate_passed.contains(&StageId::AggregateVerify) {
            // The compiler has not returned on this fold yet. Both gates were
            // dispatched together, so there is nothing to dispatch here and
            // nothing to wait on but the verdict already in flight.
            return Decisions { outcome: Outcome::AggregateReviewPassed { bloom: *bloom, rolls }, effects };
        }

        let (resolved, resolution) = resolution_effects(record, *bloom, integration);
        effects.extend(resolution);
        return Decisions { outcome: Outcome::Resolved(resolved), effects };
    }
    if rolls >= CANDIDATE_REVIEW_PASS_CEILING {
        // The delta-confirm still failed: the two-pass ceiling parks the bloom
        // to the owner (ADR-0151's hold vocabulary at bloom scope). The fold
        // stays held (the owner's decision context), no member re-opens, no
        // further review dispatches; the failing review's record artifact is
        // the parked question an adopting answer must name to re-arm the
        // cycle. The sealed catalog budget is not consulted — a valid authored
        // `retry_budget` of 3 must not buy a third judgment.
        //
        // The finding is filed first (#4977): a ceiling refusal is a refusal of
        // the composed tree with its evidence in hand, exactly as the re-weave
        // below is, so it belongs on the composition's channel rather than
        // living only as the park's question. That is what an operator
        // adjudicates and what the study counts.
        effects.push(finding_of(*bloom, integration.tree, evidence, implicated));
        effects.push(Decision::RecordReviewPark { bloom: *bloom, question: Some(evidence.detail) });
        return Decisions {
            outcome: Outcome::AggregateReviewParked { bloom: *bloom, rolls, question: evidence.detail },
            effects,
        };
    }
    // First blocking verdict: repair in the composition (ADR-0191 §4/§5). The
    // implicated set is recorded on the finding — it files follow-up work for a
    // member whose code the verdict genuinely names, and it directs the reader —
    // but nothing is dispatched against a member and no claim is revoked. A
    // member that passed its review is done; the weave is what repairs, at the
    // seam, against the composed tree that was refused.
    let repair = reweave(
        record,
        bloom,
        &Refusal {
            refused_at: StageId::AggregateReview,
            tree: integration.tree,
            head: integration.head,
            evidence,
            implicated,
        },
    );
    effects.extend(repair.effects);

    Decisions { outcome: repair.outcome, effects }
}

/// Reduce an aggregate-review executor environment fault (ADR-0176) — the
/// dispatched review reporting that it could not judge the fold at all.
///
/// A branch entirely separate from [`reduce_aggregate_review_completed`],
/// because nothing here is a verdict about a candidate: the fold stays held,
/// every member keeps its claim and its cursor, and no findings are written. The
/// fault records against the held fold and, while the sealed `AggregateReview`
/// budget allows, redispatches the *same* tree and head under a fresh order.
/// At the ceiling it emits no dispatch — the folded record is the terminal
/// bloom-scoped wedge an operator reads, and recovery is an explicit successor
/// once the environment is repaired, never a reactor poll that quietly buys
/// another attempt.
///
/// Refused on exactly the three axes a fold-bound aggregate verdict carrying no
/// implication can be refused on: an unknown or inactive bloom, no held
/// integration, and a subject that is not the held fold's tree.
pub(super) fn reduce_aggregate_review_executor_fault(
    snapshot: &Snapshot,
    bloom: &BloomId,
    evidence: &Evidence,
) -> Decisions {
    let (record, integration) = match held_fold_under_review(snapshot, bloom, evidence) {
        Ok(held) => held,
        Err(refusal) => return Decisions::rejected(Outcome::AggregateReviewRejected(refusal)),
    };

    // ADR-0219 hook, the bloom-level twin of the one in
    // `reduce_member_executor_fault`: inside an admin session the critic's
    // fault is recorded and charged to nobody, because the operator is the one
    // moving the fold and a spent roll would wedge the gate they are about to
    // re-run.
    if super::admin::absorbs_faults(record) {
        return super::admin::absorbed_fault(*bloom, None, evidence);
    }

    // The same rule the `RecordEvidence` fold applies, read here so the ceiling
    // decides against the count the record will actually reach.
    let fault = AggregateReviewFault::next(record.aggregate_fault.as_ref(), integration.tree, evidence.detail);
    let budget = record.stage_catalog.retry_budget_of(StageId::AggregateReview).unwrap_or(1);
    let mut effects = alloc::vec![Decision::RecordEvidence { bloom: *bloom, evidence: evidence.clone() }];

    if fault.rolls >= budget {
        return Decisions { outcome: Outcome::AggregateReviewExecutorWedged { bloom: *bloom, fault, budget }, effects };
    }

    // The same held tree and head, under a fresh order: the fold was never
    // judged, so re-running the review is the whole retry — not a re-fold, and
    // not a member lap. The roll stays the critic's own unspent cursor. The
    // helper is what withholds that work order under an operator hold (#5100).
    effects.extend(super::aggregate_verify::aggregate_review_dispatch(
        record,
        *bloom,
        integration.tree,
        integration.head,
    ));

    Decisions { outcome: Outcome::AggregateReviewExecutorFaulted { bloom: *bloom, fault, budget }, effects }
}

#[cfg(test)]
mod tests {
    use alloc::borrow::ToOwned;
    use alloc::vec;
    use alloc::vec::Vec;

    use crate::ids::{BloomId, StageId, WorkpieceId};
    use crate::reduce::{AggregateReviewError, Decision, Decisions, Fact, Outcome, Snapshot};
    use crate::testing::{claim, digest, draft, event, membership, step};
    use crate::values::{CompositionFinding, Evidence, EvidenceKind, Withdrawal, WithdrawalCause};

    /// Three members sealed, one withdrawn, the other two integrated and folded,
    /// the mechanical gate already green — the position the critic's verdict
    /// arrives at in a bloom an operator shed a member from mid-walk.
    fn fold_missing_a_withdrawn_member() -> (Snapshot, BloomId) {
        let spec = draft(1, vec![membership("alpha", 10), membership("beta", 11), membership("gone", 12)]).seal();
        let bloom = spec.id();
        let snapshot = Snapshot::new(digest(1)).with_green_base(digest(1));
        let (snapshot, _) = step(&snapshot, &event("seal", Fact::Seal(spec)));
        let (snapshot, _) = step(
            &snapshot,
            &event(
                "withdraw",
                Fact::Withdraw {
                    bloom,
                    withdrawals: vec![Withdrawal {
                        workpiece: WorkpieceId("gone".to_owned()),
                        cause: WithdrawalCause::Operator,
                        reason: "pulled out of the wave".to_owned(),
                        operator: "operator".to_owned(),
                    }],
                    cascade: false,
                },
            ),
        );
        let (snapshot, _) = step(&snapshot, &event("i-a", Fact::Integrate { bloom, claim: claim("alpha", 10, 100) }));
        let (snapshot, _) = step(&snapshot, &event("i-b", Fact::Integrate { bloom, claim: claim("beta", 11, 101) }));
        let (snapshot, _) = step(
            &snapshot,
            &event("fold", Fact::Resolve { bloom, tree: digest(40), head: digest(41), lineage: vec![] }),
        );
        let (snapshot, _) = step(
            &snapshot,
            &event(
                "verify",
                Fact::AggregateVerifyCompleted {
                    bloom,
                    passed: true,
                    evidence: Evidence {
                        subject: digest(40),
                        kind: EvidenceKind::VerificationResult,
                        detail: digest(51),
                    },
                },
            ),
        );
        (snapshot, bloom)
    }

    fn review_failed(bloom: BloomId, implicated: &[&str]) -> Fact {
        Fact::AggregateReviewCompleted {
            bloom,
            passed: false,
            evidence: Evidence { subject: digest(40), kind: EvidenceKind::ReviewFinding, detail: digest(60) },
            implicated: implicated.iter().map(|name| WorkpieceId((*name).to_owned())).collect(),
        }
    }

    fn findings(decisions: &Decisions) -> Vec<CompositionFinding> {
        decisions
            .effects
            .iter()
            .filter_map(|effect| match effect {
                Decision::RecordCompositionFinding { finding, .. } => Some(finding.clone()),
                _ => None,
            })
            .collect()
    }

    // Tripwire (bloom 0f16e207): a blocking verdict whose every implicated
    // member was withdrawn judges obligations the fold was never under — those
    // members produced no claim and contributed no candidate. Answering it with
    // a re-weave sets a repair lane re-authoring work an operator removed on
    // purpose; answering it with the old `NotAMember` refusal leaves the critic's
    // half of the composite gate unfiled and the bloom on a fold no verdict can
    // complete. It counts as the critic's pass instead, with the observation
    // filed on the composition's channel.
    #[test]
    fn a_blocking_verdict_about_only_withdrawn_members_passes_the_composition() {
        let (snapshot, bloom) = fold_missing_a_withdrawn_member();

        let (_, decisions) = step(&snapshot, &event("r1", review_failed(bloom, &["gone"])));

        assert!(
            matches!(decisions.outcome, Outcome::Resolved(_)),
            "the fold's other gate is already green, so this verdict resolves the bloom: {:?}",
            decisions.outcome,
        );
        assert!(
            !decisions.effects.iter().any(|effect| matches!(effect, Decision::DispatchAttempt { .. })),
            "nothing re-weaves for a member that left the bloom: {:?}",
            decisions.effects,
        );
        assert!(
            decisions.effects.iter().any(|effect| matches!(
                effect,
                Decision::RecordAggregateGatePass { stage: StageId::AggregateReview, .. }
            )),
            "the critic's half of the composite gate is filed: {:?}",
            decisions.effects,
        );
        assert_eq!(
            findings(&decisions).first().map(|finding| finding.implicated.clone()),
            Some(vec![WorkpieceId("gone".to_owned())]),
            "the dropped verdict is still filed as an observation naming the member it judged",
        );
        assert!(
            decisions.effects.iter().any(|effect| matches!(
                effect,
                Decision::RecordEvidence { evidence, .. } if evidence.kind == EvidenceKind::ReviewAdvisory
            )),
            "the recorded evidence re-kinds to an advisory, so the journal says it blocked nothing",
        );
    }

    // Tripwire (bloom 0f16e207): the live half of a mixed verdict is a real
    // refusal of the composed tree and still re-weaves — but the finding it
    // files must not carry the withdrawn member, or the repair lane and every
    // reader downstream of it re-derive an obligation from the sealed spec.
    #[test]
    fn a_mixed_verdict_keeps_only_its_live_implication() {
        let (snapshot, bloom) = fold_missing_a_withdrawn_member();

        let (_, decisions) = step(&snapshot, &event("r1", review_failed(bloom, &["gone", "alpha"])));

        assert_eq!(
            findings(&decisions).first().map(|finding| finding.implicated.clone()),
            Some(vec![WorkpieceId("alpha".to_owned())]),
            "the withdrawn member is dropped from the finding's routing label: {:?}",
            decisions.effects,
        );
        assert!(
            matches!(decisions.outcome, Outcome::CompositionRewoven { .. }),
            "a verdict still naming a live member repairs the weave: {:?}",
            decisions.outcome,
        );
    }

    // Tripwire (#5327): the withdrawn carve-out is not a general amnesty. A
    // workpiece no sealed membership names is still malformed, and the whole
    // verdict is refused before any effect applies.
    #[test]
    fn a_verdict_naming_a_stranger_is_still_refused() {
        let (snapshot, bloom) = fold_missing_a_withdrawn_member();

        let (_, decisions) = step(&snapshot, &event("r1", review_failed(bloom, &["wp-ghost"])));

        assert!(
            matches!(
                decisions.outcome,
                Outcome::AggregateReviewRejected(AggregateReviewError::NotAMember(ref wp)) if wp.0 == "wp-ghost"
            ),
            "a stranger keeps the refusal: {:?}",
            decisions.outcome,
        );
        assert!(decisions.effects.is_empty(), "a refused verdict applies nothing: {:?}", decisions.effects);
    }
}
