//! Representative wire values for the golden decision fixtures.
//!
//! These constructors are the one vocabulary the fixture command and the
//! golden guards share: `cargo xtask fixtures regen` encodes them, and the
//! tests compare those bytes to the checked-in files.

use core::iter::once;

use crate::ids::{BloomId, IdempotencyKey, StageId, WorkpieceId};
use crate::port::{ClaimRefKind, ProjectedReceipt};
use crate::reduce::{
    Decision, Decisions, Event, Fact, FoldedIntegration, Outcome, RecordedRead, RecordedRefusal, StageProgress,
};
use crate::values::{
    Adjudication, AgentProfile, BaseReceipt, BaseVerdict, CandidatePreparationPlan, CandidateRef, CompatibilityPreview,
    CompatibilityPreviewPlan, CompatibilityPreviewRecord, CompositionContractTemplate, CompositionFinding,
    CompositionInput, CompositionPlan, ConfigRegistry, ConstructContext, ConstructionAdmission, ConstructionCheckpoint,
    ContextualAttemptDispatch, ContextualInvocationTemplate, ContextualResolutionClaim, CoordinationDiagnostic,
    CoordinationPolicy, CoordinationState, DeclaredEvidence, DeclaredLanes, DeclaredVerifiers, Disposition, Evidence,
    EvidenceKind, ExecutionLimits, FailureScope, GenerationMember, Harness, LandingReceipt, LaneEntrypoint,
    MemberCandidate, MemberContractPin, MemberDependency, MemberPin, MemberVerifyLatency, MemberVerifyOutcome,
    MemberVerifyRequest, NetworkProfile, OperatorHold, OperatorProposal, OperatorRepair, OrphanClaimRelease,
    OrphanClaimReleaseCompletion, PartialHeadRepairDispatch, PartialHeadRepairPlan, PipelineManifest,
    PrecheckDiagnostic, PrecheckMember, PrecheckNode, PrecheckPlan, PrecheckPolicy, PrecheckResult, PrecheckState,
    PreparedCandidate, ReasoningEffort, ResolutionClaim, ResolutionProof, ResolvedBloom, ResolvedModel,
    SharedRunDispatch, SharedRunExecution, SharedRunMode, SharedRunNode, SharedRunPhase, SharedRunPlan,
    SharedRunRecord, SpendQuiesce, StableHeadReservation, StageBinding, StageCatalog, SurvivorGroup, ToolPolicy,
    Transformation, VerificationContract, VerificationMode, VerificationObligation, VerifyFailure, VerifyFailureSet,
    VerifyGateSet, VerifyProof, VerifyReuse, Wedge, Withdrawal, WithdrawalCause,
};

use super::digest;

fn configs() -> ConfigRegistry {
    let mut registry = ConfigRegistry::default();
    registry.insert_named("aether.bloomery.model_override", digest(17));
    registry
}

fn profile() -> AgentProfile {
    AgentProfile {
        harness: Harness::Codex,
        model: "gpt-5-codex".into(),
        effort: ReasoningEffort::Max,
        tools: ToolPolicy::Allow(vec!["read".into()]),
    }
}

fn transformation() -> Transformation {
    Transformation {
        command: "verify.check".into(),
        inputs: vec![digest(12)],
        checkout: digest(13),
        diff_base: Some(digest(14)),
        outputs: vec!["verdict".into()],
        image: "iama/verify:1".into(),
        limits: ExecutionLimits { wall_clock_secs: 900 },
        network: NetworkProfile::Restricted,
        description: Some("verify the candidate".into()),
        model: Some(ResolvedModel {
            harness: Harness::Claude,
            model: "claude-opus-4-8".into(),
            effort: ReasoningEffort::High,
        }),
    }
}

fn precheck_records(bloom: BloomId) -> Vec<Decision> {
    let plan = PrecheckPlan {
        bloom,
        base: digest(60),
        members: vec![
            PrecheckMember {
                workpiece: WorkpieceId("alpha".into()),
                scope_revision: digest(61),
                candidate: CandidateRef { tree: digest(62), checkout: digest(63) },
            },
            PrecheckMember {
                workpiece: WorkpieceId("beta".into()),
                scope_revision: digest(64),
                candidate: CandidateRef { tree: digest(65), checkout: digest(66) },
            },
        ],
        gate_set: digest(67),
    };
    let node = PrecheckNode { plan: plan.digest(), tree: digest(68), head: digest(69), gate_set: plan.gate_set };
    let state = |result, diagnostic| {
        Box::new(PrecheckState {
            policy: PrecheckPolicy { run_budget: 2 },
            latest_plan: Some(plan.clone()),
            prepared: Some(node.clone()),
            issued: Some(node.clone()),
            issued_runs: 1,
            result: Some(result),
            diagnostic,
            final_join: Some(node.clone()),
            promoted: true,
            paused: true,
        })
    };

    vec![
        Decision::RecordPrecheckState {
            bloom,
            state: Some(state(
                PrecheckResult::Passed { node: node.digest(), evidence: digest(70) },
                Some(PrecheckDiagnostic::PreparationRefused { plan: plan.digest(), detail: digest(71) }),
            )),
        },
        Decision::RecordPrecheckState {
            bloom,
            state: Some(state(
                PrecheckResult::Failed { node: node.digest(), evidence: digest(72) },
                Some(PrecheckDiagnostic::VerificationFailed { node: node.digest(), detail: digest(73) }),
            )),
        },
        Decision::RecordPrecheckState {
            bloom,
            state: Some(state(
                PrecheckResult::HostFault { node: node.digest(), evidence: digest(74) },
                Some(PrecheckDiagnostic::HostFault { node: node.digest(), detail: digest(75) }),
            )),
        },
        Decision::RecordPrecheckState {
            bloom,
            state: Some(state(PrecheckResult::SkippedBeforeStart { node: node.digest() }, None)),
        },
        Decision::QueuePrecheckPlan { bloom, plan },
        Decision::OfferPrecheck {
            bloom,
            node: node.clone(),
            transformation: transformation(),
            profile: profile(),
            configs: configs(),
        },
        Decision::DispatchPrecheck {
            bloom,
            node: node.clone(),
            transformation: transformation(),
            profile: profile(),
            configs: configs(),
        },
        Decision::CancelPrecheck { bloom, node: node.digest() },
        Decision::PromotePrecheck { bloom, node: node.digest() },
    ]
}

