//! Pure coordination transitions for eager heads and shared verification.

use alloc::collections::btree_map::Entry;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::cmp::min;
use core::mem::take;

use super::aggregate_verify::at_park_ceiling;
use super::attempt::{DispatchTargets, SealedLine, move_effects_with_candidate, stage_binding};
use super::boundary::EventBoundary;
use super::gate::AGGREGATE_VERIFY_GATE;
use super::{BloomRecord, BloomStatus, CoordinationError, Decision, Decisions, Outcome, Snapshot, StageProgress};
use crate::digest::{ContentAddressed, Digest, digest_of};
use crate::ids::{BloomId, StageId, WorkpieceId};
use crate::reads;
use crate::values::{
    BloomSpec, CandidatePreparation, CandidatePreparationPlan, CandidateRef, CompatibilityPreview,
    CompatibilityPreviewPlan, CompatibilityPreviewRecord, CompositionContractTemplate, CompositionInput,
    CompositionPlan, ConstructContext, ConstructionAdmission, ConstructionCheckpoint, ContextualAttemptDispatch,
    ContextualInvocationTemplate, ContextualResolutionClaim, CoordinationDiagnostic, CoordinationPolicy,
    CoordinationState, Evidence, EvidenceKind, FailureScope, GenerationMember, IntegrationAppendPlan, IntegrationHead,
    MemberCandidate, MemberContractPin, MemberPin, MemberVerifyLatency, MemberVerifyOutcome, MemberVerifyRequest,
    OperatorHold, PartialHeadRepairCompletion, PartialHeadRepairDispatch, PartialHeadRepairPlan, PipelineManifest,
    PreparedCandidate, ResolutionClaim, ResolutionProof, SharedRunCompletion, SharedRunDispatch, SharedRunExecution,
    SharedRunMode, SharedRunNode, SharedRunPhase, SharedRunPlan, SharedRunPreparation, SharedRunRecord,
    StableHeadReservation, StageCatalog, SurvivorGroup, Transformation, VERIFY_CHECK_COMMAND, VERIFY_MEMBER_COMMAND,
    VerificationContract, VerificationMode, VerificationObligation, VerifyFailureSet, VerifyGateSet, Wedge,
    host_class_digest, verification_environment_digest,
};

#[derive(serde::Serialize)]
struct InvocationIdentity<'a> {
    transformation: &'a Transformation,
    profile: &'a crate::AgentProfile,
    configs: &'a crate::ConfigRegistry,
}

impl ContentAddressed for InvocationIdentity<'_> {
    const DOMAIN: &'static str = "aether.bloomery.member_invocation.v1";
}

fn workpiece_key(workpiece: &WorkpieceId) -> &str {
    workpiece.0.as_str()
}

fn rejected(error: CoordinationError) -> Decisions {
    Decisions::rejected(Outcome::CoordinationRejected(error))
}

fn accepted(bloom: BloomId, subject: Digest, effects: Vec<Decision>) -> Decisions {
    Decisions { outcome: Outcome::CoordinationAdvanced { bloom, subject }, effects }
}

fn record_state(bloom: BloomId, state: CoordinationState) -> Decision {
    Decision::RecordCoordinationState { bloom, state: Some(state) }
}

fn hold_coordination(bloom: BloomId, reason: String, effects: &mut Vec<Decision>) {
    if effects
        .iter()
        .any(|effect| matches!(effect, Decision::RecordOperatorHold { bloom: owner, .. } if *owner == bloom))
    {
        return;
    }
    effects.push(Decision::RecordOperatorHold {
        bloom,
        hold: OperatorHold { reason, operator: String::from("aether-bloomery") },
    });
}

fn active_state<'a>(
    snapshot: &'a Snapshot,
    bloom: &BloomId,
) -> Result<(&'a BloomRecord, &'a CoordinationState), CoordinationError> {
    let record = snapshot.blooms.get(bloom).ok_or(CoordinationError::UnknownOrInactiveBloom)?;
    if record.status != BloomStatus::Sealed {
        return Err(CoordinationError::UnknownOrInactiveBloom);
    }
    record.coordination.as_deref().map(|state| (record, state)).ok_or(CoordinationError::Disabled)
}

pub(super) fn initialized_effects(
    snapshot: &Snapshot,
    spec: &BloomSpec,
    catalog: &StageCatalog,
    manifest: &PipelineManifest,
    policy: Option<CoordinationPolicy>,
) -> Result<Vec<Decision>, CoordinationError> {
    let Some(policy) = policy else {
        return Ok(Vec::new());
    };
    if !policy.is_valid() {
        return Err(CoordinationError::InvalidPolicy);
    }
    let bloom = spec.id();
    let base = CandidateRef {
        tree: snapshot.base_trees.get(&spec.base()).copied().unwrap_or_else(|| spec.base()),
        checkout: spec.base(),
    };
    let members = spec
        .members()
        .iter()
        .map(|member| GenerationMember { workpiece: member.workpiece.clone(), scope_revision: member.scope_revision })
        .collect();
    let binding = stage_binding(catalog, StageId::AggregateVerify);
    let transformation = Transformation::for_aggregate_verify(&binding, base.tree, base.checkout, base.checkout);
    let host_class = host_class_digest(&policy.host_class);
    let environment = verification_environment_digest(&transformation.image, transformation.network, host_class);
    let composition_contract = CompositionContractTemplate {
        gate_set: VerifyGateSet::fold_of(manifest).digest(),
        gate_identities: gates_for_manifest(manifest, VERIFY_CHECK_COMMAND),
        invocation: ContextualInvocationTemplate {
            command: transformation.command,
            extra_inputs: transformation.inputs.into_iter().skip(1).collect(),
            diff_base: transformation.diff_base,
            outputs: transformation.outputs,
            image: transformation.image.clone(),
            limits: transformation.limits,
            network: transformation.network,
            description: transformation.description,
            model: transformation.model,
            profile: binding.profile,
            configs: spec.configs().clone(),
        },
        environment,
        host_class,
    };
    Ok(alloc::vec![record_state(bloom, CoordinationState::new(policy, composition_contract, bloom, base, members),)])
}

fn current_member(record: &BloomRecord, pin: &MemberPin) -> bool {
    record
        .spec
        .members()
        .iter()
        .any(|member| member.workpiece == pin.workpiece && member.scope_revision == pin.scope_revision)
        && !record.withdrawn.contains_key(&pin.workpiece)
}

fn exact_request(record: &BloomRecord, state: &CoordinationState, request: &MemberVerifyRequest) -> bool {
    if request.bloom != state.integration.generation.bloom || !current_member(record, &request.member) {
        return false;
    }
    let Some((scope_revision, candidate, diff_base)) = request.contract.member_delta() else {
        return false;
    };
    *scope_revision == request.member.scope_revision
        && *candidate == request.member.candidate
        && *diff_base == request.contract.diff_base
        && request.input.members.contains(&request.member)
        && request.contract.host_class == host_class_digest(&state.policy.host_class)
        && request.contract.gate_set == VerifyGateSet::member_of(&record.pipeline_manifest).digest()
        && request.contract.invocation
            == digest_of(&InvocationIdentity {
                transformation: &request.transformation,
                profile: &request.profile,
                configs: &request.configs,
            })
        && record.progress.get(&request.member.workpiece).is_some_and(|progress| {
            progress.stage == StageId::Verify
                && progress.candidate == Some(request.member.candidate)
                && progress.repair_rolls == request.attempt
        })
        && request.contract.environment
            == verification_environment_digest(
                &request.transformation.image,
                request.transformation.network,
                request.contract.host_class,
            )
}

fn hold_invalidated_inherited_construction(
    record: &BloomRecord,
    state: &CoordinationState,
    affected: &BTreeSet<WorkpieceId>,
    effects: &mut Vec<Decision>,
) {
    if record.operator_hold.is_some() {
        return;
    }
    let inherited = state.admitted_construction.values().find_map(|admission| {
        if affected.contains(&admission.dispatch.workpiece) {
            return None;
        }
        admission
            .dispatch
            .context
            .starting_head
            .coverage
            .iter()
            .find(|pin| affected.contains(&pin.workpiece))
            .map(|invalidated| (admission, invalidated))
    });
    let Some((admission, invalidated)) = inherited else {
        return;
    };
    hold_coordination(
        record.spec.id(),
        format!(
            "construction for {} remains pinned to an inherited head containing invalidated {}; rescope or supersede before release",
            admission.dispatch.workpiece.0, invalidated.workpiece.0
        ),
        effects,
    );
}

fn gates_for(record: &BloomRecord, command: &str) -> Vec<String> {
    gates_for_manifest(&record.pipeline_manifest, command)
}

fn gates_for_manifest(manifest: &PipelineManifest, command: &str) -> Vec<String> {
    manifest.verifiers.runs.get(command).cloned().unwrap_or_default()
}

/// The blast radius of `changed` — every member that reaches it through a
/// dependency edge or through a coverage set it shares, taken to a fixpoint.
fn affected_closure(record: &BloomRecord, state: &CoordinationState, changed: &WorkpieceId) -> BTreeSet<WorkpieceId> {
    let mut affected = BTreeSet::from([changed.clone()]);
    loop {
        let before = affected.len();
        for edge in &record.dependencies {
            if affected.contains(&edge.depends_on) {
                affected.insert(edge.member.clone());
            }
        }

        // A live request's input carries the context head's whole coverage, so
        // an ejected peer reaches the fixpoint through it as surely as through
        // an admitted or queued contribution.
        for input in state
            .integration
            .admitted
            .iter()
            .chain(&state.integration.queued)
            .chain(state.requests.iter().map(|request| &request.input))
        {
            if input.members.iter().any(|pin| affected.contains(&pin.workpiece)) {
                affected.extend(input.members.iter().map(|pin| pin.workpiece.clone()));
            }
        }

        if affected.len() == before {
            return affected;
        }
    }
}

fn invalidate_member_version(
    record: &BloomRecord,
    state: &mut CoordinationState,
    changed: &WorkpieceId,
    effects: &mut Vec<Decision>,
) -> Result<(), CoordinationError> {
    let next_epoch = state.integration.generation.epoch.checked_add(1).ok_or(CoordinationError::InvalidPlan)?;
    let affected = affected_closure(record, state, changed);
    let survives = |workpiece: &String| !affected.iter().any(|member| workpiece == workpiece_key(member));

    hold_invalidated_inherited_construction(record, state, &affected, effects);
    for run in state.runs.iter_mut().filter(|run| {
        !run.is_terminal() && run.plan.requests.iter().any(|request| affected.contains(&request.member.workpiece))
    }) {
        run.stale = true;
        effects.push(Decision::CancelSharedRun { plan: run.plan.digest() });
    }

    let invalidated_requests = state
        .requests
        .iter()
        .filter(|request| affected.contains(&request.member.workpiece))
        .map(MemberVerifyRequest::digest)
        .collect::<BTreeSet<_>>();
    state.requests.retain(|request| !affected.contains(&request.member.workpiece));
    for group in &mut state.survivor_groups {
        group.requests.retain(|request| !invalidated_requests.contains(request));
    }
    state.survivor_groups.retain(|group| !group.requests.is_empty());

    let valid_contextual_nodes = state
        .runs
        .iter()
        .filter_map(|run| run.node.as_ref())
        .filter(|node| node.coverage.iter().all(|pin| !affected.contains(&pin.workpiece)))
        .map(SharedRunNode::digest)
        .collect::<BTreeSet<_>>();
    state.claims.retain(|workpiece, claim| {
        survives(workpiece)
            && match &claim.proof {
                ResolutionProof::Standalone(_) => true,
                ResolutionProof::InComposition { node, .. } => valid_contextual_nodes.contains(node),
            }
    });

    state.prepared.retain(|workpiece, _| survives(workpiece));
    state.preparations.retain(|plan| !affected.contains(&plan.workpiece));
    state.contexts.retain(|workpiece, _| survives(workpiece));
    state.checkpoints.retain(|workpiece, _| survives(workpiece));
    state.queued_construction.retain(|workpiece, _| survives(workpiece));
    state.admitted_construction.retain(|workpiece, _| survives(workpiece));
    state.preview_plans.clear();
    state.previews.clear();
    state.partial_head_repair = None;

    let retained = state
        .integration
        .admitted
        .iter()
        .chain(&state.integration.queued)
        .filter(|input| input.members.iter().all(|pin| !affected.contains(&pin.workpiece)))
        .cloned()
        .collect::<Vec<_>>();

    state.integration.generation.epoch = next_epoch;
    let generation = state.integration.generation.digest();
    state.integration.head = IntegrationHead {
        generation,
        node: generation,
        candidate: state.integration.generation.base,
        plan: Digest::default(),
        coverage: Vec::new(),
    };
    state.integration.queued.clear();
    for input in retained {
        if !state.integration.queued.iter().any(|current| current.digest() == input.digest()) {
            state.integration.queued.push(input);
        }
    }
    state.integration.admitted.clear();
    state.integration.in_flight = None;
    state.integration.known_red = None;
    state.integration.unproved_repair = None;
    state.integration.reservation = None;
    state.integration.movement_count = 0;
    state.final_in_flight = false;
    state.final_dispatched = false;
    Ok(())
}

#[derive(Clone, Copy)]
struct VerifyDispatch<'a> {
    workpiece: &'a WorkpieceId,
    transformation: &'a Transformation,
    profile: &'a crate::AgentProfile,
    configs: &'a crate::ConfigRegistry,
    progress: &'a StageProgress,
}

fn request_for_dispatch(
    snapshot: &Snapshot,
    record: &BloomRecord,
    state: &CoordinationState,
    dispatch: VerifyDispatch<'_>,
) -> Option<MemberVerifyRequest> {
    let VerifyDispatch { workpiece, transformation, profile, configs, progress } = dispatch;
    let member = record.spec.members().iter().find(|member| member.workpiece == *workpiece)?;
    let candidate = progress.candidate?;
    let context = state.contexts.get(workpiece_key(workpiece)).cloned();
    let base = context.as_ref().map_or_else(
        || CandidateRef {
            tree: snapshot.base_trees.get(&record.spec.base()).copied().unwrap_or_else(|| record.spec.base()),
            checkout: record.spec.base(),
        },
        |context| context.starting_head.candidate,
    );
    let host_class = host_class_digest(&state.policy.host_class);
    let mut obligations = gates_for(record, VERIFY_MEMBER_COMMAND)
        .into_iter()
        .map(|identity| VerificationObligation::Gate { identity })
        .collect::<Vec<_>>();
    obligations.push(VerificationObligation::MemberDelta {
        scope_revision: member.scope_revision,
        candidate,
        diff_base: base,
    });
    let contract = VerificationContract {
        gate_set: VerifyGateSet::member_of(&record.pipeline_manifest).digest(),
        obligations,
        diff_base: base,
        invocation: digest_of(&InvocationIdentity { transformation, profile, configs }),
        environment: verification_environment_digest(&transformation.image, transformation.network, host_class),
        host_class,
    };
    let mut input_members = context.as_ref().map_or_else(Vec::new, |context| context.starting_head.coverage.clone());
    let member_pin = MemberPin { workpiece: workpiece.clone(), scope_revision: member.scope_revision, candidate };
    if !input_members.contains(&member_pin) {
        input_members.push(member_pin.clone());
    }
    Some(MemberVerifyRequest {
        bloom: state.integration.generation.bloom,
        member: member_pin,
        input: CompositionInput { node: candidate.tree, candidate, members: input_members },
        attempt: progress.repair_rolls,
        context,
        contract,
        transformation: transformation.clone(),
        profile: profile.clone(),
        configs: configs.clone(),
    })
}

fn append_ready(record: &BloomRecord, state: &CoordinationState, input: &CompositionInput) -> bool {
    input.members.iter().any(|pin| !state.integration.head.coverage.contains(pin))
        && input.members.iter().all(|pin| {
            current_member(record, pin)
                && record.dependencies.iter().filter(|edge| edge.member == pin.workpiece).all(|edge| {
                    input.members.iter().any(|member| member.workpiece == edge.depends_on)
                        || state.integration.head.coverage.iter().any(|member| member.workpiece == edge.depends_on)
                })
        })
}

fn unlocks_ready_dependent(record: &BloomRecord, state: &CoordinationState, input: &CompositionInput) -> bool {
    record.spec.members().iter().any(|member| {
        if record.progress.contains_key(&member.workpiece)
            || record.claims.contains_key(&member.workpiece)
            || state.claims.contains_key(workpiece_key(&member.workpiece))
            || record.withdrawn.contains_key(&member.workpiece)
        {
            return false;
        }
        let dependencies =
            record.dependencies.iter().filter(|edge| edge.member == member.workpiece).collect::<Vec<_>>();
        !dependencies.is_empty()
            && dependencies.iter().any(|edge| {
                input.members.iter().any(|pin| pin.workpiece == edge.depends_on)
                    && !state.integration.head.coverage.iter().any(|pin| pin.workpiece == edge.depends_on)
            })
            && dependencies.iter().all(|edge| {
                state.integration.head.coverage.iter().chain(&input.members).any(|pin| pin.workpiece == edge.depends_on)
            })
    })
}

fn schedule_append(record: &BloomRecord, state: &mut CoordinationState, effects: &mut Vec<Decision>) {
    let dependency_only = !state.policy.eager_integration && !state.final_in_flight;
    if (dependency_only && state.policy.verification != VerificationMode::Contextual)
        || state.integration.in_flight.is_some()
        || state.integration.known_red == Some(state.integration.head.node)
        || record.operator_hold.is_some()
    {
        return;
    }
    let Some(input) = state
        .integration
        .queued
        .iter()
        .find(|input| {
            append_ready(record, state, input)
                && (!dependency_only || unlocks_ready_dependent(record, state, input))
                && state.integration.reservation.as_ref().is_none_or(|reservation| {
                    reservation.generation == state.integration.generation.digest()
                        && reservation.node == state.integration.head.node
                        && input.members.iter().any(|member| member.workpiece == reservation.owner)
                })
        })
        .cloned()
    else {
        return;
    };
    let plan = IntegrationAppendPlan {
        bloom: state.integration.generation.bloom,
        generation: state.integration.generation.digest(),
        expected_parent: state.integration.head.clone(),
        inputs: alloc::vec![input],
    };
    state.integration.in_flight = Some(plan.clone());
    effects.push(Decision::DispatchIntegrationAppend { plan });
}

fn queue_context_dispatch(
    record: &BloomRecord,
    state: &mut CoordinationState,
    bloom: BloomId,
    workpiece: &WorkpieceId,
    progress: &StageProgress,
    context: ConstructContext,
    effects: &mut Vec<Decision>,
) {
    let member = record.spec.members().iter().find(|member| member.workpiece == *workpiece).expect("sealed member");
    let binding = stage_binding(&record.stage_catalog, progress.stage);
    let subject = progress.candidate.map_or(member.scope_revision, |candidate| candidate.tree);
    let transformation = Transformation::for_member_stage(
        &binding,
        subject,
        progress.candidate.map_or(context.starting_head.candidate.checkout, |candidate| candidate.checkout),
        context.starting_head.candidate.checkout,
    );
    let dispatch = ContextualAttemptDispatch {
        bloom,
        workpiece: workpiece.clone(),
        stage: progress.stage,
        attempt: progress.attempts,
        transformation,
        scope_revision: member.scope_revision,
        candidate: progress.candidate.map(|candidate| candidate.tree),
        profile: binding.profile,
        configs: member.configs.layered_over(record.spec.configs()),
        context,
    };
    effects.push(Decision::AdvanceStage { bloom, workpiece: workpiece.clone(), progress: *progress });
    if progress.stage == StageId::Construct {
        state.queued_construction.insert(workpiece.0.clone(), dispatch.clone());
        effects.push(Decision::QueueConstructionAdmission { dispatch });
    } else {
        effects.push(Decision::DispatchContextualAttempt { dispatch });
    }
}

fn release_ready_dependents(record: &BloomRecord, state: &mut CoordinationState, effects: &mut Vec<Decision>) {
    if state.integration.known_red == Some(state.integration.head.node) {
        return;
    }
    let bloom = state.integration.generation.bloom;
    for member in record.spec.members() {
        if record.progress.contains_key(&member.workpiece)
            || record.claims.contains_key(&member.workpiece)
            || state.claims.contains_key(workpiece_key(&member.workpiece))
            || record.withdrawn.contains_key(&member.workpiece)
        {
            continue;
        }
        let dependencies = record.dependencies.iter().filter(|edge| edge.member == member.workpiece);
        if dependencies.clone().count() == 0
            || !dependencies
                .clone()
                .all(|edge| state.integration.head.coverage.iter().any(|pin| pin.workpiece == edge.depends_on))
        {
            continue;
        }
        let context = ConstructContext {
            bloom_base: state.integration.generation.base,
            starting_head: state.integration.head.clone(),
        };
        state.contexts.insert(member.workpiece.0.clone(), context.clone());
        let progress = StageProgress {
            stage: StageId::Construct,
            attempts: 1,
            candidate: None,
            repair_rolls: 0,
            seen_verify_failures: VerifyFailureSet::EMPTY,
            fold_checkpoint: None,
            fold_conflict_evidence: None,
            reconcile_assembles_base: false,
        };
        queue_context_dispatch(record, state, bloom, &member.workpiece, &progress, context, effects);
    }
}

fn retire_displaced_head_work(record: &BloomRecord, state: &mut CoordinationState, effects: &mut Vec<Decision>) {
    let head = state.integration.head.clone();
    for run in state.runs.iter_mut().filter(|run| {
        !run.is_terminal() && run.plan.composition.as_ref().is_some_and(|composition| composition.base != head)
    }) {
        run.stale = true;
        effects.push(Decision::CancelSharedRun { plan: run.plan.digest() });
    }
    let displaced = take(&mut state.preparations);
    for plan in displaced {
        if plan.context.starting_head == state.integration.head {
            state.preparations.push(plan);
            continue;
        }
        if record
            .progress
            .get(&plan.workpiece)
            .is_none_or(|progress| progress.stage != StageId::Reconcile || progress.candidate != Some(plan.authored))
        {
            continue;
        }
        let context = ConstructContext {
            bloom_base: state.integration.generation.base,
            starting_head: state.integration.head.clone(),
        };
        state.contexts.insert(plan.workpiece.0.clone(), context.clone());
        let replacement = CandidatePreparationPlan { context, ..plan };
        state.preparations.push(replacement.clone());
        effects.push(Decision::DispatchCandidatePreparation { plan: replacement });
    }
    state.preview_plans.clear();
    state.previews.clear();
    refresh_unadmitted_construction(record, state, effects);
}

