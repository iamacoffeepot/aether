//! Reducer-owned scheduling and settlement for aggregate pre-checks.

use alloc::collections::{BTreeMap, BTreeSet};

use super::aggregate_verify::{aggregate_verify_dispatch, reduce_aggregate_verify_completed};
use super::attempt::stage_binding;
use super::verify_memo::proof_of;
use super::{BloomRecord, BloomStatus, Decision, Decisions, Outcome, PrecheckError, Snapshot};
use crate::digest::Digest;
use crate::ids::{BloomId, StageId, WorkpieceId};
use crate::values::{
    BloomSpec, CandidateRef, Evidence, EvidenceKind, PipelineManifest, PrecheckCompletion, PrecheckDiagnostic,
    PrecheckMember, PrecheckNode, PrecheckPlan, PrecheckPolicy, PrecheckPreparation, PrecheckResult, PrecheckState,
    StageCatalog, Transformation, VerifyGateSet,
};

fn candidates_of(record: &BloomRecord) -> BTreeMap<WorkpieceId, CandidateRef> {
    let mut candidates = BTreeMap::new();
    for member in record.spec.members() {
        if record.withdrawn.contains_key(&member.workpiece) {
            continue;
        }
        let candidate = record
            .progress
            .get(&member.workpiece)
            .filter(|progress| progress.stage == StageId::Verify)
            .and_then(|progress| progress.candidate)
            .or_else(|| {
                let claim = record.claims.get(&member.workpiece)?;
                let vehicle = record.vehicles.get(&member.workpiece)?;
                if claim.scope_revision != member.scope_revision {
                    return None;
                }
                (claim.candidate == vehicle.tree).then_some(*vehicle)
            });
        if let Some(candidate) = candidate {
            candidates.insert(member.workpiece.clone(), candidate);
        }
    }
    candidates
}

/// Build the eligible plan from a record without cloning the snapshot.
pub(super) fn plan_of(record: &BloomRecord, bloom: BloomId) -> Option<PrecheckPlan> {
    plan_from_candidates(record, bloom, &candidates_of(record))
}

pub(super) fn initialized_effects(
    spec: &BloomSpec,
    catalog: &StageCatalog,
    manifest: &PipelineManifest,
    policy: Option<PrecheckPolicy>,
    preceding: &[Decision],
) -> Vec<Decision> {
    let Some(policy) = policy else {
        return alloc::vec![];
    };
    let bloom = spec.id();
    let mut record = BloomRecord::empty(spec.clone());
    record.stage_catalog.clone_from(catalog);
    record.pipeline_manifest.clone_from(manifest);
    let mut candidates = BTreeMap::new();
    for effect in preceding {
        match effect {
            Decision::RecordCandidateVehicle { bloom: owner, workpiece, vehicle } if *owner == bloom => {
                candidates.insert(workpiece.clone(), *vehicle);
            }
            Decision::AdvanceStage { bloom: owner, workpiece, progress }
                if *owner == bloom && progress.stage == StageId::Verify =>
            {
                if let Some(candidate) = progress.candidate {
                    candidates.insert(workpiece.clone(), candidate);
                }
            }
            _ => {}
        }
    }
    let mut state = PrecheckState::new(policy);
    state.latest_plan = plan_from_candidates(&record, bloom, &candidates);
    let mut effects = alloc::vec![record_state(bloom, state.clone())];
    if let Some(plan) = state.latest_plan {
        effects.push(Decision::QueuePrecheckPlan { bloom, plan });
    }
    effects
}

fn plan_from_candidates(
    record: &BloomRecord,
    bloom: BloomId,
    candidates: &BTreeMap<WorkpieceId, CandidateRef>,
) -> Option<PrecheckPlan> {
    let members = record
        .spec
        .members()
        .iter()
        .filter_map(|member| {
            candidates.get(&member.workpiece).copied().map(|candidate| PrecheckMember {
                workpiece: member.workpiece.clone(),
                scope_revision: member.scope_revision,
                candidate,
            })
        })
        .collect::<Vec<_>>();
    if members.len() < 2 {
        return None;
    }
    Some(PrecheckPlan {
        bloom,
        base: record.spec.base(),
        members,
        gate_set: VerifyGateSet::for_stage_of(StageId::AggregateVerify, &record.pipeline_manifest)?.digest(),
    })
}

