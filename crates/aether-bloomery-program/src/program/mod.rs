//! Bloomery WASM programs.
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
//! closed set [`Http`], [`Process`], [`Workspace`], and [`Entropy`]; each is
//! Sampled. A call through one relays like a fetch: through the bundle root
//! to the driver, which maps the API to a provider it holds, answers it
//! itself, or refuses it. A [`Workspace`] run that exhausts its allotment or
//! fails in the executor ends the invocation as [`Invoked::Faulted`] without
//! the program seeing it.
//!
//! [`Ran<P>`](Ran) is the typed view of one recorded run of `P`: a
//! `bloomery.transition` whose program is `P`, citing its input and result,
//! for a reactor rule to trigger on.
//!
//! [`tool_definition`] renders a declared program as a responses-API function
//! tool: its `///` doc, and the JSON Schema of its arguments with every
//! field's doc. A tool's input is [`Tooled<A, B>`](Tooled): the tree the call
//! works on and the value `B`, both bound by the loop that runs it, and the
//! arguments `A` the model writes. A tool that changes the tree returns
//! [`Edited`]. [`ToolSchema`] stores a tool's arguments' or result's schema as data, for a reader that
//! links neither type.
//!
//! `#![no_std]` + `alloc`. Guests cannot link the journal.

mod declare;
mod env;
mod invoke;
mod ran;
mod root;
mod section;
mod tool;

pub use aether_bloomery_derive::program;
pub use aether_bloomery_kinds as kinds;
pub use aether_bloomery_kinds::{Invoke, Invoked, Refusal};
pub use declare::Program;
#[doc(hidden)]
pub use declare::{AsyncProgram, SyncProgram};
pub use env::{
    Async, Entropy, Env, Http, InjectedApi, Pending, PendingArtifact, PendingCall, Process, Sync, Workspace,
};
pub use invoke::{AsyncSession, PollResult, Started, invoke, start_async, unreachable_staged};
pub use ran::Ran;
pub use root::{Admission, ProgramEntry, ProgramTable, Root, dispatch, start_invocation};
pub use section::{Declaration, DeclarationsError, ProgramKind, declarations};
pub use tool::{
    Edited, ErasedEdited, ErasedTooled, MAX_FUNCTION_NAME_BYTES, NoBound, NoDetail, ToolArguments, ToolDefinitionError,
    ToolSchema, Tooled, function_name, program_name, tool_definition, tooled,
};

#[doc(hidden)]
pub mod __macro_internals {
    pub use aether_data::{Digest, Kind, KindId, MailboxId, RequestId, Schema, StaticSchema, require_documented};
    pub use alloc::collections::BTreeMap;
    pub use alloc::string::ToString;
    pub use alloc::vec::Vec;
    use core::marker::PhantomData;

    use super::env::{Entropy, Http, Process, Workspace};
    pub use crate::program::declare::{AsyncProgram, SyncProgram};
    pub use crate::program::env::{InjectedApi, Pending, PendingArtifact, PendingCall};
    pub use crate::program::invoke::{PollResult, Started};
    pub use crate::program::root::{program_table, start_invocation};
    pub use crate::program::section::{
        MODE_PURE, MODE_SAMPLED, ProgramRecord, api_mask, program_record_len, write_program_record,
    };

    /// The handle `#[program]` maps each API name to, by the name it
    /// accepts. Capability-backed handles keep their target inside their own
    /// `Binding`; [`crate::Entropy`] has none, and the driver answers it
    /// itself from the operating system's randomness.
    pub mod api {
        /// Handle of the `Http` API.
        pub type Http = super::Http;
        /// Handle of the `Process` API.
        pub type Process = super::Process;
        /// Handle of the `Workspace` API.
        pub type Workspace = super::Workspace;
        /// Handle of the `Entropy` API.
        pub type Entropy = super::Entropy;
    }

    /// Compiles only when `Api` is the handle `Expected`. `#[program]` emits
    /// one per trailing binding, pairing the author's type with the [`api`]
    /// row its name selects.
    pub struct CheckHandle<Api, Expected>(PhantomData<fn() -> (Api, Expected)>);

    impl<T> CheckHandle<T, T> {
        pub const OK: () = ();
    }

    pub struct RejectSampledOnPure<const SAMPLED: bool>;

    impl RejectSampledOnPure<false> {
        pub const OK: () = ();
    }
}
