//! The per-workpiece model override (ADR-0149 §The line, #3511, ADR-0174).
//!
//! [`ModelOverride`] is a configuration kind sealed into a member's
//! [`ConfigRegistry`](crate::values::ConfigRegistry) — the bloom pins its
//! address, so what it names is frozen at seal and therefore attestable. The
//! runner lane and the coordinator both resolve the effective model + effort the
//! same way: each field of the override falls through to the stage's
//! [`AgentProfile`] default when unset, so "the model that ran is the model the
//! bloom promised" holds by construction.
//!
//! A workpiece's `scope_revision` stays an opaque [`Digest`](crate::Digest) naming approved
//! scope content. The override used to ride inside it for want of anywhere else
//! attested to live; the registry is that place, and the two are separate
//! concerns again.

use alloc::string::String;

use serde::{Deserialize, Serialize};

use crate::values::{AgentProfile, Harness, ReasoningEffort};

/// A per-workpiece override of *how* the model lanes run for this workpiece
/// (ADR-0149 §The line, #3511) — the successor of today's free-text `model:*`
/// label. Each field is optional: an unset field falls through to the stage
/// [`AgentProfile`] default at [resolution](Self::resolve), so an empty override
/// changes nothing and a set field pins exactly that value into the sealed
/// bloom.
///
/// This is the **configurable** face of the calibration. The stage catalog names
/// the line's defaults and changing it re-digests the catalog; this is how an
/// operator picks something else for one bloom, through the REST control API at
/// staging time, without editing the line. What deliberately does *not* exist is
/// an ambient env or config-file override (#4327 deleted exactly that): a knob
/// that overrode the sealed profile would let a receipt attest a model that
/// never ran. An override sealed into the scope revision is attestable — the
/// bloom pins its digest — so choice and attestation are not in tension.
#[derive(aether_data::Kind, aether_data::Schema, Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[kind(name = "aether.bloomery.model_override")]
pub struct ModelOverride {
    /// The harness and model to run this workpiece's model lanes under,
    /// overriding the stage profile's. `None` → the profile default.
    ///
    /// One field carrying both, never two independent ones: a model id belongs
    /// to the provider its harness talks to, so an override that set a model
    /// without its harness would hand the calibrated CLI an id it cannot
    /// resolve, and the run would die at the child with a remote, late error.
    /// Pairing them in the type makes that unrepresentable rather than merely
    /// discouraged.
    pub agent: Option<AgentSelection>,
    /// The reasoning effort, overriding the stage profile default. `None` → the
    /// profile default. Its own field because effort is harness-agnostic — every
    /// harness offers the tier ladder, so it composes with either agent.
    pub reasoning_effort: Option<ReasoningEffort>,
}

/// A harness and the model it runs — the unit a [`ModelOverride`] selects,
/// because neither half is meaningful against the other's provider.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct AgentSelection {
    /// The harness to fork.
    pub harness: Harness,
    /// The model id, which must be one `harness` can resolve.
    pub model: String,
}

impl ModelOverride {
    /// Resolve the effective harness + model + effort against a stage's profile
    /// default: each set field wins, each unset field falls through to
    /// `default`. The coordinator (when it dispatches) and the runner (when it
    /// reads the sealed scope-revision by digest) apply this identical rule, so
    /// what runs is exactly what the sealed scope-revision froze.
    #[must_use]
    pub fn resolve(&self, default: &AgentProfile) -> ResolvedModel {
        let (harness, model) = self
            .agent
            .as_ref()
            .map_or_else(|| (default.harness, default.model.clone()), |agent| (agent.harness, agent.model.clone()));
        ResolvedModel { harness, model, effort: self.reasoning_effort.unwrap_or(default.effort) }
    }
}

/// The effective harness + model + reasoning effort a model-lane attempt runs
/// under, after resolving a [`ModelOverride`] against its stage profile default.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct ResolvedModel {
    /// The harness the attempt's lane forks — the stage profile's, or the one
    /// the workpiece's [`AgentSelection`] named.
    pub harness: Harness,
    /// The model the attempt runs under, always one `harness` can resolve.
    pub model: String,
    /// The reasoning effort the attempt runs at.
    pub effort: ReasoningEffort,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::values::ToolPolicy;

    fn profile(model: &str, effort: ReasoningEffort) -> AgentProfile {
        AgentProfile { harness: Harness::Claude, model: String::from(model), effort, tools: ToolPolicy::Full }
    }

    // An unset override field falls through to the stage profile default; a set
    // field wins. This is the rule the coordinator and the runner both apply.
    #[test]
    fn resolve_falls_through_unset_fields_and_honors_set_ones() {
        let default = profile("claude-sonnet-5", ReasoningEffort::Medium);

        let empty = ModelOverride::default().resolve(&default);
        assert_eq!(empty.model, "claude-sonnet-5", "unset agent → profile default");
        assert_eq!(empty.harness, Harness::Claude, "unset agent → profile harness too");
        assert_eq!(empty.effort, ReasoningEffort::Medium, "unset effort → profile default");

        let both = ModelOverride {
            agent: Some(AgentSelection { harness: Harness::Claude, model: String::from("claude-opus-4-8") }),
            reasoning_effort: Some(ReasoningEffort::High),
        }
        .resolve(&default);
        assert_eq!(both.model, "claude-opus-4-8", "set agent wins");
        assert_eq!(both.effort, ReasoningEffort::High, "set effort wins");

        let agent_only = ModelOverride {
            agent: Some(AgentSelection { harness: Harness::Claude, model: String::from("claude-opus-4-8") }),
            reasoning_effort: None,
        }
        .resolve(&default);
        assert_eq!(agent_only.model, "claude-opus-4-8", "set agent wins");
        assert_eq!(agent_only.effort, ReasoningEffort::Medium, "unset effort still falls through");
    }

    // Tripwire: harness and model move as one, in both directions. An override
    // that could set a model while leaving the profile's harness standing would
    // hand the calibrated CLI an id from another provider — a run that dies at
    // the child, late and remotely. Overriding onto a different harness must
    // carry that harness's own model, and declining to override must leave both
    // halves of the profile's pair intact.
    #[test]
    fn the_harness_and_model_are_overridden_together_or_not_at_all() {
        let muse = AgentProfile {
            harness: Harness::Muse,
            model: String::from("muse-spark-1.2-contributor"),
            effort: ReasoningEffort::Medium,
            tools: ToolPolicy::Full,
        };

        // Overriding onto another harness carries that harness's model with it.
        let onto_claude = ModelOverride {
            agent: Some(AgentSelection { harness: Harness::Claude, model: String::from("claude-opus-5") }),
            reasoning_effort: Some(ReasoningEffort::Max),
        }
        .resolve(&muse);
        assert_eq!(onto_claude.harness, Harness::Claude);
        assert_eq!(onto_claude.model, "claude-opus-5");
        assert_eq!(onto_claude.effort, ReasoningEffort::Max, "effort composes with either agent");

        // Overriding only the effort leaves the profile's pair whole.
        let effort_only = ModelOverride { agent: None, reasoning_effort: Some(ReasoningEffort::High) }.resolve(&muse);
        assert_eq!(effort_only.harness, Harness::Muse);
        assert_eq!(effort_only.model, "muse-spark-1.2-contributor");
        assert_eq!(effort_only.effort, ReasoningEffort::High);
    }
}
