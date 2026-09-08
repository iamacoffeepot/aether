//! Benchmark blooms: replaying landed history across profile cells (ADR-0184
//! §Benchmark blooms, issue #4871).
//!
//! Calibration needs comparable runs, and the live repository cannot produce
//! them: a work order lands exactly once, so "the same task under four profiles"
//! is unrunnable against real mainline. ADR-0184's answer is to replay landed
//! history against the fixture repository — one bloom per profile cell over the
//! same order and the same base, the sealed
//! [`ModelOverride`](aether_bloomery::ModelOverride) as the cell
//! selector, repeated for sample size — and let those rows flow into the same
//! capability ledger live operation feeds, told apart by their trial store.
//!
//! Three pieces, in the order a run uses them:
//!
//! - [`golden`] — what a landed pull request is, read back as a runnable task,
//!   and what a *set* of them is as a versioned value.
//! - [`run`] — the seal loop as a pure plan: a set plus cells plus a sample size
//!   becomes one [`BloomSpec`](aether_bloomery::BloomSpec) per
//!   `(task, cell, sample)`.
//! - `api::runtime::benchmark` — the operator door, which is the only part that
//!   reads a repository, admits a fact, or holds a request.
//!
//! # Trial mode is the door, not a warning
//!
//! [`require_trial_mode`] refuses a live-classed coordinator outright. That is
//! the containment every other decision here rests on: a benchmark bloom's
//! members carry an observation approval rather than a walked tier ladder
//! ([`run`]), and its extraction reads a fixture repository by type
//! ([`golden`]), and neither is defensible on a coordinator whose journal is the
//! estate's own history and whose backend is the estate's own repository.
//!
//! # What this does not do
//!
//! Grading. A golden task carries the reference answer its landing produced, and
//! nothing here compares a benchmark bloom's candidate against it — the ledger
//! measures what the gates saw, which is ADR-0184's whole claim, and a
//! diff-similarity score would be a second, unattested column beside it.

pub mod golden;
pub mod run;

#[cfg(test)]
mod tests;

pub use golden::{GoldenTask, GoldenTaskError, GoldenTaskSet, extract};
pub use run::{
    BenchmarkAdmission, BenchmarkBloomView, BenchmarkPlan, BenchmarkRefusal, BenchmarkReport, CellAddress,
    MAX_BENCHMARK_BLOOMS, PlannedBloom, benchmark_workpiece, plan, require_trial_mode,
};