struct CoordinationGolden {
    bloom: BloomId,
    base: CandidateRef,
    candidate: CandidateRef,
    state: CoordinationState,
    request: MemberVerifyRequest,
    run: SharedRunPlan,
    node: SharedRunNode,
    context: ConstructContext,
    contextual: ContextualAttemptDispatch,
    checkpoint: ConstructionCheckpoint,
    append: crate::IntegrationAppendPlan,
    partial: PartialHeadRepairPlan,
    verification: Evidence,
}

fn coordination_state(
    bloom: BloomId,
    workpiece: &WorkpieceId,
    base: CandidateRef,
) -> (CoordinationState, CompositionContractTemplate) {
    let policy = CoordinationPolicy {
        verification: VerificationMode::Contextual,
        eager_integration: true,
        max_run_members: 4,
        max_serial_requests: 3,
        max_attribution_probes: 2,
        movement_budget: 2,
        reservation_millis: 30_000,
        host_class: "fixture".into(),
    };
    let template = CompositionContractTemplate {
        gate_set: digest(85),
        gate_identities: vec!["cargo-test".into()],
        invocation: ContextualInvocationTemplate {
            command: "verify.check".into(),
            extra_inputs: vec![digest(84)],
            diff_base: Some(base.checkout),
            outputs: vec!["verdict".into()],
            image: "iama/verify:1".into(),
            limits: ExecutionLimits { wall_clock_secs: 900 },
            network: NetworkProfile::None,
            description: None,
            model: None,
            profile: profile(),
            configs: configs(),
        },
        environment: digest(86),
        host_class: digest(87),
    };
    let state = CoordinationState::new(
        policy,
        template.clone(),
        bloom,
        base,
        vec![GenerationMember { workpiece: workpiece.clone(), scope_revision: digest(88) }],
    );
    (state, template)
}

fn coordination_fixture(bloom: BloomId) -> CoordinationGolden {
    let workpiece = WorkpieceId("alpha".into());
    let base = CandidateRef { tree: digest(80), checkout: digest(81) };
    let candidate = CandidateRef { tree: digest(82), checkout: digest(83) };
    let (state, template) = coordination_state(bloom, &workpiece, base);
    let pin = MemberPin { workpiece: workpiece.clone(), scope_revision: digest(88), candidate };
    let input = CompositionInput { node: digest(89), candidate, members: vec![pin.clone()] };
    let context = ConstructContext { bloom_base: base, starting_head: state.integration.head.clone() };
    let contract = VerificationContract {
        gate_set: digest(90),
        obligations: vec![
            VerificationObligation::Gate { identity: "cargo-test".into() },
            VerificationObligation::MemberDelta { scope_revision: pin.scope_revision, candidate, diff_base: base },
        ],
        diff_base: base,
        invocation: digest(91),
        environment: template.environment,
        host_class: template.host_class,
    };
    let request = MemberVerifyRequest {
        bloom,
        member: pin.clone(),
        input: input.clone(),
        attempt: 0,
        context: Some(context.clone()),
        contract,
        transformation: transformation(),
        profile: profile(),
        configs: configs(),
    };
    let composition = CompositionPlan {
        bloom,
        base: state.integration.head.clone(),
        inputs: vec![input.clone()],
        requests: vec![request.clone()],
        contract: template
            .bind(vec![MemberContractPin { request: request.digest(), contract: request.contract.digest() }]),
    };
    let run = SharedRunPlan {
        mode: SharedRunMode::Contextual,
        requests: vec![request.clone()],
        composition: Some(composition),
        probe_budget: 2,
        execution_attempt: 0,
    };
    let node = SharedRunNode { plan: run.digest(), candidate, coverage: vec![pin] };
    let contextual = ContextualAttemptDispatch {
        bloom,
        workpiece,
        stage: StageId::Construct,
        attempt: 1,
        transformation: transformation(),
        scope_revision: digest(88),
        candidate: None,
        profile: profile(),
        configs: configs(),
        context: context.clone(),
    };
    let checkpoint = ConstructionCheckpoint {
        bloom,
        workpiece: contextual.workpiece.clone(),
        scope_revision: contextual.scope_revision,
        nonce: digest(92),
        observation: 1,
        starting_checkout: context.starting_head.candidate.checkout,
        candidate,
    };
    let append = crate::IntegrationAppendPlan {
        bloom,
        generation: state.integration.generation.digest(),
        expected_parent: state.integration.head.clone(),
        inputs: vec![input.clone()],
    };
    let partial = PartialHeadRepairPlan {
        bloom,
        generation: state.integration.generation.digest(),
        head: state.integration.head.clone(),
        inputs: vec![input],
        evidence: digest(93),
        attempt: 0,
    };
    let verification = Evidence { subject: candidate.tree, kind: EvidenceKind::VerificationResult, detail: digest(94) };

    CoordinationGolden {
        bloom,
        base,
        candidate,
        state,
        request,
        run,
        node,
        context,
        contextual,
        checkpoint,
        append,
        partial,
        verification,
    }
}

