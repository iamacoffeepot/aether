//! The workspace contract of ADR-0237: a run of steps over a stored tree, the
//! environment it runs in, and the import that turns a digest-pinned image into
//! a tree.
//!
//! The identity half of the ADR-0122 split is always on: the mail kinds
//! ([`Run`] / [`RunResult`], [`Import`] / [`ImportResult`]), the stored
//! [`Environment`], the values they carry, the [`WorkspaceCapability`]
//! marker, and the [`WorkspaceConfig`] domain struct. The runtime half, behind
//! the `runtime` feature, is the `aether.workspace` actor that answers them
//! (ADR-0237 decision 8): a root singleton that talks to the Docker Engine API
//! through a private client and writes what it imports into the journal
//! through the [`ArtifactStore`](aether_bloomery_journal::ArtifactStore) it is
//! composed with. It answers [`Import`] so far; `Run` follows.
//!
//! Every constrained value is a newtype with a private field, a fallible
//! `new`, and the same check on every decode path (`#[storage(validate)]`), so
//! an invalid value cannot be built in code or arrive in mail or from the
//! journal. No kind carries a mailbox id, an actor reference, a duration, a
//! host name, or a timestamp.
//!
//! `no_std` + `alloc` without the `runtime` feature, so a wasm program can
//! cite these kinds.

#![cfg_attr(not(feature = "runtime"), no_std)]
#![forbid(unsafe_code)]

extern crate alloc;

/// Implement `aether_data::Invariant`, `Display`, and `Error` for error enums
/// that carry a `reason(self) -> &'static str` method.
macro_rules! invariant_errors {
    ($($error:ty),+ $(,)?) => {$(
        impl ::aether_data::Invariant for $error {
            fn reason(&self) -> &'static str {
                Self::reason(*self)
            }
        }

        impl ::core::fmt::Display for $error {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                f.write_str(Self::reason(*self))
            }
        }

        impl ::core::error::Error for $error {}
    )+};
}

mod config;
mod kinds;

pub use kinds::{
    EnvVar, EnvVarError, Environment, ImageRef, ImageRefError, Import, ImportResult, MAX_STEPS, Mount, Mounts,
    MountsError, Network, Outcome, Platform, PlatformError, Provides, Refusal, Resource, Run, RunResult, RustToolchain,
    RustToolchainError, Scratch, ScratchError, Step, StepOutcome, Steps, StepsError, Tool, ToolName, ToolNameError,
    ToolRecord, Tools, ToolsError, TreePath, TreePathError,
};

pub use config::{DEFAULT_ENDPOINT, WorkspaceConfig};

#[cfg(feature = "runtime")]
pub use config::{WorkspaceConfigLayer, WorkspaceOverlay};
#[cfg(feature = "runtime")]
pub use runtime::WorkspaceParams;

/// Only for tests: the scripted Engine API server the runtime's tests dial.
#[cfg(all(unix, any(test, feature = "test-support")))]
pub use runtime::testing;

/// `aether.workspace` actor **identity** (ADR-0122 split). A ZST carrying only
/// the addressing and the per-handler markers `#[actor]` emits always-on. It is
/// a root singleton, so a program binding can name it in `depends(...)`
/// (ADR-0230). The state-bearing runtime lives behind `feature = "runtime"`.
#[actor(singleton, root)]
pub struct WorkspaceCapability;

use aether_actor::actor;

#[cfg(feature = "runtime")]
mod runtime;