fn refresh_unadmitted_construction(record: &BloomRecord, state: &mut CoordinationState, effects: &mut Vec<Decision>) {
    if state.integration.known_red == Some(state.integration.head.node) {
        // A red head blocks new inheritance, so re-pinning a queued order onto
        // it would hand an author unproven context (ADR-0218).
        return;
    }
    let pending = state.queued_construction.clone();
    for (workpiece, mut dispatch) in pending {
        if state.admitted_construction.contains_key(workpiece.as_str())
            || record.progress.get(&dispatch.workpiece).is_none_or(|progress| progress.stage != StageId::Construct)
        {
            continue;
        }
        let context = ConstructContext {
            bloom_base: state.integration.generation.base,
            starting_head: state.integration.head.clone(),
        };
        if dispatch.context == context {
            continue;
        }
        dispatch.context = context.clone();
        dispatch.transformation.checkout = context.starting_head.candidate.checkout;
        state.contexts.insert(workpiece.clone(), context);
        state.queued_construction.insert(workpiece, dispatch.clone());
        effects.push(Decision::QueueConstructionAdmission { dispatch });
    }
}

/// Queue a composition-owned repair of the exact current red eager head.
/// Pre-check calls this only for a failure bound to that head; stale failures
/// remain diagnostics and never reach this seam.
pub(super) fn schedule_partial_head_repair(
    record: &BloomRecord,
    state: &mut CoordinationState,
    evidence: Digest,
    effects: &mut Vec<Decision>,
) {
    if state.integration.known_red != Some(state.integration.head.node)
        || state.partial_head_repair.is_some()
        || record.operator_hold.is_some()
    {
        return;
    }
    let plan = PartialHeadRepairPlan {
        bloom: state.integration.generation.bloom,
        generation: state.integration.generation.digest(),
        head: state.integration.head.clone(),
        inputs: state.integration.admitted.clone(),
        evidence,
        attempt: state.partial_head_repair_attempts,
    };
    dispatch_partial_head_repair(record, state, plan, effects);
}

/// Promote the red head an accepted partial-head repair produced, once that
/// exact head carries its own aggregate proof, and resume the work the red
/// verdict suspended. A verdict established anywhere else is left alone: a
/// passing pre-check never clears a separately established red (ADR-0218).
pub(super) fn promote_proved_repair(
    record: &BloomRecord,
    state: &mut CoordinationState,
    node: Digest,
    effects: &mut Vec<Decision>,
) {
    if state.integration.head.node != node
        || state.integration.known_red != Some(node)
        || state.integration.unproved_repair != Some(node)
    {
        return;
    }
    state.integration.known_red = None;
    state.integration.unproved_repair = None;

    release_ready_dependents(record, state, effects);
    schedule_append(record, state, effects);
    finalize_selected_root(record, state, effects);
}

fn dispatch_partial_head_repair(
    record: &BloomRecord,
    state: &mut CoordinationState,
    plan: PartialHeadRepairPlan,
    effects: &mut Vec<Decision>,
) {
    let budget = record.stage_catalog.retry_budget_of(StageId::Refine).unwrap_or(1);
    if plan.attempt >= budget {
        state.partial_head_repair = None;
        state.diagnostics.push(CoordinationDiagnostic {
            subject: plan.digest(),
            scope: FailureScope::Unattributed { evidence: plan.evidence },
        });
        hold_coordination(
            plan.bloom,
            format!("partial-head repair budget exhausted after {} attempts", plan.attempt),
            effects,
        );
        return;
    }
    let binding = stage_binding(&record.stage_catalog, StageId::Refine);
    let transformation = Transformation::for_member_stage(
        &binding,
        plan.head.candidate.tree,
        plan.head.candidate.checkout,
        state.integration.generation.base.checkout,
    );
    state.partial_head_repair = Some(plan.clone());
    effects.push(Decision::DispatchPartialHeadRepair {
        dispatch: PartialHeadRepairDispatch {
            plan,
            transformation,
            scope_revision: record.spec.base(),
            profile: binding.profile,
            configs: record.spec.configs().clone(),
        },
    });
}

fn schedule_shared_node_repair(
    record: &BloomRecord,
    state: &mut CoordinationState,
    run: &SharedRunRecord,
    evidence: Digest,
    effects: &mut Vec<Decision>,
) {
    let (Some(node), Some(composition)) = (run.node.as_ref(), run.plan.composition.as_ref()) else {
        return;
    };
    if state.partial_head_repair.is_some() || record.operator_hold.is_some() {
        return;
    }
    let mut inputs = Vec::new();
    if !composition.base.coverage.is_empty() {
        inputs.push(CompositionInput {
            node: composition.base.node,
            candidate: composition.base.candidate,
            members: composition.base.coverage.clone(),
        });
    }
    for input in &composition.inputs {
        if !inputs.iter().any(|current| current.digest() == input.digest()) {
            inputs.push(input.clone());
        }
    }
    dispatch_partial_head_repair(
        record,
        state,
        PartialHeadRepairPlan {
            bloom: state.integration.generation.bloom,
            generation: state.integration.generation.digest(),
            head: IntegrationHead {
                generation: state.integration.generation.digest(),
                node: node.candidate.tree,
                candidate: node.candidate,
                plan: run.plan.digest(),
                coverage: node.coverage.clone(),
            },
            inputs,
            evidence,
            attempt: state.partial_head_repair_attempts,
        },
        effects,
    );
}

fn valid_partial_repair_completion(expected: &PartialHeadRepairPlan, completion: &PartialHeadRepairCompletion) -> bool {
    match completion {
        PartialHeadRepairCompletion::Repaired { evidence, .. } => {
            evidence.kind == EvidenceKind::VerificationResult && evidence.validates(&expected.head.candidate.tree)
        }
        PartialHeadRepairCompletion::Refused { evidence } => {
            matches!(evidence.kind, EvidenceKind::ReviewFinding | EvidenceKind::RepairTriage)
                && evidence.validates(&expected.head.candidate.tree)
        }
        PartialHeadRepairCompletion::HostFault { evidence } => {
            evidence.kind == EvidenceKind::ExecutorFault && evidence.validates(&expected.head.candidate.tree)
        }
    }
}

fn apply_repaired_partial_head(
    record: &BloomRecord,
    state: &mut CoordinationState,
    expected: &PartialHeadRepairPlan,
    candidate: CandidateRef,
    evidence: &Evidence,
    repairs_selected_head: bool,
    effects: &mut Vec<Decision>,
) -> Result<(), CoordinationError> {
    effects.push(Decision::RecordEvidence { bloom: expected.bloom, evidence: evidence.clone() });
    let repaired = CompositionInput { node: candidate.tree, candidate, members: expected.head.coverage.clone() };
    if repairs_selected_head {
        let Some(epoch) = state.integration.generation.epoch.checked_add(1) else {
            return Err(CoordinationError::InvalidPlan);
        };
        for run in state.runs.iter_mut().filter(|run| !run.is_terminal()) {
            run.stale = true;
            effects.push(Decision::CancelSharedRun { plan: run.plan.digest() });
        }
        state.integration.generation.epoch = epoch;
        let generation = state.integration.generation.digest();
        state.integration.head = IntegrationHead {
            generation,
            node: candidate.tree,
            candidate,
            plan: expected.digest(),
            coverage: expected.head.coverage.clone(),
        };
        state.integration.admitted = alloc::vec![repaired.clone()];
        state.integration.queued.clear();
        state.integration.in_flight = None;
        state.integration.known_red = Some(candidate.tree);
        state.integration.unproved_repair = Some(candidate.tree);
        state.integration.reservation = None;
        state.integration.movement_count = 0;
        state.partial_head_repair_attempts = 0;
        state.final_dispatched = false;
        state.claims.clear();
    }
    for request in &mut state.requests {
        if repaired.members.contains(&request.member) {
            state.claims.remove(workpiece_key(&request.member.workpiece));
            request.input = repaired.clone();
            effects.push(Decision::QueueMemberVerification { request: request.clone() });
        }
    }
    if repairs_selected_head {
        retire_displaced_head_work(record, state, effects);
        schedule_append(record, state, effects);
    }
    Ok(())
}

pub(super) fn reduce_partial_head_repaired(
    snapshot: &Snapshot,
    bloom: &BloomId,
    plan: Digest,
    completion: &PartialHeadRepairCompletion,
) -> Decisions {
    let (record, state) = match active_state(snapshot, bloom) {
        Ok(active) => active,
        Err(error) => return rejected(error),
    };
    let Some(expected) = state.partial_head_repair.as_ref() else {
        return rejected(CoordinationError::NotReady);
    };
    let repairs_selected_head =
        expected.head == state.integration.head && state.integration.known_red == Some(expected.head.node);
    let repairs_shared_node = state.runs.iter().any(|run| {
        run.plan.digest() == expected.head.plan
            && run.node.as_ref().is_some_and(|node| {
                node.candidate == expected.head.candidate && node.coverage == expected.head.coverage
            })
            && run.completed.iter().any(|outcome| {
                matches!(outcome, MemberVerifyOutcome::Failed {
                    scope: FailureScope::Interaction { evidence, .. },
                    ..
                } if *evidence == expected.evidence)
            })
    });
    if expected.digest() != plan
        || expected.bloom != *bloom
        || expected.generation != state.integration.generation.digest()
        || (!repairs_selected_head && !repairs_shared_node)
    {
        return rejected(CoordinationError::PlanMismatch { expected: expected.digest(), got: plan });
    }
    if !valid_partial_repair_completion(expected, completion) {
        return rejected(CoordinationError::InvalidEvidenceKind);
    }
    let mut next = state.clone();
    next.partial_head_repair = None;
    next.partial_head_repair_attempts = next.partial_head_repair_attempts.saturating_add(1);
    let mut effects = Vec::new();
    match completion {
        PartialHeadRepairCompletion::Repaired { candidate, evidence } => {
            if let Err(error) = apply_repaired_partial_head(
                record,
                &mut next,
                expected,
                *candidate,
                evidence,
                repairs_selected_head,
                &mut effects,
            ) {
                return rejected(error);
            }
        }
        PartialHeadRepairCompletion::Refused { evidence } => {
            next.diagnostics.push(CoordinationDiagnostic {
                subject: plan,
                scope: FailureScope::Interaction { members: expected.head.coverage.clone(), evidence: evidence.detail },
            });
            effects.push(Decision::RecordEvidence { bloom: *bloom, evidence: evidence.clone() });
        }
        PartialHeadRepairCompletion::HostFault { evidence } => {
            next.diagnostics.push(CoordinationDiagnostic {
                subject: plan,
                scope: FailureScope::Unattributed { evidence: evidence.detail },
            });
        }
    }
    if !matches!(completion, PartialHeadRepairCompletion::Repaired { .. }) {
        let retry = PartialHeadRepairPlan { attempt: next.partial_head_repair_attempts, ..expected.clone() };
        dispatch_partial_head_repair(record, &mut next, retry, &mut effects);
    }
    effects.insert(0, record_state(*bloom, next));
    accepted(*bloom, plan, effects)
}

pub(super) fn reduce_integration_advanced(
    snapshot: &Snapshot,
    bloom: &BloomId,
    plan: Digest,
    head: &IntegrationHead,
) -> Decisions {
    let (record, state) = match active_state(snapshot, bloom) {
        Ok(active) => active,
        Err(error) => return rejected(error),
    };
    let Some(expected) = state.integration.in_flight.as_ref() else {
        return rejected(CoordinationError::NotReady);
    };
    if expected.digest() != plan {
        return rejected(CoordinationError::PlanMismatch { expected: expected.digest(), got: plan });
    }
    if expected.inputs.len() != 1 || head.plan != plan || head.generation != expected.generation {
        return rejected(CoordinationError::InvalidPlan);
    }
    let input = &expected.inputs[0];
    let mut coverage = expected.expected_parent.coverage.clone();
    for pin in &input.members {
        if coverage.iter().any(|current| current.workpiece == pin.workpiece && current != pin) {
            return rejected(CoordinationError::MemberVersionMismatch { workpiece: pin.workpiece.clone() });
        }
        if !coverage.contains(pin) {
            coverage.push(pin.clone());
        }
    }
    if head.coverage != coverage {
        return rejected(CoordinationError::InvalidPlan);
    }
    let mut next = state.clone();
    next.integration.head = head.clone();
    next.final_dispatched = false;
    next.integration.in_flight = None;
    next.integration.known_red = None;
    next.integration.unproved_repair = None;
    next.integration.queued.retain(|queued| queued.digest() != input.digest());
    if !next.integration.admitted.iter().any(|admitted| admitted.digest() == input.digest()) {
        next.integration.admitted.push(input.clone());
    }
    if next.integration.reservation.as_ref().is_some_and(|reservation| {
        reservation.generation == expected.generation
            && reservation.node == expected.expected_parent.node
            && input.members.iter().any(|member| member.workpiece == reservation.owner)
    }) {
        next.integration.reservation = None;
        next.integration.movement_count = 0;
    }
    let mut effects = Vec::new();
    retire_displaced_head_work(record, &mut next, &mut effects);
    release_ready_dependents(record, &mut next, &mut effects);
    schedule_append(record, &mut next, &mut effects);
    finalize_selected_root(record, &mut next, &mut effects);
    effects.insert(0, record_state(*bloom, next));
    accepted(*bloom, head.digest(), effects)
}

#[derive(Clone, Copy)]
pub(super) struct IntegrationConflict<'a> {
    pub bloom: &'a BloomId,
    pub plan: Digest,
    pub generation: Digest,
    pub expected_parent: Digest,
    pub input: &'a CompositionInput,
    pub at: CandidateRef,
    pub evidence: &'a Evidence,
    pub observed_at_unix_millis: u64,
}

pub(super) fn reduce_integration_conflicted(snapshot: &Snapshot, conflict: IntegrationConflict<'_>) -> Decisions {
    let IntegrationConflict { bloom, plan, generation, expected_parent, input, at, evidence, observed_at_unix_millis } =
        conflict;
    let (record, state) = match active_state(snapshot, bloom) {
        Ok(active) => active,
        Err(error) => return rejected(error),
    };
    let Some(expected) = state.integration.in_flight.as_ref() else {
        return rejected(CoordinationError::NotReady);
    };
    if expected.digest() != plan
        || expected.generation != generation
        || expected.expected_parent.node != expected_parent
    {
        return rejected(CoordinationError::PlanMismatch { expected: expected.digest(), got: plan });
    }
    if expected.inputs.as_slice() != [input.clone()]
        || evidence.kind != EvidenceKind::FoldConflict
        || evidence.subject != at.tree
    {
        return rejected(CoordinationError::InvalidPlan);
    }
    let mut next = state.clone();
    next.integration.in_flight = None;
    next.integration.movement_count = next.integration.movement_count.saturating_add(1);
    next.diagnostics.push(CoordinationDiagnostic {
        subject: input.digest(),
        scope: FailureScope::Interaction { members: input.members.clone(), evidence: evidence.detail },
    });
    let mut effects = alloc::vec![Decision::RecordEvidence { bloom: *bloom, evidence: evidence.clone() }];
    for pin in &input.members {
        let Some(member) = record.spec.members().iter().find(|member| member.workpiece == pin.workpiece) else {
            return rejected(CoordinationError::MemberVersionMismatch { workpiece: pin.workpiece.clone() });
        };
        let context = ConstructContext {
            bloom_base: next.integration.generation.base,
            starting_head: expected.expected_parent.clone(),
        };
        next.contexts.insert(pin.workpiece.0.clone(), context.clone());
        let old = record.progress.get(&pin.workpiece).copied();
        let progress = StageProgress {
            stage: StageId::Reconcile,
            attempts: 1,
            candidate: Some(pin.candidate),
            repair_rolls: old.map_or(0, |progress| progress.repair_rolls),
            seen_verify_failures: old.map_or(VerifyFailureSet::EMPTY, |progress| progress.seen_verify_failures),
            fold_checkpoint: Some(expected_parent),
            fold_conflict_evidence: Some(evidence.detail),
            reconcile_assembles_base: false,
        };
        let _ = member;
        queue_context_dispatch(record, &mut next, *bloom, &pin.workpiece, &progress, context, &mut effects);
    }
    if next.integration.movement_count >= next.policy.movement_budget
        && let Some(owner) = input.members.first()
    {
        let Some(deadline) = observed_at_unix_millis.checked_add(next.policy.reservation_millis) else {
            return rejected(CoordinationError::InvalidPlan);
        };
        next.integration.reservation = Some(StableHeadReservation {
            owner: owner.workpiece.clone(),
            generation,
            node: expected_parent,
            movement_count: next.integration.movement_count,
            deadline_unix_millis: deadline,
            hold: evidence.detail,
        });
    }
    effects.insert(0, record_state(*bloom, next));
    accepted(*bloom, plan, effects)
}

pub(super) fn reduce_integration_refused(
    snapshot: &Snapshot,
    bloom: &BloomId,
    plan: Digest,
    generation: Digest,
    expected_parent: Digest,
    detail: Digest,
) -> Decisions {
    let (_, state) = match active_state(snapshot, bloom) {
        Ok(active) => active,
        Err(error) => return rejected(error),
    };
    let Some(expected) = state.integration.in_flight.as_ref() else {
        return rejected(CoordinationError::NotReady);
    };
    if expected.digest() != plan
        || expected.generation != generation
        || expected.expected_parent.node != expected_parent
    {
        return rejected(CoordinationError::PlanMismatch { expected: expected.digest(), got: plan });
    }
    let mut next = state.clone();
    next.integration.in_flight = None;
    next.diagnostics
        .push(CoordinationDiagnostic { subject: plan, scope: FailureScope::Unattributed { evidence: detail } });
    accepted(*bloom, plan, alloc::vec![record_state(*bloom, next)])
}

fn apply_prepared_candidate(
    snapshot: &Snapshot,
    record: &BloomRecord,
    state: &mut CoordinationState,
    expected: &CandidatePreparationPlan,
    prepared: &PreparedCandidate,
    effects: &mut Vec<Decision>,
) -> Result<(), CoordinationError> {
    if prepared.authored != expected.authored
        || prepared.context != expected.context
        || prepared.diff_base != expected.context.starting_head.candidate
        || state.integration.generation.digest() != expected.context.starting_head.generation
        || state.integration.head != expected.context.starting_head
        || record.progress.get(&expected.workpiece).is_none_or(|progress| {
            progress.stage != StageId::Reconcile || progress.candidate != Some(expected.authored)
        })
    {
        return Err(CoordinationError::InvalidPlan);
    }
    let Some(member) = record
        .spec
        .members()
        .iter()
        .find(|member| member.workpiece == expected.workpiece && member.scope_revision == expected.scope_revision)
    else {
        return Err(CoordinationError::MemberVersionMismatch { workpiece: expected.workpiece.clone() });
    };
    let prior = state
        .requests
        .iter()
        .rev()
        .find(|request| request.member.workpiece == member.workpiece)
        .map(|request| request.member.candidate)
        .or_else(|| state.claims.get(workpiece_key(&member.workpiece)).map(|claim| claim.member.candidate));
    if prior.is_some_and(|current| current != prepared.candidate) {
        return Err(CoordinationError::MemberVersionMismatch { workpiece: member.workpiece.clone() });
    }
    state.prepared.insert(member.workpiece.0.clone(), prepared.clone());
    let old = record.progress.get(&member.workpiece).copied().ok_or(CoordinationError::NotReady)?;
    let progress = StageProgress { stage: StageId::Verify, attempts: 1, candidate: Some(prepared.candidate), ..old };
    let binding = stage_binding(&record.stage_catalog, StageId::Verify);
    let transformation = Transformation::for_member_stage(
        &binding,
        prepared.candidate.tree,
        prepared.candidate.checkout,
        prepared.diff_base.checkout,
    );
    let member_configs = member.configs.layered_over(record.spec.configs());
    let request = request_for_dispatch(
        snapshot,
        record,
        state,
        VerifyDispatch {
            workpiece: &member.workpiece,
            transformation: &transformation,
            profile: &binding.profile,
            configs: &member_configs,
            progress: &progress,
        },
    )
    .expect("prepared member is current");
    if !state.requests.iter().any(|current| current.digest() == request.digest()) {
        state.requests.push(request.clone());
    }
    effects.push(Decision::AdvanceStage { bloom: expected.bloom, workpiece: member.workpiece.clone(), progress });
    effects.push(Decision::QueueMemberVerification { request });
    Ok(())
}

pub(super) fn reduce_candidate_prepared(
    snapshot: &Snapshot,
    bloom: &BloomId,
    plan: Digest,
    preparation: &CandidatePreparation,
) -> Decisions {
    let (record, state) = match active_state(snapshot, bloom) {
        Ok(active) => active,
        Err(error) => return rejected(error),
    };
    let Some(expected) = state.preparation(plan) else {
        return rejected(CoordinationError::PlanMismatch { expected: Digest::default(), got: plan });
    };
    if expected.bloom != *bloom {
        return rejected(CoordinationError::InvalidPlan);
    }
    let mut next = state.clone();
    next.preparations.retain(|pending| pending.digest() != plan);
    let mut effects = Vec::new();
    match preparation {
        CandidatePreparation::Prepared(prepared) => {
            if let Err(error) = apply_prepared_candidate(snapshot, record, &mut next, expected, prepared, &mut effects)
            {
                return rejected(error);
            }
        }
        CandidatePreparation::Conflict { evidence } => {
            if evidence.kind != EvidenceKind::FoldConflict
                || evidence.subject != expected.context.starting_head.candidate.tree
            {
                return rejected(CoordinationError::InvalidEvidenceKind);
            }
            next.diagnostics.push(CoordinationDiagnostic {
                subject: plan,
                scope: FailureScope::Interaction {
                    members: alloc::vec![MemberPin {
                        workpiece: expected.workpiece.clone(),
                        scope_revision: expected.scope_revision,
                        candidate: expected.authored,
                    }],
                    evidence: evidence.detail,
                },
            });
            effects.push(Decision::RecordEvidence { bloom: *bloom, evidence: evidence.clone() });
            let Some(cursor) = record.progress.get(&expected.workpiece).copied() else {
                return rejected(CoordinationError::NotReady);
            };
            let attempt = cursor.attempts.saturating_add(1);
            if attempt <= record.stage_catalog.retry_budget_of(StageId::Reconcile).unwrap_or(1) {
                let progress = StageProgress {
                    stage: StageId::Reconcile,
                    attempts: attempt,
                    fold_checkpoint: Some(expected.context.starting_head.node),
                    fold_conflict_evidence: Some(evidence.detail),
                    ..cursor
                };
                queue_context_dispatch(
                    record,
                    &mut next,
                    *bloom,
                    &expected.workpiece,
                    &progress,
                    expected.context.clone(),
                    &mut effects,
                );
            } else {
                effects.push(Decision::RecordWedge {
                    bloom: *bloom,
                    workpiece: expected.workpiece.clone(),
                    wedge: Wedge {
                        stage: StageId::Reconcile,
                        evidence: evidence.detail,
                        repeated_verifiers: VerifyFailureSet::EMPTY,
                    },
                });
            }
        }
        CandidatePreparation::Refused { detail } => {
            next.diagnostics.push(CoordinationDiagnostic {
                subject: plan,
                scope: FailureScope::Unattributed { evidence: *detail },
            });
            effects.push(Decision::RecordWedge {
                bloom: *bloom,
                workpiece: expected.workpiece.clone(),
                wedge: Wedge {
                    stage: StageId::Reconcile,
                    evidence: *detail,
                    repeated_verifiers: VerifyFailureSet::EMPTY,
                },
            });
        }
    }
    effects.insert(0, record_state(*bloom, next));
    accepted(*bloom, plan, effects)
}