fn coordination_run(fixture: &CoordinationGolden) -> SharedRunRecord {
    let fault = Evidence { subject: fixture.candidate.tree, kind: EvidenceKind::ExecutorFault, detail: digest(95) };
    let proof = VerifyProof {
        gate_set: fixture.request.contract.gate_set,
        stage: StageId::Verify,
        evidence: fixture.verification.clone(),
    };
    SharedRunRecord {
        plan: fixture.run.clone(),
        node: Some(fixture.node.clone()),
        phase: SharedRunPhase::Terminal,
        stale: false,
        physical_run: Some(digest(96)),
        completed: vec![
            MemberVerifyOutcome::PassedStandalone { request: fixture.request.digest(), proof },
            MemberVerifyOutcome::PassedIn {
                request: fixture.request.digest(),
                node: fixture.node.digest(),
                receipt: fixture.verification.clone(),
            },
            MemberVerifyOutcome::Failed {
                request: fixture.request.digest(),
                scope: FailureScope::Attributed {
                    members: vec![fixture.request.member.clone()],
                    evidence: fixture.verification.detail,
                },
                failures: once(VerifyFailure::Fmt).collect(),
                evidence: fixture.verification.clone(),
            },
            MemberVerifyOutcome::HostFault { request: fixture.request.digest(), evidence: fault },
            MemberVerifyOutcome::Survived {
                request: fixture.request.digest(),
                node: fixture.node.digest(),
                observation: digest(97),
            },
            MemberVerifyOutcome::Pending { request: fixture.request.digest(), observation: digest(98) },
        ],
        unfinished: vec![fixture.request.digest()],
        latencies: vec![MemberVerifyLatency {
            request: fixture.request.digest(),
            member: fixture.request.member.clone(),
            latency_millis: 42,
        }],
    }
}

fn coordination_pending_run(
    fixture: &CoordinationGolden,
    phase: SharedRunPhase,
    execution_attempt: u32,
) -> SharedRunRecord {
    let plan = SharedRunPlan { execution_attempt, ..fixture.run.clone() };
    let node = (!matches!(phase, SharedRunPhase::Preparing))
        .then(|| SharedRunNode { plan: plan.digest(), ..fixture.node.clone() });
    SharedRunRecord {
        plan,
        node,
        phase,
        stale: false,
        physical_run: matches!(phase, SharedRunPhase::Running).then_some(digest(99)),
        completed: Vec::new(),
        unfinished: vec![fixture.request.digest()],
        latencies: Vec::new(),
    }
}

fn populate_coordination_claims(fixture: &mut CoordinationGolden) {
    let proof = VerifyProof {
        gate_set: fixture.request.contract.gate_set,
        stage: StageId::Verify,
        evidence: fixture.verification.clone(),
    };
    fixture.state.requests.push(fixture.request.clone());
    let runs = [
        coordination_pending_run(fixture, SharedRunPhase::Preparing, 1),
        coordination_pending_run(fixture, SharedRunPhase::Ready, 2),
        coordination_pending_run(fixture, SharedRunPhase::Running, 3),
        coordination_run(fixture),
    ];
    fixture.state.runs.extend(runs);
    fixture.state.claims.insert(
        fixture.request.member.workpiece.0.clone(),
        ContextualResolutionClaim { member: fixture.request.member.clone(), proof: ResolutionProof::Standalone(proof) },
    );
    fixture.state.claims.insert(
        String::from("beta"),
        ContextualResolutionClaim {
            member: MemberPin {
                workpiece: WorkpieceId("beta".into()),
                scope_revision: digest(99),
                candidate: fixture.candidate,
            },
            proof: ResolutionProof::InComposition {
                node: fixture.node.digest(),
                receipt: fixture.verification.clone(),
                plan: fixture.run.digest(),
                request: fixture.request.digest(),
                contract: fixture.run.composition.as_ref().expect("fixture composition").contract.digest(),
            },
        },
    );
    fixture.state.prepared.insert(
        fixture.request.member.workpiece.0.clone(),
        PreparedCandidate {
            authored: fixture.candidate,
            candidate: fixture.candidate,
            context: fixture.context.clone(),
            diff_base: fixture.base,
        },
    );
    fixture.state.preparations.push(CandidatePreparationPlan {
        bloom: fixture.bloom,
        workpiece: fixture.request.member.workpiece.clone(),
        scope_revision: fixture.request.member.scope_revision,
        authored: fixture.candidate,
        context: fixture.context.clone(),
    });
}

