//! Building a member's `construct.implement` work order (ADR-0149 §The line,
//! #3511).
//!
//! The coordinator resolves the member's effective model + reasoning effort —
//! the stage's calibrated [`AgentProfile`](aether_bloomery::AgentProfile) default
//! (#3510) overridden by the sealed scope-revision's [`ModelOverride`] (#3511) —
//! and shapes the model-driven [`Transformation`] the executor port dispatches.
//! [`dispatch_model`] is that rule; the executor reactor overlays its result onto
//! every model lane it dispatches. The runner re-reads the same sealed
//! scope-revision by digest and re-resolves with the identical rule, so the
//! dispatched model is exactly the one the bloom froze and the study stage grades
//! cost against exactly that.

use aether_bloomery::{
    Digest, ModelOverride, Nonce, ResolvedModel, ScopeRevision, StageCatalog, StageId, Transformation, WorkOrder,
};

/// The typed command id the model-driven construct lane dispatches. The runner's
/// `xtask transform` entrypoint maps this id to a headless-Claude invocation.
/// The canonical lane details live in [`Transformation::for_member_stage`]; the
/// catalog's exported constant is re-exported here for the study/dispatch
/// consumers that name the command (#3668).
pub use aether_bloomery::CONSTRUCT_IMPLEMENT_COMMAND;

/// The effective model + reasoning effort a model-driven lane dispatches under:
/// the stage's calibrated [`AgentProfile`](aether_bloomery::AgentProfile)
/// (ADR-0149 §The line) with each set field of `model_override` winning over it.
///
/// The one place that rule lives. The host overlays the result onto the
/// dispatched [`Transformation::model`] the same way it overlays the work-order
/// description, because the reducer names the stage catalog by digest and never
/// resolves it — so without this overlay the lane runs at whatever model the
/// runner's ambient default happens to be, and a receipt attests a configuration
/// that did not run.
///
/// The executor reactor passes an empty override: a member's sealed override
/// lives in the typed [`ScopeRevision`] its `scope_revision` digest addresses,
/// and no host-side store holds that content yet, so the resolution today always
/// yields the stage's calibrated default. Threading the sealed override in is a
/// change of this call's second argument, not of the rule.
#[must_use]
pub fn dispatch_model(stage: StageId, model_override: &ModelOverride) -> ResolvedModel {
    model_override.resolve(&StageCatalog::profile_of(stage))
}

/// Build the `construct.implement` work order for a member and report the
/// effective model + effort it resolves to.
///
/// The effective model is the Construct stage's `AgentProfile` default overridden
/// per the member's sealed `ScopeRevision`; the returned [`ResolvedModel`] is the
/// coordinator's view of what will run, which the runner re-derives identically
/// from the same sealed content. The order pins the exact scope-revision digest
/// it resolved (`transformation.inputs`), so the dispatched model is a function
/// of the frozen revision, not a dispatch-time choice. The transformation itself
/// is built by the shared [`Transformation::for_member_stage`] the reducer also
/// dispatches through, so the host and the reducer never drift on the lane shape.
///
/// `base` is the bloom's sealed base — the git commit the attempt's worker checks
/// out (ADR-0149 §Execution, #3572), threaded onto the transformation as its
/// checkout target. It is a separate axis from the scope-revision subject: the
/// subject binds the returned evidence, the base names the tree the work runs on.
#[must_use]
pub fn build_construct_order(scope_revision: &ScopeRevision, base: Digest, nonce: Nonce) -> (WorkOrder, ResolvedModel) {
    let resolved = dispatch_model(StageId::Construct, &scope_revision.model_override);
    let mut transformation = Transformation::for_member_stage(StageId::Construct, scope_revision.digest(), base);
    transformation.model = Some(resolved.clone());
    (WorkOrder { transformation, nonce }, resolved)
}

#[cfg(test)]
mod tests {
    use aether_bloomery::{AgentSelection, Harness, ModelOverride, StageCatalog, StageId};

    use super::*;

    #[test]
    fn no_override_resolves_the_construct_profile_default() {
        let default_construct = StageCatalog::profile_of(StageId::Construct);
        let base = Digest::from_bytes([5; 32]);
        let (order, resolved) = build_construct_order(&ScopeRevision::default(), base, Nonce("n-1".to_owned()));

        assert_eq!(
            resolved.model, default_construct.model,
            "unset override → the Construct AgentProfile default model"
        );
        assert_eq!(resolved.effort, default_construct.effort, "unset override → the default effort");
        assert_eq!(order.transformation.command, CONSTRUCT_IMPLEMENT_COMMAND);
        assert_eq!(order.transformation.checkout, base, "the sealed base is the attempt's checkout target");
        assert_eq!(order.nonce, Nonce("n-1".to_owned()));
    }

    #[test]
    fn a_set_override_wins_over_the_default() {
        let default_construct = StageCatalog::profile_of(StageId::Construct);
        let overridden = ScopeRevision {
            model_override: ModelOverride {
                agent: Some(AgentSelection { harness: Harness::Claude, model: "claude-sonnet-5".to_owned() }),
                reasoning_effort: None,
            },
        };
        let (_, resolved) = build_construct_order(&overridden, Digest::from_bytes([5; 32]), Nonce("n-2".to_owned()));

        assert_eq!(resolved.model, "claude-sonnet-5", "the set model override wins");
        assert_eq!(resolved.effort, default_construct.effort, "the unset effort still falls through to the default");
    }

    // Tripwire: the dispatched order pins the exact sealed scope-revision it
    // resolved against, so the model that dispatches is a function of the frozen
    // revision — a later revision is a different digest and a different order.
    #[test]
    fn the_order_pins_the_scope_revision_it_resolved() {
        let rev = ScopeRevision {
            model_override: ModelOverride {
                agent: Some(AgentSelection { harness: Harness::Claude, model: "claude-sonnet-5".to_owned() }),
                reasoning_effort: None,
            },
        };
        let (order, _) = build_construct_order(&rev, Digest::from_bytes([5; 32]), Nonce("n-3".to_owned()));
        assert!(
            order.transformation.inputs.contains(&rev.digest()),
            "the dispatched order pins the exact scope-revision digest it resolved",
        );
    }
}
