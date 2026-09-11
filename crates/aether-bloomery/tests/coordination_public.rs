//! Public reducer tripwires for optional coordination and contextual claims.

#![allow(clippy::unwrap_used)]

mod common;

use aether_bloomery::reduce::CoordinationError;
use aether_bloomery::{
    AgentProfile, BloomId, BloomRecord, BloomSpec, CandidatePreparation, CandidatePreparationPlan, CandidateRef,
    CompositionContractTemplate, CompositionInput, CompositionPlan, ConfigRegistry, ConstructContext,
    ConstructionAdmission, ConstructionCheckpoint, ContextualAttemptDispatch, ContextualInvocationTemplate,
    ContextualResolutionClaim, CoordinationPolicy, CoordinationState, Decision, Digest, Evidence, EvidenceKind,
    ExecutionLimits, Fact, GenerationMember, Harness, IntegrationHead, MemberContractPin, MemberPin,
    MemberVerifyOutcome, MemberVerifyRequest, NetworkProfile, Nonce, Outcome, PreparedCandidate, ReasoningEffort,
    ResolutionProof, SharedRunMode, SharedRunNode, SharedRunPhase, SharedRunPlan, SharedRunRecord, Snapshot,
    SpendWindow, StageCatalog, StageId, ToolPolicy, Transformation, VerificationContract, VerificationMode,
    VerificationObligation, construction_nonce_digest, reduce,
};
use aether_data::wire::to_vec;
use common::{compiled_resolved, digest, draft, event, membership, step, workpiece};

fn candidate(tree: u8, checkout: u8) -> CandidateRef {
    CandidateRef { tree: digest(tree), checkout: digest(checkout) }
}

fn profile() -> AgentProfile {
    AgentProfile {
        harness: Harness::Grok,
        model: String::from("grok"),
        effort: ReasoningEffort::Low,
        tools: ToolPolicy::None,
    }
}

fn transformation(subject: CandidateRef, diff_base: CandidateRef) -> Transformation {
    Transformation {
        command: String::from("verify.member"),
        inputs: vec![subject.tree],
        checkout: subject.checkout,
        diff_base: Some(diff_base.checkout),
        outputs: Vec::new(),
        image: String::from("iama/verify:1"),
        limits: ExecutionLimits { wall_clock_secs: 60 },
        network: NetworkProfile::None,
        description: None,
        model: None,
    }
}

fn invocation_template() -> ContextualInvocationTemplate {
    ContextualInvocationTemplate {
        command: String::from("verify.check"),
        extra_inputs: Vec::new(),
        diff_base: Some(digest(1)),
        outputs: Vec::new(),
        image: String::from("iama/verify:1"),
        limits: ExecutionLimits { wall_clock_secs: 60 },
        network: NetworkProfile::None,
        description: None,
        model: None,
        profile: profile(),
        configs: ConfigRegistry::default(),
    }
}

fn coordination_state(spec: &BloomSpec) -> CoordinationState {
    let policy = CoordinationPolicy {
        verification: VerificationMode::Contextual,
        eager_integration: true,
        max_run_members: 4,
        max_serial_requests: 4,
        max_attribution_probes: 4,
        movement_budget: 2,
        reservation_millis: 1_000,
        host_class: String::from("linux-x86_64"),
    };
    let template = CompositionContractTemplate {
        gate_set: digest(30),
        gate_identities: vec![String::from("verify.clippy")],
        invocation: invocation_template(),
        environment: digest(31),
        host_class: digest(32),
    };
    let members = spec
        .members()
        .iter()
        .map(|member| GenerationMember { workpiece: member.workpiece.clone(), scope_revision: member.scope_revision })
        .collect();
    CoordinationState::new(
        policy,
        template,
        spec.id(),
        CandidateRef { tree: spec.base(), checkout: spec.base() },
        members,
    )
}

