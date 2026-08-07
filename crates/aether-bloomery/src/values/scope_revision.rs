//! The typed scope-revision content and its per-workpiece model override
//! (ADR-0149 §The value vocabulary / §The line, #3511).
//!
//! A workpiece's `scope_revision` is an opaque [`Digest`] everywhere it is
//! *named* ([`Workpiece`](crate::values::Workpiece) /
//! [`Membership`](crate::values::Membership)); [`ScopeRevision`] is the minimal
//! typed content that digest *addresses*, so the value is decodable and the
//! per-workpiece [`ModelOverride`] it carries is frozen at seal (a sealed bloom
//! pins the exact digest) and therefore attestable. The runner lane and the
//! coordinator both resolve the effective model + effort the same way — each
//! field of the override falls through to the stage's
//! [`AgentProfile`] default when unset — so "the
//! model that ran is the model the bloom promised" holds by construction.

use alloc::string::String;

use serde::{Deserialize, Serialize};

use crate::digest::{ContentAddressed, Digest, digest_of};
use crate::values::{AgentProfile, Harness, ReasoningEffort};

/// A per-workpiece override of the construct lane's model + reasoning effort
/// (ADR-0149 §The line, #3511) — the successor of today's free-text `model:*`
/// label. Each field is optional: an unset field falls through to the stage
/// [`AgentProfile`] default at
/// [resolution](Self::resolve), so an empty override changes nothing and a set
/// field pins exactly that value into the sealed bloom.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct ModelOverride {
    /// The model to run this workpiece's construct stage under, overriding the
    /// stage profile default. `None` → the profile default.
    pub model: Option<String>,
    /// The reasoning effort, overriding the stage profile default. `None` → the
    /// profile default.
    pub reasoning_effort: Option<ReasoningEffort>,
}

impl ModelOverride {
    /// Resolve the effective model + effort against a stage's profile default:
    /// each set field wins, each unset field falls through to `default`. The
    /// coordinator (when it dispatches) and the runner (when it reads the sealed
    /// scope-revision by digest) apply this identical rule, so the dispatched
    /// model is exactly the one the sealed scope-revision froze.
    #[must_use]
    pub fn resolve(&self, default: &AgentProfile) -> ResolvedModel {
        ResolvedModel {
            harness: default.harness,
            model: self.model.clone().unwrap_or_else(|| default.model.clone()),
            effort: self.reasoning_effort.unwrap_or(default.effort),
        }
    }
}

/// The effective harness + model + reasoning effort a construct attempt runs
/// under, after resolving a [`ModelOverride`] against its stage profile default.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct ResolvedModel {
    /// The harness the attempt's lane forks. Resolved straight from the stage
    /// profile — [`ModelOverride`] deliberately offers no harness field, so
    /// which CLI runs a stage is a line-wide calibration a per-workpiece scope
    /// revision cannot reach around. It still travels this channel rather than
    /// its own so it inherits the attestation the model already has: the sealed
    /// bloom pins the profile digest, and the profile names the harness.
    pub harness: Harness,
    /// The model the attempt runs under.
    pub model: String,
    /// The reasoning effort the attempt runs at.
    pub effort: ReasoningEffort,
}

/// The typed content a workpiece's `scope_revision` digest addresses (ADR-0149
/// §The value vocabulary, #3511). Until this slice the revision was an opaque
/// digest over untyped bytes; this is the minimal typed carrier so the
/// per-workpiece [`ModelOverride`] is decodable and, because a sealed bloom pins
/// the exact [`digest`](Self::digest), frozen at seal.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct ScopeRevision {
    /// The per-workpiece model/effort override for the construct lane.
    pub model_override: ModelOverride,
}

impl ContentAddressed for ScopeRevision {
    const DOMAIN: &'static str = "aether.bloomery.scope_revision";
}

impl ScopeRevision {
    /// The scope revision's content-addressed digest — the value a
    /// [`Workpiece`](crate::values::Workpiece) /
    /// [`Membership`](crate::values::Membership) pins as its `scope_revision`.
    #[must_use]
    pub fn digest(&self) -> Digest {
        digest_of(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::values::ToolPolicy;

    fn profile(model: &str, effort: ReasoningEffort) -> AgentProfile {
        AgentProfile { harness: Harness::Claude, model: String::from(model), effort, tools: ToolPolicy::Full }
    }

    // A scope revision carrying an override round-trips through its content-
    // addressed digest: the same content yields the same address, and a changed
    // override changes it — so a sealed bloom that pins the digest pins the
    // exact override.
    #[test]
    fn scope_revision_digest_is_content_addressed() {
        let base = ScopeRevision::default();
        assert_eq!(base.digest(), ScopeRevision::default().digest(), "same content, same address");

        let overridden = ScopeRevision {
            model_override: ModelOverride { model: Some(String::from("claude-opus-4-8")), reasoning_effort: None },
        };
        assert_ne!(base.digest(), overridden.digest(), "a changed override moves the address");
    }

    // An unset override field falls through to the stage profile default; a set
    // field wins. This is the rule the coordinator and the runner both apply.
    #[test]
    fn resolve_falls_through_unset_fields_and_honors_set_ones() {
        let default = profile("claude-sonnet-5", ReasoningEffort::Medium);

        let empty = ModelOverride::default().resolve(&default);
        assert_eq!(empty.model, "claude-sonnet-5", "unset model → profile default");
        assert_eq!(empty.effort, ReasoningEffort::Medium, "unset effort → profile default");

        let both = ModelOverride {
            model: Some(String::from("claude-opus-4-8")),
            reasoning_effort: Some(ReasoningEffort::High),
        }
        .resolve(&default);
        assert_eq!(both.model, "claude-opus-4-8", "set model wins");
        assert_eq!(both.effort, ReasoningEffort::High, "set effort wins");

        let model_only =
            ModelOverride { model: Some(String::from("claude-opus-4-8")), reasoning_effort: None }.resolve(&default);
        assert_eq!(model_only.model, "claude-opus-4-8", "set model wins");
        assert_eq!(model_only.effort, ReasoningEffort::Medium, "unset effort still falls through");
    }

    // The harness is line-wide calibration, not per-workpiece scope: it comes
    // from the stage profile whatever the override says, because `ModelOverride`
    // carries no harness field to say it with. A resolution that read the
    // harness from anywhere else would let a sealed scope revision run a stage
    // under a CLI its profile digest does not attest.
    #[test]
    fn resolve_takes_the_harness_from_the_profile_and_no_override_reaches_it() {
        let muse = AgentProfile {
            harness: Harness::Muse,
            model: String::from("claude-sonnet-5"),
            effort: ReasoningEffort::Medium,
            tools: ToolPolicy::Full,
        };

        let fully_overridden = ModelOverride {
            model: Some(String::from("claude-opus-4-8")),
            reasoning_effort: Some(ReasoningEffort::Max),
        }
        .resolve(&muse);
        assert_eq!(fully_overridden.harness, Harness::Muse, "the profile's harness survives a full model override");
        assert_eq!(ModelOverride::default().resolve(&muse).harness, Harness::Muse, "and an empty one");
    }
}
