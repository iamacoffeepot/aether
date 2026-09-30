//! Portable guest SDK for bloomery authoring: programs, views, reactors, and
//! the `bundle` export generator.
//!
//! - Programs: stateless functions over an injected closure. [`Program`],
//!   [`Env`], [`invoke()`], and the program bundle root's state, [`Root`].
//! - [`view`](mod@view): folds over a contiguous journal prefix. [`View`], [`Heads`],
//!   [`At`], [`Cited`], and `#[view]` / `#[fold]`.
//! - [`reactor`](mod@reactor): signature-based reactors. [`Reactor`], [`Guard`],
//!   [`Owner`], and `#[reactor]` / `#[rule]`. The reactor bundle root's state
//!   is [`reactor::Root`].
//! - `bundle`: the export generator that replaces every `#[program]` and
//!   `#[reactor]` a module exports with one hidden bundle root at
//!   [`BUNDLE_NAMESPACE`].
//!
//! Exact author form:
//!
//! ```ignore
//! aether_actor::export!(
//!     public = [Summarize, SourcePublisher],
//!     generators = [aether_bloomery_program::bundle],
//! );
//! ```
//!
//! Every authoring item is exported at the crate root, except the reactor
//! [`reactor::Root`]. `#![no_std]` + `alloc`. Guests cannot link the journal.

#![no_std]

extern crate alloc;
extern crate self as aether_bloomery_program;

mod bundle;
mod program;
pub mod reactor;
pub mod view;

#[doc(hidden)]
pub use aether_bloomery_derive::__bundle_export_generate;
pub use aether_bloomery_kinds::{BUNDLE_NAMESPACE, PROGRAMS_SECTION};
#[doc(hidden)]
pub use program::__macro_internals;
pub use program::{
    Admission, Async, AsyncSession, Declaration, DeclarationsError, Env, Http, InjectedApi, Invoke, Invoked,
    MAX_FUNCTION_NAME_BYTES, Pending, PendingArtifact, PendingCall, PollResult, Process, Program, ProgramEntry,
    ProgramTable, Ran, Refusal, Root, Started, Sync, ToolDefinitionError, ToolSchema, Workspace, declarations,
    dispatch, function_name, invoke, kinds, program, program_name, start_async, start_invocation, tool_definition,
    unreachable_staged,
};
#[doc(hidden)]
pub use program::{AsyncProgram, SyncProgram};
pub use reactor::{
    And, Arg, ArmVisitor, AsAt, AsCited, AsGuard, AsView, AtArg, CitedArg, Direct, EvaluateFail, Guard, GuardArg,
    Intent, Nil, NoViews, Output, Owner, Params, PrepareError, Prepared, Reactor, ReactorList, Trigger, ViewArg,
    ViewSet, prepare, reactor, rule,
};
pub use view::{
    ActivationFoldError, Activations, At, Cited, CitedError, HeadActivation, HeadFoldError, HeadHistory, Heads,
    Outcome, Publish, PublishError, Request, RequestFoldError, Requests, SelectedReactor, SelectionError,
    SequenceError, View, ViewCursor, ViewFoldError, fold, select_reactors, view,
};