fn request(bloom: BloomId, pin: &MemberPin, base: CandidateRef) -> MemberVerifyRequest {
    let transformation = transformation(pin.candidate, base);
    MemberVerifyRequest {
        bloom,
        member: pin.clone(),
        input: CompositionInput { node: pin.candidate.tree, candidate: pin.candidate, members: vec![pin.clone()] },
        attempt: 0,
        context: None,
        contract: VerificationContract {
            gate_set: digest(40),
            obligations: vec![
                VerificationObligation::Gate { identity: String::from("verify.clippy") },
                VerificationObligation::MemberDelta {
                    scope_revision: pin.scope_revision,
                    candidate: pin.candidate,
                    diff_base: base,
                },
            ],
            diff_base: base,
            invocation: digest(41),
            environment: digest(42),
            host_class: digest(43),
        },
        transformation,
        profile: profile(),
        configs: ConfigRegistry::default(),
    }
}

fn contextual_snapshot() -> (Snapshot, BloomId, CandidateRef, Vec<Digest>) {
    let spec = draft(1, vec![membership("alpha", 10), membership("beta", 11)]).seal();
    let bloom = spec.id();
    let base = CandidateRef { tree: spec.base(), checkout: spec.base() };
    let mut state = coordination_state(&spec);
    let base_head = state.integration.head.clone();
    let alpha = MemberPin { workpiece: workpiece("alpha"), scope_revision: digest(10), candidate: candidate(50, 51) };
    let beta = MemberPin { workpiece: workpiece("beta"), scope_revision: digest(11), candidate: candidate(52, 53) };
    let mut requests = vec![request(bloom, &alpha, base), request(bloom, &beta, base)];
    for request in &mut requests {
        request.contract.environment = state.composition_contract.environment;
        request.contract.host_class = state.composition_contract.host_class;
    }
    let contract = state.composition_contract.bind(
        requests
            .iter()
            .map(|request| MemberContractPin { request: request.digest(), contract: request.contract.digest() })
            .collect(),
    );
    let inputs = requests.iter().map(|request| request.input.clone()).collect::<Vec<_>>();
    let composition = CompositionPlan {
        bloom,
        base: base_head,
        inputs: inputs.clone(),
        requests: requests.clone(),
        contract: contract.clone(),
    };
    let plan = SharedRunPlan {
        mode: SharedRunMode::Contextual,
        requests: requests.clone(),
        composition: Some(composition),
        probe_budget: 4,
        execution_attempt: 0,
    };
    let final_root = candidate(70, 71);
    let node =
        SharedRunNode { plan: plan.digest(), candidate: final_root, coverage: vec![alpha.clone(), beta.clone()] };
    state.runs.push(SharedRunRecord {
        plan: plan.clone(),
        node: Some(node.clone()),
        phase: SharedRunPhase::Terminal,
        stale: false,
        physical_run: Some(digest(72)),
        completed: requests
            .iter()
            .map(|request| MemberVerifyOutcome::PassedIn {
                request: request.digest(),
                node: node.digest(),
                receipt: Evidence {
                    subject: final_root.tree,
                    kind: EvidenceKind::VerificationResult,
                    detail: digest(73),
                },
            })
            .collect(),
        unfinished: Vec::new(),
        latencies: Vec::new(),
    });
    for request in &requests {
        state.claims.insert(
            request.member.workpiece.0.clone(),
            ContextualResolutionClaim {
                member: request.member.clone(),
                proof: ResolutionProof::InComposition {
                    node: node.digest(),
                    receipt: Evidence {
                        subject: final_root.tree,
                        kind: EvidenceKind::VerificationResult,
                        detail: digest(73),
                    },
                    plan: plan.digest(),
                    request: request.digest(),
                    contract: contract.digest(),
                },
            },
        );
    }
    let generation = state.integration.generation.digest();
    state.integration.admitted.clone_from(&inputs);
    state.integration.head = IntegrationHead {
        generation,
        node: node.digest(),
        candidate: final_root,
        plan: digest(74),
        coverage: vec![alpha, beta],
    };

    let mut snapshot = Snapshot::new(spec.base());
    snapshot.active.extend(spec.members().iter().map(|member| (member.workpiece.clone(), bloom)));
    snapshot.blooms.insert(bloom, BloomRecord { coordination: Some(Box::new(state)), ..BloomRecord::empty(spec) });
    let lineage = inputs.into_iter().map(|input| input.candidate.tree).collect();
    (snapshot, bloom, final_root, lineage)
}

