//! Frozen v1 wire shape of journaled [`Decisions`] (ADR-0187).
//!
//! #5330 appended `reconcile_assembles_base` to [`StageProgress`], which sits
//! inside [`Decision::AdvanceStage`] mid-row — a positional codec cannot treat
//! that field as missing. This module freezes the pre-#5330 shape so a v1
//! (or unstamped) journal row upcasts instead of aborting replay. Never edit
//! these types: a later shape change adds a v2 mirror.

use alloc::string::String;
use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use super::{Decision, Decisions, FoldedIntegration, Outcome, StageProgress};
use crate::digest::Digest;
use crate::ids::{BloomId, StageId, WorkpieceId};
use crate::port::ProjectedReceipt;
use crate::values::{
    Adjudication, AgentProfile, CandidateRef, CompositionFinding, ConfigRegistry, Evidence, MemberCandidate,
    MemberDependency, OperatorHold, OperatorRepair, OrphanClaimRelease, OrphanClaimReleaseCompletion, ResolutionClaim,
    ResolvedBloom, SpendQuiesce, StageCatalog, Transformation, VerifyFailureSet, VerifyProof, VerifyReuse, Wedge,
};

/// Pre-#5330 [`StageProgress`]: seven fields, no `reconcile_assembles_base`.
#[derive(aether_data::Schema, Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct StageProgressV1 {
    pub stage: StageId,
    pub attempts: u32,
    pub candidate: Option<CandidateRef>,
    pub repair_rolls: u32,
    pub seen_verify_failures: VerifyFailureSet,
    #[serde(default)]
    pub fold_checkpoint: Option<Digest>,
    #[serde(default)]
    pub fold_conflict_evidence: Option<Digest>,
}

impl From<StageProgressV1> for StageProgress {
    fn from(progress: StageProgressV1) -> Self {
        Self {
            stage: progress.stage,
            attempts: progress.attempts,
            candidate: progress.candidate,
            repair_rolls: progress.repair_rolls,
            seen_verify_failures: progress.seen_verify_failures,
            fold_checkpoint: progress.fold_checkpoint,
            fold_conflict_evidence: progress.fold_conflict_evidence,
            reconcile_assembles_base: false,
        }
    }
}

