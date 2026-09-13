//! Shared seat fold for the calibration and metrics ledgers.
//!
//! Both ledgers walk the same dispatch decisions and resolve the same agent.
//! The fold lives once so a mechanical lane cannot mint a seat in one table
//! while the other refuses it, and so an unpriced study record cannot be
//! summed as free in one table while the other counts it apart.

use alloc::string::String;
use alloc::vec::Vec;

use crate::digest::Digest;
use crate::ids::{BloomId, StageId, WorkpieceId};
use crate::reduce::Decision;
use crate::values::{
    AgentProfile, ConfigRegistry, ConfigScopes, DispatchKey, ModelOverride, ResolvedConfigs, ResolvedModel,
    SharedRunDispatch, SharedRunExecution, is_model_lane,
};

/// One dispatch as both seat ledgers fold it.
pub struct SeatDispatch<'a> {
    pub bloom: BloomId,
    pub key: DispatchKey,
    pub stage: StageId,
    pub workpiece: String,
    pub command: &'a str,
    pub profile: &'a AgentProfile,
    pub registry: &'a ConfigRegistry,
    pub displayed: Digest,
}

impl<'a> SeatDispatch<'a> {
    /// The seats this effect dispatched — empty for an effect that runs no
    /// lane, and one per member request for a physical run that executes
    /// several member transformations in one slot.
    ///
    /// Aggregate review keeps its persisted [`DispatchKey::Bloom`] slot and
    /// stage identity. The timeline workpiece is the composition (ADR-0191):
    /// painting it as an empty bloom-level row beside the composition cursor
    /// was a second tail for the same subject.
    ///
    /// The ADR-0218 vocabulary folds beside the older one because it dispatches
    /// the same work under new names: a contextual attempt is a member attempt
    /// whose checkout inherits an eager integration head, a shared run is the
    /// member Verify fan-out grouped into one physical execution, and a
    /// partial-head repair is the composition's own Refine re-entry. Each keys
    /// the slot its pre-ADR-0218 counterpart keys, so a bloom that runs the new
    /// machinery has the timeline, dispatch rows, and seats the old one had.
    ///
    /// Three families deliberately fold to nothing.
    /// [`Decision::DispatchIntegrationAppend`],
    /// [`Decision::DispatchCandidatePreparation`],
    /// [`Decision::DispatchSharedRunPreparation`] and
    /// [`Decision::DispatchCompatibilityPreview`] carry neither a command nor a
    /// profile: they ask the source to move a tree between dispatches, so there
    /// is no seat to recompute, and a fabricated command would enter the shared
    /// calibration fold as a lane that never ran.
    /// [`Decision::DispatchAggregateVerify`] and its speculative twin
    /// [`Decision::DispatchPrecheck`] are the whole-bloom mechanical gate, which
    /// has always stayed off these rows; admitting the pre-check copy while the
    /// gate itself mints nothing would count the rehearsal and not the run.
    /// [`Decision::DispatchBaseVerify`] names a base commit and no bloom, and
    /// every row here is bloom-keyed, so folding it would charge one
    /// whole-workspace gate run to whichever bloom happened to seal on it.
    pub fn from_effect(effect: &'a Decision) -> Vec<Self> {
        match effect {
            Decision::DispatchAttempt {
                bloom,
                workpiece,
                stage,
                transformation,
                scope_revision,
                candidate,
                profile,
                configs,
            } => alloc::vec![Self {
                bloom: *bloom,
                key: DispatchKey::Member { workpiece: workpiece.clone(), stage: *stage },
                stage: *stage,
                workpiece: workpiece.0.clone(),
                command: &transformation.command,
                profile,
                registry: configs,
                displayed: candidate.unwrap_or(*scope_revision),
            }],
            Decision::DispatchContextualAttempt { dispatch } => alloc::vec![Self {
                bloom: dispatch.bloom,
                key: DispatchKey::Member { workpiece: dispatch.workpiece.clone(), stage: dispatch.stage },
                stage: dispatch.stage,
                workpiece: dispatch.workpiece.0.clone(),
                command: &dispatch.transformation.command,
                profile: &dispatch.profile,
                registry: &dispatch.configs,
                displayed: dispatch.candidate.unwrap_or(dispatch.scope_revision),
            }],
            // The composition repairs its own red head at the `Refine` binding
            // its plan was budgeted against, and the composition keys a member
            // slot like any other workpiece (ADR-0191).
            Decision::DispatchPartialHeadRepair { dispatch } => alloc::vec![Self {
                bloom: dispatch.plan.bloom,
                key: DispatchKey::Member { workpiece: WorkpieceId::composition(), stage: StageId::Refine },
                stage: StageId::Refine,
                workpiece: String::from(WorkpieceId::COMPOSITION),
                command: &dispatch.transformation.command,
                profile: &dispatch.profile,
                registry: &dispatch.configs,
                displayed: dispatch.plan.head.candidate.tree,
            }],
            Decision::DispatchSharedRun { dispatch } => shared_run_seats(dispatch),
            Decision::DispatchAggregateReview { bloom, transformation, profile, configs, .. } => transformation
                .inputs
                .first()
                .map(|displayed| Self {
                    bloom: *bloom,
                    key: DispatchKey::Bloom { stage: StageId::AggregateReview },
                    stage: StageId::AggregateReview,
                    workpiece: String::from(WorkpieceId::COMPOSITION),
                    command: &transformation.command,
                    profile,
                    registry: configs,
                    displayed: *displayed,
                })
                .into_iter()
                .collect(),
            // The retrospect reader is a bloom-level model lane against the
            // receipt its landing produced (ADR-0216), so it seats like the
            // critic rather than vanishing into the study records it writes.
            Decision::DispatchStudy { bloom, transformation, profile, configs } => transformation
                .inputs
                .first()
                .map(|displayed| Self {
                    bloom: *bloom,
                    key: DispatchKey::Bloom { stage: StageId::Study },
                    stage: StageId::Study,
                    workpiece: String::from(WorkpieceId::COMPOSITION),
                    command: &transformation.command,
                    profile,
                    registry: configs,
                    displayed: *displayed,
                })
                .into_iter()
                .collect(),
            _ => Vec::new(),
        }
    }

