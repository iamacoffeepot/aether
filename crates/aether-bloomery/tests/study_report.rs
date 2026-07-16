//! The forecast-grading study report (ADR-0151, issue #3525): `grade` is a pure
//! read over a snapshot that folds a bloom's admitted study records into actual
//! cost / wall-clock / retries and grades them against the sealed forecast.
//!
//! These cases exercise the owned fold logic — the token/duration summation, the
//! journal-derived retry count, and the per-axis over/under deltas — not the
//! serde/wire machinery the value types derive.

#![allow(clippy::unwrap_used)]

mod common;

use std::collections::BTreeMap;

use aether_bloomery::{
    BloomId, Digest, Evidence, EvidenceKind, Fact, Forecast, Snapshot, StudyCost, StudyRecord, grade,
};
use common::{digest, draft, event, membership, step};

/// A study record carrying the graded cost columns (the `bloom` / `subject`
/// fields are immaterial to the grade — it reads only `cost`).
fn study(cost: StudyCost) -> StudyRecord {
    StudyRecord { bloom: BloomId(digest(0)), subject: digest(0), cost }
}

/// A cost with the token columns and duration the grade sums; the cache-write
/// TTL splits are set so a double-count would be visible.
fn cost(input: u64, cache_write: u64, cache_read: u64, output: u64, duration_millis: u64) -> StudyCost {
    StudyCost {
        cost_micro_usd: 0,
        turns: 0,
        duration_millis,
        input_tokens: input,
        cache_write_tokens: cache_write,
        cache_write_1h_tokens: cache_write,
        cache_write_5m_tokens: 0,
        cache_read_tokens: cache_read,
        output_tokens: output,
    }
}

/// Seal a bloom on a fresh snapshot carrying the given forecast.
fn sealed_with_forecast(forecast: Forecast) -> (Snapshot, BloomId) {
    let mut d = draft(1, vec![membership("wp", 10)]);
    d.forecast = forecast;
    let spec = d.seal();
    let bloom = spec.id();
    let (snapshot, _) = step(&Snapshot::new(digest(1)), &event("seal", Fact::Seal(spec)));
    (snapshot, bloom)
}

// ADR-0151 — the grade sums a bloom's study records into actual tokens (uncached
// input + cache-write + cache-read + output, the TTL splits not re-added),
// whole-second wall-clock, and retries beyond the first attempt, then reports the
// signed over/under delta per axis against the sealed forecast.
#[test]
fn grade_folds_actuals_and_signs_the_deltas() {
    // Forecast chosen so cost overshoots, wall-clock undershoots, and retries
    // overshoot — a mixed-sign grade that exercises both delta directions.
    let (snapshot, bloom) =
        sealed_with_forecast(Forecast { predicted_cost: 500, predicted_secs: 3, predicted_retries: 0 });

    let record_a = study(cost(100, 50, 200, 30, 1500)); // 380 tokens, 1500 ms
    let record_b = study(cost(200, 0, 100, 20, 800)); //  320 tokens,  800 ms
    let mut records = BTreeMap::new();
    records.insert(digest(80), record_a);
    records.insert(digest(81), record_b);

    let mut snapshot = snapshot;
    for (key, subject, detail) in [(0u8, digest(70), digest(80)), (1u8, digest(71), digest(81))] {
        let evidence = Evidence { subject, kind: EvidenceKind::StudyRecord, detail };
        let (next, _) = step(&snapshot, &event(&format!("admit-{key}"), Fact::AdmitEvidence { bloom, evidence }));
        snapshot = next;
    }

    let report = grade(&snapshot, |d: &Digest| records.get(d).copied());
    assert_eq!(report.blooms.len(), 1);
    let g = report.blooms[0];
    assert_eq!(g.bloom, bloom);
    assert_eq!(g.actual_cost, 700, "380 + 320 tokens, TTL splits not double-counted");
    assert_eq!(g.actual_secs, 2, "2300 ms floors to 2 whole seconds");
    assert_eq!(g.actual_retries, 1, "two attempts is one retry beyond the first");
    assert_eq!(g.cost_delta, 200, "700 actual over 500 predicted");
    assert_eq!(g.secs_delta, -1, "2 actual under 3 predicted");
    assert_eq!(g.retries_delta, 1, "1 actual over 0 predicted");
}

// ADR-0151 — a bloom with no study evidence grades to zero actuals against its
// forecast, so every delta is the negated prediction (fully under budget).
#[test]
fn grade_of_a_bloom_with_no_study_evidence_is_zero_actuals() {
    let (snapshot, bloom) =
        sealed_with_forecast(Forecast { predicted_cost: 500, predicted_secs: 3, predicted_retries: 2 });

    let report = grade(&snapshot, |_: &Digest| None);
    let g = report.blooms[0];
    assert_eq!(g.bloom, bloom);
    assert_eq!((g.actual_cost, g.actual_secs, g.actual_retries), (0, 0, 0));
    assert_eq!((g.cost_delta, g.secs_delta, g.retries_delta), (-500, -3, -2));
}

// ADR-0151 — the retry axis is journal-derived: a study record whose bytes the
// resolver cannot read still counts as an attempt (so retries reflect the log),
// but contributes no cost or wall-clock.
#[test]
fn unresolved_study_records_still_count_toward_retries() {
    let (snapshot, bloom) = sealed_with_forecast(Forecast::default());

    let mut snapshot = snapshot;
    for i in 0..3u8 {
        let evidence = Evidence { subject: digest(70 + i), kind: EvidenceKind::StudyRecord, detail: digest(80 + i) };
        let (next, _) = step(&snapshot, &event(&format!("admit-{i}"), Fact::AdmitEvidence { bloom, evidence }));
        snapshot = next;
    }

    // The resolver reads nothing, so cost/time stay zero while the three logged
    // attempts still yield two retries.
    let report = grade(&snapshot, |_: &Digest| None);
    let g = report.blooms[0];
    assert_eq!((g.actual_cost, g.actual_secs), (0, 0));
    assert_eq!(g.actual_retries, 2, "three logged attempts is two retries, independent of resolvability");
}