/// Pre-#5330 [`Decision`]. Discriminants match [`Decision`] in declaration order;
/// `AdvanceStage.progress` is [`StageProgressV1`].
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum DecisionV1 {
    ClaimMembership {
        workpiece: WorkpieceId,
        bloom: BloomId,
    },
    ReleaseMembership {
        workpiece: WorkpieceId,
        bloom: BloomId,
    },
    InheritClaim {
        bloom: BloomId,
        claim: ResolutionClaim,
    },
    RecordResolution {
        bloom: BloomId,
        claim: ResolutionClaim,
    },
    RecordEvidence {
        bloom: BloomId,
        evidence: Evidence,
    },
    MarkSuperseded {
        bloom: BloomId,
        by: BloomId,
    },
    SetResolved {
        bloom: BloomId,
        resolved: ResolvedBloom,
    },
    AdvanceMainline {
        from: Digest,
        to: Digest,
    },
    EmitReceipt(ProjectedReceipt),
    ReleaseHold {
        bloom: BloomId,
        question: Digest,
    },
    RedispatchStage {
        bloom: BloomId,
        question: Digest,
        answer: Digest,
        words: Vec<u8>,
    },
    DispatchAttempt {
        bloom: BloomId,
        workpiece: WorkpieceId,
        stage: StageId,
        transformation: Transformation,
        scope_revision: Digest,
        candidate: Option<Digest>,
        profile: AgentProfile,
        configs: ConfigRegistry,
    },
    AdvanceStage {
        bloom: BloomId,
        workpiece: WorkpieceId,
        progress: StageProgressV1,
    },
    DispatchLand {
        bloom: BloomId,
        expected_base: Digest,
        new_head: Digest,
    },
    DispatchIntegration {
        bloom: BloomId,
        base: Digest,
        members: Vec<MemberCandidate>,
        adopt_from: Option<BloomId>,
    },
    RecordIntegration {
        bloom: BloomId,
        integration: Option<FoldedIntegration>,
    },
    RecordAggregateRoll {
        bloom: BloomId,
        rolls: u32,
    },
    RevokeResolution {
        bloom: BloomId,
        workpiece: WorkpieceId,
    },
    DispatchAggregateReview {
        bloom: BloomId,
        transformation: Transformation,
        roll: u32,
        profile: AgentProfile,
        configs: ConfigRegistry,
    },
    RecordReviewPark {
        bloom: BloomId,
        question: Option<Digest>,
    },
    RecordWedge {
        bloom: BloomId,
        workpiece: WorkpieceId,
        wedge: Wedge,
    },
    DispatchAggregateVerify {
        bloom: BloomId,
        transformation: Transformation,
        roll: u32,
        profile: AgentProfile,
    },
    RecordAggregateVerifyRoll {
        bloom: BloomId,
        rolls: u32,
    },
    RecordLandingRoll {
        bloom: BloomId,
        rolls: u32,
    },
    SetUnresolved {
        bloom: BloomId,
    },
    RecordObservation {
        head: Digest,
    },
    RecordOrphanClaimRelease {
        request: Digest,
        target: OrphanClaimRelease,
        completion: Option<OrphanClaimReleaseCompletion>,
    },
    DispatchOrphanClaimRelease {
        request: Digest,
        target: OrphanClaimRelease,
    },
    RecordVerifyProof {
        bloom: BloomId,
        proof: VerifyProof,
    },
    RecordVerifyReuse {
        bloom: BloomId,
        reuse: VerifyReuse,
    },
    RecordStageCatalog {
        bloom: BloomId,
        catalog: StageCatalog,
    },
    RecordCompositionFinding {
        bloom: BloomId,
        finding: CompositionFinding,
    },
    RecordAdjudication {
        bloom: BloomId,
        adjudication: Adjudication,
    },
    RecordOperatorRepair {
        bloom: BloomId,
        repair: OperatorRepair,
    },
    RecordOperatorHold {
        bloom: BloomId,
        hold: OperatorHold,
    },
    RecordOperatorRelease {
        bloom: BloomId,
        release: OperatorHold,
    },
    DeferDispatch {
        bloom: BloomId,
        workpiece: WorkpieceId,
    },
    RecordSpendQuiesce {
        quiesce: Option<SpendQuiesce>,
    },
    RecordMemberDependencies {
        bloom: BloomId,
        edges: Vec<MemberDependency>,
    },
    RecordHostFault {
        bloom: BloomId,
        workpiece: WorkpieceId,
        findings: String,
        evidence: Digest,
    },
    ClearHostFault {
        bloom: BloomId,
        workpiece: WorkpieceId,
    },
    RecordCandidateVehicle {
        bloom: BloomId,
        workpiece: WorkpieceId,
        vehicle: CandidateRef,
    },
    DeferAggregate {
        bloom: BloomId,
        stage: StageId,
    },
    DispatchSplice {
        bloom: BloomId,
        workpiece: WorkpieceId,
        base: Digest,
        members: Vec<MemberCandidate>,
        adopt_from: Option<BloomId>,
    },
    RecordMemberMachinery {
        bloom: BloomId,
        workpiece: WorkpieceId,
        stage: StageId,
        rolls: u32,
        evidence: Digest,
    },
}