fn populate_coordination_observations(fixture: &mut CoordinationGolden) {
    fixture.state.contexts.insert(fixture.request.member.workpiece.0.clone(), fixture.context.clone());
    fixture.state.checkpoints.insert(fixture.checkpoint.workpiece.0.clone(), fixture.checkpoint.clone());
    fixture.state.queued_construction.insert(fixture.contextual.workpiece.0.clone(), fixture.contextual.clone());
    fixture.state.admitted_construction.insert(
        fixture.contextual.workpiece.0.clone(),
        ConstructionAdmission { nonce: fixture.checkpoint.nonce, dispatch: fixture.contextual.clone() },
    );
    fixture.state.preview_plans.push(CompatibilityPreviewPlan {
        bloom: fixture.bloom,
        generation: fixture.partial.generation,
        base: fixture.base,
        checkpoints: vec![fixture.checkpoint.clone()],
    });
    fixture.state.previews.extend([
        CompatibilityPreviewRecord { plan: digest(100), result: CompatibilityPreview::Clean { tree: digest(101) } },
        CompatibilityPreviewRecord {
            plan: digest(102),
            result: CompatibilityPreview::Conflict { evidence: digest(103) },
        },
        CompatibilityPreviewRecord { plan: digest(104), result: CompatibilityPreview::Refused { detail: digest(105) } },
    ]);
    fixture.state.diagnostics.extend([
        CoordinationDiagnostic {
            subject: digest(106),
            scope: FailureScope::Attributed {
                members: vec![fixture.request.member.clone()],
                evidence: fixture.verification.detail,
            },
        },
        CoordinationDiagnostic {
            subject: digest(107),
            scope: FailureScope::Interaction {
                members: vec![fixture.request.member.clone(), fixture.node.coverage[0].clone()],
                evidence: fixture.verification.detail,
            },
        },
        CoordinationDiagnostic {
            subject: digest(108),
            scope: FailureScope::Inherited { head: fixture.node.digest(), evidence: fixture.verification.detail },
        },
        CoordinationDiagnostic {
            subject: digest(109),
            scope: FailureScope::Unattributed { evidence: fixture.verification.detail },
        },
    ]);
    fixture.state.survivor_groups.push(SurvivorGroup {
        source_plan: fixture.run.digest(),
        source_node: fixture.node.digest(),
        requests: vec![fixture.request.digest()],
    });
    fixture.state.partial_head_repair = Some(fixture.partial.clone());
    fixture.state.integration.reservation = Some(StableHeadReservation {
        owner: fixture.request.member.workpiece.clone(),
        generation: fixture.partial.generation,
        node: fixture.state.integration.head.node,
        movement_count: 2,
        deadline_unix_millis: 123_456,
        hold: digest(110),
    });
}

fn coordination_decisions(fixture: CoordinationGolden) -> Vec<Decision> {
    vec![
        Decision::RecordCoordinationState { bloom: fixture.bloom, state: Some(Box::new(fixture.state)) },
        Decision::DispatchIntegrationAppend { plan: fixture.append },
        Decision::DispatchCandidatePreparation {
            plan: CandidatePreparationPlan {
                bloom: fixture.bloom,
                workpiece: fixture.contextual.workpiece.clone(),
                scope_revision: fixture.contextual.scope_revision,
                authored: fixture.candidate,
                context: fixture.context,
            },
        },
        Decision::QueueMemberVerification { request: Box::new(fixture.request.clone()) },
        Decision::DispatchSharedRunPreparation { plan: fixture.run.clone() },
        Decision::DispatchSharedRun {
            dispatch: SharedRunDispatch { plan: fixture.run.clone(), execution: SharedRunExecution::Serial },
        },
        Decision::DispatchSharedRun {
            dispatch: SharedRunDispatch {
                plan: fixture.run.clone(),
                execution: SharedRunExecution::Contextual {
                    node: Box::new(fixture.node),
                    transformation: Box::new(transformation()),
                    profile: profile(),
                    configs: configs(),
                },
            },
        },
        Decision::CancelSharedRun { plan: fixture.run.digest() },
        Decision::CancelMemberVerification { request: fixture.request.digest() },
        Decision::DispatchCompatibilityPreview {
            plan: CompatibilityPreviewPlan {
                bloom: fixture.bloom,
                generation: fixture.partial.generation,
                base: fixture.base,
                checkpoints: vec![fixture.checkpoint],
            },
        },
        Decision::QueueConstructionAdmission { dispatch: fixture.contextual.clone() },
        Decision::DispatchContextualAttempt { dispatch: fixture.contextual },
        Decision::DispatchPartialHeadRepair {
            dispatch: PartialHeadRepairDispatch {
                plan: fixture.partial,
                transformation: transformation(),
                scope_revision: fixture.base.checkout,
                profile: profile(),
                configs: configs(),
            },
        },
    ]
}