fn run_decision(record: &BloomRecord, bloom: BloomId, node: &PrecheckNode, dispatch: bool) -> Decision {
    let binding = stage_binding(&record.stage_catalog, StageId::AggregateVerify);
    let transformation = Transformation::for_aggregate_verify(&binding, node.tree, node.head, record.spec.base());
    if dispatch {
        Decision::DispatchPrecheck {
            bloom,
            node: node.clone(),
            transformation,
            profile: binding.profile,
            configs: record.spec.configs().clone(),
        }
    } else {
        Decision::OfferPrecheck {
            bloom,
            node: node.clone(),
            transformation,
            profile: binding.profile,
            configs: record.spec.configs().clone(),
        }
    }
}

fn rejected(error: PrecheckError) -> Decisions {
    Decisions::rejected(Outcome::PrecheckRejected(error))
}

fn record_state(bloom: BloomId, state: PrecheckState) -> Decision {
    Decision::RecordPrecheckState { bloom, state: Some(state) }
}

fn active_state<'a>(
    snapshot: &'a Snapshot,
    bloom: &BloomId,
) -> Result<(&'a BloomRecord, &'a PrecheckState), PrecheckError> {
    let record = snapshot.blooms.get(bloom).ok_or(PrecheckError::UnknownOrInactiveBloom)?;
    if record.status != BloomStatus::Sealed {
        return Err(PrecheckError::UnknownOrInactiveBloom);
    }
    let state = record.precheck.as_ref().ok_or(PrecheckError::Disabled)?;
    Ok((record, state))
}

pub(super) fn reduce_precheck_prepared(
    snapshot: &Snapshot,
    bloom: &BloomId,
    plan: Digest,
    preparation: &PrecheckPreparation,
) -> Decisions {
    let (record, state) = match active_state(snapshot, bloom) {
        Ok(value) => value,
        Err(error) => return rejected(error),
    };
    let Some(expected_plan) = state.latest_plan.as_ref().map(PrecheckPlan::digest) else {
        return rejected(PrecheckError::PlanMismatch { expected: Digest::default(), got: plan });
    };
    if expected_plan != plan {
        return rejected(PrecheckError::PlanMismatch { expected: expected_plan, got: plan });
    }

    let mut next = state.clone();
    let (outcome, mut effects) = match preparation {
        PrecheckPreparation::Prepared(node) => {
            if node.plan != plan || node.gate_set != state.latest_plan.as_ref().expect("checked").gate_set {
                return rejected(PrecheckError::InvalidPreparation);
            }
            next.prepared = Some(node.clone());
            next.result = None;
            next.diagnostic = None;
            (Outcome::PrecheckPrepared { bloom: *bloom, node: node.digest() }, alloc::vec![])
        }
        PrecheckPreparation::Refused { detail } => {
            next.prepared = None;
            next.result = None;
            next.diagnostic = Some(PrecheckDiagnostic::PreparationRefused { plan, detail: *detail });
            (Outcome::PrecheckPreparationRefused { bloom: *bloom, plan, diagnostic: *detail }, alloc::vec![])
        }
    };
    effects.push(record_state(*bloom, next.clone()));
    if let Some(node) = next.prepared.as_ref()
        && next.can_request(node.digest())
        && record.operator_hold.is_none()
    {
        effects.push(run_decision(record, *bloom, node, false));
    }
    Decisions { outcome, effects }
}

