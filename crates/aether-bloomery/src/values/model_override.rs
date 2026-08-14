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
//!
//! The override discriminates by stage as well as by member (#4601). One
//! member's Construct and its Refine re-entry are different lanes at different
//! costs, and the motivating case for the whole mechanism — print cheap, escalate
//! on the repair a failing Verify routes into — is a sentence about exactly that
//! difference. A flat override collapses it, so
//! [`per_stage`](ModelOverride::per_stage) carries the entries that differ and
//! the member-wide fields carry the floor.

use alloc::collections::BTreeMap;
use alloc::string::String;

use serde::{Deserialize, Serialize};

use crate::ids::StageId;
use crate::values::stage::dispatched_command;
use crate::values::{AgentProfile, Harness, ReasoningEffort, StageCatalog, is_model_lane};

/// A per-workpiece override of *how* the model lanes run for this workpiece
/// (ADR-0149 §The line, #3511). This is runtime configuration sealed into the
/// bloom, independent of the contributor workflow's Plan routing. Each field is
/// optional: an unset field falls through to the stage
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
    /// Per-stage exceptions to the two fields above. A stage keyed here takes
    /// its entry's set fields; every other stage takes the member-wide fields;
    /// anything still unset falls through to the stage's calibrated profile.
    ///
    /// Keyed by the stage itself, so "at most one entry per stage" is a property
    /// of the type rather than a rule someone has to check. That costs nothing on
    /// the wire — a fieldless enum is already the fixed-width discriminant a map
    /// key sorts on, and already its own name in JSON, so the authoring form is
    /// `{"Construct": {..}}` and the sealed bytes are insertion-order
    /// independent (#4622).
    #[serde(default)]
    pub per_stage: BTreeMap<StageId, StageOverride>,
}

/// One stage's exception to a [`ModelOverride`]'s member-wide fields.
///
/// Carries the same two axes rather than a nested `ModelOverride`, which would
/// admit a `per_stage` inside a `per_stage` — a nesting with no meaning, made
/// unrepresentable instead of merely undocumented.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct StageOverride {
    /// The harness and model this stage runs under. `None` → the member-wide
    /// [`ModelOverride::agent`], then the stage profile's pair.
    pub agent: Option<AgentSelection>,
    /// The reasoning effort this stage runs at. `None` → the member-wide
    /// [`ModelOverride::reasoning_effort`], then the stage profile's effort.
    pub reasoning_effort: Option<ReasoningEffort>,
}

/// Why a caller-authored [`ModelOverride`] cannot be sealed.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum OverrideError {
    /// A [`StageOverride`] is keyed to a stage the sealed catalog binds to no
    /// model lane, so nothing would ever resolve it.
    StageRunsNoModel(StageId),
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
    /// Resolve the effective harness + model + effort for `stage` against that
    /// stage's calibrated profile.
    ///
    /// Three layers, innermost first: the stage's own [`StageOverride`], then the
    /// member-wide fields, then `default`. The fall-through is per field rather
    /// than per layer, so an entry that sets only an effort still takes the
    /// member-wide agent — the same rule [`ConfigScopes`](crate::values::ConfigScopes)
    /// applies across registries, one level down.
    ///
    /// The coordinator (when it dispatches) and the runner (when it reads the
    /// sealed override by address) apply this identical rule, so what runs is
    /// exactly what the bloom froze.
    #[must_use]
    pub fn resolve(&self, stage: StageId, default: &AgentProfile) -> ResolvedModel {
        let entry = self.per_stage.get(&stage);
        let agent = entry.and_then(|entry| entry.agent.as_ref()).or(self.agent.as_ref());
        let effort = entry.and_then(|entry| entry.reasoning_effort).or(self.reasoning_effort);

        let (harness, model) = agent
            .map_or_else(|| (default.harness, default.model.clone()), |agent| (agent.harness, agent.model.clone()));
        ResolvedModel { harness, model, effort: effort.unwrap_or(default.effort) }
    }

    /// Check that every [`StageOverride`] could actually apply under `catalog`.
    ///
    /// An entry naming a stage that dispatches no model transformation is a
    /// refusal rather than a value nothing reads. The operator authored a
    /// sentence about which model runs where, and a stage that forks no agent
    /// cannot honour it — silently keeping the entry would leave them believing
    /// a model choice took effect while the receipt attests one that never ran.
    ///
    /// Judged by the command the stage's dispatch constructs, not by the
    /// binding's `process`: that string names the host position (`"review"`,
    /// `"aggregate-review"`), while the dispatched command is what
    /// [`is_model_lane`] recognizes. The two review stages dispatch
    /// `review.critic` and burn tokens under the resolved model; their process
    /// vocabulary must not refuse the pin. The catalog still has to bind the
    /// keyed stage — an unbound name is a choice nothing resolves.
    ///
    /// # Errors
    ///
    /// [`OverrideError::StageRunsNoModel`] when a keyed stage is unbound or
    /// dispatches no model transformation.
    pub fn validate(&self, catalog: &StageCatalog) -> Result<(), OverrideError> {
        self.per_stage
            .keys()
            .find(|stage| {
                !catalog
                    .binding(**stage)
                    .is_some_and(|binding| dispatched_command(binding.stage).is_some_and(is_model_lane))
            })
            .map_or(Ok(()), |stage| Err(OverrideError::StageRunsNoModel(*stage)))
    }
}

