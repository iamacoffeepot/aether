//! ADR-0237's kinds, one module per concept: paths, images, environments,
//! runs, results, imports, and the storage wake. `order` holds the one rule
//! the set-shaped values share.

mod environment;
mod image;
mod import;
mod order;
mod path;
mod result;
mod run;
mod storage;

#[cfg(test)]
mod test_support;

pub use environment::{
    Environment, Platform, PlatformError, Provides, RustToolchain, RustToolchainError, Tool, ToolName, ToolNameError,
    Tools, ToolsError,
};
pub use image::{ImageRef, ImageRefError};
pub use import::{Import, ImportError, ImportResult};
pub use path::{TreePath, TreePathError};
pub use result::{Outcome, Refusal, Resource, RunError, RunResult, StepOutcome, ToolRecord};
pub use run::{
    EnvVar, EnvVarError, MAX_STEPS, Mount, Mounts, MountsError, Network, Run, RunRequest, Scratch, ScratchError, Step,
    Steps, StepsError,
};
pub use storage::StorageWake;