pub(super) fn reduce_request_precheck(snapshot: &Snapshot, bloom: &BloomId, node: Digest) -> Decisions {
    let (record, state) = match active_state(snapshot, bloom) {
        Ok(value) => value,
        Err(error) => return rejected(error),
    };
    if record.operator_hold.is_some() {
        return rejected(PrecheckError::OnHold);
    }
    let Some(prepared) = state.prepared.as_ref() else {
        return rejected(PrecheckError::NoPreparedNode);
    };
    if prepared.digest() != node {
        return rejected(PrecheckError::NodeMismatch { expected: prepared.digest(), got: node });
    }
    if state.issued.is_some() {
        return rejected(PrecheckError::NoPreparedNode);
    }
    if state.remaining_runs() == 0 {
        return rejected(PrecheckError::BudgetExhausted { issued: state.issued_runs, budget: state.policy.run_budget });
    }
    if !state.can_request(node) {
        return rejected(PrecheckError::NoPreparedNode);
    }

    let mut next = state.clone();
    next.issued = Some(prepared.clone());
    next.issued_runs = next.issued_runs.saturating_add(1);
    next.result = None;
    let effects = alloc::vec![record_state(*bloom, next.clone()), run_decision(record, *bloom, prepared, true),];
    Decisions { outcome: Outcome::PrecheckRequested { bloom: *bloom, node, run: next.issued_runs }, effects }
}

pub(super) fn reduce_precheck_completed(
    snapshot: &Snapshot,
    bloom: &BloomId,
    node: Digest,
    completion: &PrecheckCompletion,
) -> Decisions {
    let (record, state) = match active_state(snapshot, bloom) {
        Ok(value) => value,
        Err(error) => return rejected(error),
    };
    let Some(issued) = state.issued.as_ref() else {
        return rejected(PrecheckError::NoIssuedNode);
    };
    if issued.digest() != node {
        return rejected(PrecheckError::NodeMismatch { expected: issued.digest(), got: node });
    }
    let expected_gate = VerifyGateSet::for_stage_of(StageId::AggregateVerify, &record.pipeline_manifest)
        .expect("aggregate verify has a gate")
        .digest();
    if issued.gate_set != expected_gate {
        return rejected(PrecheckError::GateMismatch { expected: expected_gate, got: issued.gate_set });
    }

    if state.joined_is(node) {
        return complete_joined(snapshot, *bloom, state, issued, completion);
    }

    let current = state.is_current_node(node);
    let mut next = state.clone();
    next.issued = None;
    let mut effects = alloc::vec![];
    match completion {
        PrecheckCompletion::Passed(evidence) => {
            if let Err(error) = validate_evidence(issued, evidence, EvidenceKind::VerificationResult) {
                return rejected(error);
            }
            effects.push(Decision::RecordEvidence { bloom: *bloom, evidence: evidence.clone() });
            effects.extend(proof_of(record, *bloom, StageId::AggregateVerify, evidence));
            if current {
                next.result = Some(PrecheckResult::Passed { node, evidence: evidence.detail });
                next.diagnostic = None;
            }
        }
        PrecheckCompletion::Failed(evidence) => {
            if let Err(error) = validate_evidence(issued, evidence, EvidenceKind::VerificationResult) {
                return rejected(error);
            }
            effects.push(Decision::RecordEvidence { bloom: *bloom, evidence: evidence.clone() });
            if current {
                next.result = Some(PrecheckResult::Failed { node, evidence: evidence.detail });
                next.diagnostic = Some(PrecheckDiagnostic::VerificationFailed { node, detail: evidence.detail });
            }
        }
        PrecheckCompletion::HostFault(evidence) => {
            if let Err(error) = validate_evidence(issued, evidence, EvidenceKind::ExecutorFault) {
                return rejected(error);
            }
            if current {
                next.result = Some(PrecheckResult::HostFault { node, evidence: evidence.detail });
                next.diagnostic = Some(PrecheckDiagnostic::HostFault { node, detail: evidence.detail });
            }
        }
        PrecheckCompletion::SkippedBeforeStart => {
            next.issued_runs = next.issued_runs.saturating_sub(1);
            if current {
                next.result = Some(PrecheckResult::SkippedBeforeStart { node });
            }
        }
    }
    effects.push(record_state(*bloom, next.clone()));
    if let Some(prepared) = next.prepared.as_ref()
        && next.can_request(prepared.digest())
        && record.operator_hold.is_none()
    {
        effects.push(run_decision(record, *bloom, prepared, false));
    }
    Decisions { outcome: Outcome::PrecheckCompleted { bloom: *bloom, node }, effects }
}