fn validate_composition(record: &BloomRecord, state: &CoordinationState, plan: &CompositionPlan) -> bool {
    if plan.bloom != state.integration.generation.bloom
        || plan.requests.is_empty()
        || plan.requests.iter().any(|request| !exact_request(record, state, request))
    {
        return false;
    }
    let ids = plan.requests.iter().map(MemberVerifyRequest::digest).collect::<Vec<_>>();
    let member_contracts = plan
        .requests
        .iter()
        .map(|request| MemberContractPin { request: request.digest(), contract: request.contract.digest() })
        .collect::<Vec<_>>();
    let mut input_ids = BTreeSet::new();
    let inputs = plan
        .requests
        .iter()
        .filter_map(|request| input_ids.insert(request.input.digest()).then_some(request.input.clone()))
        .collect::<Vec<_>>();
    if ids.iter().collect::<BTreeSet<_>>().len() != ids.len()
        || plan.base != state.integration.head
        || plan.inputs != inputs
        || plan.contract != state.composition_contract.bind(member_contracts)
    {
        return false;
    }
    plan.requests
        .iter()
        .all(|request| request.input.members.contains(&request.member) && state.composition_contract.covers(request))
        && plan
            .inputs
            .iter()
            .flat_map(|input| &input.members)
            .all(|pin| plan.requests.iter().any(|request| request.member == *pin) || plan.base.coverage.contains(pin))
}

fn validate_run_plan(record: &BloomRecord, state: &CoordinationState, plan: &SharedRunPlan) -> bool {
    let cap = match plan.mode {
        SharedRunMode::WarmSerial => min(state.policy.max_run_members, state.policy.max_serial_requests),
        SharedRunMode::Standalone | SharedRunMode::Contextual => state.policy.max_run_members,
    } as usize;
    let mode_allowed = match state.policy.verification {
        VerificationMode::Standalone => plan.mode == SharedRunMode::Standalone,
        VerificationMode::WarmSerial => matches!(plan.mode, SharedRunMode::Standalone | SharedRunMode::WarmSerial),
        VerificationMode::Contextual => matches!(plan.mode, SharedRunMode::Standalone | SharedRunMode::Contextual),
    };
    let retry_budget = stage_binding(
        &record.stage_catalog,
        if plan.mode == SharedRunMode::Contextual {
            StageId::AggregateVerify
        } else {
            StageId::Verify
        },
    )
    .retry_budget;
    mode_allowed
        && !plan.requests.is_empty()
        && plan.requests.len() <= cap
        && (plan.mode != SharedRunMode::Standalone || plan.requests.len() == 1)
        && plan.execution_attempt == state.next_execution_attempt(plan)
        && plan.execution_attempt < retry_budget
        && plan.requests.iter().all(|request| {
            exact_request(record, state, request)
                && state.requests.iter().any(|current| current.digest() == request.digest())
                && !state.claims.contains_key(workpiece_key(&request.member.workpiece))
                && state
                    .runs
                    .iter()
                    .filter(|run| !run.is_terminal())
                    .all(|run| run.plan.requests.iter().all(|retained| retained.digest() != request.digest()))
        })
        && (plan.mode == SharedRunMode::Contextual
            || plan.requests.iter().all(|request| request.input.candidate == request.member.candidate))
        && state.survivor_groups.iter().all(|group| {
            let selected = plan.requests.iter().map(MemberVerifyRequest::digest).collect::<Vec<_>>();
            !selected.iter().any(|request| group.requests.contains(request)) || selected == group.requests
        })
        && match (&plan.mode, &plan.composition) {
            (SharedRunMode::Contextual, Some(composition)) => {
                plan.probe_budget == state.policy.max_attribution_probes
                    && validate_composition(record, state, composition)
                    && composition.requests == plan.requests
            }
            (SharedRunMode::Standalone | SharedRunMode::WarmSerial, None) => plan.probe_budget == 0,
            _ => false,
        }
}

fn retained_plan_current(record: &BloomRecord, state: &CoordinationState, plan: &SharedRunPlan) -> bool {
    plan.requests.iter().all(|request| {
        exact_request(record, state, request)
            && state.requests.iter().any(|current| current.digest() == request.digest())
            && !state.claims.contains_key(workpiece_key(&request.member.workpiece))
    }) && plan.composition.as_ref().is_none_or(|composition| validate_composition(record, state, composition))
}

pub(super) fn reduce_propose_shared_run(snapshot: &Snapshot, bloom: &BloomId, plan: &SharedRunPlan) -> Decisions {
    let (record, state) = match active_state(snapshot, bloom) {
        Ok(active) => active,
        Err(error) => return rejected(error),
    };
    if record.operator_hold.is_some() {
        return rejected(CoordinationError::OnHold);
    }
    if !validate_run_plan(record, state, plan)
        || state.diagnostics.iter().any(|diagnostic| diagnostic.subject == plan.digest())
    {
        return rejected(CoordinationError::InvalidPlan);
    }
    if state.run(plan.digest()).is_some() {
        return rejected(CoordinationError::AlreadyIssued);
    }
    let mut next = state.clone();
    next.runs.push(SharedRunRecord {
        plan: plan.clone(),
        node: None,
        phase: SharedRunPhase::Preparing,
        stale: false,
        physical_run: None,
        completed: Vec::new(),
        unfinished: plan.requests.iter().map(MemberVerifyRequest::digest).collect(),
        latencies: Vec::new(),
    });
    accepted(
        *bloom,
        plan.digest(),
        alloc::vec![record_state(*bloom, next), Decision::DispatchSharedRunPreparation { plan: plan.clone() }],
    )
}

fn serial_fallbacks(state: &mut CoordinationState, refused: &SharedRunPlan, effects: &mut Vec<Decision>) {
    for request in &refused.requests {
        if request.input.candidate != request.member.candidate {
            state.diagnostics.push(CoordinationDiagnostic {
                subject: request.digest(),
                scope: FailureScope::Unattributed { evidence: refused.digest() },
            });
            hold_coordination(
                request.bloom,
                format!("shared verification could not prepare atomic input {}", request.input.digest()),
                effects,
            );
            continue;
        }
        let plan = SharedRunPlan {
            mode: SharedRunMode::Standalone,
            requests: alloc::vec![request.clone()],
            composition: None,
            probe_budget: 0,
            execution_attempt: 0,
        };
        let plan = SharedRunPlan { execution_attempt: state.next_execution_attempt(&plan), ..plan };
        if state.run(plan.digest()).is_some() {
            continue;
        }
        state.runs.push(SharedRunRecord {
            plan: plan.clone(),
            node: None,
            phase: SharedRunPhase::Preparing,
            stale: false,
            physical_run: None,
            completed: Vec::new(),
            unfinished: alloc::vec![request.digest()],
            latencies: Vec::new(),
        });
        effects.push(Decision::DispatchSharedRunPreparation { plan });
    }
}

pub(super) fn reduce_shared_run_prepared(
    snapshot: &Snapshot,
    bloom: &BloomId,
    plan: Digest,
    preparation: &SharedRunPreparation,
) -> Decisions {
    let (record, state) = match active_state(snapshot, bloom) {
        Ok(active) => active,
        Err(error) => return rejected(error),
    };
    let Some(index) = state.runs.iter().position(|run| run.plan.digest() == plan) else {
        return rejected(CoordinationError::PlanMismatch { expected: Digest::default(), got: plan });
    };
    if !state.runs[index].stale && !retained_plan_current(record, state, &state.runs[index].plan) {
        return rejected(CoordinationError::InvalidPlan);
    }
    let mut next = state.clone();
    let run = &mut next.runs[index];
    if !matches!(run.phase, SharedRunPhase::Preparing) || run.node.is_some() {
        return rejected(CoordinationError::AlreadyIssued);
    }
    if run.stale {
        run.phase = SharedRunPhase::Terminal;
        return accepted(*bloom, plan, alloc::vec![record_state(*bloom, next)]);
    }
    let mut effects = Vec::new();
    match preparation {
        SharedRunPreparation::Standalone if run.plan.mode != SharedRunMode::Contextual => {
            run.phase = SharedRunPhase::Ready;
            effects.push(Decision::DispatchSharedRun {
                dispatch: SharedRunDispatch { plan: run.plan.clone(), execution: SharedRunExecution::Serial },
            });
        }
        SharedRunPreparation::Contextual(node) if run.plan.mode == SharedRunMode::Contextual => {
            let Some(composition) = run.plan.composition.as_ref() else {
                return rejected(CoordinationError::InvalidPlan);
            };
            let mut coverage = composition.base.coverage.clone();
            for pin in composition.inputs.iter().flat_map(|input| &input.members) {
                if coverage.iter().any(|current| current.workpiece == pin.workpiece && current != pin) {
                    return rejected(CoordinationError::MemberVersionMismatch { workpiece: pin.workpiece.clone() });
                }
                if !coverage.contains(pin) {
                    coverage.push(pin.clone());
                }
            }
            if node.plan != plan || node.coverage != coverage {
                return rejected(CoordinationError::InvalidPlan);
            }
            let transformation = composition.contract.invocation.instantiate(node.candidate);
            run.node = Some(node.clone());
            run.phase = SharedRunPhase::Ready;
            effects.push(Decision::DispatchSharedRun {
                dispatch: SharedRunDispatch {
                    plan: run.plan.clone(),
                    execution: SharedRunExecution::Contextual {
                        node: node.clone(),
                        transformation,
                        profile: composition.contract.invocation.profile.clone(),
                        configs: composition.contract.invocation.configs.clone(),
                    },
                },
            });
        }
        SharedRunPreparation::Refused { detail } => {
            next.diagnostics.push(CoordinationDiagnostic {
                subject: plan,
                scope: FailureScope::Unattributed { evidence: *detail },
            });
            run.phase = SharedRunPhase::Terminal;
            let refused = run.plan.clone();
            serial_fallbacks(&mut next, &refused, &mut effects);
        }
        _ => return rejected(CoordinationError::InvalidPlan),
    }
    effects.insert(0, record_state(*bloom, next));
    accepted(*bloom, plan, effects)
}

pub(super) fn reduce_shared_run_started(
    snapshot: &Snapshot,
    bloom: &BloomId,
    plan: Digest,
    physical_run: Digest,
) -> Decisions {
    let (record, state) = match active_state(snapshot, bloom) {
        Ok(active) => active,
        Err(error) => return rejected(error),
    };
    let Some(index) = state.runs.iter().position(|run| run.plan.digest() == plan) else {
        return rejected(CoordinationError::PlanMismatch { expected: Digest::default(), got: plan });
    };
    if !state.runs[index].stale && !retained_plan_current(record, state, &state.runs[index].plan) {
        return rejected(CoordinationError::InvalidPlan);
    }
    let mut next = state.clone();
    let run = &mut next.runs[index];
    if let Some(expected) = run.physical_run
        && expected != physical_run
    {
        return rejected(CoordinationError::RunMismatch { expected, got: physical_run });
    }
    if !run.is_ready() || run.physical_run.is_some() || run.unfinished.is_empty() {
        return rejected(CoordinationError::AlreadyIssued);
    }
    run.phase = SharedRunPhase::Running;
    run.physical_run = Some(physical_run);
    let mut effects = alloc::vec![record_state(*bloom, next)];
    if state.runs[index].stale {
        effects.push(Decision::CancelSharedRun { plan });
    }
    accepted(*bloom, physical_run, effects)
}

fn validate_latency(plan: &SharedRunPlan, latencies: &[MemberVerifyLatency]) -> bool {
    let mut seen = BTreeSet::new();
    latencies.iter().all(|latency| {
        seen.insert(latency.request)
            && plan
                .requests
                .iter()
                .any(|request| request.digest() == latency.request && request.member == latency.member)
    })
}

fn scope_members_are_planned(run: &SharedRunRecord, members: &[MemberPin]) -> bool {
    !members.is_empty()
        && members.iter().enumerate().all(|(index, member)| !members[..index].contains(member))
        && members.iter().all(|member| run.plan.requests.iter().any(|request| request.member == *member))
}

fn valid_failure_scope(run: &SharedRunRecord, scope: &FailureScope, evidence: &Evidence) -> bool {
    let evidence_id = match scope {
        FailureScope::Attributed { members, evidence } => {
            if !scope_members_are_planned(run, members) {
                return false;
            }
            evidence
        }
        FailureScope::Interaction { members, evidence } => {
            // Interaction is a claim about one physical composition node: its
            // repair and its polluted-survivor group both address that node, so
            // a run without one cannot carry the scope at all.
            if members.len() < 2 || run.node.is_none() || !scope_members_are_planned(run, members) {
                return false;
            }
            evidence
        }
        FailureScope::Inherited { head, evidence } => {
            let inherited =
                run.plan.composition.as_ref().is_some_and(|composition| composition.base.node == *head)
                    || run.plan.requests.iter().any(|request| {
                        request.context.as_ref().is_some_and(|context| context.starting_head.node == *head)
                    });
            if !inherited {
                return false;
            }
            evidence
        }
        FailureScope::Unattributed { evidence } => evidence,
    };
    *evidence_id == evidence.detail
}

fn blocked_members(
    record: &BloomRecord,
    run: &SharedRunRecord,
    outcomes: &[MemberVerifyOutcome],
) -> BTreeSet<WorkpieceId> {
    let mut blocked = BTreeSet::new();
    for outcome in outcomes {
        match outcome {
            MemberVerifyOutcome::Failed { scope, .. } => match scope {
                FailureScope::Attributed { members, .. } | FailureScope::Interaction { members, .. } => {
                    blocked.extend(members.iter().map(|member| member.workpiece.clone()));
                }
                FailureScope::Inherited { head, .. } => {
                    if run.plan.composition.as_ref().is_some_and(|composition| composition.base.node == *head) {
                        blocked.extend(run.plan.requests.iter().map(|request| request.member.workpiece.clone()));
                    } else {
                        blocked.extend(
                            run.plan
                                .requests
                                .iter()
                                .filter(|request| {
                                    request.context.as_ref().is_some_and(|context| context.starting_head.node == *head)
                                })
                                .map(|request| request.member.workpiece.clone()),
                        );
                    }
                }
                FailureScope::Unattributed { .. } => {
                    blocked.extend(run.plan.requests.iter().map(|request| request.member.workpiece.clone()));
                }
            },
            MemberVerifyOutcome::Pending { request, .. } => {
                if let Some(request) = run.plan.requests.iter().find(|candidate| candidate.digest() == *request) {
                    blocked.insert(request.member.workpiece.clone());
                }
            }
            MemberVerifyOutcome::HostFault { .. } => {
                blocked.extend(run.plan.requests.iter().map(|request| request.member.workpiece.clone()));
            }
            MemberVerifyOutcome::PassedStandalone { .. }
            | MemberVerifyOutcome::PassedIn { .. }
            | MemberVerifyOutcome::Survived { .. } => {}
        }
    }
    loop {
        let before = blocked.len();
        for edge in &record.dependencies {
            if blocked.contains(&edge.depends_on) {
                blocked.insert(edge.member.clone());
            }
        }
        for input in run.plan.requests.iter().map(|request| &request.input) {
            if input.members.iter().any(|member| blocked.contains(&member.workpiece)) {
                blocked.extend(input.members.iter().map(|member| member.workpiece.clone()));
            }
        }
        if blocked.len() == before {
            return blocked;
        }
    }
}

fn atomic_input_for_claim(state: &CoordinationState, claim: &ContextualResolutionClaim) -> Option<CompositionInput> {
    match &claim.proof {
        ResolutionProof::Standalone(_) => state
            .requests
            .iter()
            .rev()
            .find(|request| request.member == claim.member)
            .map(|request| request.input.clone()),
        ResolutionProof::InComposition { plan, node, .. } => {
            let run = state.run(*plan)?;
            let recorded = run.node.as_ref().filter(|recorded| recorded.digest() == *node)?;
            Some(CompositionInput {
                node: recorded.digest(),
                candidate: recorded.candidate,
                members: recorded.coverage.clone(),
            })
        }
    }
}

fn final_fold_if_ready(record: &BloomRecord, state: &mut CoordinationState, effects: &mut Vec<Decision>) {
    if state.policy.eager_integration || state.final_in_flight {
        return;
    }
    let claims = record
        .spec
        .members()
        .iter()
        .filter(|member| !record.withdrawn.contains_key(&member.workpiece))
        .map(|member| {
            state
                .claims
                .get(workpiece_key(&member.workpiece))
                .filter(|claim| state.has_exact_claim(&claim.member))
                .cloned()
        })
        .collect::<Option<Vec<_>>>();
    let Some(claims) = claims else {
        return;
    };
    state.final_in_flight = true;
    if state.policy.verification == VerificationMode::Contextual {
        let Some(inputs) = claims.iter().map(|claim| atomic_input_for_claim(state, claim)).collect::<Option<Vec<_>>>()
        else {
            state.final_in_flight = false;
            return;
        };
        for input in inputs {
            queue_input(state, input);
        }
        schedule_append(record, state, effects);
        return;
    }
    state.final_dispatched = true;
    effects.push(Decision::DispatchIntegration {
        bloom: record.spec.id(),
        base: record.spec.base(),
        members: claims
            .into_iter()
            .map(|claim| MemberCandidate { workpiece: claim.member.workpiece, candidate: claim.member.candidate.tree })
            .collect(),
        adopt_from: None,
    });
}

fn finalize_selected_root(record: &BloomRecord, state: &mut CoordinationState, effects: &mut Vec<Decision>) {
    if !state.uses_selected_root()
        || state.final_dispatched
        || state.integration.in_flight.is_some()
        || !state.integration.queued.is_empty()
        || state.integration.known_red == Some(state.integration.head.node)
        || record.operator_hold.is_some()
    {
        return;
    }
    let active = record
        .spec
        .members()
        .iter()
        .filter(|member| !record.withdrawn.contains_key(&member.workpiece))
        .collect::<Vec<_>>();
    if state.integration.head.coverage.len() != active.len()
        || active.iter().any(|member| {
            state
                .integration
                .head
                .coverage
                .iter()
                .find(|pin| pin.workpiece == member.workpiece && pin.scope_revision == member.scope_revision)
                .is_none_or(|pin| !state.has_exact_claim(pin))
        })
    {
        return;
    }
    let Some(roll) = record.aggregate_verify_rolls.checked_add(1) else {
        return;
    };
    if at_park_ceiling(record, StageId::AggregateVerify, roll) {
        return;
    }
    let head = state.integration.head.clone();
    let lineage = state.integration.admitted.iter().map(|input| input.candidate.tree).collect::<Vec<_>>();
    let decided = state.contextual_aggregate_proof(&head).map_or_else(
        || {
            super::integrate::folded(
                record,
                record.spec.id(),
                head.candidate.tree,
                head.candidate.checkout,
                &lineage,
                roll,
            )
        },
        |evidence| {
            super::integrate::folded_with_contextual_proof(
                record,
                record.spec.id(),
                head.candidate.tree,
                head.candidate.checkout,
                &lineage,
                roll,
                evidence,
            )
        },
    );
    state.final_dispatched = true;
    effects.extend(decided.effects);
}

fn validate_contextual_pass(
    run: &SharedRunRecord,
    request: &MemberVerifyRequest,
    outcome: &MemberVerifyOutcome,
) -> bool {
    let MemberVerifyOutcome::PassedIn { request: request_id, node, receipt } = outcome else {
        return false;
    };
    let (Some(actual_node), Some(composition)) = (run.node.as_ref(), run.plan.composition.as_ref()) else {
        return false;
    };
    *request_id == request.digest()
        && *node == actual_node.digest()
        && receipt.kind == EvidenceKind::VerificationResult
        && receipt.validates(&actual_node.candidate.tree)
        && composition
            .contract
            .members
            .iter()
            .any(|member| member.request == *request_id && member.contract == request.contract.digest())
}

fn validates_immutable_run_outcome(
    run: &SharedRunRecord,
    request: &MemberVerifyRequest,
    outcome: &MemberVerifyOutcome,
) -> bool {
    match outcome {
        MemberVerifyOutcome::PassedStandalone { proof, .. } => {
            run.plan.mode != SharedRunMode::Contextual
                && proof.stage == StageId::Verify
                && proof.verified().tree == request.member.candidate.tree
                && proof.gate_set == request.contract.gate_set
                && proof.evidence.kind == EvidenceKind::VerificationResult
                && proof.evidence.validates(&request.member.candidate.tree)
        }
        MemberVerifyOutcome::PassedIn { .. } => validate_contextual_pass(run, request, outcome),
        MemberVerifyOutcome::Failed { scope, failures, evidence, .. } => {
            let subject = run.node.as_ref().map_or(request.member.candidate.tree, |node| node.candidate.tree);
            evidence.kind == EvidenceKind::VerificationResult
                && evidence.validates(&subject)
                && !failures.is_empty()
                && valid_failure_scope(run, scope, evidence)
        }
        MemberVerifyOutcome::HostFault { evidence, .. } => {
            let subject = run.node.as_ref().map_or(request.member.candidate.tree, |node| node.candidate.tree);
            evidence.kind == EvidenceKind::ExecutorFault && evidence.validates(&subject)
        }
        MemberVerifyOutcome::Survived { node, .. } => {
            run.plan.mode == SharedRunMode::Contextual
                && run.node.as_ref().is_some_and(|current| current.digest() == *node)
        }
        MemberVerifyOutcome::Pending { .. } => true,
    }
}