fn coordination_records(bloom: BloomId) -> Vec<Decision> {
    let mut fixture = coordination_fixture(bloom);
    populate_coordination_claims(&mut fixture);
    populate_coordination_observations(&mut fixture);
    coordination_decisions(fixture)
}

fn resolution_claim(workpiece: WorkpieceId) -> ResolutionClaim {
    ResolutionClaim {
        workpiece,
        scope_revision: digest(15),
        candidate: digest(16),
        evidence: Evidence { subject: digest(16), kind: EvidenceKind::ResolutionClaim, detail: digest(21) },
    }
}

fn verify_proof() -> VerifyProof {
    VerifyProof {
        gate_set: digest(22),
        stage: StageId::Verify,
        evidence: Evidence { subject: digest(23), kind: EvidenceKind::VerificationResult, detail: digest(24) },
    }
}

fn stage_catalog() -> StageCatalog {
    StageCatalog {
        bindings: vec![StageBinding {
            stage: StageId::Construct,
            consumes: vec!["bloom.ready".into()],
            produces: vec!["bloom.candidate".into()],
            profile: profile(),
            process: "construct.implement".into(),
            completion_gate: "pr-open".into(),
            retry_budget: 2,
            wall_clock_secs: 3_600,
        }],
    }
}

/// A small hand-authored manifest, for the reason [`stage_catalog`] is one: a
/// fixture built from the compiled vocabulary would move its pinned bytes every
/// time an identity or a lane command changed, which says nothing about the
/// shape these bytes exist to freeze.
fn pipeline_manifest() -> PipelineManifest {
    PipelineManifest {
        version: 1,
        entrypoint: LaneEntrypoint { program: "cargo".into(), args: vec!["xtask".into(), "transform".into()] },
        lanes: DeclaredLanes { model: vec!["construct.implement".into()], mechanical: vec!["verify.check".into()] },
        verifiers: DeclaredVerifiers {
            identities: vec!["verify.fmt".into(), "verify.clippy".into()],
            runs: once(("verify.check".into(), vec!["verify.fmt".into()])).collect(),
        },
        evidence: DeclaredEvidence { envelope: 1 },
    }
}

fn orphan_claim_release(workpiece: WorkpieceId, holder: BloomId) -> OrphanClaimRelease {
    OrphanClaimRelease { ref_kind: ClaimRefKind::Workpiece(workpiece), expected_holder: holder }
}

fn advance_stage(bloom: BloomId, workpiece: WorkpieceId) -> Decision {
    Decision::AdvanceStage {
        bloom,
        workpiece,
        progress: StageProgress {
            stage: StageId::Construct,
            attempts: 1,
            candidate: Some(CandidateRef { tree: digest(2), checkout: digest(3) }),
            repair_rolls: 0,
            seen_verify_failures: VerifyFailureSet::one(VerifyFailure::Clippy),
            fold_checkpoint: Some(digest(4)),
            fold_conflict_evidence: Some(digest(5)),
            reconcile_assembles_base: false,
        },
    }
}

fn dispatch_attempt(bloom: BloomId, workpiece: WorkpieceId) -> Decision {
    Decision::DispatchAttempt {
        bloom,
        workpiece,
        stage: StageId::Verify,
        transformation: transformation(),
        scope_revision: digest(15),
        candidate: Some(digest(16)),
        profile: profile(),
        configs: configs(),
    }
}

fn dispatch_aggregate_review(bloom: BloomId) -> Decision {
    Decision::DispatchAggregateReview {
        bloom,
        transformation: transformation(),
        roll: 2,
        profile: profile(),
        configs: configs(),
    }
}

/// The bloom-level reader a landing decides (ADR-0216). Shaped like the
/// aggregate review's row minus its pass counter: the reader runs once, so
/// there is no roll for the fixture to freeze.
fn dispatch_study(bloom: BloomId) -> Decision {
    Decision::DispatchStudy { bloom, transformation: transformation(), profile: profile(), configs: configs() }
}

fn dispatch_aggregate_verify(bloom: BloomId) -> Decision {
    Decision::DispatchAggregateVerify { bloom, transformation: transformation(), roll: 3, profile: profile() }
}

fn dispatch_splice(bloom: BloomId, workpiece: WorkpieceId, successor: BloomId) -> Decision {
    Decision::DispatchSplice {
        bloom,
        workpiece,
        base: digest(41),
        members: vec![MemberCandidate { workpiece: WorkpieceId("beta".into()), candidate: digest(42) }],
        adopt_from: Some(successor),
    }
}

fn set_resolved(bloom: BloomId, workpiece: WorkpieceId) -> Decision {
    Decision::SetResolved {
        bloom,
        resolved: ResolvedBloom {
            bloom,
            tree: digest(18),
            head: digest(19),
            lineage: vec![digest(20)],
            resolution_claims: vec![resolution_claim(workpiece)],
        },
    }
}

