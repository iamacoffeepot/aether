//! Stored program declarations, the events that name them, and the mail a
//! driver exchanges with a program bundle.
//!
//! A program is a contract: name, input kind, result kind, mode, intent.
//! A bundle carries one or more programs. Identity recorded in events is
//! [`ProgramRef`]: the bundle digest plus the program name.

mod events;
mod fault;
mod invoke;
mod mode;
mod name;
mod reference;
mod refusal;
mod request;

use alloc::string::String;

use aether_data::KindId;

pub use events::{ProgramHeadMoved, Transition};
pub use fault::{Detail, DetailError, Fault, FaultReason};
pub use invoke::{ClaimedDigest, ClosureArtifact, DigestMismatch, Invoke, Invoked};
pub use mode::Mode;
pub use name::{
    NativeOrigin, NativeOriginError, ProgramName, ProgramNameError, ReactorName, ReactorNameError, RuleName,
    RuleNameError,
};
pub use reference::ProgramRef;
pub use refusal::Refusal;
pub use request::{RequestSource, Requested};

/// A stored declaration. Identity is the artifact digest; there is no id field.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "bloomery.program")]
pub struct Program {
    pub name: ProgramName,
    /// The one input kind.
    pub input: KindId,
    /// The one result kind.
    pub result: KindId,
    pub mode: Mode,
    /// One sentence of meaning for a planner.
    pub intent: String,
}
