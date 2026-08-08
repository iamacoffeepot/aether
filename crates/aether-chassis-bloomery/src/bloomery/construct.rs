//! Building a member's `construct.implement` work order (ADR-0149 §The line,
//! #3511).
//!
//! The coordinator resolves the member's effective model + reasoning effort —
//! the profile the bloom's sealed stage catalog calibrates the stage at,
//! overridden by the [`ModelOverride`] the member's config registry seals
//! (ADR-0174) — and shapes the model-driven [`Transformation`](aether_bloomery::Transformation) the executor
//! port dispatches. [`dispatch_model`] is that rule; the executor reactor overlays
//! its result onto every model lane it dispatches. The runner resolves the same
//! sealed override by address and re-resolves with the identical rule, so the
//! dispatched model is exactly the one the bloom froze and the study stage grades
//! cost against exactly that.

use aether_bloomery::{AgentProfile, ModelOverride, ResolvedModel};

/// The typed command id the model-driven construct lane dispatches. The runner's
/// `xtask transform` entrypoint maps this id to a headless-Claude invocation.
/// The canonical lane details live in [`Transformation::for_member_stage`](aether_bloomery::Transformation::for_member_stage); the
/// catalog's exported constant is re-exported here for the study/dispatch
/// consumers that name the command (#3668).
pub use aether_bloomery::CONSTRUCT_IMPLEMENT_COMMAND;

/// The effective model + reasoning effort a model-driven lane dispatches under:
/// the stage's sealed [`AgentProfile`] with each
/// set field of `model_override` winning over it.
///
/// The one place that rule lives. The host overlays the result onto the
/// dispatched [`Transformation::model`](aether_bloomery::Transformation::model)
/// the same way it overlays the work-order description, because the reducer
/// authors neither — without the overlay the lane runs at whatever model the
/// runner's ambient default happens to be, and a receipt attests a configuration
/// that did not run.
///
/// `profile` comes off the dispatch payload, resolved by the reducer from the
/// bloom's sealed catalog (ADR-0174). It is not looked up here, and that is the
/// point: reaching for [`StageCatalog::profile_of`](aether_bloomery::StageCatalog::profile_of)
/// would run the *compiled* line for a bloom that sealed a different one — the
/// same divergence #4324 and #4327 closed for the model and the harness, one
/// layer down.
#[must_use]
pub fn dispatch_model(profile: &AgentProfile, model_override: &ModelOverride) -> ResolvedModel {
    model_override.resolve(profile)
}
