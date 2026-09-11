//! Reducer-owned scheduling and settlement for aggregate pre-checks.

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};

use super::aggregate_verify::{aggregate_verify_dispatch, reduce_aggregate_verify_completed};
use super::attempt::stage_binding;
use super::coordination::{promote_proved_repair, schedule_partial_head_repair};
use super::verify_memo::proof_of;
use super::{BloomRecord, BloomStatus, Decision, Decisions, Outcome, PrecheckError, Snapshot};
use crate::digest::Digest;
use crate::ids::{BloomId, StageId, WorkpieceId};
use crate::values::{
    BloomSpec, CandidateRef, CoordinationState, Evidence, EvidenceKind, PipelineManifest, PrecheckCompletion,
    PrecheckDiagnostic, PrecheckMember, PrecheckNode, PrecheckPlan, PrecheckPolicy, PrecheckPreparation,
    PrecheckResult, PrecheckState, StageCatalog, Transformation, VerifyGateSet,
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
    if let Some(coordination) = record.coordination.as_deref().filter(|state| selected_root_policy(state)) {
        return selected_head_plan(record, bloom, coordination);
    }
    plan_from_candidates(record, bloom, &candidates_of(record))
}

fn selected_root_policy(state: &CoordinationState) -> bool {
    state.policy.eager_integration || state.policy.verification == crate::VerificationMode::Contextual
}

fn selected_head_plan(record: &BloomRecord, bloom: BloomId, coordination: &CoordinationState) -> Option<PrecheckPlan> {
    if !coordination.uses_selected_root_for_precheck() {
        return None;
    }
    let head = &coordination.integration.head;
    if head.coverage.is_empty() || head.generation != coordination.integration.generation.digest() {
        return None;
    }
    Some(PrecheckPlan {
        bloom,
        base: head.candidate.checkout,
        members: head
            .coverage
            .iter()
            .map(|pin| PrecheckMember {
                workpiece: pin.workpiece.clone(),
                scope_revision: pin.scope_revision,
                candidate: pin.candidate,
            })
            .collect(),
        gate_set: VerifyGateSet::for_stage_of(StageId::AggregateVerify, &record.pipeline_manifest)?.digest(),
    })
}

fn selected_head_node(plan: &PrecheckPlan, coordination: &CoordinationState) -> PrecheckNode {
    // The source already materialized and checked this head. Re-folding its
    // leaves would discard interaction repairs and could change merge order.
    PrecheckNode {
        plan: plan.digest(),
        tree: coordination.integration.head.candidate.tree,
        head: coordination.integration.head.candidate.checkout,
        gate_set: plan.gate_set,
    }
}