fn queue_input(state: &mut CoordinationState, input: CompositionInput) {
    if input.members.iter().any(|pin| !state.integration.head.coverage.contains(pin))
        && !state.integration.queued.iter().any(|queued| queued.digest() == input.digest())
    {
        state.integration.queued.push(input);
    }
}

fn contextual_member_repair(
    record: &BloomRecord,
    bloom: BloomId,
    pin: &MemberPin,
    failed: VerifyFailureSet,
    evidence: Digest,
) -> Option<Vec<Decision>> {
    let member = record
        .spec
        .members()
        .iter()
        .find(|member| member.workpiece == pin.workpiece && member.scope_revision == pin.scope_revision)?;
    let cursor = record.progress.get(&pin.workpiece).copied()?;
    if cursor.stage != StageId::Verify || cursor.candidate != Some(pin.candidate) {
        return None;
    }
    let failed = record.pipeline_manifest.intern_set(failed);
    let seen = record.pipeline_manifest.intern_set(cursor.seen_verify_failures);
    let repeated = failed.intersection(seen);
    let seen_verify_failures = seen.union(failed);
    let rolls = cursor.repair_rolls + u32::from(!repeated.is_empty());
    if !repeated.is_empty() && rolls >= record.stage_catalog.retry_budget_of(StageId::Verify).unwrap_or(1) {
        return Some(alloc::vec![
            Decision::AdvanceStage {
                bloom,
                workpiece: pin.workpiece.clone(),
                progress: StageProgress { repair_rolls: rolls, seen_verify_failures, ..cursor },
            },
            Decision::RecordWedge {
                bloom,
                workpiece: pin.workpiece.clone(),
                wedge: Wedge { stage: StageId::Verify, evidence, repeated_verifiers: repeated },
            },
        ]);
    }
    let progress = StageProgress {
        stage: StageId::Refine,
        attempts: 1,
        repair_rolls: rolls,
        seen_verify_failures,
        fold_conflict_evidence: None,
        reconcile_assembles_base: false,
        ..cursor
    };
    Some(
        move_effects_with_candidate(
            bloom,
            &pin.workpiece,
            member.scope_revision,
            &progress,
            DispatchTargets { subject: pin.candidate.tree, checkout: pin.candidate.checkout },
            Some(pin.candidate.tree),
            SealedLine::of(record, member),
        )
        .into(),
    )
}

pub(super) fn reduce_coordination_resolve(
    record: &BloomRecord,
    bloom: BloomId,
    tree: Digest,
    head: Digest,
    lineage: &[Digest],
) -> Decisions {
    let roll = record.aggregate_verify_rolls.saturating_add(1);
    let pending = || record.holds.iter().next().copied();
    EventBoundary::new(AGGREGATE_VERIFY_GATE, bloom)
        .require(
            "bloom_sealed",
            || record.status == BloomStatus::Sealed,
            || reads![status: format!("{:?}", record.status), required: "Sealed"],
            || Outcome::ResolveRejected(super::ResolveError::UnknownOrInactiveBloom),
        )
        .require(
            "no_open_question",
            || pending().is_none(),
            || reads![questions: record.holds.len()],
            || {
                pending().map_or(Outcome::ResolveRejected(super::ResolveError::UnknownOrInactiveBloom), |question| {
                    Outcome::ResolveRejected(super::ResolveError::PendingDecision { question })
                })
            },
        )
        .require(
            "under_verify_budget",
            || !at_park_ceiling(record, StageId::AggregateVerify, roll),
            || reads![roll: roll, spent: record.aggregate_verify_rolls],
            || Outcome::ResolveRejected(super::ResolveError::ReviewCeiling { rolls: record.aggregate_verify_rolls }),
        )
        .decide(|| coordination_resolve_after_boundary(record, bloom, tree, head, lineage, roll))
}

fn coordination_resolve_after_boundary(
    record: &BloomRecord,
    bloom: BloomId,
    tree: Digest,
    head: Digest,
    lineage: &[Digest],
    roll: u32,
) -> Decisions {
    let state = record.coordination.as_deref().expect("coordination resolve is opted in");
    let active = record
        .spec
        .members()
        .iter()
        .filter(|member| !record.withdrawn.contains_key(&member.workpiece))
        .collect::<Vec<_>>();
    if !state.uses_selected_root() {
        let expected_lineage = active
            .iter()
            .map(|member| {
                state
                    .claims
                    .get(&member.workpiece.0)
                    .filter(|claim| state.has_exact_claim(&claim.member))
                    .map(|claim| claim.member.candidate.tree)
            })
            .collect::<Option<Vec<_>>>();
        if record.status != BloomStatus::Sealed
            || !state.final_in_flight
            || expected_lineage.as_deref() != Some(lineage)
        {
            return rejected(CoordinationError::NotReady);
        }
        return super::integrate::folded(record, bloom, tree, head, lineage, roll);
    }
    let selected = &state.integration.head;
    if record.status != BloomStatus::Sealed
        || state.integration.in_flight.is_some()
        || !state.integration.queued.is_empty()
        || state.integration.known_red == Some(selected.node)
        || selected.candidate != (CandidateRef { tree, checkout: head })
        || selected.generation != state.integration.generation.digest()
        || selected.coverage.len() != active.len()
        || active.iter().any(|member| {
            selected.coverage.iter().filter(|pin| pin.workpiece == member.workpiece).count() != 1
                || selected
                    .coverage
                    .iter()
                    .find(|pin| pin.workpiece == member.workpiece)
                    .is_none_or(|pin| pin.scope_revision != member.scope_revision || !state.has_exact_claim(pin))
        })
        || lineage != state.integration.admitted.iter().map(|input| input.candidate.tree).collect::<Vec<_>>()
    {
        return rejected(CoordinationError::NotReady);
    }
    if let Some(evidence) = state.contextual_aggregate_proof(selected) {
        return super::integrate::folded_with_contextual_proof(record, bloom, tree, head, lineage, roll, evidence);
    }
    super::integrate::folded(record, bloom, tree, head, lineage, roll)
}

struct SharedCompletionContext<'a> {
    snapshot: &'a Snapshot,
    record: &'a BloomRecord,
    bloom: BloomId,
    run: &'a SharedRunRecord,
    attributed: &'a BTreeSet<WorkpieceId>,
}

fn apply_attributed_failure(
    context: &SharedCompletionContext<'_>,
    state: &mut CoordinationState,
    members: &[MemberPin],
    failures: VerifyFailureSet,
    evidence: Digest,
    effects: &mut Vec<Decision>,
) -> Result<(), CoordinationError> {
    for member in members {
        state.claims.remove(workpiece_key(&member.workpiece));
        let Some(repair) = contextual_member_repair(context.record, context.bloom, member, failures, evidence) else {
            return Err(CoordinationError::InvalidPlan);
        };
        effects.extend(repair);
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct FailedOutcome<'a> {
    scope: &'a FailureScope,
    failures: VerifyFailureSet,
    evidence: &'a Evidence,
}

fn apply_failed_outcome(
    context: &SharedCompletionContext<'_>,
    state: &mut CoordinationState,
    request: &MemberVerifyRequest,
    failure: FailedOutcome<'_>,
    polluted: &mut BTreeSet<WorkpieceId>,
    effects: &mut Vec<Decision>,
) -> Result<(), CoordinationError> {
    let FailedOutcome { scope, failures, evidence } = failure;
    state.diagnostics.push(CoordinationDiagnostic { subject: request.digest(), scope: scope.clone() });
    state.claims.remove(workpiece_key(&request.member.workpiece));
    effects.push(Decision::RecordEvidence { bloom: context.bloom, evidence: evidence.clone() });
    match scope {
        FailureScope::Attributed { members, .. } => {
            apply_attributed_failure(context, state, members, failures, evidence.detail, effects)?;
        }
        FailureScope::Inherited { head, .. } => {
            state.integration.known_red = Some(*head);
            let blocked = state
                .requests
                .iter()
                .filter(|candidate| {
                    candidate.context.as_ref().is_some_and(|context| context.starting_head.node == *head)
                })
                .map(|candidate| candidate.member.workpiece.clone())
                .collect::<BTreeSet<_>>();
            state.claims.retain(|workpiece, _| !blocked.iter().any(|blocked| workpiece == workpiece_key(blocked)));
        }
        FailureScope::Interaction { members, .. } if !context.attributed.is_empty() => {
            polluted.extend(
                members
                    .iter()
                    .filter(|member| !context.attributed.contains(&member.workpiece))
                    .map(|member| member.workpiece.clone()),
            );
        }
        FailureScope::Interaction { .. } | FailureScope::Unattributed { .. } => {}
    }
    Ok(())
}

fn queue_run_retry(
    state: &mut CoordinationState,
    run: &SharedRunRecord,
    request: &MemberVerifyRequest,
    effects: &mut Vec<Decision>,
) {
    if run.plan.mode == SharedRunMode::Contextual {
        serial_fallbacks(
            state,
            &SharedRunPlan {
                mode: SharedRunMode::Contextual,
                requests: alloc::vec![request.clone()],
                composition: run.plan.composition.clone(),
                probe_budget: run.plan.probe_budget,
                execution_attempt: run.plan.execution_attempt,
            },
            effects,
        );
    } else {
        effects.push(Decision::QueueMemberVerification { request: request.clone() });
    }
}

fn apply_shared_host_fault(
    context: &SharedCompletionContext<'_>,
    state: &mut CoordinationState,
    request: &MemberVerifyRequest,
    evidence: &Evidence,
    effects: &mut Vec<Decision>,
) {
    let retry_stage = if context.run.plan.mode == SharedRunMode::Contextual {
        StageId::AggregateVerify
    } else {
        StageId::Verify
    };
    let retry_budget = stage_binding(&context.record.stage_catalog, retry_stage).retry_budget;
    if context.run.plan.execution_attempt.saturating_add(1) < retry_budget {
        effects.push(Decision::QueueMemberVerification { request: request.clone() });
        return;
    }
    state.diagnostics.push(CoordinationDiagnostic {
        subject: request.digest(),
        scope: FailureScope::Unattributed { evidence: evidence.detail },
    });
    hold_coordination(
        context.bloom,
        format!(
            "shared verification host-fault budget exhausted after {} attempts",
            context.run.plan.execution_attempt.saturating_add(1)
        ),
        effects,
    );
}

fn apply_run_outcome(
    context: &SharedCompletionContext<'_>,
    state: &mut CoordinationState,
    outcome: &MemberVerifyOutcome,
    polluted: &mut BTreeSet<WorkpieceId>,
    effects: &mut Vec<Decision>,
) -> Result<(), CoordinationError> {
    let Some(request) = context.run.plan.requests.iter().find(|request| request.digest() == outcome.request()) else {
        return Err(CoordinationError::RequestMismatch { request: outcome.request() });
    };
    if !exact_request(context.record, state, request) {
        return Err(CoordinationError::MemberVersionMismatch { workpiece: request.member.workpiece.clone() });
    }
    match outcome {
        MemberVerifyOutcome::PassedStandalone { proof, .. } => {
            state.claims.insert(
                request.member.workpiece.0.clone(),
                ContextualResolutionClaim {
                    member: request.member.clone(),
                    proof: ResolutionProof::Standalone(proof.clone()),
                },
            );
            if !state.policy.eager_integration {
                let claim = ResolutionClaim {
                    workpiece: request.member.workpiece.clone(),
                    scope_revision: request.member.scope_revision,
                    candidate: request.member.candidate.tree,
                    evidence: proof.evidence.clone(),
                };
                effects.extend(
                    super::integrate::claim_effects(context.snapshot, context.record, context.bloom, &claim, None)
                        .into_iter()
                        .filter(|effect| !matches!(effect, Decision::DispatchIntegration { .. })),
                );
            }
            queue_input(state, request.input.clone());
        }
        MemberVerifyOutcome::PassedIn { receipt, .. } => {
            let node = context.run.node.as_ref().expect("validated contextual node");
            let contract = context.run.plan.composition.as_ref().expect("validated composition").contract.digest();
            state.claims.insert(
                request.member.workpiece.0.clone(),
                ContextualResolutionClaim {
                    member: request.member.clone(),
                    proof: ResolutionProof::InComposition {
                        node: node.digest(),
                        receipt: receipt.clone(),
                        plan: context.run.plan.digest(),
                        request: request.digest(),
                        contract,
                    },
                },
            );
        }
        MemberVerifyOutcome::Failed { scope, failures, evidence, .. } => {
            apply_failed_outcome(
                context,
                state,
                request,
                FailedOutcome { scope, failures: *failures, evidence },
                polluted,
                effects,
            )?;
        }
        MemberVerifyOutcome::HostFault { evidence, .. } => {
            apply_shared_host_fault(context, state, request, evidence, effects);
        }
        MemberVerifyOutcome::Survived { .. } => {
            effects.push(Decision::QueueMemberVerification { request: request.clone() });
        }
        MemberVerifyOutcome::Pending { .. } => queue_run_retry(state, context.run, request, effects),
    }
    Ok(())
}

fn validate_shared_completion(
    record: &BloomRecord,
    current: &SharedRunRecord,
    completion: &SharedRunCompletion,
) -> Result<BTreeSet<WorkpieceId>, CoordinationError> {
    if !current.is_running()
        || current.physical_run != Some(completion.run)
        || !validate_latency(&current.plan, &completion.latencies)
    {
        return Err(CoordinationError::RunMismatch {
            expected: current.physical_run.unwrap_or_default(),
            got: completion.run,
        });
    }
    let outstanding = current.unfinished.iter().copied().collect::<BTreeSet<_>>();
    let mut observed = BTreeSet::new();
    if completion
        .outcomes
        .iter()
        .any(|outcome| !outstanding.contains(&outcome.request()) || !observed.insert(outcome.request()))
    {
        return Err(CoordinationError::InvalidPlan);
    }
    let mut attributed = BTreeSet::new();
    if completion.outcomes.iter().any(|outcome| {
        matches!(outcome, MemberVerifyOutcome::Failed {
            scope: FailureScope::Attributed { members, .. },
            ..
        } if members.iter().any(|member| !attributed.insert(member.workpiece.clone())))
    }) {
        return Err(CoordinationError::InvalidPlan);
    }
    let positions = completion
        .outcomes
        .iter()
        .filter_map(|outcome| current.plan.requests.iter().position(|request| request.digest() == outcome.request()))
        .collect::<Vec<_>>();
    let expected_unfinished = outstanding.difference(&observed).copied().collect::<BTreeSet<_>>();
    if positions.windows(2).any(|positions| positions[0] >= positions[1])
        || completion.unfinished.iter().copied().collect::<BTreeSet<_>>() != expected_unfinished
        || completion.unfinished.len() != expected_unfinished.len()
        || completion.latencies.iter().map(|latency| latency.request).collect::<BTreeSet<_>>() != observed
    {
        return Err(CoordinationError::InvalidPlan);
    }
    if completion.outcomes.iter().any(|outcome| {
        current
            .plan
            .requests
            .iter()
            .find(|request| request.digest() == outcome.request())
            .is_none_or(|request| !validates_immutable_run_outcome(current, request, outcome))
    }) {
        return Err(CoordinationError::InvalidEvidenceKind);
    }
    let blocked = blocked_members(record, current, &completion.outcomes);
    if completion.outcomes.iter().any(|outcome| {
        matches!(outcome, MemberVerifyOutcome::Survived { request, .. } if current
            .plan
            .requests
            .iter()
            .find(|candidate| candidate.digest() == *request)
            .is_none_or(|candidate| blocked.contains(&candidate.member.workpiece)))
    }) || completion.outcomes.iter().any(|outcome| matches!(outcome, MemberVerifyOutcome::PassedIn { .. }))
        && (!completion.unfinished.is_empty()
            || completion.outcomes.len() != outstanding.len()
            || completion.outcomes.iter().any(|outcome| !matches!(outcome, MemberVerifyOutcome::PassedIn { .. })))
    {
        return Err(CoordinationError::InvalidPlan);
    }
    Ok(attributed)
}

fn retain_contextual_success(state: &mut CoordinationState, run: &SharedRunRecord, completion: &SharedRunCompletion) {
    if run.plan.mode != SharedRunMode::Contextual
        || !completion.outcomes.iter().all(|outcome| matches!(outcome, MemberVerifyOutcome::PassedIn { .. }))
        || !completion.unfinished.is_empty()
    {
        return;
    }
    let Some(node) = run.node.as_ref() else {
        return;
    };
    if state.integration.known_red == Some(state.integration.head.node)
        && node.candidate == state.integration.head.candidate
        && node.coverage == state.integration.head.coverage
    {
        state.integration.known_red = None;
        state.integration.unproved_repair = None;
    }
    queue_input(
        state,
        CompositionInput { node: node.digest(), candidate: node.candidate, members: node.coverage.clone() },
    );
}

fn retain_survivors_and_schedule_repair(
    record: &BloomRecord,
    state: &mut CoordinationState,
    run: &SharedRunRecord,
    completion: &SharedRunCompletion,
    polluted: &BTreeSet<WorkpieceId>,
    effects: &mut Vec<Decision>,
) {
    let survivors = completion
        .outcomes
        .iter()
        .filter_map(|outcome| match outcome {
            MemberVerifyOutcome::Survived { request, .. } => Some(*request),
            _ => None,
        })
        .collect::<Vec<_>>();
    if let Some(node) = run.node.as_ref().filter(|_| !survivors.is_empty()) {
        state.survivor_groups.push(SurvivorGroup {
            source_plan: run.plan.digest(),
            source_node: node.digest(),
            requests: survivors,
        });
    }
    let polluted_requests = run
        .plan
        .requests
        .iter()
        .filter(|request| polluted.contains(&request.member.workpiece))
        .map(MemberVerifyRequest::digest)
        .collect::<Vec<_>>();
    if let Some(node) = run.node.as_ref().filter(|_| !polluted_requests.is_empty()) {
        for request in run.plan.requests.iter().filter(|request| polluted.contains(&request.member.workpiece)) {
            effects.push(Decision::QueueMemberVerification { request: request.clone() });
        }
        state.survivor_groups.push(SurvivorGroup {
            source_plan: run.plan.digest(),
            source_node: node.digest(),
            requests: polluted_requests,
        });
        return;
    }
    if let Some(evidence) = completion.outcomes.iter().find_map(|outcome| match outcome {
        MemberVerifyOutcome::Failed { scope: FailureScope::Interaction { evidence, .. }, .. } => Some(*evidence),
        _ => None,
    }) {
        schedule_shared_node_repair(record, state, run, evidence, effects);
    } else if let Some(evidence) = completion.outcomes.iter().find_map(|outcome| match outcome {
        MemberVerifyOutcome::Failed { scope: FailureScope::Inherited { head, evidence }, .. }
            if *head == state.integration.head.node =>
        {
            Some(*evidence)
        }
        _ => None,
    }) {
        schedule_partial_head_repair(record, state, evidence, effects);
    }
}

fn settle_run_record(run: &mut SharedRunRecord, completion: &SharedRunCompletion) {
    run.phase = SharedRunPhase::Terminal;
    run.completed.extend(completion.outcomes.clone());
    run.unfinished.clone_from(&completion.unfinished);
    run.latencies.extend(completion.latencies.clone());
}

pub(super) fn reduce_shared_run_completed(
    snapshot: &Snapshot,
    bloom: &BloomId,
    completion: &SharedRunCompletion,
) -> Decisions {
    let (record, state) = match active_state(snapshot, bloom) {
        Ok(active) => active,
        Err(error) => return rejected(error),
    };
    let Some(index) = state.runs.iter().position(|run| run.plan.digest() == completion.plan) else {
        return rejected(CoordinationError::PlanMismatch { expected: Digest::default(), got: completion.plan });
    };
    let current = &state.runs[index];
    let attributed = match validate_shared_completion(record, current, completion) {
        Ok(attributed) => attributed,
        Err(error) => return rejected(error),
    };
    if current.stale {
        let mut next = state.clone();
        let mut effects = Vec::new();
        let run_snapshot = next.runs[index].clone();
        let context =
            SharedCompletionContext { snapshot, record, bloom: *bloom, run: &run_snapshot, attributed: &attributed };
        for request in &run_snapshot.plan.requests {
            if !next.requests.iter().any(|current| current.digest() == request.digest())
                || !exact_request(record, &next, request)
            {
                continue;
            }
            if run_snapshot.plan.mode == SharedRunMode::Contextual {
                effects.push(Decision::QueueMemberVerification { request: request.clone() });
                continue;
            }
            match completion.outcomes.iter().find(|outcome| outcome.request() == request.digest()) {
                Some(outcome @ MemberVerifyOutcome::PassedStandalone { .. }) => {
                    let mut polluted = BTreeSet::new();
                    if let Err(error) = apply_run_outcome(&context, &mut next, outcome, &mut polluted, &mut effects) {
                        return rejected(error);
                    }
                }
                Some(_) | None => queue_run_retry(&mut next, &run_snapshot, request, &mut effects),
            }
        }
        settle_run_record(&mut next.runs[index], completion);
        schedule_append(record, &mut next, &mut effects);
        final_fold_if_ready(record, &mut next, &mut effects);
        finalize_selected_root(record, &mut next, &mut effects);
        effects.insert(0, record_state(*bloom, next));
        return accepted(*bloom, completion.run, effects);
    }
    let mut next = state.clone();
    let mut effects = Vec::new();
    let run_snapshot = next.runs[index].clone();
    let run_requests = run_snapshot.plan.requests.iter().map(MemberVerifyRequest::digest).collect::<Vec<_>>();
    let mut polluted_interaction_members = BTreeSet::new();
    next.survivor_groups.retain(|group| group.requests != run_requests);
    let context =
        SharedCompletionContext { snapshot, record, bloom: *bloom, run: &run_snapshot, attributed: &attributed };
    for outcome in &completion.outcomes {
        if let Err(error) =
            apply_run_outcome(&context, &mut next, outcome, &mut polluted_interaction_members, &mut effects)
        {
            return rejected(error);
        }
    }
    retain_contextual_success(&mut next, &run_snapshot, completion);
    for request in &completion.unfinished {
        if let Some(request) = run_snapshot.plan.requests.iter().find(|candidate| candidate.digest() == *request) {
            queue_run_retry(&mut next, &run_snapshot, request, &mut effects);
        }
    }
    retain_survivors_and_schedule_repair(
        record,
        &mut next,
        &run_snapshot,
        completion,
        &polluted_interaction_members,
        &mut effects,
    );
    settle_run_record(&mut next.runs[index], completion);
    schedule_append(record, &mut next, &mut effects);
    final_fold_if_ready(record, &mut next, &mut effects);
    finalize_selected_root(record, &mut next, &mut effects);
    effects.insert(0, record_state(*bloom, next));
    accepted(*bloom, completion.run, effects)
}

pub(super) fn reduce_reservation_expired(
    snapshot: &Snapshot,
    bloom: &BloomId,
    reservation: &StableHeadReservation,
    observed_at_unix_millis: u64,
) -> Decisions {
    let (record, state) = match active_state(snapshot, bloom) {
        Ok(active) => active,
        Err(error) => return rejected(error),
    };
    if state.integration.reservation.as_ref() != Some(reservation) {
        return rejected(CoordinationError::ReservationMismatch);
    }
    if observed_at_unix_millis < reservation.deadline_unix_millis {
        return rejected(CoordinationError::ReservationNotExpired);
    }
    let mut next = state.clone();
    next.integration.reservation = None;
    next.integration.movement_count = 0;
    let mut effects = Vec::new();
    schedule_append(record, &mut next, &mut effects);
    finalize_selected_root(record, &mut next, &mut effects);
    effects.insert(0, record_state(*bloom, next));
    accepted(*bloom, reservation.node, effects)
}

pub(super) fn reduce_checkpoint_observed(snapshot: &Snapshot, checkpoint: &ConstructionCheckpoint) -> Decisions {
    let (record, state) = match active_state(snapshot, &checkpoint.bloom) {
        Ok(active) => active,
        Err(error) => return rejected(error),
    };
    let Some(member) =
        record.spec.members().iter().find(|member| {
            member.workpiece == checkpoint.workpiece && member.scope_revision == checkpoint.scope_revision
        })
    else {
        return rejected(CoordinationError::MemberVersionMismatch { workpiece: checkpoint.workpiece.clone() });
    };
    let Some(admission) = state.admitted_construction.get(workpiece_key(&member.workpiece)) else {
        return rejected(CoordinationError::NotReady);
    };
    if admission.nonce != checkpoint.nonce
        || admission.dispatch.context.starting_head.candidate.checkout != checkpoint.starting_checkout
        || admission.dispatch.scope_revision != checkpoint.scope_revision
        || record.progress.get(&member.workpiece).is_none_or(|progress| progress.stage != StageId::Construct)
        || state
            .checkpoints
            .get(workpiece_key(&member.workpiece))
            .is_some_and(|current| current.observation >= checkpoint.observation)
    {
        return rejected(CoordinationError::NotReady);
    }
    let mut next = state.clone();
    next.checkpoints.insert(member.workpiece.0.clone(), checkpoint.clone());
    let mut compatible = next
        .checkpoints
        .values()
        .filter(|checkpoint| {
            next.admitted_construction
                .get(workpiece_key(&checkpoint.workpiece))
                .is_some_and(|admission| admission.nonce == checkpoint.nonce)
                && record
                    .progress
                    .get(&checkpoint.workpiece)
                    .is_some_and(|progress| progress.stage == StageId::Construct)
        })
        .cloned()
        .collect::<Vec<_>>();
    compatible.sort_by_key(|candidate| {
        record.spec.members().iter().position(|member| member.workpiece == candidate.workpiece).unwrap_or(usize::MAX)
    });
    let mut effects = Vec::new();
    if compatible.len() >= 2 {
        let plan = CompatibilityPreviewPlan {
            bloom: checkpoint.bloom,
            generation: next.integration.generation.digest(),
            base: next.integration.head.candidate,
            checkpoints: compatible,
        };
        if !next.preview_plans.iter().any(|current| current.digest() == plan.digest()) {
            next.preview_plans.push(plan.clone());
            effects.push(Decision::DispatchCompatibilityPreview { plan });
        }
    }
    effects.insert(0, record_state(checkpoint.bloom, next));
    accepted(checkpoint.bloom, checkpoint.digest(), effects)
}

pub(super) fn reduce_request_construction_admission(
    snapshot: &Snapshot,
    admission: &ConstructionAdmission,
) -> Decisions {
    let bloom = admission.dispatch.bloom;
    let (record, state) = match active_state(snapshot, &bloom) {
        Ok(active) => active,
        Err(error) => return rejected(error),
    };
    let workpiece = &admission.dispatch.workpiece;
    let Some(queued) = state.queued_construction.get(workpiece_key(workpiece)) else {
        return rejected(CoordinationError::NotReady);
    };
    if state.integration.known_red == Some(state.integration.head.node) {
        // Every other head-pinning seam refuses while the selected head is red;
        // admitting here would dispatch an author order onto unproven context.
        return rejected(CoordinationError::NotReady);
    }
    if queued != &admission.dispatch
        || admission.dispatch.stage != StageId::Construct
        || admission.dispatch.context.starting_head != state.integration.head
        || admission.dispatch.context.bloom_base != state.integration.generation.base
        || admission.dispatch.transformation.checkout != state.integration.head.candidate.checkout
        || state.admitted_construction.contains_key(workpiece_key(workpiece))
        || record.progress.get(workpiece).is_none_or(|progress| {
            progress.stage != StageId::Construct || progress.attempts != admission.dispatch.attempt
        })
        || record
            .spec
            .members()
            .iter()
            .all(|member| member.workpiece != *workpiece || member.scope_revision != admission.dispatch.scope_revision)
    {
        return rejected(CoordinationError::InvalidPlan);
    }
    let mut next = state.clone();
    next.queued_construction.remove(workpiece_key(workpiece));
    next.admitted_construction.insert(workpiece.0.clone(), admission.clone());
    accepted(
        bloom,
        admission.digest(),
        alloc::vec![
            record_state(bloom, next),
            Decision::DispatchContextualAttempt { dispatch: admission.dispatch.clone() },
        ],
    )
}

pub(super) fn reduce_compatibility_previewed(
    snapshot: &Snapshot,
    bloom: &BloomId,
    plan: Digest,
    result: &CompatibilityPreview,
) -> Decisions {
    let (_, state) = match active_state(snapshot, bloom) {
        Ok(active) => active,
        Err(error) => return rejected(error),
    };
    let Some(expected) = state.preview_plans.iter().find(|candidate| candidate.digest() == plan) else {
        return rejected(CoordinationError::PlanMismatch { expected: Digest::default(), got: plan });
    };
    if expected.bloom != *bloom || expected.generation != state.integration.generation.digest() {
        return rejected(CoordinationError::GenerationMismatch {
            expected: state.integration.generation.digest(),
            got: expected.generation,
        });
    }
    if expected.checkpoints.iter().any(|checkpoint| {
        state.checkpoints.get(workpiece_key(&checkpoint.workpiece)).is_none_or(|current| current != checkpoint)
    }) {
        return rejected(CoordinationError::InvalidPlan);
    }
    let mut next = state.clone();
    next.previews.retain(|recorded| recorded.plan != plan);
    next.previews.push(CompatibilityPreviewRecord { plan, result: result.clone() });
    accepted(*bloom, plan, alloc::vec![record_state(*bloom, next)])
}

fn coordination_state_mut<'a>(
    snapshot: &Snapshot,
    bloom: BloomId,
    states: &'a mut BTreeMap<BloomId, CoordinationState>,
) -> Option<&'a mut CoordinationState> {
    match states.entry(bloom) {
        Entry::Occupied(entry) => Some(entry.into_mut()),
        Entry::Vacant(entry) => snapshot
            .blooms
            .get(&bloom)
            .and_then(|record| record.coordination.as_deref())
            .map(|state| entry.insert(state.clone())),
    }
}

