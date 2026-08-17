//! The control core (ADR-0149 §The control core).
//!
//! One pure function — [`reduce`] — owns every state transition. Events are
//! admitted facts with idempotency keys; decisions are value objects
//! destined for a transactional outbox; **side effects never occur inside
//! the reducer**. The journal plus the content-addressed artifact bytes are
//! the only truth, and a [`Snapshot`] is the rebuildable projection the
//! reducer reads.
//!
//! [`reduce`] *decides* — it reads a snapshot and returns [`Decisions`]. It
//! never mutates the snapshot. [`Snapshot::apply`] *evolves* — it folds a
//! decided event's effects into the next snapshot. A live admission is
//! `reduce` then `apply`; journal replay is `apply` alone over the decisions
//! each row recorded at admission (ADR-0190) — the record is what was
//! decided, and re-deciding history under a newer reducer rewrites it.
//!
//! The active-membership uniqueness constraint (at most one active bloom per
//! workpiece) lives in the store in production (ADR-0149 §The control core);
//! the reducer enforces the same rule over its projection so seal decisions
//! are correct before the store transaction commits.

mod aggregate_verify;
mod attempt;
mod composition;
mod decision;
mod error;
mod event;
mod evidence;
mod fold_conflict;
mod grant;
mod integrate;
mod land;
mod landing;
mod observe;
mod operator;
mod operator_hold;
mod orphan_claim;
mod outcome;
mod readiness;
mod review;
mod seal;
mod snapshot;
mod splice;
mod verify;
mod verify_memo;
mod view;

pub use decision::Decision;
pub use error::{
    AdjudicationError, AdmitEvidenceError, AdoptAnswerError, AggregateReviewError, AggregateVerifyError,
    AttemptCompletedError, BaseMismatch, FoldConflictError, GrantAttemptsError, HostFaultError, IntegrateError,
    LandError, LandingRejectedError, MemberExecutorFaultError, OperatorHoldError, OperatorRepairError,
    OrphanClaimReleaseError, ResolveError, SealConflict, SealError, SupersedeError, VerifyFailedError,
};
pub use event::{Event, Fact};
pub use outcome::{DECISIONS_SCHEMA, Decisions, DecisionsSchemaError, Outcome, decode_recorded_decisions};
pub use seal::is_active_unlanded;
pub use snapshot::{
    AggregateReviewFault, BloomRecord, BloomStatus, FoldedIntegration, HostFaultHold, MemberMachineryFault, Snapshot,
    StageProgress,
};
pub use view::view_of;

use crate::values::{ResolvedConfigs, SpendWindow};

use aggregate_verify::reduce_aggregate_verify_completed;
use attempt::{reduce_attempt_completed, reduce_member_executor_fault};
use evidence::{reduce_admit_evidence, reduce_adopt_answer};
use fold_conflict::reduce_fold_conflict;
use grant::reduce_grant_attempts;
use integrate::{reduce_integrate, reduce_resolve};
use land::reduce_land;
use landing::reduce_landing_rejected;
use observe::{reduce_observe_mainline, reduce_observe_mainline_diverged};
use operator::{reduce_operator_adjudication, reduce_operator_repair};
use operator_hold::{reduce_operator_hold, reduce_operator_release};
use orphan_claim::{reduce_complete_orphan_claim_release, reduce_request_orphan_claim_release};
use review::{reduce_aggregate_review_completed, reduce_aggregate_review_executor_fault};
use seal::{reduce_seal, reduce_supersede, reduce_surface_overlap};
use verify::{reduce_resume_host_fault, reduce_verify_failed, reduce_verify_host_fault};

