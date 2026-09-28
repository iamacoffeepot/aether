//! Programs: `Call` handling, the invocation pipeline, outcomes, and the
//! relay of program API calls.

mod api;
mod call;
mod outcome;
mod pipeline;
mod queue;

pub use queue::DigestQueue;