fn validate_evidence(node: &PrecheckNode, evidence: &Evidence, kind: EvidenceKind) -> Result<(), PrecheckError> {
    if evidence.kind != kind {
        return Err(PrecheckError::InvalidEvidenceKind);
    }
    evidence
        .validates(&node.tree)
        .then_some(())
        .ok_or(PrecheckError::SubjectMismatch { expected: node.tree, got: evidence.subject })
}

fn complete_joined(
    snapshot: &Snapshot,
    bloom: BloomId,
    state: &PrecheckState,
    issued: &PrecheckNode,
    completion: &PrecheckCompletion,
) -> Decisions {
    let record = snapshot.blooms.get(&bloom).expect("active joined bloom");
    let integration = record.integration.as_ref().expect("joined resolve recorded its fold");
    match completion {
        PrecheckCompletion::Passed(evidence) | PrecheckCompletion::Failed(evidence) => {
            if let Err(error) = validate_evidence(issued, evidence, EvidenceKind::VerificationResult) {
                return rejected(error);
            }
            let passed = matches!(completion, PrecheckCompletion::Passed(_));
            let mut settled = reduce_aggregate_verify_completed(snapshot, &bloom, passed, evidence);
            if matches!(settled.outcome, Outcome::AggregateVerifyRejected(_)) {
                return settled;
            }
            let mut next = state.clone();
            next.issued = None;
            next.result = Some(if passed {
                PrecheckResult::Passed { node: issued.digest(), evidence: evidence.detail }
            } else {
                next.diagnostic =
                    Some(PrecheckDiagnostic::VerificationFailed { node: issued.digest(), detail: evidence.detail });
                PrecheckResult::Failed { node: issued.digest(), evidence: evidence.detail }
            });
            settled.effects.push(record_state(bloom, next));
            settled
        }
        PrecheckCompletion::HostFault(evidence) => {
            if let Err(error) = validate_evidence(issued, evidence, EvidenceKind::ExecutorFault) {
                return rejected(error);
            }
            let mut next = state.clone();
            next.prepared = None;
            next.issued = None;
            next.final_join = None;
            next.promoted = false;
            next.result = Some(PrecheckResult::HostFault { node: issued.digest(), evidence: evidence.detail });
            next.diagnostic = Some(PrecheckDiagnostic::HostFault { node: issued.digest(), detail: evidence.detail });
            let mut effects = alloc::vec![record_state(bloom, next)];
            effects.extend(aggregate_verify_dispatch(record, bloom, integration.tree, integration.head));
            Decisions { outcome: Outcome::PrecheckCompleted { bloom, node: issued.digest() }, effects }
        }
        PrecheckCompletion::SkippedBeforeStart => {
            let mut next = state.clone();
            next.prepared = None;
            next.issued = None;
            next.issued_runs = next.issued_runs.saturating_sub(1);
            next.final_join = None;
            next.promoted = false;
            next.result = Some(PrecheckResult::SkippedBeforeStart { node: issued.digest() });
            let mut effects = alloc::vec![record_state(bloom, next)];
            effects.extend(aggregate_verify_dispatch(record, bloom, integration.tree, integration.head));
            Decisions { outcome: Outcome::PrecheckCompleted { bloom, node: issued.digest() }, effects }
        }
    }
}

/// Exact issued pre-check that final resolution may join.
pub(super) fn final_join(record: &BloomRecord, bloom: BloomId, tree: Digest, _head: Digest) -> Option<PrecheckNode> {
    let state = record.precheck.as_ref()?;
    let issued = state.issued.as_ref()?;
    let plan = plan_of(record, bloom)?;
    let active_members = record.spec.members().len().saturating_sub(record.withdrawn.len());
    (plan.members.len() == active_members
        && issued.plan == plan.digest()
        && issued.tree == tree
        && issued.gate_set == plan.gate_set)
        .then_some(issued.clone())
}