    /// Whether the sealed command is a model lane.
    pub fn is_model_lane(&self) -> bool {
        is_model_lane(self.command)
    }

    /// The sealed catalog profile with the member's override resolved over it.
    pub fn agent(&self, configs: &ResolvedConfigs) -> ResolvedModel {
        configs
            .resolve::<ModelOverride>(ConfigScopes::bloom_wide(self.registry))
            .ok()
            .flatten()
            .unwrap_or_default()
            .resolve(self.stage, self.profile)
    }
}

/// The seats one approved physical run dispatched (ADR-0218).
///
/// A serial run — standalone or a warm lease — executes each logical request's
/// own member transformation in the slot, so every request is the member Verify
/// span its pre-ADR-0218 [`Decision::DispatchAttempt`] would have been. A
/// contextual run executes one combined gate over the immutable composed node
/// instead: one composition-level Verify span, not one per covered member,
/// because one execution happened and charging it to each member it covers
/// would multiply a single run across the ledger. It is still the members'
/// Verify — not the whole-bloom aggregate gate — so it keeps the `Verify` stage
/// against the composition workpiece.
fn shared_run_seats(dispatch: &SharedRunDispatch) -> Vec<SeatDispatch<'_>> {
    match &dispatch.execution {
        SharedRunExecution::Serial => dispatch
            .plan
            .requests
            .iter()
            .map(|request| SeatDispatch {
                bloom: request.bloom,
                key: DispatchKey::Member { workpiece: request.member.workpiece.clone(), stage: StageId::Verify },
                stage: StageId::Verify,
                workpiece: request.member.workpiece.0.clone(),
                command: &request.transformation.command,
                profile: &request.profile,
                registry: &request.configs,
                displayed: request.member.candidate.tree,
            })
            .collect(),
        SharedRunExecution::Contextual { node, transformation, profile, configs } => dispatch
            .plan
            .composition
            .as_ref()
            .map(|composition| SeatDispatch {
                bloom: composition.bloom,
                key: DispatchKey::Member { workpiece: WorkpieceId::composition(), stage: StageId::Verify },
                stage: StageId::Verify,
                workpiece: String::from(WorkpieceId::COMPOSITION),
                command: &transformation.command,
                profile,
                registry: configs,
                displayed: node.candidate.tree,
            })
            .into_iter()
            .collect(),
    }
}

/// A priced dollar column, or `None` when the record is unpriced.
///
/// `cost == 0` is unpriced, never free: a missing price row must not enter a
/// sum or a mean as a zero.
pub fn priced_micro_usd(cost_micro_usd: u64) -> Option<u64> {
    (cost_micro_usd > 0).then_some(cost_micro_usd)
}
