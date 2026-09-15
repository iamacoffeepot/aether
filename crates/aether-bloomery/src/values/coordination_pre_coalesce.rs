//! Frozen pre-coalesce wire shapes of [`CoordinationPolicy`] and
//! [`CoordinationState`] (ADR-0187 / #5947).
//!
//! #5947 appended `coalesce_millis` to the sealed policy. The wire encoding is
//! positional and untagged, so a policy sealed without that field cannot be
//! read by a decoder that expects it — it runs out of bytes — and a journaled
//! [`CoordinationState`] that carries the old policy sits in the middle of
//! `Decision::RecordCoordinationState`, so the same missing field shifts every
//! later byte of that variant. This module freezes the eight-field policy and
//! the coordination state that embeds it so those rows upcast instead of
//! aborting replay. Never edit these fields: a later policy change adds its
//! own frozen mirror beside this one.
//!
//! These types exist to *decode*. The identities themselves are the pinned
//! digest literals in the persisted registry, never computed from these types
//! (#5500). Embedding live leaf types is sound for decoding exactly as long as
//! they only ever gain tail-appended enum variants.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use super::coordination_pre_red_verify::CoordinationPolicyPreRedVerify;
use super::{
    CandidatePreparationPlan, CompatibilityPreviewPlan, CompatibilityPreviewRecord, CompositionContractTemplate,
    ConstructContext, ConstructionAdmission, ConstructionCheckpoint, ContextualAttemptDispatch,
    ContextualResolutionClaim, CoordinationDiagnostic, CoordinationPolicy, CoordinationState, EagerIntegrationState,
    MemberVerifyRequest, PartialHeadRepairPlan, PreparedCandidate, SharedRunRecord, SurvivorGroup, VerificationMode,
};

/// Pre-#5947 [`CoordinationPolicy`]: eight fields, no coalescing hold.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct CoordinationPolicyPreCoalesce {
    pub verification: VerificationMode,
    pub eager_integration: bool,
    pub max_run_members: u32,
    pub max_serial_requests: u32,
    pub max_attribution_probes: u32,
    pub movement_budget: u32,
    pub reservation_millis: u64,
    pub host_class: String,
}

impl From<CoordinationPolicyPreCoalesce> for CoordinationPolicy {
    /// Carry a pre-coalesce policy forward with the hold absent.
    ///
    /// Absent is [`super::DEFAULT_COALESCE_MILLIS`]: a bloom sealed before the
    /// field existed still has sibling constructs finishing minutes apart, and
    /// inventing a zero hold here would keep the per-member runs the field
    /// exists to stop.
    fn from(prior: CoordinationPolicyPreCoalesce) -> Self {
        // Chained through the next era's frozen shape rather than filling
        // today's fields directly: each era decides exactly the field it
        // introduced, so a third change adds one hop instead of another copy
        // of every decision before it.
        Self::from(CoordinationPolicyPreRedVerify {
            verification: prior.verification,
            eager_integration: prior.eager_integration,
            max_run_members: prior.max_run_members,
            max_serial_requests: prior.max_serial_requests,
            max_attribution_probes: prior.max_attribution_probes,
            movement_budget: prior.movement_budget,
            reservation_millis: prior.reservation_millis,
            host_class: prior.host_class,
            coalesce_millis: None,
        })
    }
}

/// Pre-#5947 [`CoordinationState`]: the policy field is
/// [`CoordinationPolicyPreCoalesce`]. Every other field matches today's
/// layout.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct CoordinationStatePreCoalesce {
    pub policy: CoordinationPolicyPreCoalesce,
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

impl From<CoordinationStatePreCoalesce> for CoordinationState {
    fn from(prior: CoordinationStatePreCoalesce) -> Self {
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
