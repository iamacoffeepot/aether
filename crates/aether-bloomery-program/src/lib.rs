//! Portable guest SDK for bloomery WASM programs.
//!
//! A program is a stateless function over an injected closure, the same
//! injected-data sandbox reactors use. The native driver sends
//! [`Invoke`]; [`invoke()`] runs one sync [`Program`] and replies [`Invoked`].
//! [`Env<Sync>`] is injected lookup: a miss is [`Refusal::InputMissing`].
//! [`Env<Async>::read`] fetches a missing digest through the bundle root and
//! the driver, which forwards it to the journal, and resumes when
//! [`kinds::ReadArtifactResult`] arrives. Only native code writes journal
//! records. A program's identity is its bundle digest plus name, not a stored
//! declaration digest. [`Root`] is the bundle root's state: the program table
//! plus the live-seq table.
//!
//! An async program may take trailing API bindings after `env`, from the
//! closed set [`Http`], [`Process`], and [`Workspace`]; each is Sampled. A
//! [`Workspace`] run that exhausts its allotment or fails in the executor
//! ends the invocation as [`Invoked::Faulted`] without the program seeing it.
//!
//! `#![no_std]` + `alloc`. Guests cannot link the journal.

#![no_std]

extern crate alloc;
extern crate self as aether_bloomery_program;

mod declare;
mod env;
mod invoke;
mod root;
mod section;

pub use aether_bloomery_kinds as kinds;
pub use aether_bloomery_kinds::{Invoke, Invoked, Refusal};
pub use aether_bloomery_program_derive::program;
pub use declare::Program;
#[doc(hidden)]
pub use declare::{AsyncProgram, SyncProgram};
pub use env::{Async, Env, Http, InjectedApi, Pending, PendingArtifact, PendingCall, Process, Sync, Workspace};
pub use invoke::{AsyncSession, PollResult, Started, invoke, start_async, unreachable_staged};
pub use root::{Admission, ProgramEntry, ProgramTable, Root, dispatch, start_invocation};
pub use section::{DeclarationsError, declarations};

#[doc(hidden)]
pub mod __macro_internals {
    pub use aether_data::{Kind, KindId, MailboxId, RequestId};
    pub use alloc::collections::BTreeMap;
    pub use alloc::string::ToString;
    pub use alloc::vec::Vec;

    pub use crate::declare::{AsyncProgram, SyncProgram};
    pub use crate::env::{InjectedApi, Pending, PendingArtifact, PendingCall};
    pub use crate::invoke::{PollResult, Started};
    pub use crate::root::{program_table, start_invocation};
    pub use crate::section::{MODE_PURE, MODE_SAMPLED, program_record_len, write_program_record};

    /// The target capability of each program API, by the name `#[program]`
    /// accepts. The bundle generator declares these as the invocation's
    /// dependencies: an alias is expanded before coherence, so two APIs are
    /// two concrete `DependsOn` impls rather than overlapping projections.
    pub mod api_target {
        /// Target of [`crate::Http`].
        pub type Http = aether_http::HttpCapability;
        /// Target of [`crate::Process`].
        pub type Process = aether_process::ProcessCapability;
        /// Target of [`crate::Workspace`].
        pub type Workspace = aether_workspace::WorkspaceCapability;
    }

    /// Compiles only when `Api`'s [`InjectedApi::Target`] is `T`. `#[program]`
    /// emits one per trailing binding, pairing the author's type with the
    /// [`api_target`] row its name selects.
    pub const fn check_target<Api: InjectedApi<Target = T>, T>() {}

    pub struct RejectSampledOnPure<const SAMPLED: bool>;

    impl RejectSampledOnPure<false> {
        pub const OK: () = ();
    }
}
