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
//! (reaching `TestBench`'s `pub(crate)` drive methods) and unit-testable.

pub mod harness;
pub mod report;
