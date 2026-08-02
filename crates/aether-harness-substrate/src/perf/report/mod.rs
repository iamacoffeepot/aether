//! Trial JSON schema + the noise-aware paired comparison (ADR-0085).
//!
//! A [`TrialReport`] is one fresh-process run of the sweep, serialised
//! as JSON by the `perf-trial` bin. [`compare`] takes K base + K
//! candidate trials (interleaved on one runner) and, per
//! (worker-count × topology × metric × percentile) cell, computes the
//! **paired delta** `δ_t = cand_t − base_t`. Because base and candidate
//! ran adjacent on the same runner, shared run-to-run drift cancels in
//! each δ — so the verdict rests on the *change* distribution, not on
//! two independent clouds (ADR-0085 §3).
//!
//! Verdict rule (a deterministic paired test in the ADR's posture — no
//! bootstrap RNG, so it is reproducible and the fixtures below pin it):
//! a cell flags `improved` / `regressed` only when the paired deltas
//! both (a) **agree on direction** for at least `consistency` of trials
//! and (b) have a median whose magnitude clears
//! `max(effect_floor × IQR(δ), rel_floor × base_median)` — i.e. the
//! change is large relative to its own spread *and* above a practical
//! relative-significance floor. Otherwise `stable`. This is what makes
//! uniform run-order drift (δ ≈ 0 after pairing) and one-off tail
//! outliers (median is robust) read as stable rather than false
//! regressions.
//!
//! # Two-level versioning (iamacoffeepot/aether#1206)
//!
//! The report is versioned at two independent levels so a metric-set
//! change no longer blinds the whole comparison:
//!
//! - The envelope [`TrialReport::schema`] tag ([`TRIAL_SCHEMA`]) guards
//!   only the *container* shape — "a report is a list of named,
//!   versioned sections". It bumps rarely (and a pre-sections report on
//!   the wrong envelope still can't be sectioned, so the comparator
//!   keeps its whole-container skip for that case alone).
//! - Each [`RawSection`] carries its own `version`. Adding or changing a
//!   metric bumps only *that* section's version; every other section
//!   still pairs and gets a verdict. A section new or version-mismatched
//!   on one side renders "new this run — no baseline" without blinding
//!   the sections that *are* comparable.
//!
//! A section's `body` is kept as an opaque [`serde_json::Value`] until
//! the comparator has confirmed both sides agree on its name and
//! version. That generalises the old probe-before-parse: an unknown or
//! mismatched section stays opaque (and renders as uncompared) rather
//! than serde-hard-failing the decode of the sections that *can* be
//! read.

mod compare;
#[cfg(test)]
mod fixture;
mod keep_up;
mod latency;
mod metric;
mod render;
mod throughput;
mod trial;

pub use compare::{CompareConfig, ComparisonReport, Direction, SectionReport, UncomparedReason, Verdict, compare};
pub use keep_up::{KeepUpCell, KeepUpComparison, KeepUpSection};
pub use latency::{CellComparison, LatencySection, is_latency_section};
pub use metric::{CellJson, Metric};
pub use render::{PLOT_ANCHOR_PREFIX, STICKY_MARKER, bistable_count, headline_counts, markdown};
pub use throughput::{ThroughputCell, ThroughputComparison, ThroughputSection};
pub use trial::{RawSection, TRIAL_SCHEMA, TrialReport, probe_schema};
