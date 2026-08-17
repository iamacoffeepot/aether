//! Workflow-dispatch input keys shared with the in-process fake.
//!
//! The GitHub Actions executor lives in `aether-bloomery-github`; these strings
//! are the correlation contract that fake and executor must spell identically
//! (#3668), so they sit with the fake rather than in the REST adapter.

/// The `workflow_dispatch` input key carrying the typed lane command — the
/// correlation contract with the external wrapper workflow, whose `inputs:`
/// block names these exact strings (#3668). One constant per key (its siblings
/// below), shared with the fake and the tests, so a drifted key cannot
/// silently dispatch a run the wrapper reads as blank.
pub const INPUT_COMMAND: &str = "command";

/// The input key carrying the evidence-binding subject — see [`INPUT_COMMAND`].
pub const INPUT_SUBJECT: &str = "subject";

/// The input key carrying the correlation nonce — see [`INPUT_COMMAND`].
pub const INPUT_NONCE: &str = "nonce";

/// The input key carrying the displayed digest — see [`INPUT_COMMAND`].
pub const INPUT_DISPLAYED: &str = "displayed";

/// The input key carrying the coordinator-resolved model. Only the model
/// wrapper declares it, so only a model-lane dispatch sends it — see
/// [`INPUT_COMMAND`].
pub const INPUT_MODEL: &str = "model";

/// The input key carrying the resolved reasoning-effort tier — the model
/// wrapper's sibling of [`INPUT_MODEL`].
pub const INPUT_EFFORT: &str = "effort";