fn assert_not_ready(snapshot: &Snapshot, bloom: BloomId, tree: CandidateRef, lineage: Vec<Digest>) {
    let decisions = reduce(
        snapshot,
        &event("resolve", Fact::Resolve { bloom, tree: tree.tree, head: tree.checkout, lineage }),
        &compiled_resolved(),
        &SpendWindow::default(),
    );
    assert_eq!(decisions.outcome, Outcome::CoordinationRejected(CoordinationError::NotReady));
    assert!(decisions.effects.is_empty());
}

#[test]
fn absent_coordination_policy_preserves_legacy_verify_dispatch_bytes() {
    let spec = draft(1, vec![membership("alpha", 10)]).seal();
    let bloom = spec.id();
    let base = Snapshot::new(digest(1)).with_green_base(digest(1));
    let (snapshot, _) = step(&base, &event("seal", Fact::Seal(spec.clone())));
    assert!(snapshot.blooms[&bloom].coordination.is_none());

    let captured = candidate(20, 21);
    let (_, decisions) = step(
        &snapshot,
        &event(
            "construct-passed",
            Fact::AttemptCompleted {
                bloom,
                workpiece: workpiece("alpha"),
                stage: StageId::Construct,
                passed: true,
                evidence: Evidence { subject: digest(80), kind: EvidenceKind::VerificationResult, detail: digest(81) },
                candidate: Some(captured),
            },
        ),
    );
    let actual = decisions
        .effects
        .iter()
        .find(|effect| matches!(effect, Decision::DispatchAttempt { stage: StageId::Verify, .. }))
        .expect("legacy Verify dispatch");
    let binding = StageCatalog::binding_of(StageId::Verify);
    let expected = Decision::DispatchAttempt {
        bloom,
        workpiece: workpiece("alpha"),
        stage: StageId::Verify,
        transformation: Transformation::for_member_stage(&binding, captured.tree, captured.checkout, spec.base()),
        scope_revision: digest(10),
        candidate: Some(captured.tree),
        profile: binding.profile,
        configs: spec.members()[0].configs.layered_over(spec.configs()),
    };
    assert_eq!(to_vec(actual).unwrap(), to_vec(&expected).unwrap());
}

#[test]
fn candidate_preparation_rejects_a_stale_same_scope_candidate_after_replacement() {
    let spec = draft(1, vec![membership("alpha", 10)]).seal();
    let bloom = spec.id();
    let base = Snapshot::new(digest(1)).with_green_base(digest(1));
    let (mut snapshot, _) = step(&base, &event("seal", Fact::Seal(spec.clone())));
    let authored = candidate(20, 21);
    let replacement = candidate(22, 23);
    let mut state = coordination_state(&spec);
    let context = ConstructContext { bloom_base: candidate(1, 1), starting_head: state.integration.head.clone() };
    let plan = CandidatePreparationPlan {
        bloom,
        workpiece: workpiece("alpha"),
        scope_revision: digest(10),
        authored,
        context: context.clone(),
    };
    state.preparations.push(plan.clone());
    let record = snapshot.blooms.get_mut(&bloom).unwrap();
    let progress = record.progress.get_mut(&workpiece("alpha")).unwrap();
    progress.stage = StageId::Reconcile;
    progress.candidate = Some(replacement);
    record.coordination = Some(Box::new(state));

    let decisions = reduce(
        &snapshot,
        &event(
            "stale-preparation",
            Fact::CandidatePrepared {
                bloom,
                plan: plan.digest(),
                preparation: CandidatePreparation::Prepared(PreparedCandidate {
                    authored,
                    candidate: candidate(24, 25),
                    context,
                    diff_base: candidate(1, 1),
                }),
            },
        ),
        &compiled_resolved(),
        &SpendWindow::default(),
    );
    assert_eq!(decisions.outcome, Outcome::CoordinationRejected(CoordinationError::InvalidPlan));
    assert!(decisions.effects.is_empty());
}

