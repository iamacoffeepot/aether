//! Shared seat fold for the calibration and metrics ledgers.
//!
//! Both ledgers walk the same dispatch decisions and resolve the same agent.
//! The fold lives once so a mechanical lane cannot mint a seat in one table
//! while the other refuses it, and so an unpriced study record cannot be
//! summed as free in one table while the other counts it apart.

use alloc::string::String;

use crate::digest::Digest;
use crate::ids::{BloomId, StageId};
use crate::reduce::Decision;
use crate::values::{
    AgentProfile, ConfigRegistry, ConfigScopes, DispatchKey, ModelOverride, ResolvedConfigs, ResolvedModel,
    is_model_lane,
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
    /// The member or bloom-review dispatch this effect is, if it is one.
    pub fn from_effect(effect: &'a Decision) -> Option<Self> {
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
            } => Some(Self {
                bloom: *bloom,
                key: DispatchKey::Member { workpiece: workpiece.clone(), stage: *stage },
                stage: *stage,
                workpiece: workpiece.0.clone(),
                command: &transformation.command,
                profile,
                registry: configs,
                displayed: candidate.unwrap_or(*scope_revision),
            }),
            Decision::DispatchAggregateReview { bloom, transformation, profile, configs, .. } => {
                let displayed = transformation.inputs.first().copied()?;
                Some(Self {
                    bloom: *bloom,
                    key: DispatchKey::Bloom { stage: StageId::AggregateReview },
                    stage: StageId::AggregateReview,
                    workpiece: String::new(),
                    command: &transformation.command,
                    profile,
                    registry: configs,
                    displayed,
                })
            }
            _ => None,
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

/// A priced dollar column, or `None` when the record is unpriced.
///
/// `cost == 0` is unpriced, never free: a missing price row must not enter a
/// sum or a mean as a zero.
pub fn priced_micro_usd(cost_micro_usd: u64) -> Option<u64> {
    (cost_micro_usd > 0).then_some(cost_micro_usd)
}