fn replace_construct_dispatch(
    snapshot: &Snapshot,
    advances: &BTreeMap<(BloomId, WorkpieceId), StageProgress>,
    states: &mut BTreeMap<BloomId, CoordinationState>,
    effect: Decision,
    output: &mut Vec<Decision>,
) {
    let Decision::DispatchAttempt {
        bloom,
        workpiece,
        stage: StageId::Construct,
        mut transformation,
        scope_revision,
        candidate,
        profile,
        configs,
    } = effect
    else {
        unreachable!("construct replacement receives a construct dispatch")
    };
    let Some(state) = coordination_state_mut(snapshot, bloom, states) else {
        output.push(Decision::DispatchAttempt {
            bloom,
            workpiece,
            stage: StageId::Construct,
            transformation,
            scope_revision,
            candidate,
            profile,
            configs,
        });
        return;
    };
    if state.admitted_construction.contains_key(workpiece_key(&workpiece)) {
        output.push(Decision::DispatchAttempt {
            bloom,
            workpiece,
            stage: StageId::Construct,
            transformation,
            scope_revision,
            candidate,
            profile,
            configs,
        });
        return;
    }
    let context = ConstructContext {
        bloom_base: state.integration.generation.base,
        starting_head: state.integration.head.clone(),
    };
    transformation.checkout = context.starting_head.candidate.checkout;
    let dispatch = ContextualAttemptDispatch {
        bloom,
        workpiece: workpiece.clone(),
        stage: StageId::Construct,
        attempt: advances.get(&(bloom, workpiece.clone())).map_or(1, |progress| progress.attempts),
        transformation,
        scope_revision,
        candidate,
        profile,
        configs,
        context: context.clone(),
    };
    state.contexts.insert(workpiece.0.clone(), context);
    state.queued_construction.insert(workpiece.0, dispatch.clone());
    output.push(Decision::QueueConstructionAdmission { dispatch });
}

fn replace_verify_dispatch(
    snapshot: &Snapshot,
    advances: &BTreeMap<(BloomId, WorkpieceId), StageProgress>,
    states: &mut BTreeMap<BloomId, CoordinationState>,
    effect: Decision,
    output: &mut Vec<Decision>,
) -> Result<(), CoordinationError> {
    let Decision::DispatchAttempt { bloom, ref workpiece, stage: StageId::Verify, .. } = effect else {
        unreachable!("verify replacement receives a verify dispatch")
    };
    let Some(record) = snapshot.blooms.get(&bloom) else {
        output.push(effect);
        return Ok(());
    };
    let Some(base_state) = record.coordination.as_deref() else {
        output.push(effect);
        return Ok(());
    };
    let Decision::DispatchAttempt { transformation, profile, configs, .. } = &effect else {
        unreachable!()
    };
    let state = states.entry(bloom).or_insert_with(|| base_state.clone());
    let Some(progress) =
        advances.get(&(bloom, workpiece.clone())).copied().or_else(|| record.progress.get(workpiece).copied())
    else {
        output.push(effect);
        return Ok(());
    };
    if let Some(candidate) = progress.candidate {
        let prior = state
            .requests
            .iter()
            .rev()
            .find(|request| request.member.workpiece == *workpiece)
            .map(|request| request.member.candidate)
            .or_else(|| state.claims.get(workpiece_key(workpiece)).map(|claim| claim.member.candidate))
            .or_else(|| {
                state.integration.head.coverage.iter().find(|pin| pin.workpiece == *workpiece).map(|pin| pin.candidate)
            });
        if prior.is_some_and(|current| current != candidate) {
            invalidate_member_version(record, state, workpiece, output)?;
            state.contexts.insert(
                workpiece.0.clone(),
                ConstructContext {
                    bloom_base: state.integration.generation.base,
                    starting_head: state.integration.head.clone(),
                },
            );
        }
    }
    if record.progress.get(workpiece).is_some_and(|current| current.stage == StageId::Reconcile)
        && let (Some(authored), Some(context), Some(member)) = (
            progress.candidate,
            state.contexts.get(workpiece_key(workpiece)).cloned(),
            record.spec.members().iter().find(|member| member.workpiece == *workpiece),
        )
    {
        let plan = CandidatePreparationPlan {
            bloom,
            workpiece: workpiece.clone(),
            scope_revision: member.scope_revision,
            authored,
            context,
        };
        if !state.preparations.iter().any(|pending| pending.digest() == plan.digest()) {
            state.preparations.push(plan.clone());
            output.push(Decision::DispatchCandidatePreparation { plan });
        }
        return Ok(());
    }
    if let Some(request) = request_for_dispatch(
        snapshot,
        record,
        state,
        VerifyDispatch { workpiece, transformation, profile, configs, progress: &progress },
    ) && !state.requests.iter().any(|current| current.digest() == request.digest())
    {
        state.requests.push(request.clone());
        output.push(Decision::QueueMemberVerification { request });
    } else {
        output.push(effect);
    }
    Ok(())
}

fn pristine_initial_generation(state: &CoordinationState) -> bool {
    let generation = state.integration.generation.digest();
    state.integration.generation.epoch == 0
        && state.integration.head.generation == generation
        && state.integration.head.node == generation
        && state.integration.head.candidate == state.integration.generation.base
        && state.integration.head.coverage.is_empty()
        && state.integration.queued.is_empty()
        && state.integration.admitted.is_empty()
        && state.integration.in_flight.is_none()
        && state.integration.known_red.is_none()
        && state.integration.reservation.is_none()
        && state.integration.movement_count == 0
        && state.requests.is_empty()
        && state.runs.is_empty()
        && state.claims.is_empty()
        && state.prepared.is_empty()
        && state.preparations.is_empty()
        && state.contexts.is_empty()
        && state.checkpoints.is_empty()
        && state.queued_construction.is_empty()
        && state.admitted_construction.is_empty()
        && state.preview_plans.is_empty()
        && state.previews.is_empty()
        && state.survivor_groups.is_empty()
        && state.partial_head_repair.is_none()
        && state.partial_head_repair_attempts == 0
        && !state.final_in_flight
        && !state.final_dispatched
}

fn adopt_verified_bases(snapshot: &Snapshot, effects: &[Decision], states: &mut BTreeMap<BloomId, CoordinationState>) {
    for receipt in effects.iter().filter_map(|effect| match effect {
        Decision::RecordBaseReceipt { receipt } if receipt.is_green() => Some(receipt),
        _ => None,
    }) {
        for (bloom, record) in snapshot.blooms.iter().filter(|(_, record)| record.spec.base() == receipt.base) {
            let state = match states.entry(*bloom) {
                Entry::Occupied(entry) => entry.into_mut(),
                Entry::Vacant(entry) => {
                    let Some(initial) = record.coordination.as_deref() else {
                        continue;
                    };
                    if initial.integration.generation.base.checkout != receipt.base
                        || !pristine_initial_generation(initial)
                    {
                        continue;
                    }
                    entry.insert(initial.clone())
                }
            };
            if state.integration.generation.base.checkout != receipt.base || !pristine_initial_generation(state) {
                continue;
            }
            let base = CandidateRef { tree: receipt.tree, checkout: receipt.base };
            state.integration.generation.base = base;
            let generation = state.integration.generation.digest();
            state.integration.head = IntegrationHead {
                generation,
                node: generation,
                candidate: base,
                plan: Digest::default(),
                coverage: Vec::new(),
            };
        }
    }
}