fn record_composition_finding(bloom: BloomId, workpiece: WorkpieceId) -> Decision {
    Decision::RecordCompositionFinding {
        bloom,
        finding: CompositionFinding { subject: digest(35), detail: digest(36), implicated: vec![workpiece] },
    }
}

fn record_adjudication(bloom: BloomId) -> Decision {
    Decision::RecordAdjudication {
        bloom,
        adjudication: Adjudication {
            findings: vec![digest(36)],
            // The payload-carrying disposition, so the fixture freezes the shape
            // behind it; `Accepted` is a bare discriminant and freezes nothing.
            disposition: Disposition::Deferred { issue: 4957 },
            reason: "the remaining finding is a test fixture, filed forward".into(),
            operator: "iamacoffeepot".into(),
        },
    }
}

fn record_operator_repair(bloom: BloomId, workpiece: WorkpieceId) -> Decision {
    Decision::RecordOperatorRepair {
        bloom,
        repair: OperatorRepair {
            workpiece,
            candidate: CandidateRef { tree: digest(37), checkout: digest(38) },
            reason: "one-line fix, cheaper to write than to dispatch".into(),
            operator: "iamacoffeepot".into(),
        },
    }
}

fn operator_hold(reason: &str) -> OperatorHold {
    OperatorHold { reason: reason.into(), operator: "iamacoffeepot".into() }
}

/// The operator brake's three rows: both edges of the flag, which carry the same
/// payload type and differ only in which direction they move it, and the
/// deferral a raised hold records each time it swallows a dispatch.
fn brake_records(bloom: BloomId, workpiece: WorkpieceId) -> [Decision; 3] {
    [
        Decision::RecordOperatorHold { bloom, hold: operator_hold("the fixture bloom is spending on a refusal") },
        Decision::RecordOperatorRelease { bloom, release: operator_hold("the refusal cleared; let it run") },
        Decision::DeferDispatch { bloom, workpiece },
    ]
}

/// Every row a withdrawal writes (#5327), and both `WithdrawalCause` variants
/// so the completeness walk freezes the stranded-dependent axis rather than
/// only the operator-named one.
fn withdrawal_records(bloom: BloomId, workpiece: WorkpieceId) -> [Decision; 5] {
    let dependent = WorkpieceId("beta".into());
    [
        Decision::RecordWithdrawal {
            bloom,
            withdrawal: Withdrawal {
                workpiece: workpiece.clone(),
                cause: WithdrawalCause::Operator,
                reason: "the fixture member is being taken out of the line".into(),
                operator: "fixture-operator".into(),
            },
        },
        Decision::RecordWithdrawal {
            bloom,
            withdrawal: Withdrawal {
                workpiece: dependent,
                cause: WithdrawalCause::Dependency { on: workpiece.clone() },
                reason: "its construct base left the line".into(),
                operator: "fixture-operator".into(),
            },
        },
        Decision::CancelDispatch { bloom, workpiece: workpiece.clone() },
        Decision::ReleaseMemberClaimRef { bloom, workpiece },
        Decision::MarkBloomWithdrawn { bloom },
    ]
}

/// Both aggregate deferrals a raised hold records (#5100), so the completeness
/// walk freezes the new family rather than only the `Decision` tag.
fn brake_aggregates(bloom: BloomId) -> [Decision; 2] {
    [
        Decision::DeferAggregate { bloom, stage: StageId::AggregateVerify },
        Decision::DeferAggregate { bloom, stage: StageId::AggregateReview },
    ]
}

/// Both halves of the composite-gate join (#5327's sibling), so the walk
/// freezes the gate a pass is filed against rather than only the `Decision` tag.
fn gate_passes(bloom: BloomId) -> [Decision; 2] {
    [
        Decision::RecordAggregateGatePass { bloom, stage: StageId::AggregateVerify },
        Decision::RecordAggregateGatePass { bloom, stage: StageId::AggregateReview },
    ]
}

fn proposal_records() -> [Decision; 3] {
    let proposal = OperatorProposal {
        candidate: CandidateRef { tree: digest(50), checkout: digest(51) },
        reason: "flip an ADR status".into(),
        operator: "operator".into(),
    };
    [
        Decision::QueueProposal { proposal: proposal.clone() },
        Decision::DequeueProposal { proposal: proposal.clone() },
        Decision::DispatchProposal { proposal, base: digest(52) },
    ]
}

fn base_verify_records() -> [Decision; 3] {
    [
        Decision::RecordBaseReceipt {
            receipt: BaseReceipt {
                base: digest(40),
                tree: digest(41),
                gate_set: VerifyGateSet::base().digest(),
                verdict: BaseVerdict::Green {
                    evidence: Evidence {
                        subject: digest(41),
                        kind: EvidenceKind::VerificationResult,
                        detail: digest(42),
                    },
                },
            },
        },
        Decision::RecordBaseReceipt {
            receipt: BaseReceipt {
                base: digest(43),
                tree: digest(44),
                gate_set: VerifyGateSet::base().digest(),
                verdict: BaseVerdict::Red {
                    evidence: Evidence {
                        subject: digest(44),
                        kind: EvidenceKind::VerificationResult,
                        detail: digest(45),
                    },
                    failed: VerifyFailureSet::one(VerifyFailure::Docs),
                },
            },
        },
        Decision::DispatchBaseVerify { base: digest(40), transformation: transformation(), profile: profile() },
    ]
}

