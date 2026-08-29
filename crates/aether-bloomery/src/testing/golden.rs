//! Representative wire values for the golden decision fixtures.
//!
//! These constructors are the one vocabulary the fixture command and the
//! golden guards share: `cargo xtask fixtures regen` encodes them, and the
//! tests compare those bytes to the checked-in files.

use crate::ids::{BloomId, IdempotencyKey, StageId, WorkpieceId};
use crate::port::{ClaimRefKind, ProjectedReceipt};
use crate::reduce::{
    Decision, Decisions, Event, Fact, FoldedIntegration, Outcome, RecordedRead, RecordedRefusal, StageProgress,
};
use crate::values::{
    Adjudication, AgentProfile, BaseReceipt, BaseVerdict, CandidateRef, CompositionFinding, ConfigRegistry,
    Disposition, Evidence, EvidenceKind, ExecutionLimits, Harness, LandingReceipt, MemberCandidate, MemberDependency,
    NetworkProfile, OperatorHold, OperatorProposal, OperatorRepair, OrphanClaimRelease, OrphanClaimReleaseCompletion,
    ReasoningEffort, ResolutionClaim, ResolvedBloom, ResolvedModel, SpendQuiesce, StageBinding, StageCatalog,
    ToolPolicy, Transformation, VerifyFailure, VerifyFailureSet, VerifyGateSet, VerifyProof, VerifyReuse, Wedge,
    Withdrawal, WithdrawalCause,
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

/// Representative [`Decisions`] value whose wire bytes the golden fixture pins.
///
/// This is the one vocabulary the fixture command and the golden guards share.
#[must_use]
pub fn representative() -> Decisions {
    let bloom = BloomId(digest(1));
    let successor = BloomId(digest(9));
    let workpiece = WorkpieceId("alpha".into());
    Decisions {
        outcome: Outcome::Sealed(bloom),
        effects: vec![
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
        .into_iter()
        .chain(brake_records(bloom, workpiece.clone()))
        .chain(spend_quiesce_records(bloom))
        .chain([record_member_dependencies(bloom, workpiece.clone())])
        .chain(host_fault_records(bloom, workpiece.clone()))
        .chain([record_candidate_vehicle(bloom, workpiece.clone())])
        .chain(brake_aggregates(bloom))
        .chain([dispatch_splice(bloom, workpiece.clone(), successor)])
        .chain([record_member_machinery(bloom, workpiece.clone())])
        .chain(withdrawal_records(bloom, workpiece.clone()))
        .chain(gate_passes(bloom))
        .chain(refusal_records(bloom, workpiece))
        .chain(base_verify_records())
        .chain(proposal_records())
        .collect(),
    }
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
