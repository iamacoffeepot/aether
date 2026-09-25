//! The workspace contract of ADR-0237: a run of steps over a stored tree, the
//! environment it runs in, and the import that turns a digest-pinned image into
//! a tree.
//!
//! This is the identity half of the ADR-0122 split: the mail kinds
//! ([`Run`] / [`RunResult`], [`Import`] / [`ImportResult`]), the stored
//! [`Environment`], and the values they carry. The `aether.workspace` actor
//! that answers them is the runtime half, behind the `runtime` feature.
//!
//! Every constrained value is a newtype with a private field, a fallible
//! `new`, and the same check on every decode path (`#[storage(validate)]`), so
//! an invalid value cannot be built in code or arrive in mail or from the
//! journal. No kind carries a mailbox id, an actor reference, a duration, a
//! host name, or a timestamp.
//!
//! `#![no_std]` + `alloc`, so a wasm program can cite these kinds.

#![no_std]
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

mod kinds;

pub use kinds::{
    EnvVar, EnvVarError, Environment, ImageRef, ImageRefError, Import, ImportResult, MAX_STEPS, Mount, Mounts,
    MountsError, Network, Outcome, Platform, PlatformError, Provides, Refusal, Resource, Run, RunResult, RustToolchain,
    RustToolchainError, Scratch, ScratchError, Step, StepOutcome, Steps, StepsError, Tool, ToolName, ToolNameError,
    ToolRecord, Tools, ToolsError, TreePath, TreePathError,
};