fn replace_verify_dispatches(snapshot: &Snapshot, decisions: &mut Decisions) -> Result<(), CoordinationError> {
    let original = take(&mut decisions.effects);
    let mut output = Vec::with_capacity(original.len());
    let advances = original
        .iter()
        .filter_map(|effect| match effect {
            Decision::AdvanceStage { bloom, workpiece, progress } => Some(((*bloom, workpiece.clone()), *progress)),
            _ => None,
        })
        .collect::<BTreeMap<_, _>>();
    let verify_dispatches = original
        .iter()
        .filter_map(|effect| match effect {
            Decision::DispatchAttempt { bloom, workpiece, stage: StageId::Verify, .. } => {
                Some((*bloom, workpiece.clone()))
            }
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    let mut states = original
        .iter()
        .filter_map(|effect| match effect {
            Decision::RecordCoordinationState { bloom, state: Some(state) } => Some((*bloom, state.clone())),
            _ => None,
        })
        .collect::<BTreeMap<BloomId, CoordinationState>>();
    adopt_verified_bases(snapshot, &original, &mut states);
    for effect in original {
        match effect {
            Decision::RecordCoordinationState { state: Some(_), .. } => {}
            effect @ Decision::DispatchAttempt { stage: StageId::Construct, .. } => {
                replace_construct_dispatch(snapshot, &advances, &mut states, effect, &mut output);
            }
            Decision::AdvanceStage { bloom, ref workpiece, .. }
                if verify_dispatches.contains(&(bloom, workpiece.clone()))
                    && snapshot.blooms.get(&bloom).and_then(|record| record.coordination.as_deref()).is_some_and(
                        |state| {
                            state.contexts.contains_key(workpiece_key(workpiece))
                                && snapshot
                                    .blooms
                                    .get(&bloom)
                                    .and_then(|record| record.progress.get(workpiece))
                                    .is_some_and(|progress| progress.stage == StageId::Reconcile)
                        },
                    ) => {}
            effect @ Decision::DispatchAttempt { stage: StageId::Verify, .. } => {
                replace_verify_dispatch(snapshot, &advances, &mut states, effect, &mut output)?;
            }
            other => output.push(other),
        }
    }
    for ((bloom, workpiece), progress) in advances {
        if progress.stage == StageId::Construct {
            continue;
        }
        let state = match states.entry(bloom) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => {
                let Some(state) = snapshot.blooms.get(&bloom).and_then(|record| record.coordination.as_deref()) else {
                    continue;
                };
                entry.insert(state.clone())
            }
        };
        state.queued_construction.remove(workpiece_key(&workpiece));
        state.admitted_construction.remove(workpiece_key(&workpiece));
    }
    output.extend(states.into_iter().map(|(bloom, state)| record_state(bloom, state)));
    decisions.effects = output;
    Ok(())
}

fn apply_member_invalidations(snapshot: &Snapshot, decisions: &mut Decisions) -> Result<(), CoordinationError> {
    let mut invalidated = BTreeMap::<BloomId, BTreeMap<WorkpieceId, bool>>::new();
    for effect in &decisions.effects {
        match effect {
            Decision::RevokeResolution { bloom, workpiece }
            | Decision::AdvanceStage { bloom, workpiece, progress: StageProgress { stage: StageId::Refine, .. } } => {
                invalidated.entry(*bloom).or_default().entry(workpiece.clone()).or_insert(false);
            }
            Decision::RecordWithdrawal { bloom, withdrawal } => {
                invalidated.entry(*bloom).or_default().insert(withdrawal.workpiece.clone(), true);
            }
            _ => {}
        }
    }
    let mut appended = Vec::new();
    for (bloom, members) in invalidated {
        let Some(record) = snapshot.blooms.get(&bloom) else {
            continue;
        };
        let position = decisions.effects.iter().rposition(|effect| {
            matches!(effect, Decision::RecordCoordinationState { bloom: owner, state: Some(_)} if *owner == bloom)
        });
        let mut state = match position.map(|position| decisions.effects.remove(position)) {
            Some(Decision::RecordCoordinationState { state: Some(state), .. }) => state,
            _ => match record.coordination.as_deref() {
                Some(state) => state.clone(),
                None => continue,
            },
        };
        let retired_partial = state.partial_head_repair.as_ref().map(PartialHeadRepairPlan::digest);
        for (workpiece, withdrawn) in members {
            if withdrawn {
                state.integration.generation.members.retain(|member| member.workpiece != workpiece);
            }
            invalidate_member_version(record, &mut state, &workpiece, &mut appended)?;
        }
        if let Some(retired) = retired_partial {
            decisions.effects.retain(|effect| {
                !matches!(effect, Decision::DispatchPartialHeadRepair { dispatch } if dispatch.plan.digest() == retired)
            });
        }
        let held_now = record.operator_hold.is_some()
            || appended
                .iter()
                .any(|effect| matches!(effect, Decision::RecordOperatorHold { bloom: owner, .. } if *owner == bloom));
        if !held_now {
            refresh_unadmitted_construction(record, &mut state, &mut appended);
            schedule_append(record, &mut state, &mut appended);
        }
        decisions.effects.push(record_state(bloom, state));
    }
    decisions.effects.extend(appended);
    Ok(())
}

/// Add coordination scheduling to ordinary reducer decisions without touching
/// blooms whose sealed policy is absent.
pub(super) fn schedule(snapshot: &Snapshot, mut decisions: Decisions) -> Decisions {
    if let Err(error) = replace_verify_dispatches(snapshot, &mut decisions)
        .and_then(|()| apply_member_invalidations(snapshot, &mut decisions))
    {
        return rejected(error);
    }
    let mut terminal = BTreeSet::new();
    for effect in &decisions.effects {
        match effect {
            Decision::MarkSuperseded { bloom, .. } | Decision::MarkBloomWithdrawn { bloom } => {
                terminal.insert(*bloom);
            }
            _ => {}
        }
    }
    for bloom in terminal {
        let Some(state) = snapshot.blooms.get(&bloom).and_then(|record| record.coordination.as_deref()) else {
            continue;
        };
        for run in state.runs.iter().filter(|run| !run.is_terminal()) {
            decisions.effects.push(Decision::CancelSharedRun { plan: run.plan.digest() });
        }
        decisions.effects.push(Decision::RecordCoordinationState { bloom, state: None });
    }
    decisions
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{digest, draft, membership};
    use crate::values::{
        ConfigRegistry, Harness, ReasoningEffort, ToolPolicy, VerifyProof, Withdrawal, WithdrawalCause,
    };
    use alloc::boxed::Box;
    use core::iter::once;
    use core::slice::from_ref;

    fn policy() -> CoordinationPolicy {
        CoordinationPolicy {
            verification: VerificationMode::Contextual,
            eager_integration: true,
            max_run_members: 4,
            max_serial_requests: 4,
            max_attribution_probes: 3,
            movement_budget: 2,
            reservation_millis: 1_000,
            host_class: String::from("test-host"),
        }
    }

    fn fixture() -> (BloomRecord, CoordinationState) {
        let spec = draft(1, alloc::vec![membership("alpha", 10), membership("beta", 11)]).seal();
        let record = BloomRecord::empty(spec.clone());
        let state = initialized_effects(
            &Snapshot::new(spec.base()),
            &spec,
            &record.stage_catalog,
            &record.pipeline_manifest,
            Some(policy()),
        )
        .expect("valid policy")
        .into_iter()
        .find_map(|effect| match effect {
            Decision::RecordCoordinationState { state: Some(state), .. } => Some(state),
            _ => None,
        })
        .expect("coordination state");
        (record, state)
    }

    fn pin(workpiece: &str, scope: u8, tree: u8) -> MemberPin {
        MemberPin {
            workpiece: WorkpieceId(String::from(workpiece)),
            scope_revision: digest(scope),
            candidate: CandidateRef { tree: digest(tree), checkout: digest(tree.saturating_add(1)) },
        }
    }

    /// Put every sealed member at Verify on its own captured candidate and
    /// return the exact requests that dispatch would mint for them.
    fn current_requests(record: &mut BloomRecord, state: &CoordinationState) -> Vec<MemberVerifyRequest> {
        let members = record.spec.members().to_vec();
        let mut requests = Vec::new();
        for (index, member) in members.iter().enumerate() {
            let tree = 20 + u8::try_from(index).expect("small fixture") * 2;
            let candidate = CandidateRef { tree: digest(tree), checkout: digest(tree + 1) };
            let progress = StageProgress {
                stage: StageId::Verify,
                attempts: 1,
                candidate: Some(candidate),
                repair_rolls: 0,
                seen_verify_failures: VerifyFailureSet::EMPTY,
                fold_checkpoint: None,
                fold_conflict_evidence: None,
                reconcile_assembles_base: false,
            };
            record.progress.insert(member.workpiece.clone(), progress);
            let binding = stage_binding(&record.stage_catalog, StageId::Verify);
            let transformation =
                Transformation::for_member_stage(&binding, candidate.tree, candidate.checkout, record.spec.base());
            requests.push(
                request_for_dispatch(
                    &Snapshot::new(record.spec.base()),
                    record,
                    state,
                    VerifyDispatch {
                        workpiece: &member.workpiece,
                        transformation: &transformation,
                        profile: &binding.profile,
                        configs: &member.configs.layered_over(record.spec.configs()),
                        progress: &progress,
                    },
                )
                .expect("current request"),
            );
        }
        requests
    }

    fn active_contextual_run() -> (Snapshot, BloomId, SharedRunPlan, SharedRunNode) {
        let (mut record, mut state) = fixture();
        let bloom = record.spec.id();
        let requests = current_requests(&mut record, &state);
        state.requests.clone_from(&requests);
        let composition = CompositionPlan {
            bloom,
            base: state.integration.head.clone(),
            inputs: requests.iter().map(|request| request.input.clone()).collect(),
            contract: state.composition_contract.bind(
                requests
                    .iter()
                    .map(|request| MemberContractPin { request: request.digest(), contract: request.contract.digest() })
                    .collect(),
            ),
            requests: requests.clone(),
        };
        let plan = SharedRunPlan {
            mode: SharedRunMode::Contextual,
            requests,
            composition: Some(composition),
            probe_budget: state.policy.max_attribution_probes,
            execution_attempt: 0,
        };
        let node = SharedRunNode {
            plan: plan.digest(),
            candidate: CandidateRef { tree: digest(60), checkout: digest(61) },
            coverage: plan.requests.iter().map(|request| request.member.clone()).collect(),
        };
        state.runs.push(SharedRunRecord {
            plan: plan.clone(),
            node: Some(node.clone()),
            phase: SharedRunPhase::Running,
            stale: false,
            physical_run: Some(digest(62)),
            completed: Vec::new(),
            unfinished: plan.requests.iter().map(MemberVerifyRequest::digest).collect(),
            latencies: Vec::new(),
        });
        record.coordination = Some(Box::new(state));
        let mut snapshot = Snapshot::new(record.spec.base());
        snapshot.blooms.insert(bloom, record);
        (snapshot, bloom, plan, node)
    }

    fn active_inherited_construction()
    -> (Snapshot, BloomId, WorkpieceId, WorkpieceId, ConstructContext, ConstructionAdmission) {
        let (mut record, mut state) = fixture();
        let bloom = record.spec.id();
        let author = record.spec.members()[0].clone();
        let inherited = record.spec.members()[1].clone();
        let inherited_candidate = CandidateRef { tree: digest(70), checkout: digest(71) };
        let inherited_pin = MemberPin {
            workpiece: inherited.workpiece.clone(),
            scope_revision: inherited.scope_revision,
            candidate: inherited_candidate,
        };
        state.integration.admitted.push(CompositionInput {
            node: digest(72),
            candidate: inherited_candidate,
            members: alloc::vec![inherited_pin.clone()],
        });
        let inherited_head = IntegrationHead {
            generation: state.integration.generation.digest(),
            node: digest(72),
            candidate: inherited_candidate,
            plan: digest(73),
            coverage: alloc::vec![inherited_pin],
        };
        state.integration.head = inherited_head.clone();
        let context =
            ConstructContext { bloom_base: state.integration.generation.base, starting_head: inherited_head.clone() };
        let binding = stage_binding(&record.stage_catalog, StageId::Construct);
        record.progress.insert(
            author.workpiece.clone(),
            StageProgress {
                stage: StageId::Construct,
                attempts: 1,
                candidate: None,
                repair_rolls: 0,
                seen_verify_failures: VerifyFailureSet::EMPTY,
                fold_checkpoint: None,
                fold_conflict_evidence: None,
                reconcile_assembles_base: false,
            },
        );
        let admission = ConstructionAdmission {
            nonce: digest(74),
            dispatch: ContextualAttemptDispatch {
                bloom,
                workpiece: author.workpiece.clone(),
                stage: StageId::Construct,
                attempt: 1,
                transformation: Transformation::for_member_stage(
                    &binding,
                    author.scope_revision,
                    inherited_head.candidate.checkout,
                    inherited_head.candidate.checkout,
                ),
                scope_revision: author.scope_revision,
                candidate: None,
                profile: binding.profile,
                configs: author.configs.layered_over(record.spec.configs()),
                context: context.clone(),
            },
        };
        state.contexts.insert(author.workpiece.0.clone(), context.clone());
        state.admitted_construction.insert(author.workpiece.0.clone(), admission.clone());
        state.integration.queued.push(CompositionInput {
            node: digest(76),
            candidate: CandidateRef { tree: digest(76), checkout: digest(77) },
            members: alloc::vec![MemberPin {
                workpiece: author.workpiece.clone(),
                scope_revision: author.scope_revision,
                candidate: CandidateRef { tree: digest(76), checkout: digest(77) },
            }],
        });
        record.coordination = Some(Box::new(state));
        let mut snapshot = Snapshot::new(record.spec.base());
        snapshot.blooms.insert(bloom, record);
        (snapshot, bloom, author.workpiece, inherited.workpiece, context, admission)
    }

    fn install_recorded_state(snapshot: &mut Snapshot, bloom: BloomId, decisions: &Decisions) -> CoordinationState {
        let state = decisions.effects.iter().find_map(|effect| match effect {
            Decision::RecordCoordinationState { state: Some(state), .. } => Some(state.clone()),
            _ => None,
        });
        let state = state.expect("decision records coordination state");
        snapshot.blooms.get_mut(&bloom).expect("record").coordination = Some(Box::new(state.clone()));
        state
    }

    fn contextual_plan_for(state: &CoordinationState) -> SharedRunPlan {
        let requests = state.requests.clone();
        let mut seen = BTreeSet::new();
        let inputs = requests
            .iter()
            .filter_map(|request| seen.insert(request.input.digest()).then_some(request.input.clone()))
            .collect();
        let composition = CompositionPlan {
            bloom: state.integration.generation.bloom,
            base: state.integration.head.clone(),
            inputs,
            contract: state.composition_contract.bind(
                requests
                    .iter()
                    .map(|request| MemberContractPin { request: request.digest(), contract: request.contract.digest() })
                    .collect(),
            ),
            requests: requests.clone(),
        };
        SharedRunPlan {
            mode: SharedRunMode::Contextual,
            requests,
            composition: Some(composition),
            probe_budget: state.policy.max_attribution_probes,
            execution_attempt: 0,
        }
    }

    #[test]
    fn compiled_contract_covers_member_requests_and_rejects_gate_or_config_drift() {
        let (mut record, state) = fixture();
        let member = record.spec.members()[0].clone();
        let candidate = CandidateRef { tree: digest(20), checkout: digest(21) };
        let progress = StageProgress {
            stage: StageId::Verify,
            attempts: 1,
            candidate: Some(candidate),
            repair_rolls: 0,
            seen_verify_failures: VerifyFailureSet::EMPTY,
            fold_checkpoint: None,
            fold_conflict_evidence: None,
            reconcile_assembles_base: false,
        };
        record.progress.insert(member.workpiece.clone(), progress);
        let binding = stage_binding(&record.stage_catalog, StageId::Verify);
        let transformation =
            Transformation::for_member_stage(&binding, candidate.tree, candidate.checkout, record.spec.base());
        let request = request_for_dispatch(
            &Snapshot::new(record.spec.base()),
            &record,
            &state,
            VerifyDispatch {
                workpiece: &member.workpiece,
                transformation: &transformation,
                profile: &binding.profile,
                configs: &member.configs.layered_over(record.spec.configs()),
                progress: &progress,
            },
        )
        .expect("current request");
        assert!(state.composition_contract.covers(&request));

        let mut missing_gate = state.composition_contract.clone();
        missing_gate.gate_identities.clear();
        assert!(!missing_gate.covers(&request));

        let mut different_configs = request;
        different_configs.configs.insert_named("test.config", digest(99));
        assert!(!state.composition_contract.covers(&different_configs));
        different_configs.contract.invocation = digest_of(&InvocationIdentity {
            transformation: &different_configs.transformation,
            profile: &different_configs.profile,
            configs: &different_configs.configs,
        });
        let mut divergent = state;
        divergent.requests = alloc::vec![different_configs.clone()];
        let composition = CompositionPlan {
            bloom: record.spec.id(),
            base: divergent.integration.head.clone(),
            inputs: alloc::vec![different_configs.input.clone()],
            requests: alloc::vec![different_configs.clone()],
            contract: divergent.composition_contract.bind(alloc::vec![MemberContractPin {
                request: different_configs.digest(),
                contract: different_configs.contract.digest(),
            }]),
        };
        let contextual = SharedRunPlan {
            mode: SharedRunMode::Contextual,
            requests: alloc::vec![different_configs.clone()],
            composition: Some(composition),
            probe_budget: divergent.policy.max_attribution_probes,
            execution_attempt: 0,
        };
        assert!(!validate_run_plan(&record, &divergent, &contextual));
        let standalone = SharedRunPlan {
            mode: SharedRunMode::Standalone,
            requests: alloc::vec![different_configs],
            composition: None,
            probe_budget: 0,
            execution_attempt: 0,
        };
        assert!(validate_run_plan(&record, &divergent, &standalone));
    }

    #[test]
    fn late_initial_state_does_not_overwrite_a_replaced_construct_dispatch() {
        let (record, state) = fixture();
        let bloom = record.spec.id();
        let member = record.spec.members()[0].clone();
        let binding = stage_binding(&record.stage_catalog, StageId::Construct);
        let dispatch = Decision::DispatchAttempt {
            bloom,
            workpiece: member.workpiece.clone(),
            stage: StageId::Construct,
            transformation: Transformation::for_member_stage(
                &binding,
                member.scope_revision,
                record.spec.base(),
                record.spec.base(),
            ),
            scope_revision: member.scope_revision,
            candidate: None,
            profile: binding.profile,
            configs: member.configs.layered_over(record.spec.configs()),
        };
        let mut snapshot = Snapshot::new(record.spec.base());
        snapshot.blooms.insert(bloom, record);
        let mut decisions = Decisions {
            outcome: Outcome::CoordinationAdvanced { bloom, subject: member.scope_revision },
            effects: alloc::vec![dispatch, record_state(bloom, state)],
        };

        replace_verify_dispatches(&snapshot, &mut decisions).expect("construct replacement should be valid");

        let recorded = decisions
            .effects
            .iter()
            .find_map(|effect| match effect {
                Decision::RecordCoordinationState { bloom: owner, state: Some(state) } if *owner == bloom => {
                    Some(state)
                }
                _ => None,
            })
            .expect("replacement should retain coordination state");
        assert!(recorded.contexts.contains_key("alpha"));
        assert!(recorded.queued_construction.contains_key("alpha"));
        assert!(decisions.effects.iter().any(|effect| {
            matches!(effect, Decision::QueueConstructionAdmission { dispatch }
                if dispatch.workpiece == member.workpiece
                    && dispatch.context == recorded.contexts["alpha"])
        }));
    }

    #[test]
    fn green_base_receipt_pins_the_exact_tree_before_releasing_construction() {
        let (mut record, state) = fixture();
        let bloom = record.spec.id();
        let base = record.spec.base();
        let tree = digest(90);
        let member = record.spec.members()[0].clone();
        let binding = stage_binding(&record.stage_catalog, StageId::Construct);
        let dispatch = Decision::DispatchAttempt {
            bloom,
            workpiece: member.workpiece.clone(),
            stage: StageId::Construct,
            transformation: Transformation::for_member_stage(&binding, member.scope_revision, base, base),
            scope_revision: member.scope_revision,
            candidate: None,
            profile: binding.profile,
            configs: member.configs.layered_over(record.spec.configs()),
        };
        record.coordination = Some(Box::new(state));
        let mut snapshot = Snapshot::new(base);
        snapshot.blooms.insert(bloom, record);
        let receipt = crate::BaseReceipt {
            base,
            tree,
            gate_set: VerifyGateSet::base().digest(),
            verdict: crate::BaseVerdict::Green {
                evidence: Evidence { subject: tree, kind: EvidenceKind::VerificationResult, detail: digest(91) },
            },
        };
        let decisions = schedule(
            &snapshot,
            Decisions {
                outcome: Outcome::BaseProven { base, tree, released: alloc::vec![bloom] },
                effects: alloc::vec![Decision::RecordBaseReceipt { receipt }, dispatch],
            },
        );

        let recorded = decisions
            .effects
            .iter()
            .find_map(|effect| match effect {
                Decision::RecordCoordinationState { bloom: owner, state: Some(state) } if *owner == bloom => {
                    Some(state)
                }
                _ => None,
            })
            .expect("base release should retain coordination state");
        let expected = CandidateRef { tree, checkout: base };
        assert_eq!(recorded.integration.generation.base, expected);
        assert_eq!(recorded.integration.head.candidate, expected);
        assert!(decisions.effects.iter().any(|effect| {
            matches!(effect, Decision::QueueConstructionAdmission { dispatch }
                if dispatch.workpiece == member.workpiece
                    && dispatch.context.starting_head == recorded.integration.head)
        }));
    }

    #[test]
    fn green_base_receipt_does_not_rebind_an_admitted_construction_context() {
        let (mut record, mut state) = fixture();
        let bloom = record.spec.id();
        let base = record.spec.base();
        let member = record.spec.members()[0].clone();
        let binding = stage_binding(&record.stage_catalog, StageId::Construct);
        let original_generation = state.integration.generation.clone();
        let original_head = state.integration.head.clone();
        let context = ConstructContext { bloom_base: original_generation.base, starting_head: original_head.clone() };
        let contextual = ContextualAttemptDispatch {
            bloom,
            workpiece: member.workpiece.clone(),
            stage: StageId::Construct,
            attempt: 1,
            transformation: Transformation::for_member_stage(&binding, member.scope_revision, base, base),
            scope_revision: member.scope_revision,
            candidate: None,
            profile: binding.profile.clone(),
            configs: member.configs.layered_over(record.spec.configs()),
            context: context.clone(),
        };
        let admission = ConstructionAdmission { nonce: digest(92), dispatch: contextual };
        state.contexts.insert(member.workpiece.0.clone(), context.clone());
        state.admitted_construction.insert(member.workpiece.0.clone(), admission.clone());
        record.coordination = Some(Box::new(state));
        let mut snapshot = Snapshot::new(base);
        snapshot.blooms.insert(bloom, record);
        let tree = digest(90);
        let receipt = crate::BaseReceipt {
            base,
            tree,
            gate_set: VerifyGateSet::base().digest(),
            verdict: crate::BaseVerdict::Green {
                evidence: Evidence { subject: tree, kind: EvidenceKind::VerificationResult, detail: digest(91) },
            },
        };
        let dispatch = Decision::DispatchAttempt {
            bloom,
            workpiece: member.workpiece.clone(),
            stage: StageId::Construct,
            transformation: admission.dispatch.transformation.clone(),
            scope_revision: member.scope_revision,
            candidate: None,
            profile: binding.profile,
            configs: member.configs.layered_over(snapshot.blooms[&bloom].spec.configs()),
        };
        let decisions = schedule(
            &snapshot,
            Decisions {
                outcome: Outcome::BaseProven { base, tree, released: alloc::vec![bloom] },
                effects: alloc::vec![Decision::RecordBaseReceipt { receipt }, dispatch],
            },
        );

        let recorded = decisions
            .effects
            .iter()
            .find_map(|effect| match effect {
                Decision::RecordCoordinationState { bloom: owner, state: Some(state) } if *owner == bloom => {
                    Some(state)
                }
                _ => None,
            })
            .expect("admitted construction should retain coordination state");
        assert_eq!(recorded.integration.generation, original_generation);
        assert_eq!(recorded.integration.head, original_head);
        assert_eq!(recorded.contexts[member.workpiece.0.as_str()], context);
        assert_eq!(recorded.admitted_construction[member.workpiece.0.as_str()], admission);
        assert!(!decisions.effects.iter().any(|effect| matches!(effect, Decision::QueueConstructionAdmission { .. })));
    }

    #[test]
    fn withdrawing_an_inherited_head_member_holds_an_active_constructor_with_its_provenance() {
        let (snapshot, bloom, author, inherited, context, admission) = active_inherited_construction();
        let withdrawal = Withdrawal {
            workpiece: inherited.clone(),
            cause: WithdrawalCause::Operator,
            reason: String::from("remove inherited contribution"),
            operator: String::from("test"),
        };

        let decisions = schedule(
            &snapshot,
            Decisions {
                outcome: Outcome::CoordinationAdvanced { bloom, subject: digest(75) },
                effects: alloc::vec![Decision::RecordWithdrawal { bloom, withdrawal }],
            },
        );

        let hold = decisions.effects.iter().find_map(|effect| match effect {
            Decision::RecordOperatorHold { bloom: owner, hold } if *owner == bloom => Some(hold),
            _ => None,
        });
        assert!(hold.is_some_and(|hold| { hold.reason.contains(&author.0) && hold.reason.contains(&inherited.0) }));
        let recorded = decisions
            .effects
            .iter()
            .find_map(|effect| match effect {
                Decision::RecordCoordinationState { state: Some(state), .. } => Some(state),
                _ => None,
            })
            .expect("withdrawal records coordination state");
        assert_eq!(recorded.contexts[author.0.as_str()], context);
        assert_eq!(recorded.admitted_construction[author.0.as_str()], admission);
        assert_eq!(recorded.integration.head.candidate, recorded.integration.generation.base);
        assert_eq!(recorded.integration.queued.len(), 1);
        assert!(!recorded.integration.head.coverage.iter().any(|pin| pin.workpiece == inherited));
        assert!(!decisions.effects.iter().any(|effect| {
            matches!(
                effect,
                Decision::DispatchCandidatePreparation { .. }
                    | Decision::QueueMemberVerification { .. }
                    | Decision::DispatchIntegrationAppend { .. }
                    | Decision::RecordVerifyProof { .. }
                    | Decision::RecordWedge { .. }
            )
        }));
    }

    #[test]
    fn a_stable_head_reservation_dispatches_only_its_current_owner() {
        let (mut record, mut state) = fixture();
        let peer = CompositionInput {
            node: digest(30),
            candidate: pin("beta", 11, 30).candidate,
            members: alloc::vec![pin("beta", 11, 30)],
        };
        let owner = CompositionInput {
            node: digest(40),
            candidate: pin("alpha", 10, 40).candidate,
            members: alloc::vec![pin("alpha", 10, 40)],
        };
        state.integration.queued = alloc::vec![peer, owner.clone()];
        state.integration.reservation = Some(StableHeadReservation {
            owner: WorkpieceId(String::from("alpha")),
            generation: state.integration.generation.digest(),
            node: state.integration.head.node,
            movement_count: 2,
            deadline_unix_millis: 1_000,
            hold: digest(50),
        });
        let mut effects = Vec::new();
        schedule_append(&record, &mut state, &mut effects);
        assert!(matches!(
            effects.as_slice(),
            [Decision::DispatchIntegrationAppend { plan }]
                if plan.inputs.as_slice() == from_ref(&owner)
        ));

        let plan = state.integration.in_flight.clone().expect("owner append");
        let head = IntegrationHead {
            generation: plan.generation,
            node: digest(52),
            candidate: owner.candidate,
            plan: plan.digest(),
            coverage: owner.members,
        };
        let bloom = record.spec.id();
        record.coordination = Some(Box::new(state.clone()));
        let mut snapshot = Snapshot::new(record.spec.base());
        snapshot.blooms.insert(bloom, record.clone());
        let advanced = reduce_integration_advanced(&snapshot, &bloom, plan.digest(), &head);
        let advanced_state = advanced.effects.iter().find_map(|effect| match effect {
            Decision::RecordCoordinationState { state: Some(state), .. } => Some(state),
            _ => None,
        });
        assert!(advanced_state.is_some_and(|state| state.integration.reservation.is_none()));

        state.integration.in_flight = None;
        state.integration.reservation.as_mut().expect("reservation").node = digest(51);
        effects.clear();
        schedule_append(&record, &mut state, &mut effects);
        assert!(effects.is_empty());
    }

    #[test]
    fn deferred_contextual_integration_advances_only_to_release_a_waiting_dependent() {
        let (mut record, mut state) = fixture();
        state.policy.eager_integration = false;
        let parent = CompositionInput {
            node: digest(40),
            candidate: pin("alpha", 10, 40).candidate,
            members: alloc::vec![pin("alpha", 10, 40)],
        };
        let unrelated = CompositionInput {
            node: digest(30),
            candidate: pin("beta", 11, 30).candidate,
            members: alloc::vec![pin("beta", 11, 30)],
        };
        record.dependencies.push(crate::MemberDependency {
            member: WorkpieceId(String::from("beta")),
            depends_on: WorkpieceId(String::from("alpha")),
        });
        state.integration.queued = alloc::vec![unrelated, parent.clone()];

        let mut effects = Vec::new();
        schedule_append(&record, &mut state, &mut effects);
        let plan = state.integration.in_flight.clone().expect("dependency parent append");
        assert_eq!(plan.inputs.as_slice(), from_ref(&parent));
        assert!(state.claims.is_empty(), "dependency assembly cannot wait for every final claim");

        let head = IntegrationHead {
            generation: plan.generation,
            node: digest(53),
            candidate: parent.candidate,
            plan: plan.digest(),
            coverage: parent.members,
        };
        let bloom = record.spec.id();
        record.coordination = Some(Box::new(state));
        let mut snapshot = Snapshot::new(record.spec.base());
        snapshot.blooms.insert(bloom, record);
        let advanced = reduce_integration_advanced(&snapshot, &bloom, plan.digest(), &head);
        assert!(advanced.effects.iter().any(|effect| {
            matches!(effect, Decision::QueueConstructionAdmission { dispatch }
                if dispatch.workpiece == WorkpieceId(String::from("beta"))
                    && dispatch.context.starting_head == head)
        }));
    }

    #[test]
    fn same_scope_candidate_replacement_advances_the_generation_epoch() {
        let (record, mut state) = fixture();
        let before = state.integration.generation.digest();
        let members = state.integration.generation.members.clone();
        let base = state.integration.generation.base;
        state.partial_head_repair = Some(PartialHeadRepairPlan {
            bloom: record.spec.id(),
            generation: before,
            head: state.integration.head.clone(),
            inputs: Vec::new(),
            evidence: digest(97),
            attempt: 0,
        });
        let mut effects = Vec::new();
        invalidate_member_version(&record, &mut state, &WorkpieceId(String::from("alpha")), &mut effects)
            .expect("first replacement advances the epoch");
        assert_eq!(state.integration.generation.members, members);
        assert_eq!(state.integration.generation.base, base);
        assert_eq!(state.integration.generation.epoch, 1);
        assert_ne!(state.integration.generation.digest(), before);
        assert!(state.partial_head_repair.is_none());
    }

    #[test]
    fn invalidating_one_survivor_retains_the_remaining_group_order() {
        let (snapshot, bloom, plan, _) = active_contextual_run();
        let record = snapshot.blooms.get(&bloom).expect("record");
        let mut state = record.coordination.as_deref().expect("coordination").clone();
        state.survivor_groups.push(SurvivorGroup {
            source_plan: plan.digest(),
            source_node: digest(54),
            requests: plan.requests.iter().map(MemberVerifyRequest::digest).collect(),
        });
        let mut effects = Vec::new();
        invalidate_member_version(record, &mut state, &plan.requests[0].member.workpiece, &mut effects)
            .expect("member replacement");
        assert_eq!(state.survivor_groups[0].requests, alloc::vec![plan.requests[1].digest()]);
    }

    #[test]
    fn construction_admission_rejects_a_displaced_head_and_dispatches_the_latest_intent() {
        let (mut record, mut state) = fixture();
        let workpiece = WorkpieceId(String::from("alpha"));
        record.progress.insert(
            workpiece.clone(),
            StageProgress {
                stage: StageId::Construct,
                attempts: 1,
                candidate: None,
                repair_rolls: 0,
                seen_verify_failures: VerifyFailureSet::EMPTY,
                fold_checkpoint: None,
                fold_conflict_evidence: None,
                reconcile_assembles_base: false,
            },
        );
        let context = ConstructContext {
            bloom_base: state.integration.generation.base,
            starting_head: state.integration.head.clone(),
        };
        let dispatch = ContextualAttemptDispatch {
            bloom: record.spec.id(),
            workpiece: workpiece.clone(),
            stage: StageId::Construct,
            attempt: 1,
            transformation: Transformation::for_member_stage(
                &stage_binding(&record.stage_catalog, StageId::Construct),
                digest(10),
                context.starting_head.candidate.checkout,
                record.spec.base(),
            ),
            scope_revision: digest(10),
            candidate: None,
            profile: crate::AgentProfile {
                harness: Harness::Codex,
                model: String::from("test"),
                effort: ReasoningEffort::Low,
                tools: ToolPolicy::None,
            },
            configs: ConfigRegistry::default(),
            context,
        };
        state.queued_construction.insert(workpiece.0, dispatch.clone());
        let bloom = record.spec.id();
        let mut snapshot = Snapshot::new(record.spec.base());
        record.coordination = Some(Box::new(state));
        snapshot.blooms.insert(bloom, record);

        let admission = ConstructionAdmission { nonce: digest(60), dispatch: dispatch.clone() };
        let accepted = reduce_request_construction_admission(&snapshot, &admission);
        assert!(accepted.effects.iter().any(|effect| {
            matches!(effect, Decision::DispatchContextualAttempt { dispatch: issued } if issued == &dispatch)
        }));

        let mut displaced = admission;
        displaced.dispatch.context.starting_head.node = digest(61);
        assert!(matches!(
            reduce_request_construction_admission(&snapshot, &displaced).outcome,
            Outcome::CoordinationRejected(CoordinationError::InvalidPlan)
        ));
    }

    #[test]
    fn a_full_contextual_completion_records_exact_claims_and_one_group_input() {
        let (snapshot, bloom, plan, node) = active_contextual_run();
        let outcomes = plan
            .requests
            .iter()
            .map(|request| MemberVerifyOutcome::PassedIn {
                request: request.digest(),
                node: node.digest(),
                receipt: Evidence {
                    subject: node.candidate.tree,
                    kind: EvidenceKind::VerificationResult,
                    detail: digest(70),
                },
            })
            .collect::<Vec<_>>();
        let completion = SharedRunCompletion {
            plan: plan.digest(),
            run: digest(62),
            latencies: plan
                .requests
                .iter()
                .map(|request| MemberVerifyLatency {
                    request: request.digest(),
                    member: request.member.clone(),
                    latency_millis: 10,
                })
                .collect(),
            outcomes,
            unfinished: Vec::new(),
        };
        let decisions = reduce_shared_run_completed(&snapshot, &bloom, &completion);
        let state = decisions.effects.iter().find_map(|effect| match effect {
            Decision::RecordCoordinationState { state: Some(state), .. } => Some(state),
            _ => None,
        });
        let state = state.expect("completion records state");
        assert_eq!(state.claims.len(), plan.requests.len());
        assert!(state.runs.last().is_some_and(SharedRunRecord::is_terminal));
        let mut record = snapshot.blooms.get(&bloom).expect("record").clone();
        record.coordination = Some(Box::new(state.clone()));
        assert!(plan.requests.iter().all(|request| record.has_current_member_resolution(&request.member.workpiece)));
        record.withdrawn.insert(
            plan.requests[0].member.workpiece.clone(),
            Withdrawal {
                workpiece: plan.requests[0].member.workpiece.clone(),
                cause: WithdrawalCause::Operator,
                reason: String::from("withdraw proved member"),
                operator: String::from("test"),
            },
        );
        assert!(!record.has_current_member_resolution(&plan.requests[0].member.workpiece));
        let queued = state.integration.queued.iter().find(|input| input.candidate == node.candidate);
        assert_eq!(queued.map(|input| &input.members), Some(&node.coverage));
        assert!(!decisions.effects.iter().any(|effect| matches!(effect, Decision::RecordVerifyProof { .. })));
    }

    #[test]
    fn terminal_bloom_cancels_every_nonterminal_shared_run() {
        let (mut snapshot, bloom, plan, _) = active_contextual_run();
        let state = snapshot.blooms.get_mut(&bloom).expect("record").coordination.as_mut().expect("coordination");
        let template = state.runs[0].clone();
        state.runs =
            [SharedRunPhase::Preparing, SharedRunPhase::Ready, SharedRunPhase::Running, SharedRunPhase::Terminal]
                .into_iter()
                .enumerate()
                .map(|(attempt, phase)| {
                    let mut run = template.clone();
                    run.plan.execution_attempt = u32::try_from(attempt).expect("four fixture attempts");
                    run.phase = phase;
                    run
                })
                .collect();
        let expected =
            state.runs.iter().filter(|run| !run.is_terminal()).map(|run| run.plan.digest()).collect::<Vec<_>>();

        let decisions = schedule(
            &snapshot,
            Decisions {
                outcome: Outcome::CoordinationAdvanced { bloom, subject: plan.digest() },
                effects: alloc::vec![Decision::MarkBloomWithdrawn { bloom }],
            },
        );
        let cancelled = decisions
            .effects
            .iter()
            .filter_map(|effect| match effect {
                Decision::CancelSharedRun { plan } => Some(*plan),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert_eq!(cancelled, expected);
        assert!(decisions.effects.iter().any(
            |effect| matches!(effect, Decision::RecordCoordinationState { bloom: owner, state: None } if *owner == bloom)
        ));
    }

    #[test]
    fn deferred_contextual_completion_appends_its_proved_node_without_refolding_member_leaves() {
        let (mut snapshot, bloom, plan, node) = active_contextual_run();
        snapshot
            .blooms
            .get_mut(&bloom)
            .expect("record")
            .coordination
            .as_mut()
            .expect("coordination")
            .policy
            .eager_integration = false;
        let completion = SharedRunCompletion {
            plan: plan.digest(),
            run: digest(62),
            latencies: plan
                .requests
                .iter()
                .map(|request| MemberVerifyLatency {
                    request: request.digest(),
                    member: request.member.clone(),
                    latency_millis: 10,
                })
                .collect(),
            outcomes: plan
                .requests
                .iter()
                .map(|request| MemberVerifyOutcome::PassedIn {
                    request: request.digest(),
                    node: node.digest(),
                    receipt: Evidence {
                        subject: node.candidate.tree,
                        kind: EvidenceKind::VerificationResult,
                        detail: digest(74),
                    },
                })
                .collect(),
            unfinished: Vec::new(),
        };
        let decisions = reduce_shared_run_completed(&snapshot, &bloom, &completion);
        let state = decisions.effects.iter().find_map(|effect| match effect {
            Decision::RecordCoordinationState { state: Some(state), .. } => Some(state),
            _ => None,
        });
        let state = state.expect("completion records state");
        let append = state.integration.in_flight.as_ref().expect("final node append");
        assert_eq!(append.inputs.len(), 1);
        assert_eq!(append.inputs[0].candidate, node.candidate);
        assert_eq!(append.inputs[0].members, node.coverage);
        assert!(state.integration.queued.iter().all(|input| input.candidate == node.candidate));
    }

    #[test]
    fn stale_contextual_completion_requeues_only_unaffected_requests_without_minting_claims() {
        let (mut snapshot, bloom, plan, node) = active_contextual_run();
        let retained = plan.requests[0].clone();
        let withdrawn = plan.requests[1].clone();
        let record = snapshot.blooms.get_mut(&bloom).expect("record");
        record.withdrawn.insert(
            withdrawn.member.workpiece.clone(),
            Withdrawal {
                workpiece: withdrawn.member.workpiece,
                cause: WithdrawalCause::Operator,
                reason: String::from("cancel sibling"),
                operator: String::from("test"),
            },
        );
        let state = record.coordination.as_mut().expect("coordination");
        state.runs[0].stale = true;
        state.requests.retain(|request| request.digest() == retained.digest());
        let outcomes = plan
            .requests
            .iter()
            .map(|request| MemberVerifyOutcome::PassedIn {
                request: request.digest(),
                node: node.digest(),
                receipt: Evidence {
                    subject: node.candidate.tree,
                    kind: EvidenceKind::VerificationResult,
                    detail: digest(75),
                },
            })
            .collect::<Vec<_>>();
        let completion = SharedRunCompletion {
            plan: plan.digest(),
            run: digest(62),
            latencies: plan
                .requests
                .iter()
                .map(|request| MemberVerifyLatency {
                    request: request.digest(),
                    member: request.member.clone(),
                    latency_millis: 10,
                })
                .collect(),
            outcomes,
            unfinished: Vec::new(),
        };
        let decisions = reduce_shared_run_completed(&snapshot, &bloom, &completion);
        let state = decisions.effects.iter().find_map(|effect| match effect {
            Decision::RecordCoordinationState { state: Some(state), .. } => Some(state),
            _ => None,
        });
        assert!(state.is_some_and(|state| state.claims.is_empty() && state.runs[0].is_terminal()));
        let queued = decisions
            .effects
            .iter()
            .filter_map(|effect| match effect {
                Decision::QueueMemberVerification { request } => Some(request.digest()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(queued, alloc::vec![retained.digest()]);
    }

    #[test]
    fn stale_warm_serial_completion_retains_an_unaffected_standalone_pass() {
        let (mut snapshot, bloom, contextual, _) = active_contextual_run();
        let passed = contextual.requests[0].clone();
        let cancelled = contextual.requests[1].clone();
        let plan = SharedRunPlan {
            mode: SharedRunMode::WarmSerial,
            requests: contextual.requests,
            composition: None,
            probe_budget: 0,
            execution_attempt: 0,
        };
        let record = snapshot.blooms.get_mut(&bloom).expect("record");
        record.withdrawn.insert(
            cancelled.member.workpiece.clone(),
            Withdrawal {
                workpiece: cancelled.member.workpiece.clone(),
                cause: WithdrawalCause::Operator,
                reason: String::from("cancel sibling"),
                operator: String::from("test"),
            },
        );
        let state = record.coordination.as_mut().expect("coordination");
        state.policy.verification = VerificationMode::WarmSerial;
        state.requests.retain(|request| request.digest() == passed.digest());
        state.runs[0] = SharedRunRecord {
            plan: plan.clone(),
            node: None,
            phase: SharedRunPhase::Running,
            stale: true,
            physical_run: Some(digest(62)),
            completed: Vec::new(),
            unfinished: plan.requests.iter().map(MemberVerifyRequest::digest).collect(),
            latencies: Vec::new(),
        };
        let completion = SharedRunCompletion {
            plan: plan.digest(),
            run: digest(62),
            outcomes: alloc::vec![MemberVerifyOutcome::PassedStandalone {
                request: passed.digest(),
                proof: VerifyProof {
                    gate_set: passed.contract.gate_set,
                    stage: StageId::Verify,
                    evidence: Evidence {
                        subject: passed.member.candidate.tree,
                        kind: EvidenceKind::VerificationResult,
                        detail: digest(91),
                    },
                },
            }],
            unfinished: alloc::vec![cancelled.digest()],
            latencies: alloc::vec![MemberVerifyLatency {
                request: passed.digest(),
                member: passed.member.clone(),
                latency_millis: 10,
            }],
        };

        let decisions = reduce_shared_run_completed(&snapshot, &bloom, &completion);
        let state = decisions.effects.iter().find_map(|effect| match effect {
            Decision::RecordCoordinationState { state: Some(state), .. } => Some(state),
            _ => None,
        });
        let state = state.expect("stale serial completion records state");
        assert!(matches!(
            state.claims.get(passed.member.workpiece.0.as_str()).map(|claim| &claim.proof),
            Some(ResolutionProof::Standalone(_))
        ));
        assert!(!state.claims.contains_key(cancelled.member.workpiece.0.as_str()));
        assert!(state.runs[0].is_terminal());
        assert!(!decisions.effects.iter().any(|effect| {
            matches!(
                effect,
                Decision::QueueMemberVerification { request }
                    if request.digest() == cancelled.digest()
            )
        }));
    }

    #[test]
    fn blocked_dependency_cannot_be_reported_as_a_contextual_survivor() {
        let (mut snapshot, bloom, plan, node) = active_contextual_run();
        snapshot.blooms.get_mut(&bloom).expect("record").dependencies.push(crate::MemberDependency {
            member: plan.requests[1].member.workpiece.clone(),
            depends_on: plan.requests[0].member.workpiece.clone(),
        });
        let evidence =
            Evidence { subject: node.candidate.tree, kind: EvidenceKind::VerificationResult, detail: digest(71) };
        let completion = SharedRunCompletion {
            plan: plan.digest(),
            run: digest(62),
            outcomes: alloc::vec![
                MemberVerifyOutcome::Failed {
                    request: plan.requests[0].digest(),
                    scope: FailureScope::Attributed {
                        members: alloc::vec![plan.requests[0].member.clone()],
                        evidence: evidence.detail,
                    },
                    failures: once(crate::VerifyFailure::Fmt).collect(),
                    evidence,
                },
                MemberVerifyOutcome::Survived {
                    request: plan.requests[1].digest(),
                    node: node.digest(),
                    observation: digest(72),
                },
            ],
            unfinished: Vec::new(),
            latencies: plan
                .requests
                .iter()
                .map(|request| MemberVerifyLatency {
                    request: request.digest(),
                    member: request.member.clone(),
                    latency_millis: 10,
                })
                .collect(),
        };
        assert!(matches!(
            reduce_shared_run_completed(&snapshot, &bloom, &completion).outcome,
            Outcome::CoordinationRejected(CoordinationError::InvalidPlan)
        ));
    }

    #[test]
    fn independent_attributions_schedule_one_repair_for_each_distinct_member() {
        let (snapshot, bloom, plan, node) = active_contextual_run();
        let outcomes = plan
            .requests
            .iter()
            .enumerate()
            .map(|(index, request)| {
                let detail = digest(76 + u8::try_from(index).expect("small fixture"));
                MemberVerifyOutcome::Failed {
                    request: request.digest(),
                    scope: FailureScope::Attributed { members: alloc::vec![request.member.clone()], evidence: detail },
                    failures: once(crate::VerifyFailure::Fmt).collect(),
                    evidence: Evidence { subject: node.candidate.tree, kind: EvidenceKind::VerificationResult, detail },
                }
            })
            .collect::<Vec<_>>();
        let completion = SharedRunCompletion {
            plan: plan.digest(),
            run: digest(62),
            outcomes,
            unfinished: Vec::new(),
            latencies: plan
                .requests
                .iter()
                .map(|request| MemberVerifyLatency {
                    request: request.digest(),
                    member: request.member.clone(),
                    latency_millis: 10,
                })
                .collect(),
        };
        let decisions = reduce_shared_run_completed(&snapshot, &bloom, &completion);
        let repairs = decisions
            .effects
            .iter()
            .filter_map(|effect| match effect {
                Decision::AdvanceStage { workpiece, progress, .. } if progress.stage == StageId::Refine => {
                    Some(workpiece)
                }
                _ => None,
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(repairs.len(), plan.requests.len());
    }

    #[test]
    fn attributed_member_invalidation_requeues_the_unaffected_interaction_group() {
        let (snapshot, bloom, plan, node) = active_contextual_run();
        let attributed_detail = digest(78);
        let interaction_detail = digest(79);
        let completion = SharedRunCompletion {
            plan: plan.digest(),
            run: digest(62),
            outcomes: alloc::vec![
                MemberVerifyOutcome::Failed {
                    request: plan.requests[0].digest(),
                    scope: FailureScope::Attributed {
                        members: alloc::vec![plan.requests[0].member.clone()],
                        evidence: attributed_detail,
                    },
                    failures: once(crate::VerifyFailure::Fmt).collect(),
                    evidence: Evidence {
                        subject: node.candidate.tree,
                        kind: EvidenceKind::VerificationResult,
                        detail: attributed_detail,
                    },
                },
                MemberVerifyOutcome::Failed {
                    request: plan.requests[1].digest(),
                    scope: FailureScope::Interaction {
                        members: plan.requests.iter().map(|request| request.member.clone()).collect(),
                        evidence: interaction_detail,
                    },
                    failures: once(crate::VerifyFailure::Test).collect(),
                    evidence: Evidence {
                        subject: node.candidate.tree,
                        kind: EvidenceKind::VerificationResult,
                        detail: interaction_detail,
                    },
                },
            ],
            unfinished: Vec::new(),
            latencies: plan
                .requests
                .iter()
                .map(|request| MemberVerifyLatency {
                    request: request.digest(),
                    member: request.member.clone(),
                    latency_millis: 10,
                })
                .collect(),
        };
        let decisions = schedule(&snapshot, reduce_shared_run_completed(&snapshot, &bloom, &completion));
        let state = decisions.effects.iter().find_map(|effect| match effect {
            Decision::RecordCoordinationState { state: Some(state), .. } => Some(state),
            _ => None,
        });
        let state = state.expect("scheduled reduction records state");
        assert!(state.partial_head_repair.is_none());
        assert_eq!(state.survivor_groups[0].requests, alloc::vec![plan.requests[1].digest()]);
        assert!(!decisions.effects.iter().any(|effect| matches!(effect, Decision::DispatchPartialHeadRepair { .. })));
        assert!(decisions.effects.iter().any(|effect| {
            matches!(effect, Decision::QueueMemberVerification { request }
                if request.digest() == plan.requests[1].digest())
        }));
    }

    #[test]
    fn only_explicit_unblocked_survivors_form_the_next_atomic_group() {
        let (snapshot, bloom, plan, node) = active_contextual_run();
        let evidence =
            Evidence { subject: node.candidate.tree, kind: EvidenceKind::VerificationResult, detail: digest(73) };
        let completion = SharedRunCompletion {
            plan: plan.digest(),
            run: digest(62),
            outcomes: alloc::vec![
                MemberVerifyOutcome::Failed {
                    request: plan.requests[0].digest(),
                    scope: FailureScope::Attributed {
                        members: alloc::vec![plan.requests[0].member.clone()],
                        evidence: evidence.detail,
                    },
                    failures: once(crate::VerifyFailure::Fmt).collect(),
                    evidence,
                },
                MemberVerifyOutcome::Survived {
                    request: plan.requests[1].digest(),
                    node: node.digest(),
                    observation: digest(74),
                },
            ],
            unfinished: Vec::new(),
            latencies: plan
                .requests
                .iter()
                .map(|request| MemberVerifyLatency {
                    request: request.digest(),
                    member: request.member.clone(),
                    latency_millis: 10,
                })
                .collect(),
        };
        let decisions = reduce_shared_run_completed(&snapshot, &bloom, &completion);
        let state = decisions.effects.iter().find_map(|effect| match effect {
            Decision::RecordCoordinationState { state: Some(state), .. } => Some(state),
            _ => None,
        });
        let state = state.expect("completion records state");
        assert_eq!(state.survivor_groups.len(), 1);
        assert_eq!(state.survivor_groups[0].requests, alloc::vec![plan.requests[1].digest()]);
        assert!(decisions.effects.iter().any(|effect| {
            matches!(effect, Decision::QueueMemberVerification { request }
                if request.digest() == plan.requests[1].digest())
        }));
    }

    #[test]
    fn interaction_repair_retains_the_selected_parent_contribution() {
        let (mut snapshot, bloom, _, _) = active_contextual_run();
        let record = snapshot.blooms.get_mut(&bloom).expect("record");
        let state = record.coordination.as_mut().expect("coordination");
        let prior = state.runs[0].plan.requests[0].input.clone();
        state.integration.admitted = alloc::vec![prior.clone()];
        state.integration.head = IntegrationHead {
            generation: state.integration.generation.digest(),
            node: prior.candidate.tree,
            candidate: prior.candidate,
            plan: digest(80),
            coverage: prior.members.clone(),
        };
        let mut run = state.runs[0].clone();
        run.plan.composition.as_mut().expect("contextual plan").base = state.integration.head.clone();
        run.plan.composition.as_mut().expect("contextual plan").contract = state.composition_contract.bind(
            run.plan
                .requests
                .iter()
                .map(|request| MemberContractPin { request: request.digest(), contract: request.contract.digest() })
                .collect(),
        );
        let plan = run.plan.digest();
        let node = SharedRunNode {
            plan,
            candidate: CandidateRef { tree: digest(81), checkout: digest(82) },
            coverage: run.plan.requests.iter().map(|request| request.member.clone()).collect(),
        };
        run.node = Some(node.clone());
        run.physical_run = Some(digest(83));
        state.runs[0] = run.clone();
        let later = CompositionInput {
            node: digest(92),
            candidate: CandidateRef { tree: digest(92), checkout: digest(93) },
            members: alloc::vec![run.plan.requests[1].member.clone()],
        };
        state.integration.admitted.push(later.clone());
        state.integration.head = IntegrationHead {
            generation: state.integration.generation.digest(),
            node: digest(94),
            candidate: later.candidate,
            plan: digest(95),
            coverage: run.plan.requests.iter().map(|request| request.member.clone()).collect(),
        };
        let evidence =
            Evidence { subject: node.candidate.tree, kind: EvidenceKind::VerificationResult, detail: digest(84) };
        let outcomes = run
            .plan
            .requests
            .iter()
            .map(|request| MemberVerifyOutcome::Failed {
                request: request.digest(),
                scope: FailureScope::Interaction {
                    members: run.plan.requests.iter().map(|request| request.member.clone()).collect(),
                    evidence: evidence.detail,
                },
                failures: once(crate::VerifyFailure::Test).collect(),
                evidence: evidence.clone(),
            })
            .collect();
        let completion = SharedRunCompletion {
            plan,
            run: digest(83),
            outcomes,
            unfinished: Vec::new(),
            latencies: run
                .plan
                .requests
                .iter()
                .map(|request| MemberVerifyLatency {
                    request: request.digest(),
                    member: request.member.clone(),
                    latency_millis: 10,
                })
                .collect(),
        };
        let decisions = reduce_shared_run_completed(&snapshot, &bloom, &completion);
        let repair = decisions.effects.iter().find_map(|effect| match effect {
            Decision::DispatchPartialHeadRepair { dispatch } => Some(&dispatch.plan),
            _ => None,
        });
        let repair = repair.expect("interaction dispatches composition repair");
        assert_eq!(repair.head.candidate, node.candidate);
        assert!(repair.inputs.iter().any(|input| input.digest() == prior.digest()));
        assert!(!repair.inputs.iter().any(|input| input.digest() == later.digest()));
    }

    #[test]
    fn repaired_selected_head_stays_red_until_the_new_root_is_verified() {
        let (mut snapshot, bloom, _, node) = active_contextual_run();
        let record = snapshot.blooms.get_mut(&bloom).expect("record");
        let state = record.coordination.as_mut().expect("coordination");
        let input = CompositionInput { node: node.digest(), candidate: node.candidate, members: node.coverage.clone() };
        state.integration.head = IntegrationHead {
            generation: state.integration.generation.digest(),
            node: node.digest(),
            candidate: node.candidate,
            plan: digest(85),
            coverage: node.coverage,
        };
        state.integration.admitted = alloc::vec![input];
        state.integration.known_red = Some(state.integration.head.node);
        state.runs[0].phase = SharedRunPhase::Terminal;
        let repair = PartialHeadRepairPlan {
            bloom,
            generation: state.integration.generation.digest(),
            head: state.integration.head.clone(),
            inputs: state.integration.admitted.clone(),
            evidence: digest(86),
            attempt: 0,
        };
        state.partial_head_repair = Some(repair.clone());
        let replacement = CandidateRef { tree: digest(87), checkout: digest(88) };
        let completion = PartialHeadRepairCompletion::Repaired {
            candidate: replacement,
            evidence: Evidence {
                subject: repair.head.candidate.tree,
                kind: EvidenceKind::VerificationResult,
                detail: digest(89),
            },
        };
        let decisions = reduce_partial_head_repaired(&snapshot, &bloom, repair.digest(), &completion);
        let state = install_recorded_state(&mut snapshot, bloom, &decisions);
        assert_eq!(state.integration.head.candidate, replacement);
        assert_eq!(state.integration.known_red, Some(state.integration.head.node));
        assert!(decisions.effects.iter().any(|effect| matches!(effect, Decision::QueueMemberVerification { .. })));
        assert!(!decisions.effects.iter().any(|effect| {
            matches!(effect, Decision::DispatchIntegrationAppend { .. } | Decision::DispatchAggregateVerify { .. })
        }));

        let shared = contextual_plan_for(&state);
        let proposed = reduce_propose_shared_run(&snapshot, &bloom, &shared);
        assert!(matches!(proposed.outcome, Outcome::CoordinationAdvanced { .. }));
        install_recorded_state(&mut snapshot, bloom, &proposed);
        let node =
            SharedRunNode { plan: shared.digest(), candidate: replacement, coverage: state.integration.head.coverage };
        let prepared = reduce_shared_run_prepared(
            &snapshot,
            &bloom,
            shared.digest(),
            &SharedRunPreparation::Contextual(node.clone()),
        );
        install_recorded_state(&mut snapshot, bloom, &prepared);
        let started = reduce_shared_run_started(&snapshot, &bloom, shared.digest(), digest(90));
        install_recorded_state(&mut snapshot, bloom, &started);
        let completion = SharedRunCompletion {
            plan: shared.digest(),
            run: digest(90),
            outcomes: shared
                .requests
                .iter()
                .map(|request| MemberVerifyOutcome::PassedIn {
                    request: request.digest(),
                    node: node.digest(),
                    receipt: Evidence {
                        subject: replacement.tree,
                        kind: EvidenceKind::VerificationResult,
                        detail: digest(91),
                    },
                })
                .collect(),
            unfinished: Vec::new(),
            latencies: shared
                .requests
                .iter()
                .map(|request| MemberVerifyLatency {
                    request: request.digest(),
                    member: request.member.clone(),
                    latency_millis: 10,
                })
                .collect(),
        };
        let completed = reduce_shared_run_completed(&snapshot, &bloom, &completion);
        let verified = install_recorded_state(&mut snapshot, bloom, &completed);
        assert_eq!(verified.integration.known_red, None);
        assert!(verified.contextual_aggregate_proof(&verified.integration.head).is_some());
    }

    fn active_serial_run() -> (Snapshot, BloomId, SharedRunPlan) {
        let (mut record, mut state) = fixture();
        let bloom = record.spec.id();
        let requests = current_requests(&mut record, &state);
        state.requests.clone_from(&requests);
        let plan = SharedRunPlan {
            mode: SharedRunMode::WarmSerial,
            requests,
            composition: None,
            probe_budget: 0,
            execution_attempt: 0,
        };
        state.runs.push(SharedRunRecord {
            plan: plan.clone(),
            node: None,
            phase: SharedRunPhase::Running,
            stale: false,
            physical_run: Some(digest(62)),
            completed: Vec::new(),
            unfinished: plan.requests.iter().map(MemberVerifyRequest::digest).collect(),
            latencies: Vec::new(),
        });
        record.coordination = Some(Box::new(state));
        let mut snapshot = Snapshot::new(record.spec.base());
        snapshot.blooms.insert(bloom, record);
        (snapshot, bloom, plan)
    }

    fn recorded(decisions: &Decisions) -> &CoordinationState {
        decisions
            .effects
            .iter()
            .find_map(|effect| match effect {
                Decision::RecordCoordinationState { state: Some(state), .. } => Some(state),
                _ => None,
            })
            .expect("the decision records coordination state")
    }

    fn queued_input(workpiece: &str, scope: u8, tree: u8) -> CompositionInput {
        let member = pin(workpiece, scope, tree);
        CompositionInput { node: digest(tree), candidate: member.candidate, members: alloc::vec![member] }
    }

    fn in_flight_snapshot(record: BloomRecord, state: CoordinationState) -> (Snapshot, BloomId) {
        let bloom = record.spec.id();
        let mut record = record;
        record.coordination = Some(Box::new(state));
        let mut snapshot = Snapshot::new(record.spec.base());
        snapshot.blooms.insert(bloom, record);
        (snapshot, bloom)
    }

    #[test]
    fn a_nodeless_run_cannot_report_an_interaction_failure() {
        let (snapshot, bloom, plan) = active_serial_run();
        let outcomes = alloc::vec![
            MemberVerifyOutcome::Failed {
                request: plan.requests[0].digest(),
                scope: FailureScope::Attributed {
                    members: alloc::vec![plan.requests[0].member.clone()],
                    evidence: digest(84),
                },
                failures: once(crate::VerifyFailure::Test).collect(),
                evidence: Evidence {
                    subject: plan.requests[0].member.candidate.tree,
                    kind: EvidenceKind::VerificationResult,
                    detail: digest(84),
                },
            },
            MemberVerifyOutcome::Failed {
                request: plan.requests[1].digest(),
                scope: FailureScope::Interaction {
                    members: plan.requests.iter().map(|request| request.member.clone()).collect(),
                    evidence: digest(85),
                },
                failures: once(crate::VerifyFailure::Test).collect(),
                evidence: Evidence {
                    subject: plan.requests[1].member.candidate.tree,
                    kind: EvidenceKind::VerificationResult,
                    detail: digest(85),
                },
            },
        ];
        let completion = SharedRunCompletion {
            plan: plan.digest(),
            run: digest(62),
            outcomes,
            unfinished: Vec::new(),
            latencies: plan
                .requests
                .iter()
                .map(|request| MemberVerifyLatency {
                    request: request.digest(),
                    member: request.member.clone(),
                    latency_millis: 10,
                })
                .collect(),
        };

        let decisions = reduce_shared_run_completed(&snapshot, &bloom, &completion);

        assert_eq!(decisions.outcome, Outcome::CoordinationRejected(CoordinationError::InvalidEvidenceKind));
        assert!(decisions.effects.is_empty());
    }

    #[test]
    fn a_red_head_admits_no_new_construction_order() {
        let (mut record, mut state) = fixture();
        let workpiece = WorkpieceId(String::from("alpha"));
        record.progress.insert(
            workpiece.clone(),
            StageProgress {
                stage: StageId::Construct,
                attempts: 1,
                candidate: None,
                repair_rolls: 0,
                seen_verify_failures: VerifyFailureSet::EMPTY,
                fold_checkpoint: None,
                fold_conflict_evidence: None,
                reconcile_assembles_base: false,
            },
        );
        let context = ConstructContext {
            bloom_base: state.integration.generation.base,
            starting_head: state.integration.head.clone(),
        };
        let dispatch = ContextualAttemptDispatch {
            bloom: record.spec.id(),
            workpiece: workpiece.clone(),
            stage: StageId::Construct,
            attempt: 1,
            transformation: Transformation::for_member_stage(
                &stage_binding(&record.stage_catalog, StageId::Construct),
                digest(10),
                context.starting_head.candidate.checkout,
                record.spec.base(),
            ),
            scope_revision: digest(10),
            candidate: None,
            profile: crate::AgentProfile {
                harness: Harness::Codex,
                model: String::from("test"),
                effort: ReasoningEffort::Low,
                tools: ToolPolicy::None,
            },
            configs: ConfigRegistry::default(),
            context,
        };
        state.queued_construction.insert(workpiece.0, dispatch.clone());
        state.integration.head.node = digest(61);
        state.integration.known_red = Some(state.integration.head.node);

        let mut refreshed = Vec::new();
        refresh_unadmitted_construction(&record, &mut state.clone(), &mut refreshed);
        assert!(refreshed.is_empty(), "a red head cannot re-pin a queued construction onto itself");

        let (snapshot, _) = in_flight_snapshot(record, state);
        let admission = ConstructionAdmission { nonce: digest(60), dispatch };
        assert_eq!(
            reduce_request_construction_admission(&snapshot, &admission).outcome,
            Outcome::CoordinationRejected(CoordinationError::NotReady),
        );
    }

    #[test]
    fn invalidating_a_peer_ejects_a_request_whose_input_inherited_it() {
        let (mut record, mut state) = fixture();
        let mut requests = current_requests(&mut record, &state);
        let ejected = requests[0].member.clone();
        requests[1].input.members.insert(0, ejected.clone());
        state.requests.clone_from(&requests);

        let mut effects = Vec::new();
        invalidate_member_version(&record, &mut state, &ejected.workpiece, &mut effects).expect("replacement");

        assert!(state.requests.is_empty(), "an input carrying the ejected pin cannot outlive it");
    }

    #[test]
    fn an_append_conflict_reconciles_only_its_own_members_and_holds_the_head() {
        let (record, mut state) = fixture();
        let colliding = queued_input("alpha", 10, 40);
        let peer = queued_input("beta", 11, 30);
        state.integration.queued = alloc::vec![colliding.clone(), peer.clone()];
        state.integration.movement_count = state.policy.movement_budget - 1;
        let mut effects = Vec::new();
        schedule_append(&record, &mut state, &mut effects);
        let plan = state.integration.in_flight.clone().expect("first ready input appends");
        let head = state.integration.head.clone();
        let (snapshot, bloom) = in_flight_snapshot(record, state);
        let evidence =
            Evidence { subject: colliding.candidate.tree, kind: EvidenceKind::FoldConflict, detail: digest(55) };

        let decisions = reduce_integration_conflicted(
            &snapshot,
            IntegrationConflict {
                bloom: &bloom,
                plan: plan.digest(),
                generation: plan.generation,
                expected_parent: head.node,
                input: &colliding,
                at: colliding.candidate,
                evidence: &evidence,
                observed_at_unix_millis: 10,
            },
        );

        let next = recorded(&decisions);
        assert_eq!(next.integration.head, head);
        assert!(next.integration.in_flight.is_none());
        assert!(next.integration.queued.iter().any(|queued| queued.digest() == peer.digest()));
        let reconciled = decisions
            .effects
            .iter()
            .filter_map(|effect| match effect {
                Decision::AdvanceStage { workpiece, progress, .. } => Some((workpiece, *progress)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(matches!(
            reconciled.as_slice(),
            [(workpiece, progress)]
                if **workpiece == colliding.members[0].workpiece
                    && progress.stage == StageId::Reconcile
                    && progress.fold_checkpoint == Some(head.node)
        ));
        assert!(next.integration.reservation.as_ref().is_some_and(|reservation| {
            reservation.owner == colliding.members[0].workpiece && reservation.node == head.node
        }));
    }

    #[test]
    fn a_refused_append_clears_the_flight_without_moving_the_head() {
        let (record, mut state) = fixture();
        let input = queued_input("alpha", 10, 40);
        state.integration.queued = alloc::vec![input];
        let mut effects = Vec::new();
        schedule_append(&record, &mut state, &mut effects);
        let plan = state.integration.in_flight.clone().expect("ready input appends");
        let head = state.integration.head.clone();
        let queued = state.integration.queued.clone();
        let (snapshot, bloom) = in_flight_snapshot(record, state);

        let decisions =
            reduce_integration_refused(&snapshot, &bloom, plan.digest(), plan.generation, head.node, digest(56));

        let next = recorded(&decisions);
        assert_eq!(next.integration.head, head);
        assert_eq!(next.integration.queued, queued);
        assert!(next.integration.in_flight.is_none());
        assert!(
            next.diagnostics
                .iter()
                .any(|diagnostic| diagnostic.scope == FailureScope::Unattributed { evidence: digest(56) })
        );
        assert!(!decisions.effects.iter().any(|effect| matches!(effect, Decision::DispatchIntegrationAppend { .. })));
    }

    #[test]
    fn a_reservation_expires_only_at_its_own_deadline_and_then_resumes_appends() {
        let (record, mut state) = fixture();
        let peer = queued_input("beta", 11, 30);
        state.integration.queued = alloc::vec![peer.clone(), queued_input("alpha", 10, 40)];
        let reservation = StableHeadReservation {
            owner: WorkpieceId(String::from("alpha")),
            generation: state.integration.generation.digest(),
            node: state.integration.head.node,
            movement_count: state.policy.movement_budget,
            deadline_unix_millis: 1_000,
            hold: digest(50),
        };
        state.integration.reservation = Some(reservation.clone());
        state.integration.movement_count = state.policy.movement_budget;
        let (snapshot, bloom) = in_flight_snapshot(record, state);

        let mismatched = StableHeadReservation { movement_count: 99, ..reservation.clone() };
        assert_eq!(
            reduce_reservation_expired(&snapshot, &bloom, &mismatched, 1_000).outcome,
            Outcome::CoordinationRejected(CoordinationError::ReservationMismatch),
        );
        assert_eq!(
            reduce_reservation_expired(&snapshot, &bloom, &reservation, 999).outcome,
            Outcome::CoordinationRejected(CoordinationError::ReservationNotExpired),
        );

        let decisions = reduce_reservation_expired(&snapshot, &bloom, &reservation, 1_000);

        let next = recorded(&decisions);
        assert!(next.integration.reservation.is_none());
        assert_eq!(next.integration.movement_count, 0);
        assert!(decisions.effects.iter().any(|effect| {
            matches!(effect, Decision::DispatchIntegrationAppend { plan } if plan.inputs.as_slice() == from_ref(&peer))
        }));
    }

    #[test]
    fn a_compatibility_preview_settles_against_its_exact_checkpoint_versions() {
        let (record, mut state) = fixture();
        let bloom = record.spec.id();
        let checkpoint = ConstructionCheckpoint {
            bloom,
            workpiece: WorkpieceId(String::from("alpha")),
            scope_revision: digest(10),
            nonce: digest(74),
            observation: 1,
            starting_checkout: state.integration.head.candidate.checkout,
            candidate: CandidateRef { tree: digest(76), checkout: digest(77) },
        };
        state.checkpoints.insert(checkpoint.workpiece.0.clone(), checkpoint.clone());
        let plan = CompatibilityPreviewPlan {
            bloom,
            generation: state.integration.generation.digest(),
            base: state.integration.head.candidate,
            checkpoints: alloc::vec![checkpoint.clone()],
        };
        state.preview_plans.push(plan.clone());
        let (snapshot, _) = in_flight_snapshot(record, state);
        let result = CompatibilityPreview::Clean { tree: digest(78) };

        let decisions = reduce_compatibility_previewed(&snapshot, &bloom, plan.digest(), &result);

        assert_eq!(
            recorded(&decisions).previews,
            alloc::vec![CompatibilityPreviewRecord { plan: plan.digest(), result: result.clone() }]
        );

        let mut superseded = snapshot;
        superseded
            .blooms
            .get_mut(&bloom)
            .expect("record")
            .coordination
            .as_deref_mut()
            .expect("coordination")
            .checkpoints
            .insert(checkpoint.workpiece.0.clone(), ConstructionCheckpoint { observation: 2, ..checkpoint });
        assert_eq!(
            reduce_compatibility_previewed(&superseded, &bloom, plan.digest(), &result).outcome,
            Outcome::CoordinationRejected(CoordinationError::InvalidPlan),
        );
    }

    #[test]
    fn a_conflicted_preparation_returns_to_reconcile_and_wedges_past_its_budget() {
        let (mut record, mut state) = fixture();
        let bloom = record.spec.id();
        let workpiece = WorkpieceId(String::from("alpha"));
        let authored = CandidateRef { tree: digest(40), checkout: digest(41) };
        let plan = CandidatePreparationPlan {
            bloom,
            workpiece: workpiece.clone(),
            scope_revision: digest(10),
            authored,
            context: ConstructContext {
                bloom_base: state.integration.generation.base,
                starting_head: state.integration.head.clone(),
            },
        };
        state.preparations.push(plan.clone());
        let budget = record.stage_catalog.retry_budget_of(StageId::Reconcile).expect("reconcile budget");
        record.progress.insert(
            workpiece.clone(),
            StageProgress {
                stage: StageId::Reconcile,
                attempts: 1,
                candidate: Some(authored),
                repair_rolls: 0,
                seen_verify_failures: VerifyFailureSet::EMPTY,
                fold_checkpoint: None,
                fold_conflict_evidence: None,
                reconcile_assembles_base: false,
            },
        );
        let (mut snapshot, _) = in_flight_snapshot(record, state);
        let evidence = Evidence {
            subject: plan.context.starting_head.candidate.tree,
            kind: EvidenceKind::FoldConflict,
            detail: digest(57),
        };
        let conflict = CandidatePreparation::Conflict { evidence };

        let decisions = reduce_candidate_prepared(&snapshot, &bloom, plan.digest(), &conflict);

        assert!(recorded(&decisions).claims.is_empty());
        assert!(decisions.effects.iter().any(|effect| {
            matches!(effect, Decision::DispatchContextualAttempt { dispatch }
                if dispatch.stage == StageId::Reconcile
                    && dispatch.context.starting_head == plan.context.starting_head)
        }));
        assert!(!decisions.effects.iter().any(|effect| {
            matches!(effect, Decision::QueueMemberVerification { .. } | Decision::RecordWedge { .. })
        }));

        snapshot
            .blooms
            .get_mut(&bloom)
            .expect("record")
            .progress
            .get_mut(&workpiece)
            .expect("reconcile cursor")
            .attempts = budget;
        let exhausted = reduce_candidate_prepared(&snapshot, &bloom, plan.digest(), &conflict);
        assert!(exhausted.effects.iter().any(|effect| {
            matches!(effect, Decision::RecordWedge { wedge, .. } if wedge.stage == StageId::Reconcile)
        }));
        assert!(
            !exhausted.effects.iter().any(|effect| matches!(effect, Decision::DispatchContextualAttempt { .. })),
            "an exhausted reconcile budget dispatches no further author order",
        );
    }

    #[test]
    fn repeatedly_red_repaired_head_stops_at_the_visible_repair_budget() {
        let (record, mut state) = fixture();
        let budget = record.stage_catalog.retry_budget_of(StageId::Refine).expect("refine budget");
        state.integration.known_red = Some(state.integration.head.node);
        state.partial_head_repair_attempts = budget;
        let mut effects = Vec::new();
        schedule_partial_head_repair(&record, &mut state, digest(96), &mut effects);
        assert!(state.partial_head_repair.is_none());
        assert!(effects.iter().any(|effect| matches!(effect, Decision::RecordOperatorHold { .. })));
        assert!(!effects.iter().any(|effect| matches!(effect, Decision::DispatchPartialHeadRepair { .. })));
    }
}