/// Append coalesced scheduling effects after a member candidate transition.
pub(super) fn schedule(snapshot: &Snapshot, mut decisions: Decisions) -> Decisions {
    let observed = decisions.effects.clone();
    let mut affected = BTreeSet::new();
    let mut terminal = BTreeSet::new();
    for effect in &observed {
        match effect {
            Decision::AdvanceStage { bloom, .. }
                if snapshot.blooms.get(bloom).is_some_and(|r| r.precheck.is_some()) =>
            {
                affected.insert(*bloom);
            }
            Decision::RecordCandidateVehicle { bloom, .. }
            | Decision::RevokeResolution { bloom, .. }
            | Decision::RecordWithdrawal { bloom, .. }
            | Decision::RecordOperatorHold { bloom, .. }
            | Decision::RecordOperatorRelease { bloom, .. }
            | Decision::RecordIntegration { bloom, .. } => {
                affected.insert(*bloom);
            }
            Decision::MarkSuperseded { bloom, .. }
            | Decision::MarkBloomWithdrawn { bloom }
            | Decision::SetResolved { bloom, .. } => {
                affected.insert(*bloom);
                terminal.insert(*bloom);
            }
            _ => {}
        }
    }
    for bloom in affected {
        let Some(record) = snapshot.blooms.get(&bloom) else {
            continue;
        };
        let Some(state) = record.precheck.as_ref() else {
            continue;
        };
        if terminal.contains(&bloom) {
            if let Some(issued) = state.issued.as_ref() {
                decisions.effects.push(Decision::CancelPrecheck { bloom, node: issued.digest() });
            }
            decisions.effects.push(Decision::RecordPrecheckState { bloom, state: None });
            continue;
        }
        schedule_bloom(record, bloom, state, &observed, &mut decisions.effects);
    }
    decisions
}

