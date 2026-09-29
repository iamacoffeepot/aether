//! The workspace contract of ADR-0237: a run of steps over a stored tree, the
//! environment it runs in, and the import that turns a digest-pinned image into
//! a tree.
//!
//! The identity half of the ADR-0122 split is always on: the mail kinds
//! ([`Run`] / [`RunResult`], [`Import`] / [`ImportResult`]), the program-side
//! [`RunRequest`], the stored [`Environment`], the values they carry, the
//! [`WorkspaceCapability`] marker, and the [`WorkspaceConfig`] domain struct.
//! The runtime half, behind the `runtime` feature, is the
//! `aether.bloomery.workspace` actor that answers them (ADR-0237 decision 8,
//! ADR-0240 D7): a root singleton, one per engine, that talks to the Docker
//! Engine API through a private client. It holds no store. Each [`Run`] and
//! [`Import`] names its storage as a `source`, a path to an
//! [`ArtifactStorage`](aether_bloomery_kinds::ArtifactStorage) such as a
//! unit's journal, and every read and stage of that request goes through it
//! as mail.
//!
//! Every constrained value is a newtype with a private field, a fallible
//! `new`, and the same check on every decode path (`#[storage(validate)]`), so
//! an invalid value cannot be built in code or arrive in mail or from the
//! journal. No kind carries a mailbox id, an actor reference, a duration, a
//! host name, or a timestamp; a `source` is a checked path, proven live on
//! receipt.
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
    MountsError, Network, Outcome, Platform, PlatformError, Provides, Refusal, Resource, Run, RunRequest, RunResult,
    RustToolchain, RustToolchainError, Scratch, ScratchError, Step, StepOutcome, Steps, StepsError, StorageWake, Tool,
    ToolName, ToolNameError, ToolRecord, Tools, ToolsError, TreePath, TreePathError,
};

pub use config::{DEFAULT_ENDPOINT, WorkspaceConfig};

#[cfg(feature = "runtime")]
pub use config::{WorkspaceConfigLayer, WorkspaceOverlay};

/// Only for tests: the scripted Engine API server the runtime's tests dial.
#[cfg(all(unix, any(test, feature = "test-support")))]
pub use runtime::testing;

/// `aether.bloomery.workspace` actor **identity** (ADR-0122 split). A ZST carrying only
/// the addressing and the per-handler markers `#[actor]` emits always-on. It is
/// a root singleton, so a program binding can name it in `depends(...)`
/// (ADR-0230). The state-bearing runtime lives behind `feature = "runtime"`.
#[actor(singleton, root)]
pub struct WorkspaceCapability;

use aether_actor::actor;

#[cfg(feature = "runtime")]
mod runtime;
