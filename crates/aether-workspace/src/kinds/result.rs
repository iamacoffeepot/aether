//! The run reply: an outcome, a refusal, or a fault about the attempt
//! (ADR-0237 decision 2).
//!
//! It carries no duration, host name, or timestamp, so two correct executors
//! of the same platform class produce the same digest.

use alloc::vec::Vec;

use aether_bloomery_kinds::{Detail, Digest, OpaqueBytes, Ref, Tree};

use crate::kinds::environment::{Platform, RustToolchain, ToolName};
use crate::kinds::path::TreePath;

/// The one reply to a [`crate::Run`].
///
/// `Exhausted` and `Failed` are faults about the attempt, never results: the
/// program never observes them, and the driver records the fault.
#[aether_data::kind(name = "aether.workspace.run_result", eq, no_serde)]
pub enum RunResult {
    /// The steps ran. A non-zero exit is an outcome, not a refusal.
    Ok(Outcome),
    /// The run could not start as asked.
    Refused(Refusal),
    /// The executor's allotment ran out.
    Exhausted(Resource),
    /// The executor failed during the run for a reason outside the request.
    Failed {
        /// The bounded fault text naming the cause or the path.
        detail: Detail,
    },
}

/// The allotment a run exhausted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, aether_data::Schema)]
pub enum Resource {
    Memory,
    Time,
}

/// What a completed run produced.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Schema)]
pub struct Outcome {
    /// One per step that ran; the run stops after the first non-zero exit.
    pub steps: Vec<StepOutcome>,
    /// `/work` after the last step, minus `scratch`.
    pub tree: Ref<Tree>,
}

/// What one step produced.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Schema)]
pub struct StepOutcome {
    /// `None` when the step died by signal.
    pub exit_code: Option<i32>,
    /// Everything the step wrote to stdout.
    pub stdout: Ref<OpaqueBytes>,
    /// Everything the step wrote to stderr.
    pub stderr: Ref<OpaqueBytes>,
    /// The executable that ran.
    pub tool: ToolRecord,
}

/// The executable a step's tool name resolved to.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Schema)]
pub struct ToolRecord {
    /// The name the step asked for.
    pub name: ToolName,
    /// Its path inside the environment root.
    pub path: TreePath,
    /// The executable's bytes.
    pub file: Ref<OpaqueBytes>,
}

/// Why a run could not start as asked.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Schema)]
pub enum Refusal {
    /// The environment cannot be provided. Never a mid-run failure.
    EnvironmentUnavailable,
    /// The environment's platform is not the one the executor runs.
    PlatformMismatch { wanted: Platform, provided: Platform },
    /// The tree's `rust-toolchain.toml` asks for a toolchain the environment
    /// does not provide.
    ToolchainMismatch { tree_wants: RustToolchain, environment_provides: Option<RustToolchain> },
    /// A step named a tool the environment's table does not hold.
    UnknownTool(ToolName),
    /// An input the request cites is not stored.
    InputMissing(Digest),
}