#[test]
fn contextual_resolution_requires_the_exact_final_root_coverage_and_claims() {
    let (snapshot, bloom, final_root, lineage) = contextual_snapshot();
    let accepted = reduce(
        &snapshot,
        &event(
            "exact-resolve",
            Fact::Resolve { bloom, tree: final_root.tree, head: final_root.checkout, lineage: lineage.clone() },
        ),
        &compiled_resolved(),
        &SpendWindow::default(),
    );
    assert!(matches!(accepted.outcome, Outcome::AggregateVerifyReused { bloom: owner, .. } if owner == bloom));
    assert!(!accepted.effects.iter().any(|effect| matches!(effect, Decision::DispatchAggregateVerify { .. })));

    let mut held = snapshot.clone();
    held.blooms.get_mut(&bloom).unwrap().holds.insert(digest(76));
    let held = reduce(
        &held,
        &event(
            "held-resolve",
            Fact::Resolve { bloom, tree: final_root.tree, head: final_root.checkout, lineage: lineage.clone() },
        ),
        &compiled_resolved(),
        &SpendWindow::default(),
    );
    assert!(matches!(held.outcome, Outcome::ResolveRejected(aether_bloomery::ResolveError::PendingDecision { .. })));
    assert!(held.effects.iter().any(|effect| matches!(effect, Decision::RecordRefusal { .. })));

    let mut at_ceiling = snapshot.clone();
    at_ceiling.blooms.get_mut(&bloom).unwrap().aggregate_verify_rolls = u32::MAX;
    let at_ceiling = reduce(
        &at_ceiling,
        &event(
            "ceiling-resolve",
            Fact::Resolve { bloom, tree: final_root.tree, head: final_root.checkout, lineage: lineage.clone() },
        ),
        &compiled_resolved(),
        &SpendWindow::default(),
    );
    assert!(matches!(
        at_ceiling.outcome,
        Outcome::ResolveRejected(aether_bloomery::ResolveError::ReviewCeiling { .. })
    ));
    assert!(at_ceiling.effects.iter().any(|effect| matches!(effect, Decision::RecordRefusal { .. })));

    assert_not_ready(
        &snapshot,
        bloom,
        CandidateRef { tree: digest(75), checkout: final_root.checkout },
        lineage.clone(),
    );

    let mut wrong_coverage = snapshot.clone();
    let coverage =
        &mut wrong_coverage.blooms.get_mut(&bloom).unwrap().coordination.as_mut().unwrap().integration.head.coverage;
    let duplicate = coverage.first().unwrap().clone();
    *coverage.last_mut().unwrap() = duplicate;
    assert_not_ready(&wrong_coverage, bloom, final_root, lineage.clone());

    let mut stale_claim = snapshot;
    let state = stale_claim.blooms.get_mut(&bloom).unwrap().coordination.as_mut().unwrap();
    let claim = state.claims.get_mut("alpha").unwrap();
    let ResolutionProof::InComposition { request, .. } = &mut claim.proof else {
        panic!("contextual fixture carries contextual claims");
    };
    *request = digest(99);
    assert_not_ready(&stale_claim, bloom, final_root, lineage);
}

