//! The agent profile: a digest-addressed, versioned policy artifact naming
//! *how* a stage runs (ADR-0149 §The value vocabulary / §The line).
//!
//! A [`StageBinding`](crate::values::StageBinding) names *who* runs a stage —
//! the derived `iama-{stage}` worker identity ([`StageId::worker_identity`]) —
//! and references an [`AgentProfile`] by digest for *how* it runs: the model,
//! its reasoning effort, and its tool policy. The profile's content-addressed
//! digest is its version: a recalibration is a new digest, so a distinct
//! configuration is a distinct artifact — exactly ADR-0149's "versioned policy
//! artifact." A [`StageReceipt`](crate::values::StageReceipt) attests the exact
//! profile digest that ran, making "a configured agent profile ran one process"
//! a claim a reader can verify against the named artifact.
//!
//! [`StageId::worker_identity`]: crate::ids::StageId::worker_identity

use alloc::string::String;
use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use crate::digest::{ContentAddressed, Digest, digest_of};

/// How a stage's agent runs: model, reasoning effort, and tool policy. A
/// digest-addressed, versioned policy artifact (ADR-0149 §The value
/// vocabulary) — its [`digest`](Self::digest) *is* its version, so a changed
/// calibration is a new profile and a distinct configuration is a distinct
/// artifact.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct AgentProfile {
    /// The model identity the stage runs under (e.g. `claude-opus-4-8`).
    pub model: String,
    /// The reasoning-effort tier the harness runs the model at.
    pub effort: ReasoningEffort,
    /// The tools the stage's agent is permitted to reach.
    pub tools: ToolPolicy,
}

impl ContentAddressed for AgentProfile {
    const DOMAIN: &'static str = "aether.bloomery.agent_profile";
}

impl AgentProfile {
    /// The profile's content-addressed digest — its version: the value a
    /// [`StageBinding`](crate::values::StageBinding) references and a
    /// [`StageReceipt`](crate::values::StageReceipt) attests, so a reader can
    /// verify which configuration ran by recomputing this address.
    #[must_use]
    pub fn digest(&self) -> Digest {
        digest_of(self)
    }
}

/// The reasoning-effort tier the harness runs a model at — the harness effort
/// levels. A stage pins its tier once at calibration.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Serialize, Deserialize)]
pub enum ReasoningEffort {
    /// Minimal reasoning — the cheapest tier.
    Low,
    /// The default balanced tier.
    Medium,
    /// Deeper reasoning for design-adjacent stages.
    High,
    /// Extra-high reasoning, above `High` and below `Max`.
    XHigh,
    /// The deepest tier the harness offers.
    Max,
}

/// The tools a profile's agent may reach. Kept minimal and extensible for v1:
/// a named bounding tier today, a finer [`Allow`](Self::Allow) list when
/// neither tier fits. A new variant is a new digest, never a silent
/// reinterpretation of a stored profile.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum ToolPolicy {
    /// No tools — a pure reasoning stage.
    None,
    /// Read-only tools: inspect, never mutate.
    ReadOnly,
    /// The full tool surface the stage's process needs.
    Full,
    /// An explicit allowlist of tool names — the escape hatch when neither
    /// named tier fits.
    Allow(Vec<String>),
}

#[cfg(test)]
mod tests {
    use super::*;

    // Tripwire: the digest of a fixed AgentProfile. Computed over the profile's
    // canonical aether-wire bytes under its DOMAIN tag, so it drifts the moment
    // the content-addressing logic, the DOMAIN string, or the profile's field
    // layout changes. Recompute-and-repin only when such a change is intended.
    const GOLDEN_PROFILE_DIGEST: [u8; 32] = [
        0x5d, 0x34, 0xe7, 0x04, 0xe5, 0x51, 0xc3, 0xd0, 0xe3, 0x38, 0x19, 0xc0, 0x67, 0x3d, 0x01, 0xd2, 0x7d, 0xb1,
        0x72, 0xe7, 0x5d, 0x98, 0xd5, 0xa9, 0x56, 0x28, 0x11, 0x05, 0x26, 0x3a, 0x5e, 0xe6,
    ];

    #[test]
    fn agent_profile_digest_matches_pinned_golden() {
        let profile = AgentProfile {
            model: String::from("claude-opus-4-8"),
            effort: ReasoningEffort::High,
            tools: ToolPolicy::Full,
        };
        assert_eq!(
            *profile.digest().as_bytes(),
            GOLDEN_PROFILE_DIGEST,
            "AgentProfile content addressing drifted from the pinned golden digest"
        );
    }
}
