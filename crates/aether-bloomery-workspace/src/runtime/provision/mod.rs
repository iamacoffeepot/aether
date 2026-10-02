//! Executor provisioning (ADR-0237 decision 9, as amended by #6777 and on
//! 2026-10-01): the actor alone chooses each run's cores, memory, and
//! deadline, and admits it against its host.
//!
//! | Piece | Where |
//! |---|---|
//! | Host budget: the cores and memory the actor may hand out | [`cpuset`] parses the core list; [`budget`] holds what is free |
//! | Run key: the environment and the ordered steps, each step's tool, args, and env | [`key`] |
//! | Memory and deadline: a default for a key never seen, else the estimate times headroom, floored, then clamped to the budget's memory and the longest deadline | [`estimate`] |
//! | After exhaustion: the resource that ran out doubles, clamped | [`estimate`] |
//! | Cores: 8 to 16 per run, at most the budget's, the free cores shared among the runs that want them | [`cores`] |
//! | Admission: in arrival order, with backfill; a run behind a waiting front starts only when it fits now and its deadline ends by the front's reservation; every run is pinned to the lowest free cores and never dropped | [`queue`] |
//!
//! Every piece of state here lives in the actor's own state on its
//! dispatcher thread, so there is no lock, and nothing here is ever written
//! to the journal or placed in a result.

mod budget;
mod cores;
mod cpuset;
mod estimate;
mod key;
mod queue;

#[cfg(test)]
mod tests;

pub use budget::Budget;
pub use cpuset::CpuSet;
pub use estimate::{Amounts, Estimates, Headroom};
pub use key::RunKey;
pub use queue::RunQueue;
