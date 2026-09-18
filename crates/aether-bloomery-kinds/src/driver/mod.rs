//! Mail the native bundle driver (ADR-0226) sends and receives on its `aether.bloomery.driver` mailbox.
//!
//! A reactor rule returns intents as `ReactorIntent`s (ADR-0225 decision 4),
//! whose `kind` is the intent's mail `KindId`; the driver executes exactly
//! two intent kinds, [`CallProgram`] and [`SetHead`], and refuses any other
//! as `ReactionFailed`. Native callers outside the journal use the
//! driver's own mail: [`Call`], answered by one [`CallOutcome`], and
//! [`AwaitProcessed`], answered by [`Processed`].

mod call;
mod intent;
mod processed;

pub use call::{Call, CallOutcome, CallRefusal};
pub use intent::{CallProgram, SetHead};
pub use processed::{AwaitProcessed, Processed};
