//! Mail the native bundle driver (ADR-0226) sends and receives on its `aether.bloomery.driver` mailbox.
//!
//! A reactor rule returns intents as `ReactorIntent`s (ADR-0225 decision 4),
//! whose `kind` is the intent's mail `KindId`; the driver executes exactly
//! two intent kinds, [`CallProgram`] and [`SetHeads`], and refuses any other
//! as `ReactionFailed`. Native callers outside the journal use the
//! driver's own mail: [`Call`], answered by one [`CallOutcome`], and
//! [`AwaitProcessed`], answered by [`Processed`].

mod call;
mod call_input;
mod intent;
mod processed;
mod set_heads;

pub use call::{Call, CallOutcome, CallRefusal};
pub use call_input::{LEGACY_CALL_PROGRAM_ID, decode_call_program};
pub use intent::{CallInput, CallProgram, HeadChange, SetHeads};
pub use processed::{AwaitProcessed, Processed};
pub use set_heads::{LEGACY_SET_HEAD_ID, decode_set_heads};