fn schedule_bloom(
    record: &BloomRecord,
    bloom: BloomId,
    state: &PrecheckState,
    observed: &[Decision],
    effects: &mut Vec<Decision>,
) {
    let settlement_owned = observed
        .iter()
        .any(|effect| matches!(effect, Decision::RecordPrecheckState { bloom: owner, .. } if *owner == bloom));
    if !settlement_owned
        && state.final_join.is_some()
        && observed.iter().any(|effect| match effect {
            Decision::AdvanceStage { bloom: owner, workpiece, .. } => *owner == bloom && workpiece.is_composition(),
            Decision::RecordIntegration { bloom: owner, .. } => *owner == bloom,
            _ => false,
        })
    {
        let mut next = state.clone();
        next.prepared = None;
        next.result = None;
        next.diagnostic = None;
        next.final_join = None;
        next.promoted = false;
        if let Some(issued) = state.issued.as_ref() {
            effects.push(Decision::CancelPrecheck { bloom, node: issued.digest() });
        }
        effects.push(record_state(bloom, next));
        return;
    }

    let mut candidates = candidates_of(record);
    let mut paused = state.paused;
    for effect in observed {
        match effect {
            Decision::AdvanceStage { bloom: owner, workpiece, progress } if *owner == bloom => {
                if progress.stage == StageId::Verify {
                    if let Some(candidate) = progress.candidate {
                        candidates.insert(workpiece.clone(), candidate);
                    }
                } else {
                    candidates.remove(workpiece);
                }
            }
            Decision::RecordCandidateVehicle { bloom: owner, workpiece, vehicle } if *owner == bloom => {
                candidates.insert(workpiece.clone(), *vehicle);
            }
            Decision::RevokeResolution { bloom: owner, workpiece } if *owner == bloom => {
                candidates.remove(workpiece);
            }
            Decision::RecordWithdrawal { bloom: owner, withdrawal } if *owner == bloom => {
                candidates.remove(&withdrawal.workpiece);
            }
            Decision::RecordOperatorHold { bloom: owner, .. } if *owner == bloom => paused = true,
            Decision::RecordOperatorRelease { bloom: owner, .. } if *owner == bloom => paused = false,
            _ => {}
        }
    }

    let plan = plan_from_candidates(record, bloom, &candidates);
    if state.latest_plan.as_ref().map(PrecheckPlan::digest) == plan.as_ref().map(PrecheckPlan::digest)
        && state.paused != paused
    {
        let mut next = state.clone();
        next.paused = paused;
        effects.push(record_state(bloom, next.clone()));
        if let Some(node) = next.prepared.as_ref().filter(|node| next.can_request(node.digest())) {
            effects.push(run_decision(record, bloom, node, false));
        }
        return;
    }
    if state.latest_plan.as_ref().map(PrecheckPlan::digest) == plan.as_ref().map(PrecheckPlan::digest) {
        return;
    }
    let mut next = state.clone();
    next.latest_plan.clone_from(&plan);
    next.prepared = None;
    next.result = None;
    next.diagnostic = None;
    next.final_join = None;
    next.promoted = false;
    next.paused = paused;
    if let Some(issued) = state.issued.as_ref() {
        effects.push(Decision::CancelPrecheck { bloom, node: issued.digest() });
    }
    effects.push(record_state(bloom, next.clone()));
    if let Some(plan) = plan
        && next.remaining_runs() > 0
    {
        effects.push(Decision::QueuePrecheckPlan { bloom, plan });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{claim, digest, draft, event, membership, workpiece};
    use crate::{Fact, OperatorHold, ResolvedConfigs};

    fn candidate_record(base: u8, members: &[(&str, u8, u8, u8)]) -> (BloomId, BloomRecord) {
        let spec =
            draft(base, members.iter().map(|(name, revision, _, _)| membership(name, *revision)).collect()).seal();
        let bloom = spec.id();
        let mut record = BloomRecord::empty(spec);
        for (name, revision, tree, checkout) in members {
            let claim = claim(name, *revision, *tree);
            record.claims.insert(claim.workpiece.clone(), claim);
            record.vehicles.insert(workpiece(name), CandidateRef { tree: digest(*tree), checkout: digest(*checkout) });
        }
        (bloom, record)
    }

    fn issued_fixture() -> (Snapshot, BloomId, PrecheckNode) {
        let (bloom, mut record) = candidate_record(1, &[("one", 11, 21, 31), ("two", 12, 22, 32)]);
        let plan = plan_of(&record, bloom).expect("two captured members form a plan");
        let node = PrecheckNode { plan: plan.digest(), tree: digest(40), head: digest(41), gate_set: plan.gate_set };
        let mut state = PrecheckState::new(PrecheckPolicy { run_budget: 2 });
        state.latest_plan = Some(plan);
        state.prepared = Some(node.clone());
        state.issued = Some(node.clone());
        state.issued_runs = 1;
        record.precheck = Some(state);

        let mut snapshot = Snapshot::new(digest(1));
        snapshot.blooms.insert(bloom, record);
        (snapshot, bloom, node)
    }

    fn recorded_state(effects: &[Decision], bloom: BloomId) -> Option<&PrecheckState> {
        effects.iter().rev().find_map(|effect| match effect {
            Decision::RecordPrecheckState { bloom: owner, state } if *owner == bloom => state.as_ref(),
            _ => None,
        })
    }

    #[test]
    fn inherited_claim_needs_its_matching_checkout_vehicle() {
        let (bloom, mut record) = candidate_record(1, &[("one", 11, 21, 31), ("two", 12, 22, 32)]);
        record.vehicles.remove(&workpiece("two"));
        assert!(plan_of(&record, bloom).is_none());

        record.vehicles.insert(workpiece("two"), CandidateRef { tree: digest(99), checkout: digest(32) });
        assert!(plan_of(&record, bloom).is_none());

        record.vehicles.insert(workpiece("two"), CandidateRef { tree: digest(22), checkout: digest(32) });
        assert_eq!(plan_of(&record, bloom).expect("matching vehicle completes the plan").members.len(), 2);
    }

    #[test]
    fn final_join_accepts_the_same_tree_on_a_distinct_checkout_head() {
        let (snapshot, bloom, node) = issued_fixture();
        let record = snapshot.blooms.get(&bloom).expect("fixture record");

        assert_eq!(final_join(record, bloom, node.tree, digest(99)), Some(node));
    }

    #[test]
    fn completion_evidence_kind_must_match_the_reported_result() {
        let (snapshot, bloom, node) = issued_fixture();
        let completion = PrecheckCompletion::HostFault(Evidence {
            subject: node.tree,
            kind: EvidenceKind::VerificationResult,
            detail: digest(50),
        });

        let decisions = reduce_precheck_completed(&snapshot, &bloom, node.digest(), &completion);

        assert!(matches!(decisions.outcome, Outcome::PrecheckRejected(PrecheckError::InvalidEvidenceKind)));
        assert!(decisions.effects.is_empty());
    }

    #[test]
    fn joined_host_fault_retries_required_verify_without_composition_repair() {
        let (mut snapshot, bloom, node) = issued_fixture();
        let record = snapshot.blooms.get_mut(&bloom).expect("fixture record");
        record.integration =
            Some(super::super::FoldedIntegration { tree: node.tree, head: digest(99), lineage: vec![] });
        let state = record.precheck.as_mut().expect("fixture pre-check state");
        state.final_join = Some(node.clone());
        state.promoted = true;
        let completion = PrecheckCompletion::HostFault(Evidence {
            subject: node.tree,
            kind: EvidenceKind::ExecutorFault,
            detail: digest(51),
        });

        let decisions = reduce_precheck_completed(&snapshot, &bloom, node.digest(), &completion);

        assert!(decisions.effects.iter().any(|effect| matches!(effect, Decision::DispatchAggregateVerify { .. })));
        assert!(!decisions.effects.iter().any(|effect| matches!(
            effect,
            Decision::RecordEvidence { .. }
                | Decision::RecordCompositionFinding { .. }
                | Decision::RevokeResolution { .. }
        )));
        let state = recorded_state(&decisions.effects, bloom).expect("settled state");
        assert!(state.prepared.is_none());
        assert!(state.issued.is_none());
        assert!(state.final_join.is_none());
        assert!(!state.promoted);

        let applied = snapshot.apply(
            &event("joined-precheck-host-fault", Fact::PrecheckCompleted { bloom, node: node.digest(), completion }),
            &decisions,
            &ResolvedConfigs::default(),
        );
        assert!(applied.blooms.get(&bloom).expect("applied record").aggregate_fault.is_none());

        let mut released = state.clone();
        released.paused = true;
        snapshot.blooms.get_mut(&bloom).expect("fixture record").precheck = Some(released);
        let after_release = schedule(
            &snapshot,
            Decisions {
                outcome: Outcome::Duplicate,
                effects: vec![Decision::RecordOperatorRelease {
                    bloom,
                    release: OperatorHold { reason: "host repaired".into(), operator: "operator".into() },
                }],
            },
        );
        assert!(!after_release.effects.iter().any(|effect| matches!(effect, Decision::OfferPrecheck { .. })));
    }

    #[test]
    fn joined_skip_refunds_budget_without_reoffering_the_node() {
        let (mut snapshot, bloom, node) = issued_fixture();
        let record = snapshot.blooms.get_mut(&bloom).expect("fixture record");
        record.integration =
            Some(super::super::FoldedIntegration { tree: node.tree, head: digest(99), lineage: vec![] });
        let state = record.precheck.as_mut().expect("fixture pre-check state");
        state.final_join = Some(node.clone());
        state.promoted = true;

        let decisions =
            reduce_precheck_completed(&snapshot, &bloom, node.digest(), &PrecheckCompletion::SkippedBeforeStart);

        assert!(decisions.effects.iter().any(|effect| matches!(effect, Decision::DispatchAggregateVerify { .. })));
        let state = recorded_state(&decisions.effects, bloom).expect("settled state");
        assert!(state.prepared.is_none());
        assert!(state.issued.is_none());
        assert_eq!(state.issued_runs, 0);
        assert!(state.final_join.is_none());
    }

    #[test]
    fn joined_failure_retains_its_diagnostic_owner_after_reweave() {
        let (mut snapshot, bloom, node) = issued_fixture();
        let record = snapshot.blooms.get_mut(&bloom).expect("fixture record");
        record.integration =
            Some(super::super::FoldedIntegration { tree: node.tree, head: digest(99), lineage: vec![] });
        let state = record.precheck.as_mut().expect("fixture pre-check state");
        state.final_join = Some(node.clone());
        state.promoted = true;
        let completion = PrecheckCompletion::Failed(Evidence {
            subject: node.tree,
            kind: EvidenceKind::VerificationResult,
            detail: digest(53),
        });

        let decisions = schedule(&snapshot, reduce_precheck_completed(&snapshot, &bloom, node.digest(), &completion));

        assert!(decisions.effects.iter().any(|effect| matches!(
            effect,
            Decision::AdvanceStage { workpiece, .. } if workpiece.is_composition()
        )));
        let state = recorded_state(&decisions.effects, bloom).expect("joined failure state");
        assert!(state.issued.is_none());
        assert_eq!(state.final_join, Some(node.clone()));
        assert!(matches!(
            state.diagnostic.as_ref(),
            Some(PrecheckDiagnostic::VerificationFailed { node: failed, detail })
                if *failed == node.digest() && *detail == digest(53)
        ));
    }

    #[test]
    fn replacing_joined_fold_makes_a_late_precheck_settle_as_stale() {
        let (mut snapshot, bloom, node) = issued_fixture();
        let original = super::super::FoldedIntegration { tree: node.tree, head: digest(99), lineage: vec![] };
        let record = snapshot.blooms.get_mut(&bloom).expect("fixture record");
        record.integration = Some(original);
        let state = record.precheck.as_mut().expect("fixture pre-check state");
        state.final_join = Some(node.clone());
        state.promoted = true;
        let replacement = super::super::FoldedIntegration { tree: digest(60), head: digest(61), lineage: vec![] };

        let invalidated = schedule(
            &snapshot,
            Decisions {
                outcome: Outcome::Duplicate,
                effects: vec![Decision::RecordIntegration { bloom, integration: Some(replacement.clone()) }],
            },
        );
        let state = recorded_state(&invalidated.effects, bloom).expect("join invalidation state").clone();
        assert_eq!(state.issued, Some(node.clone()));
        assert!(state.prepared.is_none());
        assert!(state.final_join.is_none());
        assert!(!state.promoted);
        assert!(invalidated.effects.iter().any(
            |effect| matches!(effect, Decision::CancelPrecheck { node: canceled, .. } if *canceled == node.digest())
        ));

        let record = snapshot.blooms.get_mut(&bloom).expect("fixture record");
        record.integration = Some(replacement);
        record.precheck = Some(state);
        let completion = PrecheckCompletion::Passed(Evidence {
            subject: node.tree,
            kind: EvidenceKind::VerificationResult,
            detail: digest(52),
        });
        let late = reduce_precheck_completed(&snapshot, &bloom, node.digest(), &completion);

        assert!(!late.effects.iter().any(|effect| matches!(
            effect,
            Decision::RecordAggregateGatePass { .. }
                | Decision::RecordCompositionFinding { .. }
                | Decision::SetResolved { .. }
                | Decision::DispatchLand { .. }
        )));
        assert!(recorded_state(&late.effects, bloom).expect("stale settlement state").issued.is_none());
    }

    #[test]
    fn supersession_batch_clears_only_the_terminal_predecessor() {
        let (mut snapshot, predecessor, node) = issued_fixture();
        let (successor, mut successor_record) = candidate_record(2, &[("one", 11, 21, 31), ("two", 12, 22, 32)]);
        successor_record.vehicles.remove(&workpiece("two"));
        successor_record.precheck = Some(PrecheckState::new(PrecheckPolicy { run_budget: 2 }));
        snapshot.blooms.insert(successor, successor_record);

        let decisions = schedule(
            &snapshot,
            Decisions {
                outcome: Outcome::Duplicate,
                effects: vec![
                    Decision::MarkSuperseded { bloom: predecessor, by: successor },
                    Decision::RecordCandidateVehicle {
                        bloom: successor,
                        workpiece: workpiece("two"),
                        vehicle: CandidateRef { tree: digest(22), checkout: digest(32) },
                    },
                ],
            },
        );

        assert!(decisions.effects.iter().any(|effect| matches!(
            effect,
            Decision::RecordPrecheckState { bloom, state: None } if *bloom == predecessor
        )));
        assert!(decisions.effects.iter().any(|effect| matches!(
            effect,
            Decision::CancelPrecheck { bloom, node: canceled } if *bloom == predecessor && *canceled == node.digest()
        )));
        assert!(recorded_state(&decisions.effects, successor).and_then(|state| state.latest_plan.as_ref()).is_some());
        assert!(
            decisions
                .effects
                .iter()
                .any(|effect| matches!(effect, Decision::QueuePrecheckPlan { bloom, .. } if *bloom == successor))
        );
    }
}