/// The effective harness + model + reasoning effort a model-lane attempt runs
/// under, after resolving a [`ModelOverride`] against its stage profile default.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
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
    use crate::digest::Digest;
    use crate::values::{ToolPolicy, Transformation};

    fn profile(model: &str, effort: ReasoningEffort) -> AgentProfile {
        AgentProfile { harness: Harness::Claude, model: String::from(model), effort, tools: ToolPolicy::Full }
    }

    fn claude(model: &str) -> AgentSelection {
        AgentSelection { harness: Harness::Claude, model: String::from(model) }
    }

    // An unset override field falls through to the stage profile default; a set
    // field wins. This is the rule the coordinator and the runner both apply.
    #[test]
    fn resolve_falls_through_unset_fields_and_honors_set_ones() {
        let default = profile("claude-sonnet-5", ReasoningEffort::Medium);

        let empty = ModelOverride::default().resolve(StageId::Construct, &default);
        assert_eq!(empty.model, "claude-sonnet-5", "unset agent → profile default");
        assert_eq!(empty.harness, Harness::Claude, "unset agent → profile harness too");
        assert_eq!(empty.effort, ReasoningEffort::Medium, "unset effort → profile default");

        let both = ModelOverride {
            agent: Some(claude("claude-opus-4-8")),
            reasoning_effort: Some(ReasoningEffort::High),
            ..ModelOverride::default()
        }
        .resolve(StageId::Construct, &default);
        assert_eq!(both.model, "claude-opus-4-8", "set agent wins");
        assert_eq!(both.effort, ReasoningEffort::High, "set effort wins");

        let agent_only = ModelOverride { agent: Some(claude("claude-opus-4-8")), ..ModelOverride::default() }
            .resolve(StageId::Construct, &default);
        assert_eq!(agent_only.model, "claude-opus-4-8", "set agent wins");
        assert_eq!(agent_only.effort, ReasoningEffort::Medium, "unset effort still falls through");
    }

    // Tripwire: the motivating case (#4601) — one member, two model lanes, two
    // different agents. Print cheap on Construct and escalate on the Refine
    // re-entry a failing Verify routes into. A resolution that read only the
    // member-wide fields would run one of the two under the other's agent, which
    // is the collapse this whole shape exists to undo, and it would still pass
    // every per-field fall-through assertion above.
    #[test]
    fn one_member_resolves_its_two_model_lanes_to_different_agents() {
        let muse = AgentProfile {
            harness: Harness::Muse,
            model: String::from("muse-spark-1.2-contributor"),
            effort: ReasoningEffort::High,
            tools: ToolPolicy::Full,
        };
        let escalate = ModelOverride {
            per_stage: BTreeMap::from([(
                StageId::Refine,
                StageOverride { agent: Some(claude("claude-opus-5")), reasoning_effort: Some(ReasoningEffort::Max) },
            )]),
            ..ModelOverride::default()
        };

        let construct = escalate.resolve(StageId::Construct, &muse);
        assert_eq!((construct.harness, construct.model.as_str()), (Harness::Muse, "muse-spark-1.2-contributor"));
        assert_eq!(construct.effort, ReasoningEffort::High, "an unnamed stage keeps its calibration");

        let refine = escalate.resolve(StageId::Refine, &muse);
        assert_eq!((refine.harness, refine.model.as_str()), (Harness::Claude, "claude-opus-5"));
        assert_eq!(refine.effort, ReasoningEffort::Max);
    }

    // Tripwire: the three layers compose per field, not per layer. A stage entry
    // setting only the effort still takes the member-wide agent — resolution that
    // treated a present entry as the whole answer would drop the member-wide
    // agent for exactly the stages an operator bothered to tune.
    #[test]
    fn a_stage_entry_layers_over_the_member_wide_fields_field_by_field() {
        let default = profile("claude-sonnet-5", ReasoningEffort::Low);
        let override_ = ModelOverride {
            agent: Some(claude("claude-opus-5")),
            reasoning_effort: Some(ReasoningEffort::Medium),
            per_stage: BTreeMap::from([(
                StageId::Refine,
                StageOverride { agent: None, reasoning_effort: Some(ReasoningEffort::Max) },
            )]),
        };

        let refine = override_.resolve(StageId::Refine, &default);
        assert_eq!(refine.model, "claude-opus-5", "the entry sets no agent, so the member-wide one stands");
        assert_eq!(refine.effort, ReasoningEffort::Max, "the entry's effort wins over the member-wide one");

        let construct = override_.resolve(StageId::Construct, &default);
        assert_eq!(construct.effort, ReasoningEffort::Medium, "an unnamed stage takes the member-wide effort");
    }

    // Tripwire: the seal door and the dispatch overlay honour the same stages.
    // A key validate admits is a key some constructor's command is a model lane,
    // and a key some constructor's command is a model lane is a key validate
    // admits. Review and AggregateReview dispatch `review.critic` (a model lane)
    // even though their binding process is host-position vocabulary that
    // `is_model_lane` rejects — judging process was the live seal bounce.
    // Verify and the other mechanical / pre-seal stages stay refused.
    #[test]
    fn admitted_override_keys_are_exactly_the_stages_whose_dispatch_runs_a_model() {
        let line = StageCatalog::line();
        let for_stage = |stage| ModelOverride {
            per_stage: BTreeMap::from([(
                stage,
                StageOverride { agent: Some(claude("claude-opus-5")), reasoning_effort: None },
            )]),
            ..ModelOverride::default()
        };

        for stage in StageId::ALL {
            let override_ = for_stage(*stage);
            let admitted = override_.validate(&line);
            let dispatch_runs_model =
                command_the_stage_dispatches(*stage).is_some_and(|command| is_model_lane(&command));

            match (admitted, dispatch_runs_model) {
                (Ok(()), true) => {
                    let resolved = override_.resolve(*stage, &StageCatalog::profile_of(*stage));
                    assert_eq!(
                        (resolved.harness, resolved.model.as_str()),
                        (Harness::Claude, "claude-opus-5"),
                        "{stage:?} must resolve the keyed agent its dispatch runs"
                    );
                }
                (Err(OverrideError::StageRunsNoModel(refused)), false) => {
                    assert_eq!(refused, *stage);
                }
                (admitted, dispatch_runs_model) => {
                    panic!(
                        "{stage:?}: validate returned {admitted:?}, but dispatch model-lane is {dispatch_runs_model}"
                    );
                }
            }
        }

        assert_eq!(ModelOverride::default().validate(&line), Ok(()), "overriding nothing is always sealable");
    }

    fn command_the_stage_dispatches(stage: StageId) -> Option<String> {
        let binding = StageCatalog::binding_of(stage);
        let digest = Digest::from_bytes([0; 32]);
        match stage {
            StageId::Construct | StageId::Refine | StageId::Reconcile | StageId::Review | StageId::Verify => {
                Some(Transformation::for_member_stage(&binding, digest, digest, digest).command)
            }
            StageId::AggregateReview => {
                Some(Transformation::for_aggregate_review(&binding, digest, digest, digest).command)
            }
            StageId::AggregateVerify => Some(Transformation::for_aggregate_verify(&binding, digest, digest).command),
            StageId::Sketch
            | StageId::Scope
            | StageId::Approve
            | StageId::Integrate
            | StageId::Land
            | StageId::Study => None,
        }
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
            agent: Some(claude("claude-opus-5")),
            reasoning_effort: Some(ReasoningEffort::Max),
            ..ModelOverride::default()
        }
        .resolve(StageId::Construct, &muse);
        assert_eq!(onto_claude.harness, Harness::Claude);
        assert_eq!(onto_claude.model, "claude-opus-5");
        assert_eq!(onto_claude.effort, ReasoningEffort::Max, "effort composes with either agent");

        // Overriding only the effort leaves the profile's pair whole.
        let effort_only = ModelOverride { reasoning_effort: Some(ReasoningEffort::High), ..ModelOverride::default() }
            .resolve(StageId::Construct, &muse);
        assert_eq!(effort_only.harness, Harness::Muse);
        assert_eq!(effort_only.model, "muse-spark-1.2-contributor");
        assert_eq!(effort_only.effort, ReasoningEffort::High);
    }
}
