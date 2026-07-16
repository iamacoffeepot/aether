//! The forecast-grading study report (ADR-0151, issue #3525).
//!
//! [`grade`] is a pure read over a [`Snapshot`]: per bloom, it folds the
//! admitted [`EvidenceKind::StudyRecord`] entries in the bloom's evidence log
//! into actual cost / wall-clock / retries and grades them against the sealed
//! [`Forecast`]. It resolves each study evidence's `detail` digest to its
//! [`StudyRecord`] bytes through a read-only resolver — the evidence log holds
//! digests, not the cost columns, so a snapshot-only signature could not read
//! actuals; resolving content-addressed bytes is a read, inside ADR-0151's
//! "pure read / no side effects" envelope.
//!
//! It mutates nothing and opens no port: the report is a projection any
//! consumer (the REST control surface, the outward mirror, the study stage's
//! own attempt) computes from the journal-derived snapshot.

use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use crate::digest::Digest;
use crate::ids::BloomId;
use crate::reduce::Snapshot;
use crate::values::{EvidenceKind, Forecast, StudyRecord};

/// A forecast grade for one bloom: the summed actuals from its admitted study
/// records, the sealed forecast they are graded against, and the per-axis
/// over/under delta (`actual − predicted`, so a positive delta overshot the
/// forecast).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct BloomGrade {
    /// The graded bloom.
    pub bloom: BloomId,
    /// The bloom's sealed forecast — the promise the actuals are graded against.
    pub forecast: Forecast,
    /// Actual total cost in tokens, summed over every resolved study record
    /// (uncached input + cache-write + cache-read + output; the cache-write
    /// TTL splits are already summed in cache-write and are not re-added).
    pub actual_cost: u64,
    /// Actual wall-clock time in whole seconds, summed over the study records'
    /// durations.
    pub actual_secs: u64,
    /// Actual retries — study-record attempts beyond the first (a single
    /// attempt is zero retries, matching `Budget::retry_cap`'s beyond-the-first
    /// semantics). Derived from the journal's evidence log, so it stands even
    /// for a record whose bytes the resolver cannot read.
    pub actual_retries: u32,
    /// `actual_cost − predicted_cost` (positive overshot the forecast).
    pub cost_delta: i64,
    /// `actual_secs − predicted_secs` (positive overshot the forecast).
    pub secs_delta: i64,
    /// `actual_retries − predicted_retries` (positive overshot the forecast).
    pub retries_delta: i64,
}

/// The signed over/under delta `actual − predicted`, computed on the unsigned
/// magnitudes so no `u64 → i64` cast can wrap: a positive result overshot the
/// forecast, a negative one came in under. The saturating fallbacks are
/// unreachable for real token/second counts (they never approach `i64::MAX`) and
/// keep the function total.
fn delta(actual: u64, predicted: u64) -> i64 {
    if actual >= predicted {
        i64::try_from(actual - predicted).unwrap_or(i64::MAX)
    } else {
        -i64::try_from(predicted - actual).unwrap_or(i64::MAX)
    }
}

/// The per-bloom forecast grade for every bloom the snapshot knows.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct StudyReport {
    /// One grade per bloom, in the snapshot's bloom-id order.
    pub blooms: Vec<BloomGrade>,
}

/// Grade every bloom's admitted study records against its sealed forecast
/// (ADR-0151). Pure: reads the snapshot and resolves study-record bytes through
/// `source`, mutates nothing.
///
/// `source` resolves a study evidence's `detail` digest to its [`StudyRecord`]
/// bytes, returning `None` when the artifact is unavailable — an unresolvable
/// record still counts toward the retry axis (that count is journal-derived) but
/// contributes no cost or time. A bloom with no study evidence grades to zero
/// actuals against its forecast.
#[must_use]
pub fn grade(snapshot: &Snapshot, source: impl Fn(&Digest) -> Option<StudyRecord>) -> StudyReport {
    let blooms = snapshot
        .blooms
        .iter()
        .map(|(id, record)| {
            let forecast = record.spec.forecast();
            let mut actual_cost = 0u64;
            let mut actual_millis = 0u64;
            let mut attempts = 0u32;
            for evidence in &record.evidence {
                if evidence.kind != EvidenceKind::StudyRecord {
                    continue;
                }
                attempts = attempts.saturating_add(1);
                if let Some(study) = source(&evidence.detail) {
                    let cost = &study.cost;
                    actual_cost = actual_cost
                        .saturating_add(cost.input_tokens)
                        .saturating_add(cost.cache_write_tokens)
                        .saturating_add(cost.cache_read_tokens)
                        .saturating_add(cost.output_tokens);
                    actual_millis = actual_millis.saturating_add(cost.duration_millis);
                }
            }
            let actual_secs = actual_millis / 1000;
            let actual_retries = attempts.saturating_sub(1);
            BloomGrade {
                bloom: *id,
                forecast,
                actual_cost,
                actual_secs,
                actual_retries,
                cost_delta: delta(actual_cost, forecast.predicted_cost),
                secs_delta: delta(actual_secs, forecast.predicted_secs),
                retries_delta: i64::from(actual_retries) - i64::from(forecast.predicted_retries),
            }
        })
        .collect();
    StudyReport { blooms }
}