fn refusal_records(bloom: BloomId, workpiece: WorkpieceId) -> [Decision; 2] {
    [
        Decision::RecordRefusal {
            bloom,
            workpiece: Some(workpiece),
            refusal: RecordedRefusal {
                gate: "dispatch".into(),
                guard: "candidate_ref_present".into(),
                reads: vec![RecordedRead { field: "member".into(), value: "alpha".into() }],
            },
        },
        Decision::RecordRefusal {
            bloom,
            workpiece: None,
            refusal: RecordedRefusal {
                gate: "land".into(),
                guard: "bloom_resolved".into(),
                reads: vec![RecordedRead { field: "status".into(), value: "Sealed".into() }],
            },
        },
    ]
}

fn record_member_dependencies(bloom: BloomId, workpiece: WorkpieceId) -> Decision {
    Decision::RecordMemberDependencies {
        bloom,
        edges: vec![MemberDependency { member: workpiece, depends_on: WorkpieceId("beta".into()) }],
    }
}

fn record_candidate_vehicle(bloom: BloomId, workpiece: WorkpieceId) -> Decision {
    Decision::RecordCandidateVehicle {
        bloom,
        workpiece,
        vehicle: CandidateRef { tree: digest(39), checkout: digest(40) },
    }
}

fn record_member_machinery(bloom: BloomId, workpiece: WorkpieceId) -> Decision {
    Decision::RecordMemberMachinery { bloom, workpiece, stage: StageId::Verify, rolls: 2, evidence: digest(41) }
}

/// The host-fault hold and its cadence clear (#5020), so the completeness
/// walk freezes the findings string and the evidence digest the resume keys on.
fn host_fault_records(bloom: BloomId, workpiece: WorkpieceId) -> [Decision; 2] {
    [
        Decision::RecordHostFault {
            bloom,
            workpiece: workpiece.clone(),
            findings: "Verification did not run. missing `jscpd`.".into(),
            evidence: digest(35),
        },
        Decision::ClearHostFault { bloom, workpiece },
    ]
}

/// Both `SpendQuiesce` payload variants, so the completeness walk freezes the
/// axis shapes rather than only the `Option` tag.
fn spend_quiesce_records(bloom: BloomId) -> [Decision; 2] {
    [
        Decision::RecordSpendQuiesce {
            quiesce: Some(SpendQuiesce::Window {
                window: "bloomery/daily/2026-08-14".into(),
                spent_micro_usd: 1_000_000,
                ceiling_micro_usd: 500_000,
            }),
        },
        Decision::RecordSpendQuiesce {
            quiesce: Some(SpendQuiesce::Bloom {
                window: "bloomery/daily/2026-08-14".into(),
                bloom,
                spent_micro_usd: 250_000,
                ceiling_micro_usd: 200_000,
            }),
        },
    ]
}

fn bloom_lifecycle_records(bloom: BloomId, successor: BloomId, workpiece: &WorkpieceId) -> Vec<Decision> {
    vec![
        Decision::ClaimMembership { workpiece: workpiece.clone(), bloom },
        Decision::ReleaseMembership { workpiece: workpiece.clone(), bloom },
        Decision::InheritClaim { bloom: successor, claim: resolution_claim(workpiece.clone()) },
        Decision::RecordResolution { bloom, claim: resolution_claim(workpiece.clone()) },
        Decision::RevokeResolution { bloom, workpiece: workpiece.clone() },
        advance_stage(bloom, workpiece.clone()),
        Decision::RecordStageCatalog { bloom, catalog: stage_catalog() },
        Decision::RecordEvidence {
            bloom,
            evidence: Evidence { subject: digest(6), kind: EvidenceKind::VerificationResult, detail: digest(7) },
        },
        Decision::AdvanceMainline { from: digest(8), to: digest(9) },
        Decision::DispatchLand { bloom, expected_base: digest(8), new_head: digest(10) },
        Decision::EmitReceipt(ProjectedReceipt {
            receipt: LandingReceipt { bloom, previous_base: digest(8), new_head: digest(10) },
            members: vec![workpiece.clone()],
        }),
        Decision::RecordObservation { head: digest(10) },
        Decision::RecordAggregateRoll { bloom, rolls: 1 },
        Decision::RecordAggregateVerifyRoll { bloom, rolls: 2 },
        Decision::RecordLandingRoll { bloom, rolls: 3 },
        Decision::RecordWedge {
            bloom,
            workpiece: workpiece.clone(),
            wedge: Wedge {
                stage: StageId::Verify,
                evidence: digest(11),
                repeated_verifiers: [VerifyFailure::Fmt, VerifyFailure::Dup].into_iter().collect(),
            },
        },
        Decision::MarkSuperseded { bloom, by: successor },
    ]
}

