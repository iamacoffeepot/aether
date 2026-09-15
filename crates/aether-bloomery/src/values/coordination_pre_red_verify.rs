//! Frozen pre-red-verify wire shapes of [`CoordinationPolicy`] and
//! [`CoordinationState`] (ADR-0187 / ADR-0218 §Amendment: low tolerance).
//!
//! The low-tolerance amendment appended `red_verify` to the sealed policy. The
//! wire encoding is positional and untagged, so a policy sealed without that
//! field cannot be read by a decoder that expects it — it runs out of bytes —
//! and a journaled [`CoordinationState`] that carries the old policy sits in
//! the middle of `Decision::RecordCoordinationState`, so the same missing field
//! shifts every later byte of that variant. This module freezes the nine-field
//! policy and the coordination state that embeds it so those rows upcast
//! instead of aborting replay. Never edit these fields: a later policy change
//! adds its own frozen mirror beside this one.
//!
//! These types exist to *decode*. The identities themselves are the pinned
//! digest literals in the persisted registry, never computed from these types
//! (#5500). Embedding live leaf types is sound for decoding exactly as long as
//! they only ever gain tail-appended enum variants.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use super::{
    CandidatePreparationPlan, CompatibilityPreviewPlan, CompatibilityPreviewRecord, CompositionContractTemplate,
    ConstructContext, ConstructionAdmission, ConstructionCheckpoint, ContextualAttemptDispatch,
    ContextualResolutionClaim, CoordinationDiagnostic, CoordinationPolicy, CoordinationState, EagerIntegrationState,
    MemberVerifyRequest, PartialHeadRepairPlan, PreparedCandidate, RedVerify, SharedRunRecord, SurvivorGroup,
    VerificationMode,
};

/// Pre-amendment [`CoordinationPolicy`]: nine fields, no red-verify disposition.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct CoordinationPolicyPreRedVerify {
    pub verification: VerificationMode,
    pub eager_integration: bool,
    pub max_run_members: u32,
    pub max_serial_requests: u32,
    pub max_attribution_probes: u32,
    pub movement_budget: u32,
    pub reservation_millis: u64,
    pub host_class: String,
    pub coalesce_millis: Option<u64>,
}

impl From<CoordinationPolicyPreRedVerify> for CoordinationPolicy {
    /// Carry a pre-amendment policy forward on [`RedVerify::Eject`].
    ///
    /// Deliberately not the [`RedVerify::Refine`] such a bloom actually ran
    /// under. The knob is a standing operator instruction rather than a record
    /// of what a bloom once did, and the instruction of 2026-09-15 is that a
    /// member which has not gone green leaves rather than buying another lap.
    /// A bloom that wants the old loop seals `Refine` explicitly.
    fn from(prior: CoordinationPolicyPreRedVerify) -> Self {
        Self {
            verification: prior.verification,
            eager_integration: prior.eager_integration,
            max_run_members: prior.max_run_members,
            max_serial_requests: prior.max_serial_requests,
            max_attribution_probes: prior.max_attribution_probes,
            movement_budget: prior.movement_budget,
            reservation_millis: prior.reservation_millis,
            host_class: prior.host_class,
            coalesce_millis: prior.coalesce_millis,
            red_verify: RedVerify::Eject,
        }
    }
}

/// Pre-amendment [`CoordinationState`]: the policy field is
/// [`CoordinationPolicyPreRedVerify`]. Every other field matches today's
/// layout.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct CoordinationStatePreRedVerify {
    pub policy: CoordinationPolicyPreRedVerify,
    pub composition_contract: CompositionContractTemplate,
    pub integration: EagerIntegrationState,
    pub requests: Vec<MemberVerifyRequest>,
    pub runs: Vec<SharedRunRecord>,
    pub claims: BTreeMap<String, ContextualResolutionClaim>,
    pub prepared: BTreeMap<String, PreparedCandidate>,
    pub preparations: Vec<CandidatePreparationPlan>,
    pub contexts: BTreeMap<String, ConstructContext>,
    pub checkpoints: BTreeMap<String, ConstructionCheckpoint>,
    pub queued_construction: BTreeMap<String, ContextualAttemptDispatch>,
    pub admitted_construction: BTreeMap<String, ConstructionAdmission>,
    pub preview_plans: Vec<CompatibilityPreviewPlan>,
    pub previews: Vec<CompatibilityPreviewRecord>,
    pub diagnostics: Vec<CoordinationDiagnostic>,
    pub survivor_groups: Vec<SurvivorGroup>,
    pub partial_head_repair: Option<PartialHeadRepairPlan>,
    pub partial_head_repair_attempts: u32,
    pub final_in_flight: bool,
    pub final_dispatched: bool,
}

impl From<CoordinationStatePreRedVerify> for CoordinationState {
    fn from(prior: CoordinationStatePreRedVerify) -> Self {
        Self {
            policy: CoordinationPolicy::from(prior.policy),
            composition_contract: prior.composition_contract,
            integration: prior.integration,
            requests: prior.requests,
            runs: prior.runs,
            claims: prior.claims,
            prepared: prior.prepared,
            preparations: prior.preparations,
            contexts: prior.contexts,
            checkpoints: prior.checkpoints,
            queued_construction: prior.queued_construction,
            admitted_construction: prior.admitted_construction,
            preview_plans: prior.preview_plans,
            previews: prior.previews,
            diagnostics: prior.diagnostics,
            survivor_groups: prior.survivor_groups,
            partial_head_repair: prior.partial_head_repair,
            partial_head_repair_attempts: prior.partial_head_repair_attempts,
            final_in_flight: prior.final_in_flight,
            final_dispatched: prior.final_dispatched,
        }
    }
}
