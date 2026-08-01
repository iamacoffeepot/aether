//! Lifecycle-latency perf tooling (iamacoffeepot/aether#1077).
//!
//! - [`harness`] — the sweep engine ([`harness::run_sweep`]) lifted out
//!   of the `#[cfg(test)]` latency harness so the `perf-trial` bin can
//!   drive it.
//! - [`report`] — the trial JSON schema ([`report::TrialReport`]) and
//!   the noise-aware paired comparison ([`report::compare`], ADR-0085)
//!   the `perf-compare` bin renders into a sticky PR comment.
//!
//! The bins (`src/bin/perf-trial.rs`, `src/bin/perf-compare.rs`) are
//! thin shells over these; the logic lives here so it is in-crate
//! (reaching `SubstrateHarness`'s `pub(crate)` drive methods) and unit-testable.

pub mod harness;
// Per-cell process isolation for the sweep (iamacoffeepot/aether#4177): a cell
// booted after other cells inherits their process state, and that inheritance —
// not anything the cell executes — decides which of two execution modes it
// lands in. `isolate` re-execs one child per cell so the modes are independent.
pub mod isolate;
// The real-`Registry` read-scaling + owner-ceiling benchmark
// (iamacoffeepot/aether#4176), driven by the `perf-registry` bin.
pub mod registry;
pub mod report;