/// Reduce one event against a snapshot into decisions. Pure: reads the
/// snapshot, returns decisions, mutates nothing (ADR-0149 §The control core).
///
/// `configs` is the sealed configuration content the caller resolved before
/// reducing (ADR-0174). The reducer seals a registry of addresses on its own but
/// cannot fetch what one names, so a configuration it must *read* arrives as an
/// argument — see [`ResolvedConfigs`]. A caller that supplies less than the event
/// needs gets a refusal naming the kind, never a silent fall-through to a
/// default, so an under-filled set is a loud caller bug rather than a bloom
/// quietly running an unattested configuration.
///
/// `spend` is the window's measured spend the caller resolved before reducing
/// (ADR-0192). The reducer compares two numbers at the seal door but cannot
/// fetch the priced column behind a study-record digest, so the measurement
/// arrives as an argument — see [`SpendWindow`]. A caller with nothing to
/// measure passes the default, which is an empty window that never quiesces
/// against a present positive ceiling.
///
/// [`Snapshot::apply`] still takes `configs` so a pre-[`Decision::RecordStageCatalog`]
/// row can resolve a spec-sealed catalog the way it always did; a newly sealed
/// bloom's catalog arrives as a recorded effect and the fold does not re-read
/// configuration for it.
#[must_use]
pub fn reduce(snapshot: &Snapshot, event: &Event, configs: &ResolvedConfigs, spend: &SpendWindow) -> Decisions {
    if snapshot.seen.contains(&event.idempotency_key) {
        return Decisions::rejected(Outcome::Duplicate);
    }
    match &event.fact {
        Fact::Seal(spec) => reduce_seal(snapshot, spec, configs, spend, &[]),
        Fact::Supersede { predecessor, successor } => reduce_supersede(snapshot, predecessor, successor, configs, &[]),
        Fact::GraphSeal { predecessor: None, spec, edges } => reduce_seal(snapshot, spec, configs, spend, edges),
        Fact::GraphSeal { predecessor: Some(predecessor), spec, edges } => {
            reduce_supersede(snapshot, predecessor, spec, configs, edges)
        }
        Fact::Integrate { bloom, claim } => reduce_integrate(snapshot, bloom, claim),
        Fact::AdmitEvidence { bloom, evidence } => reduce_admit_evidence(snapshot, bloom, evidence),
        Fact::AdoptAnswer { bloom, answer } => reduce_adopt_answer(snapshot, bloom, answer),
        Fact::AttemptCompleted { bloom, workpiece, stage, passed, evidence, candidate } => {
            reduce_attempt_completed(snapshot, bloom, workpiece, *stage, *passed, evidence, *candidate)
        }
        Fact::Resolve { bloom, tree, head, lineage } => reduce_resolve(snapshot, bloom, tree, head, lineage),
        Fact::AggregateReviewCompleted { bloom, passed, evidence, implicated } => {
            reduce_aggregate_review_completed(snapshot, bloom, *passed, evidence, implicated)
        }
        Fact::AggregateVerifyCompleted { bloom, passed, evidence } => {
            reduce_aggregate_verify_completed(snapshot, bloom, *passed, evidence)
        }
        Fact::LandingRejected { bloom, evidence } => reduce_landing_rejected(snapshot, bloom, evidence),
        Fact::Land { bloom, new_head } => reduce_land(snapshot, bloom, new_head),
        Fact::ObserveMainline { head } => reduce_observe_mainline(snapshot, head),
        Fact::ObserveMainlineDiverged { head } => reduce_observe_mainline_diverged(snapshot, head),
        Fact::GrantAttempts { bloom, workpiece, stage, attempts } => {
            reduce_grant_attempts(snapshot, bloom, workpiece, *stage, *attempts)
        }
        Fact::VerifyFailed { bloom, workpiece, evidence, failed_verifiers } => {
            reduce_verify_failed(snapshot, bloom, workpiece, evidence, *failed_verifiers)
        }
        Fact::RequestOrphanClaimRelease { request, authorization } => {
            reduce_request_orphan_claim_release(snapshot, request, authorization)
        }
        Fact::CompleteOrphanClaimRelease { request, completion } => {
            reduce_complete_orphan_claim_release(snapshot, request, *completion)
        }
        Fact::AggregateReviewExecutorFault { bloom, evidence } => {
            reduce_aggregate_review_executor_fault(snapshot, bloom, evidence)
        }
        Fact::FoldConflict { bloom, workpiece, checkpoint, head, evidence } => {
            reduce_fold_conflict(snapshot, bloom, workpiece, *checkpoint, *head, evidence)
        }
        Fact::OperatorAdjudication { bloom, adjudication } => {
            reduce_operator_adjudication(snapshot, bloom, adjudication)
        }
        Fact::OperatorRepair { bloom, repair } => reduce_operator_repair(snapshot, bloom, repair),
        Fact::OperatorHold { bloom, hold } => reduce_operator_hold(snapshot, bloom, hold),
        Fact::OperatorRelease { bloom, release } => reduce_operator_release(snapshot, bloom, release),
        Fact::SurfaceOverlap { members, intersection } => reduce_surface_overlap(members, intersection),
        Fact::VerifyHostFault { bloom, workpiece, evidence, findings } => {
            reduce_verify_host_fault(snapshot, bloom, workpiece, evidence, findings)
        }
        Fact::ResumeHostFault { bloom, workpiece } => reduce_resume_host_fault(snapshot, bloom, workpiece),
        Fact::MemberExecutorFault { bloom, workpiece, stage, evidence } => {
            reduce_member_executor_fault(snapshot, bloom, workpiece, *stage, evidence)
        }
    }
}