fn contextual_precheck_receipt(
    record: &BloomRecord,
    bloom: BloomId,
    coordination: &CoordinationState,
    node: &PrecheckNode,
) -> Option<Digest> {
    if coordination.integration.known_red == Some(coordination.integration.head.node)
        || selected_head_node(&selected_head_plan(record, bloom, coordination)?, coordination) != *node
    {
        return None;
    }
    coordination.contextual_aggregate_proof(&coordination.integration.head).map(|receipt| receipt.detail)
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
            Decision::RecordCoordinationState { bloom: owner, state } if *owner == bloom => {
                record.coordination = state.clone().map(Box::new);
            }
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
    if let Some(coordination) = record.coordination.as_deref().filter(|state| selected_root_policy(state)) {
        state.latest_plan = selected_head_plan(&record, bloom, coordination);
        state.prepared = state.latest_plan.as_ref().map(|plan| selected_head_node(plan, coordination));
        let mut effects = alloc::vec![record_state(bloom, state.clone())];
        if let Some(node) = state.prepared.as_ref().filter(|node| state.can_request(node.digest())) {
            effects.push(run_decision(&record, bloom, node, false));
        }
        return effects;
    }
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
            if let Some(coordination) = record.coordination.as_deref().filter(|state| selected_root_policy(state))
                && selected_head_plan(record, *bloom, coordination)
                    .is_none_or(|plan| selected_head_node(&plan, coordination) != *node)
            {
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
    if let Some(receipt) = record
        .coordination
        .as_deref()
        .filter(|coordination| selected_root_policy(coordination))
        .and_then(|coordination| contextual_precheck_receipt(record, *bloom, coordination, prepared))
    {
        let mut next = state.clone();
        next.result = Some(PrecheckResult::Passed { node, evidence: receipt });
        next.diagnostic = None;
        return Decisions {
            outcome: Outcome::PrecheckCompleted { bloom: *bloom, node },
            effects: alloc::vec![record_state(*bloom, next)],
        };
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
    let refunded_exhausted_run =
        matches!(completion, PrecheckCompletion::SkippedBeforeStart) && state.remaining_runs() == 0;
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
            record_head_promotion(record, *bloom, issued, &mut effects);
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
            record_head_failure(record, *bloom, issued, evidence.detail, &mut effects);
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
    if let Some(prepared) = next.prepared.as_ref() {
        if next.can_request(prepared.digest()) && record.operator_hold.is_none() {
            effects.push(run_decision(record, *bloom, prepared, false));
        }
    } else if refunded_exhausted_run
        && next.remaining_runs() > 0
        && let Some(plan) = next.latest_plan
    {
        effects.push(Decision::QueuePrecheckPlan { bloom: *bloom, plan });
    }
    Decisions { outcome: Outcome::PrecheckCompleted { bloom: *bloom, node }, effects }
}

fn record_head_failure(
    record: &BloomRecord,
    bloom: BloomId,
    issued: &PrecheckNode,
    evidence: Digest,
    effects: &mut Vec<Decision>,
) {
    let Some(coordination) = record.coordination.as_deref().filter(|state| selected_root_policy(state)) else {
        return;
    };
    let Some(plan) = selected_head_plan(record, bloom, coordination) else {
        return;
    };
    if selected_head_node(&plan, coordination) != *issued {
        return;
    }
    let mut next = coordination.clone();
    next.integration.known_red = Some(coordination.integration.head.node);
    let mut repair_effects = Vec::new();
    schedule_partial_head_repair(record, &mut next, evidence, &mut repair_effects);
    if next != *coordination {
        effects.push(Decision::RecordCoordinationState { bloom, state: Some(next) });
    }
    effects.extend(repair_effects);
}

/// The mirror of [`record_head_failure`] for the one red verdict a pre-check
/// green may lift: the repair that produced this exact head owes an aggregate
/// proof, and this run is it. Every other red stays (ADR-0218).
fn record_head_promotion(record: &BloomRecord, bloom: BloomId, issued: &PrecheckNode, effects: &mut Vec<Decision>) {
    let Some(coordination) = record.coordination.as_deref().filter(|state| selected_root_policy(state)) else {
        return;
    };
    let Some(plan) = selected_head_plan(record, bloom, coordination) else {
        return;
    };
    if selected_head_node(&plan, coordination) != *issued {
        return;
    }
    let mut next = coordination.clone();
    let mut promoted = Vec::new();
    promote_proved_repair(record, &mut next, coordination.integration.head.node, &mut promoted);
    if next != *coordination {
        effects.push(Decision::RecordCoordinationState { bloom, state: Some(next) });
    }
    effects.extend(promoted);
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
    let mut scheduled = Vec::new();
    {
        let observed = decisions.effects.as_slice();
        let mut affected = BTreeSet::new();
        let mut terminal = BTreeSet::new();
        for effect in observed {
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
                | Decision::RecordIntegration { bloom, .. }
                | Decision::RecordCoordinationState { bloom, .. } => {
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
                    scheduled.push(Decision::CancelPrecheck { bloom, node: issued.digest() });
                }
                scheduled.push(Decision::RecordPrecheckState { bloom, state: None });
                continue;
            }
            schedule_bloom(record, bloom, state, observed, &mut scheduled);
        }
    }
    decisions.effects.extend(scheduled);
    decisions
}

fn schedule_bloom(
    record: &BloomRecord,
    bloom: BloomId,
    state: &PrecheckState,
    observed: &[Decision],
    effects: &mut Vec<Decision>,
) {
    let coordination = observed
        .iter()
        .rev()
        .find_map(|effect| match effect {
            Decision::RecordCoordinationState { bloom: owner, state } if *owner == bloom => Some(state.as_ref()),
            _ => None,
        })
        .unwrap_or(record.coordination.as_deref());
    if let Some(coordination) = coordination.filter(|state| selected_root_policy(state)) {
        schedule_eager_head(record, bloom, state, coordination, observed, effects);
        return;
    }
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

fn schedule_eager_head(
    record: &BloomRecord,
    bloom: BloomId,
    state: &PrecheckState,
    coordination: &CoordinationState,
    observed: &[Decision],
    effects: &mut Vec<Decision>,
) {
    let state = observed
        .iter()
        .rev()
        .find_map(|effect| match effect {
            Decision::RecordPrecheckState { bloom: owner, state } if *owner == bloom => Some(state.as_ref()),
            _ => None,
        })
        .unwrap_or(Some(state));
    let Some(state) = state else {
        return;
    };
    if coordination.final_in_flight {
        return;
    }
    let plan = selected_head_plan(record, bloom, coordination);
    let node = plan.as_ref().map(|plan| selected_head_node(plan, coordination));
    let mut paused = state.paused;
    for effect in observed {
        match effect {
            Decision::RecordOperatorHold { bloom: owner, .. } if *owner == bloom => paused = true,
            Decision::RecordOperatorRelease { bloom: owner, .. } if *owner == bloom => paused = false,
            _ => {}
        }
    }
    let changed = state.latest_plan != plan || state.prepared != node;
    let reused = node.as_ref().and_then(|node| {
        contextual_precheck_receipt(record, bloom, coordination, node)
            .map(|receipt| PrecheckResult::Passed { node: node.digest(), evidence: receipt })
    });
    if !changed && state.paused == paused && reused.as_ref().is_none_or(|reused| state.result.as_ref() == Some(reused))
    {
        return;
    }
    let mut next = state.clone();
    next.paused = paused;
    if changed {
        next.latest_plan = plan;
        next.prepared = node;
        next.result = None;
        next.diagnostic = None;
        next.final_join = None;
        next.promoted = false;
        if let Some(issued) = state.issued.as_ref() {
            // Existing cancellation only skips a confirmed unstarted offer;
            // a started run keeps its immutable input and retained receipt.
            effects.push(Decision::CancelPrecheck { bloom, node: issued.digest() });
        }
    }
    if let Some(reused) = reused {
        // The contextual receipt already judged this exact aggregate subject
        // and contract. Keep its identity without minting a parent-tree proof.
        next.result = Some(reused);
        next.diagnostic = None;
    }
    effects.push(record_state(bloom, next.clone()));
    if let Some(node) = next.prepared.as_ref().filter(|node| next.can_request(node.digest()))
        && !observed.iter().any(|effect| {
            matches!(
                effect, Decision::OfferPrecheck { bloom: owner, node: offered, .. }
                    if *owner == bloom && offered == node
            )
        })
    {
        effects.push(run_decision(record, bloom, node, false));
    }
}

#[cfg(test)]
mod tests {
    use alloc::format;

    use aether_data::Kind;
    use aether_data::wire::to_vec;

    use super::*;
    use crate::reduce::coordination;
    use crate::reduce::reduce as reduce_event;
    use crate::testing::{claim, compiled_resolved, digest, draft, event, membership, workpiece};
    use crate::values::config_address;
    use crate::{
        CompositionPlan, Fact, IntegrationHead, MemberContractPin, MemberVerifyOutcome, MemberVerifyRequest,
        OperatorHold, ResolvedConfigs, SharedRunMode, SharedRunNode, SharedRunPhase, SharedRunPlan, SharedRunRecord,
        SpendWindow, VerificationContract, VerificationObligation,
    };

    fn step_with_configs(
        snapshot: &Snapshot,
        event: &crate::Event,
        configs: &ResolvedConfigs,
    ) -> (Snapshot, Decisions) {
        let decisions = reduce_event(snapshot, event, configs, &SpendWindow::default());
        (snapshot.apply(event, &decisions, configs), decisions)
    }

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

    fn eager_fixture() -> (Snapshot, BloomId, PrecheckNode) {
        let (bloom, mut record) = candidate_record(1, &[("one", 11, 21, 31), ("two", 12, 22, 32)]);
        let mut snapshot = Snapshot::new(digest(1));
        let effects = coordination::initialized_effects(
            &snapshot,
            &record.spec,
            &record.stage_catalog,
            &record.pipeline_manifest,
            Some(crate::CoordinationPolicy {
                verification: crate::VerificationMode::Contextual,
                eager_integration: true,
                max_run_members: 8,
                max_serial_requests: 8,
                max_attribution_probes: 32,
                movement_budget: 3,
                reservation_millis: 30_000,
                host_class: "test".to_owned(),
            }),
        )
        .expect("valid coordination policy");
        let mut coordination = effects
            .into_iter()
            .find_map(|effect| match effect {
                Decision::RecordCoordinationState { state: Some(state), .. } => Some(state),
                _ => None,
            })
            .expect("initial state");
        coordination.integration.head.coverage = record
            .spec
            .members()
            .iter()
            .rev()
            .map(|member| crate::MemberPin {
                workpiece: member.workpiece.clone(),
                scope_revision: member.scope_revision,
                candidate: record.vehicles[&member.workpiece],
            })
            .collect();
        coordination.integration.head.candidate = CandidateRef { tree: digest(40), checkout: digest(41) };
        coordination.integration.head.node = digest(42);
        coordination.integration.head.plan = digest(43);
        coordination.integration.admitted = vec![crate::CompositionInput {
            node: coordination.integration.head.node,
            candidate: coordination.integration.head.candidate,
            members: coordination.integration.head.coverage.clone(),
        }];
        record.coordination = Some(Box::new(coordination));
        let plan = plan_of(&record, bloom).expect("selected head has coverage");
        let node = selected_head_node(&plan, record.coordination.as_deref().expect("coordination"));
        let mut state = PrecheckState::new(PrecheckPolicy { run_budget: 3 });
        state.latest_plan = Some(plan);
        state.prepared = Some(node.clone());
        state.issued = Some(node.clone());
        state.issued_runs = 1;
        record.precheck = Some(state);
        snapshot.blooms.insert(bloom, record);
        (snapshot, bloom, node)
    }

    fn advance_head(snapshot: &Snapshot, bloom: BloomId, tree: u8, checkout: u8) -> Decisions {
        let mut state = snapshot.blooms[&bloom].coordination.as_deref().cloned().expect("coordination");
        state.integration.head.candidate = CandidateRef { tree: digest(tree), checkout: digest(checkout) };
        state.integration.head.node = digest(checkout);
        schedule(
            snapshot,
            Decisions {
                outcome: Outcome::PrecheckPrepared { bloom, node: digest(tree) },
                effects: vec![Decision::RecordCoordinationState { bloom, state: Some(state) }],
            },
        )
    }

    fn contextual_requests_fixture(
        bloom: BloomId,
        coordination: &CoordinationState,
        binding: &crate::StageBinding,
    ) -> Vec<MemberVerifyRequest> {
        let input = &coordination.integration.admitted[0];
        let base = coordination.integration.generation.base;
        input
            .members
            .iter()
            .map(|member| {
                let transformation = Transformation::for_member_stage(
                    binding,
                    member.candidate.tree,
                    member.candidate.checkout,
                    base.checkout,
                );
                let mut obligations = coordination
                    .composition_contract
                    .gate_identities
                    .iter()
                    .map(|identity| VerificationObligation::Gate { identity: identity.clone() })
                    .collect::<Vec<_>>();
                obligations.push(VerificationObligation::MemberDelta {
                    scope_revision: member.scope_revision,
                    candidate: member.candidate,
                    diff_base: base,
                });
                MemberVerifyRequest {
                    bloom,
                    member: member.clone(),
                    input: input.clone(),
                    attempt: 1,
                    context: None,
                    contract: VerificationContract {
                        gate_set: digest(80),
                        obligations,
                        diff_base: base,
                        invocation: digest(83),
                        environment: coordination.composition_contract.environment,
                        host_class: coordination.composition_contract.host_class,
                    },
                    transformation,
                    profile: binding.profile.clone(),
                    configs: coordination.composition_contract.invocation.configs.clone(),
                }
            })
            .collect()
    }

    fn contextual_receipt_fixture() -> (Snapshot, BloomId, PrecheckNode, Evidence) {
        let (mut snapshot, bloom, node) = eager_fixture();
        let record = snapshot.blooms.get_mut(&bloom).expect("bloom");
        record.precheck.as_mut().expect("precheck").issued = None;
        let binding = stage_binding(&record.stage_catalog, StageId::Verify);
        let coordination = record.coordination.as_deref_mut().expect("coordination");
        let base = coordination.integration.generation.base;
        let requests = contextual_requests_fixture(bloom, coordination, &binding);
        let composition = CompositionPlan {
            bloom,
            base: IntegrationHead {
                generation: coordination.integration.generation.digest(),
                node: base.tree,
                candidate: base,
                plan: digest(81),
                coverage: Vec::new(),
            },
            inputs: coordination.integration.admitted.clone(),
            requests: requests.clone(),
            contract: coordination.composition_contract.bind(
                requests
                    .iter()
                    .map(|request| MemberContractPin { request: request.digest(), contract: request.contract.digest() })
                    .collect(),
            ),
        };
        let shared_node = SharedRunNode {
            plan: composition.digest(),
            candidate: coordination.integration.head.candidate,
            coverage: coordination.integration.head.coverage.clone(),
        };
        let receipt = Evidence {
            subject: shared_node.candidate.tree,
            kind: EvidenceKind::VerificationResult,
            detail: digest(90),
        };
        let completed = requests
            .iter()
            .map(|request| MemberVerifyOutcome::PassedIn {
                request: request.digest(),
                node: shared_node.digest(),
                receipt: receipt.clone(),
            })
            .collect();
        coordination.runs.push(SharedRunRecord {
            plan: SharedRunPlan {
                mode: SharedRunMode::Contextual,
                requests,
                composition: Some(composition),
                probe_budget: 32,
                execution_attempt: 0,
            },
            node: Some(shared_node),
            phase: SharedRunPhase::Terminal,
            stale: false,
            physical_run: Some(digest(82)),
            completed,
            unfinished: Vec::new(),
            latencies: Vec::new(),
        });
        (snapshot, bloom, node, receipt)
    }

    #[test]
    fn exact_contextual_aggregate_receipt_skips_an_extra_precheck_run() {
        let (snapshot, bloom, node, receipt) = contextual_receipt_fixture();
        let decisions = advance_head(&snapshot, bloom, 40, 41);
        let state = recorded_state(&decisions.effects, bloom).expect("contextual reuse is visible");
        assert_eq!(state.result, Some(PrecheckResult::Passed { node: node.digest(), evidence: receipt.detail }));
        assert!(state.issued.is_none());
        assert_eq!(state.issued_runs, 1);
        assert!(!decisions.effects.iter().any(|effect| matches!(
            effect,
            Decision::OfferPrecheck { .. }
                | Decision::DispatchPrecheck { .. }
                | Decision::QueuePrecheckPlan { .. }
                | Decision::RecordVerifyProof { .. }
        )));

        let delayed_offer = reduce_request_precheck(&snapshot, &bloom, node.digest());
        let state = recorded_state(&delayed_offer.effects, bloom).expect("delayed offer reuses receipt");
        assert!(state.issued.is_none());
        assert_eq!(state.issued_runs, 1);
        assert_eq!(state.result, Some(PrecheckResult::Passed { node: node.digest(), evidence: receipt.detail }));
        assert!(!delayed_offer.effects.iter().any(|effect| matches!(effect, Decision::DispatchPrecheck { .. })));
    }

    #[test]
    fn contextual_precheck_reuse_requires_the_exact_checkout_and_contract() {
        let (snapshot, bloom, _, _) = contextual_receipt_fixture();
        let changed_checkout = advance_head(&snapshot, bloom, 40, 51);
        assert!(recorded_state(&changed_checkout.effects, bloom).expect("new head").result.is_none());
        assert!(changed_checkout.effects.iter().any(|effect| matches!(effect, Decision::OfferPrecheck { .. })));

        let mut changed_contract = snapshot;
        let record = changed_contract.blooms.get_mut(&bloom).expect("bloom");
        record.coordination.as_deref_mut().expect("coordination").composition_contract.invocation.image =
            String::from("different-image");
        let prepared = record.precheck.as_ref().expect("precheck").prepared.as_ref().expect("prepared").digest();
        let requested = reduce_request_precheck(&changed_contract, &bloom, prepared);
        assert!(requested.effects.iter().any(|effect| matches!(effect, Decision::DispatchPrecheck { .. })));
    }

    #[test]
    fn contextual_receipt_does_not_override_an_established_red_head() {
        let (mut snapshot, bloom, node, _) = contextual_receipt_fixture();
        let record = snapshot.blooms.get_mut(&bloom).expect("bloom");
        let coordination = record.coordination.as_deref_mut().expect("coordination");
        coordination.integration.known_red = Some(coordination.integration.head.node);
        assert_eq!(
            contextual_precheck_receipt(
                &snapshot.blooms[&bloom],
                bloom,
                snapshot.blooms[&bloom].coordination.as_deref().expect("coordination"),
                &node
            ),
            None,
        );
    }

    #[test]
    fn eager_precheck_uses_selected_root_and_actual_coverage_without_a_leaf_fold() {
        let (mut snapshot, bloom, _) = eager_fixture();
        snapshot.blooms.get_mut(&bloom).expect("bloom").precheck =
            Some(PrecheckState::new(PrecheckPolicy { run_budget: 3 }));
        let decisions = advance_head(&snapshot, bloom, 50, 51);
        let state = recorded_state(&decisions.effects, bloom).expect("scheduled head");
        let node = state.prepared.as_ref().expect("head already prepared");
        assert_eq!((node.tree, node.head), (digest(50), digest(51)));
        assert_eq!(
            state
                .latest_plan
                .as_ref()
                .expect("plan")
                .members
                .iter()
                .map(|member| &member.workpiece)
                .collect::<Vec<_>>(),
            vec![&workpiece("two"), &workpiece("one")],
        );
        assert!(
            decisions
                .effects
                .iter()
                .any(|effect| matches!(effect, Decision::OfferPrecheck { node: offered, .. } if offered == node))
        );
        assert!(
            !decisions
                .effects
                .iter()
                .any(|effect| matches!(effect, Decision::QueuePrecheckPlan { .. } | Decision::DispatchPrecheck { .. }))
        );
    }

    #[test]
    fn deferred_contextual_precheck_waits_without_rebuilding_its_repaired_leaves() {
        let (mut snapshot, bloom, expected) = eager_fixture();
        let record = snapshot.blooms.get_mut(&bloom).expect("bloom");
        record.coordination.as_deref_mut().expect("coordination").policy.eager_integration = false;
        record.precheck = Some(PrecheckState::new(PrecheckPolicy { run_budget: 3 }));
        assert!(plan_of(record, bloom).is_none());
        let decisions = advance_head(&snapshot, bloom, 40, 41);
        assert!(!decisions.effects.iter().any(|effect| matches!(
            effect,
            Decision::OfferPrecheck { .. } | Decision::DispatchPrecheck { .. } | Decision::QueuePrecheckPlan { .. }
        )));

        let record = snapshot.blooms.get_mut(&bloom).expect("bloom");
        record.coordination.as_deref_mut().expect("coordination").final_in_flight = true;
        let plan = plan_of(record, bloom).expect("final contextual fold uses selected root");
        let selected = selected_head_node(&plan, record.coordination.as_deref().expect("coordination"));
        assert_eq!(selected, expected);
        assert_eq!(plan.base, digest(41));
    }

    #[test]
    fn eager_head_changes_coalesce_while_an_issued_input_stays_immutable() {
        let (mut snapshot, bloom, issued) = eager_fixture();
        for (tree, checkout) in [(50, 51), (60, 61)] {
            let decisions = advance_head(&snapshot, bloom, tree, checkout);
            let state = recorded_state(&decisions.effects, bloom).expect("newest selected head").clone();
            assert_eq!(state.issued.as_ref(), Some(&issued));
            assert_eq!(state.prepared.as_ref().expect("pending head").tree, digest(tree));
            assert!(!decisions.effects.iter().any(|effect| matches!(
                effect,
                Decision::OfferPrecheck { .. } | Decision::DispatchPrecheck { .. } | Decision::QueuePrecheckPlan { .. }
            )));
            let record = snapshot.blooms.get_mut(&bloom).expect("bloom");
            record.precheck = Some(state);
            record.coordination.as_deref_mut().expect("coordination").integration.head.candidate =
                CandidateRef { tree: digest(tree), checkout: digest(checkout) };
            record.coordination.as_deref_mut().expect("coordination").integration.head.node = digest(checkout);
        }
        let completion = reduce_precheck_completed(
            &snapshot,
            &bloom,
            issued.digest(),
            &PrecheckCompletion::Failed(Evidence {
                subject: issued.tree,
                kind: EvidenceKind::VerificationResult,
                detail: digest(90),
            }),
        );
        let state = recorded_state(&completion.effects, bloom).expect("settled old run");
        assert!(state.issued.is_none());
        assert!(state.result.is_none(), "a stale red cannot color the selected head");
        assert!(
            completion
                .effects
                .iter()
                .any(|effect| matches!(effect, Decision::OfferPrecheck { node, .. } if node.tree == digest(60)))
        );
        assert!(!completion.effects.iter().any(|effect| matches!(effect, Decision::RecordCoordinationState { .. })));
    }

    #[test]
    fn same_coverage_repaired_root_gets_a_fresh_precheck_node() {
        let (mut snapshot, bloom, prior) = eager_fixture();
        let state = snapshot.blooms.get_mut(&bloom).expect("bloom").precheck.as_mut().expect("precheck");
        state.issued = None;
        state.result = Some(PrecheckResult::Passed { node: prior.digest(), evidence: digest(90) });
        let decisions = advance_head(&snapshot, bloom, 50, 51);
        let state = recorded_state(&decisions.effects, bloom).expect("new node");
        assert_ne!(state.latest_plan.as_ref().expect("same coverage").digest(), prior.plan);
        assert_eq!(state.latest_plan.as_ref().expect("current root plan").base, digest(51));
        assert_ne!(state.prepared.as_ref().expect("new node").digest(), prior.digest());
        assert!(state.result.is_none());
        assert!(
            decisions
                .effects
                .iter()
                .any(|effect| matches!(effect, Decision::OfferPrecheck { node, .. } if node.tree == digest(50)))
        );
    }

    #[test]
    fn exact_eager_red_marks_the_current_inherited_head() {
        let (snapshot, bloom, issued) = eager_fixture();
        let completion = reduce_precheck_completed(
            &snapshot,
            &bloom,
            issued.digest(),
            &PrecheckCompletion::Failed(Evidence {
                subject: issued.tree,
                kind: EvidenceKind::VerificationResult,
                detail: digest(90),
            }),
        );
        let state = completion
            .effects
            .iter()
            .find_map(|effect| match effect {
                Decision::RecordCoordinationState { state: Some(state), .. } => Some(state),
                _ => None,
            })
            .expect("red inherited head recorded");
        assert_eq!(state.integration.known_red, Some(state.integration.head.node));
        assert_eq!(state.partial_head_repair.as_ref().expect("repair scheduled").head, state.integration.head);
        assert!(completion.effects.iter().any(|effect| matches!(effect, Decision::DispatchPartialHeadRepair { .. })));
    }

    #[test]
    fn late_green_does_not_clear_an_independently_established_red_head() {
        let (mut snapshot, bloom, issued) = eager_fixture();
        let coordination =
            snapshot.blooms.get_mut(&bloom).expect("bloom").coordination.as_deref_mut().expect("coordination");
        coordination.integration.known_red = Some(coordination.integration.head.node);
        let completion = reduce_precheck_completed(
            &snapshot,
            &bloom,
            issued.digest(),
            &PrecheckCompletion::Passed(Evidence {
                subject: issued.tree,
                kind: EvidenceKind::VerificationResult,
                detail: digest(90),
            }),
        );
        assert!(matches!(completion.outcome, Outcome::PrecheckCompleted { .. }));
        assert!(!completion.effects.iter().any(|effect| matches!(effect, Decision::RecordCoordinationState { .. })));
        assert_eq!(
            snapshot.blooms[&bloom].coordination.as_deref().expect("coordination").integration.known_red,
            Some(digest(42)),
        );
    }

    #[test]
    fn a_repaired_red_head_resumes_on_its_own_aggregate_proof() {
        let (mut snapshot, bloom, issued) = eager_fixture();
        let record = snapshot.blooms.get_mut(&bloom).expect("bloom");
        let scope_revision = record.spec.members()[0].scope_revision;
        let replacement = crate::MemberPin {
            workpiece: record.spec.members()[0].workpiece.clone(),
            scope_revision,
            candidate: CandidateRef { tree: digest(60), checkout: digest(61) },
        };
        let coordination = record.coordination.as_deref_mut().expect("coordination");
        // Standalone verification mints no contextual receipt, so this pre-check
        // is the only proof the repaired head can ever get.
        coordination.policy.verification = crate::VerificationMode::Standalone;
        coordination.integration.known_red = Some(coordination.integration.head.node);
        coordination.integration.unproved_repair = Some(coordination.integration.head.node);
        coordination.integration.queued = vec![crate::CompositionInput {
            node: replacement.candidate.tree,
            candidate: replacement.candidate,
            members: vec![replacement],
        }];

        let completion = reduce_precheck_completed(
            &snapshot,
            &bloom,
            issued.digest(),
            &PrecheckCompletion::Passed(Evidence {
                subject: issued.tree,
                kind: EvidenceKind::VerificationResult,
                detail: digest(90),
            }),
        );

        let promoted = completion
            .effects
            .iter()
            .find_map(|effect| match effect {
                Decision::RecordCoordinationState { state: Some(state), .. } => Some(state),
                _ => None,
            })
            .expect("the repaired head's own proof promotes it");
        assert_eq!(promoted.integration.known_red, None);
        assert_eq!(promoted.integration.unproved_repair, None);
        assert!(completion.effects.iter().any(|effect| matches!(effect, Decision::DispatchIntegrationAppend { .. })));
    }

    #[test]
    fn eager_preparation_cannot_substitute_a_rebuilt_leaf_tree() {
        let (snapshot, bloom, mut node) = eager_fixture();
        node.tree = digest(99);
        let decisions = reduce_precheck_prepared(&snapshot, &bloom, node.plan, &PrecheckPreparation::Prepared(node));
        assert_eq!(decisions.outcome, Outcome::PrecheckRejected(PrecheckError::InvalidPreparation));
        assert!(decisions.effects.is_empty());
    }

    #[test]
    fn eager_final_join_requires_full_coverage_and_the_exact_selected_tree() {
        let (mut snapshot, bloom, node) = eager_fixture();
        let record = snapshot.blooms.get_mut(&bloom).expect("bloom");
        assert_eq!(final_join(record, bloom, node.tree, node.head), Some(node.clone()));
        assert!(final_join(record, bloom, digest(99), node.head).is_none());
        record.coordination.as_deref_mut().expect("coordination").integration.head.coverage.pop();
        assert!(final_join(record, bloom, node.tree, node.head).is_none());
    }

    fn recorded_state(effects: &[Decision], bloom: BloomId) -> Option<&PrecheckState> {
        effects.iter().rev().find_map(|effect| match effect {
            Decision::RecordPrecheckState { bloom: owner, state } if *owner == bloom => state.as_ref(),
            _ => None,
        })
    }

    fn capture_precheck_plan(
        mut snapshot: Snapshot,
        bloom: BloomId,
        configs: &ResolvedConfigs,
        candidates: &[(&str, u8, u8, CandidateRef)],
    ) -> (Snapshot, PrecheckPlan) {
        let mut queued = None;
        for (name, _, _, candidate) in candidates.iter().copied() {
            let completed = event(
                &format!("precheck-lifecycle-construct-{name}"),
                Fact::AttemptCompleted {
                    bloom,
                    workpiece: workpiece(name),
                    stage: StageId::Construct,
                    passed: true,
                    evidence: Evidence {
                        subject: candidate.tree,
                        kind: EvidenceKind::VerificationResult,
                        detail: digest(90),
                    },
                    candidate: Some(candidate),
                },
            );
            let (next, decisions) = step_with_configs(&snapshot, &completed, configs);
            queued = decisions.effects.iter().find_map(|effect| match effect {
                Decision::QueuePrecheckPlan { plan, .. } => Some(plan.clone()),
                _ => None,
            });
            snapshot = next;
        }
        (snapshot, queued.expect("the second immutable candidate queues a plan"))
    }

    fn seal_enabled_bloom() -> (Snapshot, BloomId, ResolvedConfigs) {
        let policy = PrecheckPolicy { run_budget: 1 };
        let policy_bytes = to_vec(&policy).expect("policy encodes");
        let policy_address = config_address(PrecheckPolicy::NAME, &policy_bytes);
        let mut authored = draft(1, vec![membership("one", 11), membership("two", 12)]);
        authored.configs.insert::<PrecheckPolicy>(policy_address);
        let spec = authored.seal();
        let bloom = spec.id();
        let mut configs = compiled_resolved();
        configs.insert(policy_address, PrecheckPolicy::NAME, policy_bytes, None);
        let snapshot = Snapshot::new(digest(1)).with_green_base(digest(1));
        let seal = event("precheck-lifecycle-seal", Fact::Seal(spec));

        let (snapshot, _) = step_with_configs(&snapshot, &seal, &configs);
        (snapshot, bloom, configs)
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
    fn obsolete_unstarted_last_run_prepares_the_latest_plan_exactly_once() {
        let (mut snapshot, bloom, old_node) = issued_fixture();
        let record = snapshot.blooms.get_mut(&bloom).expect("fixture record");
        let state = record.precheck.as_mut().expect("fixture pre-check state");
        state.policy.run_budget = 1;
        let mut latest = state.latest_plan.clone().expect("fixture plan");
        latest.members[1].candidate = CandidateRef { tree: digest(80), checkout: digest(81) };
        state.latest_plan = Some(latest.clone());
        state.prepared = None;

        let decisions =
            reduce_precheck_completed(&snapshot, &bloom, old_node.digest(), &PrecheckCompletion::SkippedBeforeStart);

        assert_eq!(
            decisions
                .effects
                .iter()
                .filter(|effect| matches!(effect, Decision::QueuePrecheckPlan { bloom: owner, plan } if *owner == bloom && plan == &latest))
                .count(),
            1,
        );
        assert!(!decisions.effects.iter().any(|effect| matches!(effect, Decision::OfferPrecheck { .. })));
        assert_eq!(recorded_state(&decisions.effects, bloom).expect("refunded state").issued_runs, 0);

        let record = snapshot.blooms.get_mut(&bloom).expect("fixture record");
        let state = record.precheck.as_mut().expect("fixture pre-check state");
        let latest_node =
            PrecheckNode { plan: latest.digest(), tree: digest(82), head: digest(83), gate_set: latest.gate_set };
        state.prepared = Some(latest_node.clone());
        let prepared =
            reduce_precheck_completed(&snapshot, &bloom, old_node.digest(), &PrecheckCompletion::SkippedBeforeStart);
        assert!(!prepared.effects.iter().any(|effect| matches!(effect, Decision::QueuePrecheckPlan { .. })));
        assert!(
            prepared
                .effects
                .iter()
                .any(|effect| matches!(effect, Decision::OfferPrecheck { node, .. } if node == &latest_node))
        );
    }

    #[test]
    fn enabled_policy_runs_public_lifecycle_and_joins_the_exact_issued_fold() {
        let (mut snapshot, bloom, configs) = seal_enabled_bloom();
        let candidates = [
            ("one", 11, 21, CandidateRef { tree: digest(21), checkout: digest(31) }),
            ("two", 12, 22, CandidateRef { tree: digest(22), checkout: digest(32) }),
        ];
        let (next, plan) = capture_precheck_plan(snapshot, bloom, &configs, &candidates);
        snapshot = next;
        let node = PrecheckNode { plan: plan.digest(), tree: digest(40), head: digest(41), gate_set: plan.gate_set };

        let prepared = event(
            "precheck-lifecycle-prepared",
            Fact::PrecheckPrepared {
                bloom,
                plan: plan.digest(),
                preparation: PrecheckPreparation::Prepared(node.clone()),
            },
        );
        let (next, offered) = step_with_configs(&snapshot, &prepared, &configs);
        assert!(offered.effects.iter().any(|effect| matches!(effect, Decision::OfferPrecheck { .. })));
        snapshot = next;

        let request = event("precheck-lifecycle-request", Fact::RequestPrecheck { bloom, node: node.digest() });
        let (next, dispatched) = step_with_configs(&snapshot, &request, &configs);
        assert!(
            dispatched
                .effects
                .iter()
                .any(|effect| matches!(effect, Decision::DispatchPrecheck { node: issued, .. } if issued == &node))
        );
        snapshot = next;

        let mut fold_dispatched = false;
        for (name, revision, tree, _) in candidates {
            let integrated = event(
                &format!("precheck-lifecycle-integrate-{name}"),
                Fact::Integrate { bloom, claim: claim(name, revision, tree) },
            );
            let (next, decisions) = step_with_configs(&snapshot, &integrated, &configs);
            fold_dispatched |=
                decisions.effects.iter().any(|effect| matches!(effect, Decision::DispatchIntegration { .. }));
            snapshot = next;
        }
        assert!(fold_dispatched, "standalone member claims trigger the ordinary fold");
        assert_eq!(snapshot.blooms[&bloom].claims.len(), 2);

        let ordinary_head = digest(42);
        let resolved = event(
            "precheck-lifecycle-resolve",
            Fact::Resolve { bloom, tree: node.tree, head: ordinary_head, lineage: vec![digest(43)] },
        );
        let (next, joined) = step_with_configs(&snapshot, &resolved, &configs);
        assert!(matches!(joined.outcome, Outcome::PrecheckJoined { node: joined, .. } if joined == node.digest()));
        assert!(joined.effects.iter().any(|effect| matches!(effect, Decision::DispatchAggregateReview { .. })));
        assert!(!joined.effects.iter().any(|effect| matches!(effect, Decision::DispatchAggregateVerify { .. })));
        assert_eq!(next.blooms[&bloom].integration.as_ref().expect("ordinary fold retained").head, ordinary_head);
        snapshot = next;

        let critic = event(
            "precheck-lifecycle-critic",
            Fact::AggregateReviewCompleted {
                bloom,
                passed: true,
                evidence: Evidence { subject: node.tree, kind: EvidenceKind::VerificationResult, detail: digest(44) },
                implicated: vec![],
            },
        );
        (snapshot, _) = step_with_configs(&snapshot, &critic, &configs);

        let completion = event(
            "precheck-lifecycle-completed",
            Fact::PrecheckCompleted {
                bloom,
                node: node.digest(),
                completion: PrecheckCompletion::Passed(Evidence {
                    subject: node.tree,
                    kind: EvidenceKind::VerificationResult,
                    detail: digest(45),
                }),
            },
        );
        let (snapshot, completed) = step_with_configs(&snapshot, &completion, &configs);
        assert!(completed.effects.iter().all(|effect| {
            !matches!(effect, Decision::RecordVerifyProof { proof, .. } if proof.stage == StageId::Verify)
        }));
        assert!(completed.effects.iter().any(|effect| {
            matches!(effect, Decision::RecordVerifyProof { proof, .. } if proof.stage == StageId::AggregateVerify)
        }));
        let record = &snapshot.blooms[&bloom];
        assert_eq!(record.resolved_head, Some(ordinary_head));
        assert_eq!(record.resolved_tree, Some(node.tree));
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
