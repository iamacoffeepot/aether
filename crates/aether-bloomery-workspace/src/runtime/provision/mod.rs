//! Executor provisioning (ADR-0237 decision 9, as amended by #6777): the
//! actor alone chooses each run's cores, memory, and deadline, and admits it
//! against its host.
//!
//! | Piece | Where |
//! |---|---|
//! | Host budget: the cores and memory the actor may hand out | [`cpuset`] parses the core list; [`budget`] holds what is free |
//! | Run key: the environment and the ordered steps, each step's tool, args, and env | [`key`] |
//! | Allotment: a default for a key never seen, else the estimate times headroom, floored, then clamped to the budget and the longest deadline | [`estimate`] |
//! | After exhaustion: the resource that ran out doubles, clamped | [`estimate`] |
//! | Admission: FIFO; a run starts when it fits the free budget, pinned to the lowest free cores, and is never dropped | [`queue`] |
//!
//! Every piece of state here lives in the actor's own state on its
//! dispatcher thread, so there is no lock, and nothing here is ever written
//! to the journal or placed in a result.

mod budget;
mod cpuset;
mod estimate;
mod key;
mod queue;

#[cfg(test)]
mod tests;

pub use budget::Budget;
pub use cpuset::CpuSet;
pub use estimate::{Amounts, Estimates, Headroom};
pub use queue::{Admitted, RunQueue};
