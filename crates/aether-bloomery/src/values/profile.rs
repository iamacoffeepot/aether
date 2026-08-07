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

/// How a stage's agent runs: harness, model, reasoning effort, and tool policy.
/// A digest-addressed, versioned policy artifact (ADR-0149 §The value
/// vocabulary) — its [`digest`](Self::digest) *is* its version, so a changed
/// calibration is a new profile and a distinct configuration is a distinct
/// artifact.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct AgentProfile {
    /// The harness that runs the model — which CLI the model lane forks.
    pub harness: Harness,
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

/// The harness a model lane forks — *which* agent CLI executes the stage, as
/// opposed to the model it executes under. A closed set, so a stored profile
/// can never name a harness the executor has no arm for, and a new harness is a
/// compile error at every match rather than an unrecognized string at dispatch.
///
/// Orthogonal to [`model`](AgentProfile::model): the harness selects the
/// process the runner spawns and the transcript shape the result record is
/// derived from, while the model selects what that process runs. The mechanical
/// lanes name no harness at all — they run a compiler.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Harness {
    /// The Claude Code CLI, headless.
    Claude,
    /// The Codex CLI, headless.
    Codex,
    /// The Muse Code CLI, headless.
    Muse,
}

impl Harness {
    /// The harness's runner-facing name — the exact token the executor renders
    /// onto the model lane's `--harness` argv and the transform entrypoint
    /// parses back, so the calibrated harness reaches the child verbatim rather
    /// than through a second spelling the runner invents (the convention
    /// [`ReasoningEffort::as_str`] already sets).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Muse => "muse",
        }
    }

    /// The harness a runner-facing name denotes, or `None` when the name
    /// matches no arm. The inverse of [`as_str`](Self::as_str) — the parse the
    /// transform entrypoint runs over its `--harness` flag, fail-closed so an
    /// unrecognized spelling is a legible refusal rather than a silent fallback
    /// to whichever harness happens to be first.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            "muse" => Some(Self::Muse),
            _ => None,
        }
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

impl ReasoningEffort {
    /// The tier's harness-facing name — the exact token the model lane hands
    /// the Claude Code CLI's `--effort` flag, so a calibrated tier reaches the
    /// child verbatim rather than through a second spelling the runner invents.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }
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
    // Repinned for #4578: the profile gains its `harness` field, which changes
    // the canonical bytes of every profile including this fixture's.
    const GOLDEN_PROFILE_DIGEST: [u8; 32] = [
        0x79, 0x57, 0x38, 0x07, 0x86, 0xca, 0x49, 0x48, 0xd0, 0x3d, 0xfb, 0xad, 0xe8, 0x7e, 0xb1, 0x00, 0x78, 0x38,
        0x91, 0x56, 0xe4, 0x9b, 0xf3, 0xc4, 0xd0, 0xc4, 0xf1, 0xf2, 0x02, 0xd7, 0x0a, 0xaa,
    ];

    #[test]
    fn agent_profile_digest_matches_pinned_golden() {
        let profile = AgentProfile {
            harness: Harness::Claude,
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

    // The two directions of the harness's runner-facing spelling are independent
    // matches, so an arm added to one and forgotten in the other renders a
    // harness the transform entrypoint then refuses to parse — a dispatch that
    // fails at the child rather than at the calibration. Round-tripping every
    // variant is what catches the half-added arm.
    #[test]
    fn every_harness_round_trips_through_its_runner_facing_name() {
        for harness in [Harness::Claude, Harness::Codex, Harness::Muse] {
            assert_eq!(
                Harness::from_name(harness.as_str()),
                Some(harness),
                "{harness:?} does not round-trip through its runner-facing name",
            );
        }
        assert_eq!(Harness::from_name("gpt"), None, "an unrecognized name is a legible refusal, not a fallback");
    }
}