fn execution_records(bloom: BloomId, successor: BloomId, workpiece: &WorkpieceId) -> Vec<Decision> {
    vec![
        dispatch_attempt(bloom, workpiece.clone()),
        Decision::RedispatchStage {
            bloom,
            question: digest(26),
            answer: digest(27),
            words: vec![0xde, 0xad, 0xbe, 0xef],
        },
        Decision::ReleaseHold { bloom, question: digest(26) },
        Decision::DispatchIntegration {
            bloom,
            base: digest(28),
            members: vec![MemberCandidate { workpiece: workpiece.clone(), candidate: digest(29) }],
            adopt_from: Some(successor),
        },
        Decision::RecordIntegration {
            bloom,
            integration: Some(FoldedIntegration { tree: digest(30), head: digest(31), lineage: vec![digest(32)] }),
        },
        dispatch_aggregate_review(bloom),
        Decision::RecordReviewPark { bloom, question: Some(digest(33)) },
        dispatch_aggregate_verify(bloom),
        set_resolved(bloom, workpiece.clone()),
        Decision::SetUnresolved { bloom },
        Decision::RecordVerifyProof { bloom, proof: verify_proof() },
        Decision::RecordVerifyReuse {
            bloom,
            reuse: VerifyReuse { stage: StageId::AggregateVerify, proof: verify_proof() },
        },
        Decision::RecordOrphanClaimRelease {
            request: digest(25),
            target: orphan_claim_release(workpiece.clone(), bloom),
            completion: Some(OrphanClaimReleaseCompletion::Changed { observed_holder: successor }),
        },
        Decision::DispatchOrphanClaimRelease {
            request: digest(34),
            target: orphan_claim_release(workpiece.clone(), successor),
        },
        record_composition_finding(bloom, workpiece.clone()),
        record_adjudication(bloom),
        record_operator_repair(bloom, workpiece.clone()),
    ]
}

/// Representative [`Decisions`] value whose wire bytes the golden fixture pins.
///
/// This is the one vocabulary the fixture command and the golden guards share.
#[must_use]
pub fn representative() -> Decisions {
    let bloom = BloomId(digest(1));
    let successor = BloomId(digest(9));
    let workpiece = WorkpieceId("alpha".into());
    let mut effects = bloom_lifecycle_records(bloom, successor, &workpiece);
    effects.extend(execution_records(bloom, successor, &workpiece));
    effects.extend(brake_records(bloom, workpiece.clone()));
    effects.extend(spend_quiesce_records(bloom));
    effects.push(record_member_dependencies(bloom, workpiece.clone()));
    effects.extend(host_fault_records(bloom, workpiece.clone()));
    effects.push(record_candidate_vehicle(bloom, workpiece.clone()));
    effects.extend(brake_aggregates(bloom));
    effects.push(dispatch_splice(bloom, workpiece.clone(), successor));
    effects.push(record_member_machinery(bloom, workpiece.clone()));
    effects.extend(withdrawal_records(bloom, workpiece.clone()));
    effects.extend(gate_passes(bloom));
    effects.extend(refusal_records(bloom, workpiece));
    effects.extend(base_verify_records());
    effects.extend(proposal_records());
    effects.push(dispatch_study(bloom));
    effects.push(Decision::RecordPipelineManifest { bloom, manifest: pipeline_manifest() });
    effects.extend(precheck_records(bloom));
    effects.extend(coordination_records(bloom));

    Decisions { outcome: Outcome::Sealed(bloom), effects }
}

fn overlap_members() -> Vec<WorkpieceId> {
    vec![WorkpieceId("alpha".into()), WorkpieceId("beta".into())]
}

fn overlap_intersection() -> Vec<String> {
    vec!["crates/aether-bloomery/**".into(), "docs/adr/**".into()]
}

/// Seal-door overlap warning as a [`Decisions`] row (`Outcome::SurfaceOverlap`).
///
/// Shared with [`surface_overlap_event`]: the same two collections are written
/// to both persisted columns.
#[must_use]
pub fn surface_overlap_decisions() -> Decisions {
    Decisions {
        outcome: Outcome::SurfaceOverlap { members: overlap_members(), intersection: overlap_intersection() },
        effects: Vec::new(),
    }
}

/// Seal-door overlap warning as an [`Event`] (`Fact::SurfaceOverlap`).
#[must_use]
pub fn surface_overlap_event() -> Event {
    Event {
        idempotency_key: IdempotencyKey("seal:alpha:beta:surface-overlap".into()),
        fact: Fact::SurfaceOverlap { members: overlap_members(), intersection: overlap_intersection() },
    }
}

/// Containment-refused event whose wire bytes the golden fixture pins.
#[must_use]
pub fn containment_refused_event() -> Event {
    Event {
        idempotency_key: IdempotencyKey("verify:alpha:containment-refused".into()),
        fact: Fact::ContainmentRefused {
            bloom: BloomId(digest(1)),
            workpiece: WorkpieceId("alpha".into()),
            evidence: Evidence { subject: digest(2), kind: EvidenceKind::VerificationResult, detail: digest(3) },
            failed_verifiers: VerifyFailureSet::one(VerifyFailure::Containment),
            violating_paths: vec!["crates/other/src/lib.rs".into()],
        },
    }
}
