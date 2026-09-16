//! Stored program declarations, the events that name them, and the record of
//! one execution.
//!
//! A program is a contract: name, input kind, result kind, mode, intent.
//! Identity is its digest. An executor is never stored. A transition is the
//! only place the three meet.

mod events;
mod fault;
mod mode;
mod name;

use alloc::string::String;

use aether_data::KindId;

pub use events::{ProgramHeadMoved, Transition};
pub use fault::{Detail, DetailError, Fault, FaultReason};
pub use mode::Mode;
pub use name::{ExecutorName, ExecutorNameError, ProgramName, ProgramNameError};

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