impl From<DecisionV1> for Decision {
    fn from(decision: DecisionV1) -> Self {
        match decision {
            DecisionV1::ClaimMembership { workpiece, bloom } => Self::ClaimMembership { workpiece, bloom },
            DecisionV1::ReleaseMembership { workpiece, bloom } => Self::ReleaseMembership { workpiece, bloom },
            DecisionV1::InheritClaim { bloom, claim } => Self::InheritClaim { bloom, claim },
            DecisionV1::RecordResolution { bloom, claim } => Self::RecordResolution { bloom, claim },
            DecisionV1::RecordEvidence { bloom, evidence } => Self::RecordEvidence { bloom, evidence },
            DecisionV1::MarkSuperseded { bloom, by } => Self::MarkSuperseded { bloom, by },
            DecisionV1::SetResolved { bloom, resolved } => Self::SetResolved { bloom, resolved },
            DecisionV1::AdvanceMainline { from, to } => Self::AdvanceMainline { from, to },
            DecisionV1::EmitReceipt(value) => Self::EmitReceipt(value),
            DecisionV1::ReleaseHold { bloom, question } => Self::ReleaseHold { bloom, question },
            DecisionV1::RedispatchStage { bloom, question, answer, words } => {
                Self::RedispatchStage { bloom, question, answer, words }
            }
            DecisionV1::DispatchAttempt {
                bloom,
                workpiece,
                stage,
                transformation,
                scope_revision,
                candidate,
                profile,
                configs,
            } => Self::DispatchAttempt {
                bloom,
                workpiece,
                stage,
                transformation,
                scope_revision,
                candidate,
                profile,
                configs,
            },
            DecisionV1::AdvanceStage { bloom, workpiece, progress } => {
                Self::AdvanceStage { bloom, workpiece, progress: progress.into() }
            }
            DecisionV1::DispatchLand { bloom, expected_base, new_head } => {
                Self::DispatchLand { bloom, expected_base, new_head }
            }
            DecisionV1::DispatchIntegration { bloom, base, members, adopt_from } => {
                Self::DispatchIntegration { bloom, base, members, adopt_from }
            }
            DecisionV1::RecordIntegration { bloom, integration } => Self::RecordIntegration { bloom, integration },
            DecisionV1::RecordAggregateRoll { bloom, rolls } => Self::RecordAggregateRoll { bloom, rolls },
            DecisionV1::RevokeResolution { bloom, workpiece } => Self::RevokeResolution { bloom, workpiece },
            DecisionV1::DispatchAggregateReview { bloom, transformation, roll, profile, configs } => {
                Self::DispatchAggregateReview { bloom, transformation, roll, profile, configs }
            }
            DecisionV1::RecordReviewPark { bloom, question } => Self::RecordReviewPark { bloom, question },
            DecisionV1::RecordWedge { bloom, workpiece, wedge } => Self::RecordWedge { bloom, workpiece, wedge },
            DecisionV1::DispatchAggregateVerify { bloom, transformation, roll, profile } => {
                Self::DispatchAggregateVerify { bloom, transformation, roll, profile }
            }
            DecisionV1::RecordAggregateVerifyRoll { bloom, rolls } => Self::RecordAggregateVerifyRoll { bloom, rolls },
            DecisionV1::RecordLandingRoll { bloom, rolls } => Self::RecordLandingRoll { bloom, rolls },
            DecisionV1::SetUnresolved { bloom } => Self::SetUnresolved { bloom },
            DecisionV1::RecordObservation { head } => Self::RecordObservation { head },
            DecisionV1::RecordOrphanClaimRelease { request, target, completion } => {
                Self::RecordOrphanClaimRelease { request, target, completion }
            }
            DecisionV1::DispatchOrphanClaimRelease { request, target } => {
                Self::DispatchOrphanClaimRelease { request, target }
            }
            DecisionV1::RecordVerifyProof { bloom, proof } => Self::RecordVerifyProof { bloom, proof },
            DecisionV1::RecordVerifyReuse { bloom, reuse } => Self::RecordVerifyReuse { bloom, reuse },
            DecisionV1::RecordStageCatalog { bloom, catalog } => Self::RecordStageCatalog { bloom, catalog },
            DecisionV1::RecordCompositionFinding { bloom, finding } => {
                Self::RecordCompositionFinding { bloom, finding }
            }
            DecisionV1::RecordAdjudication { bloom, adjudication } => Self::RecordAdjudication { bloom, adjudication },
            DecisionV1::RecordOperatorRepair { bloom, repair } => Self::RecordOperatorRepair { bloom, repair },
            DecisionV1::RecordOperatorHold { bloom, hold } => Self::RecordOperatorHold { bloom, hold },
            DecisionV1::RecordOperatorRelease { bloom, release } => Self::RecordOperatorRelease { bloom, release },
            DecisionV1::DeferDispatch { bloom, workpiece } => Self::DeferDispatch { bloom, workpiece },
            DecisionV1::RecordSpendQuiesce { quiesce } => Self::RecordSpendQuiesce { quiesce },
            DecisionV1::RecordMemberDependencies { bloom, edges } => Self::RecordMemberDependencies { bloom, edges },
            DecisionV1::RecordHostFault { bloom, workpiece, findings, evidence } => {
                Self::RecordHostFault { bloom, workpiece, findings, evidence }
            }
            DecisionV1::ClearHostFault { bloom, workpiece } => Self::ClearHostFault { bloom, workpiece },
            DecisionV1::RecordCandidateVehicle { bloom, workpiece, vehicle } => {
                Self::RecordCandidateVehicle { bloom, workpiece, vehicle }
            }
            DecisionV1::DeferAggregate { bloom, stage } => Self::DeferAggregate { bloom, stage },
            DecisionV1::DispatchSplice { bloom, workpiece, base, members, adopt_from } => {
                Self::DispatchSplice { bloom, workpiece, base, members, adopt_from }
            }
            DecisionV1::RecordMemberMachinery { bloom, workpiece, stage, rolls, evidence } => {
                Self::RecordMemberMachinery { bloom, workpiece, stage, rolls, evidence }
            }
        }
    }
}

/// Pre-#5330 journaled [`Decisions`] blob.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct DecisionsV1 {
    pub outcome: Outcome,
    pub effects: Vec<DecisionV1>,
}

impl From<DecisionsV1> for Decisions {
    fn from(recorded: DecisionsV1) -> Self {
        Self { outcome: recorded.outcome, effects: recorded.effects.into_iter().map(Decision::from).collect() }
    }
}