#[test]
fn a_contextual_aggregate_proof_needs_the_whole_settled_run_it_names() {
    let (snapshot, bloom, _, _) = contextual_snapshot();
    let settled = snapshot.blooms[&bloom].coordination.as_deref().unwrap().clone();
    let head = settled.integration.head.clone();
    assert!(settled.contextual_aggregate_proof(&head).is_some());

    let mut displaced = settled.clone();
    displaced.runs[0].stale = true;
    assert!(displaced.contextual_aggregate_proof(&head).is_none());

    let mut unreached = settled.clone();
    let pending = unreached.runs[0].plan.requests[0].digest();
    unreached.runs[0].unfinished.push(pending);
    assert!(unreached.contextual_aggregate_proof(&head).is_none());

    let mut drifted = settled.clone();
    drifted.runs[0].plan.composition.as_mut().unwrap().contract.gate_set = digest(99);
    assert!(drifted.contextual_aggregate_proof(&head).is_none());

    let mut truncated = settled;
    truncated.runs[0].completed.pop();
    assert!(truncated.contextual_aggregate_proof(&head).is_none());
}

#[test]
fn construction_checkpoint_requires_the_admitted_physical_nonce() {
    let spec = draft(1, vec![membership("alpha", 10)]).seal();
    let bloom = spec.id();
    let base = Snapshot::new(digest(1)).with_green_base(digest(1));
    let (mut snapshot, _) = step(&base, &event("seal", Fact::Seal(spec.clone())));
    let mut state = coordination_state(&spec);
    let context = ConstructContext {
        bloom_base: state.integration.generation.base,
        starting_head: state.integration.head.clone(),
    };
    let binding = StageCatalog::binding_of(StageId::Construct);
    let dispatch = ContextualAttemptDispatch {
        bloom,
        workpiece: workpiece("alpha"),
        stage: StageId::Construct,
        attempt: 1,
        transformation: Transformation::for_member_stage(
            &binding,
            digest(10),
            context.starting_head.candidate.checkout,
            context.starting_head.candidate.checkout,
        ),
        scope_revision: digest(10),
        candidate: None,
        profile: binding.profile,
        configs: spec.members()[0].configs.layered_over(spec.configs()),
        context,
    };
    state.queued_construction.insert(String::from("alpha"), dispatch.clone());
    snapshot.blooms.get_mut(&bloom).expect("sealed bloom").coordination = Some(Box::new(state));
    let nonce = Nonce(String::from("construct/alpha/1"));
    let admission = ConstructionAdmission { nonce: construction_nonce_digest(&nonce), dispatch };
    let (admitted, decisions) = step(
        &snapshot,
        &event("admit-construction", Fact::RequestConstructionAdmission { admission: admission.clone() }),
    );
    assert!(matches!(decisions.outcome, Outcome::CoordinationAdvanced { bloom: owner, .. } if owner == bloom));

    let checkpoint = ConstructionCheckpoint {
        bloom,
        workpiece: workpiece("alpha"),
        scope_revision: digest(10),
        nonce: construction_nonce_digest(&Nonce(String::from("construct/alpha/replacement"))),
        observation: 1,
        starting_checkout: admission.dispatch.context.starting_head.candidate.checkout,
        candidate: candidate(20, 21),
    };
    let rejected = reduce(
        &admitted,
        &event("wrong-construction-nonce", Fact::ConstructionCheckpointObserved { checkpoint: checkpoint.clone() }),
        &compiled_resolved(),
        &SpendWindow::default(),
    );
    assert_eq!(rejected.outcome, Outcome::CoordinationRejected(CoordinationError::NotReady));
    assert!(rejected.effects.is_empty());

    let accepted = reduce(
        &admitted,
        &event(
            "admitted-construction-nonce",
            Fact::ConstructionCheckpointObserved {
                checkpoint: ConstructionCheckpoint { nonce: admission.nonce, ..checkpoint },
            },
        ),
        &compiled_resolved(),
        &SpendWindow::default(),
    );
    assert!(matches!(accepted.outcome, Outcome::CoordinationAdvanced { bloom: owner, .. } if owner == bloom));
}
