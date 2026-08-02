//! The throughput section and its paired compare: one steady-state
//! mails/sec rate per (worker × topology) cell under saturation, classified
//! higher-is-better.

use serde::{Deserialize, Serialize};

use crate::perf::stats::{iqr_sorted, median_sorted, sorted};

use super::compare::{CompareConfig, Direction, SectionReport, Verdict, classify};
use super::trial::TrialReport;

/// One cell's measured throughput in a single trial
/// (iamacoffeepot/aether#1202): a steady-state mails/sec estimate for a (worker
/// × topology) cell under saturation — the rate over the trimmed saturated
/// middle of the run, not a full-batch makespan average
/// (iamacoffeepot/aether#1227). `mails_per_sec` is `None` when the
/// cell **truncated** — the entry relay's trace ring lapped during the run,
/// so the completed-count is undercounted and any rate computed from it
/// would be wrong. Such a cell is emitted flagged-not-dropped
/// (iamacoffeepot/aether#1226) so a missing measurement is visible in the
/// section rather than silently absent; the comparator treats it as "no
/// measurement" (`find_throughput_cell` filters `None`-rate cells out of
/// the hit set).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ThroughputCell {
    pub workers: usize,
    pub topo: String,
    /// `Some(rate)` for a measured cell; `None` when the cell truncated.
    pub mails_per_sec: Option<f64>,
}

impl ThroughputCell {
    fn key(&self) -> ThroughputKey {
        ThroughputKey { workers: self.workers, topo: self.topo.clone() }
    }
}

/// The throughput section (iamacoffeepot/aether#1202): one
/// completed-mails/sec rate per (worker × topology) cell, emitted only by
/// a `Drive::Saturate` trial. Its own `version` evolves independently of
/// the latency section's, so adding it never blinds the latency verdict.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ThroughputSection {
    pub cells: Vec<ThroughputCell>,
}

impl ThroughputSection {
    /// The section name the comparator dispatches on. Mirrors the example
    /// new section iamacoffeepot/aether#1206's fixtures already named.
    pub const NAME: &str = "throughput";
    /// The section version. Bumped when the throughput cell shape changes.
    /// `v2` (iamacoffeepot/aether#1226): `mails_per_sec` became
    /// `Option<f64>` so a truncated cell is emitted flagged
    /// (`mails_per_sec: null`) instead of dropped — an older `v1` base
    /// trial (rate always present) sections cleanly against the bump.
    pub const VERSION: &str = "v2";
}

/// Pairing key for a throughput cell (iamacoffeepot/aether#1202) — a
/// (worker × topology) cell, no metric/percentile axis since throughput
/// is a single rate per cell.
#[derive(Clone, PartialEq, Eq, Hash)]
pub(super) struct ThroughputKey {
    workers: usize,
    topo: String,
}

/// One compared throughput cell (iamacoffeepot/aether#1202): the
/// base/candidate median rate (mails/sec) with its across-trial IQR band,
/// plus the higher-is-better paired-delta verdict. The throughput analog
/// of [`CellComparison`](crate::perf::report::CellComparison) — no metric/percentile axis, since throughput is
/// a single rate per (worker × topology) cell.
#[derive(Serialize, Clone, Debug)]
pub struct ThroughputComparison {
    pub workers: usize,
    pub topo: String,
    /// Mails/sec.
    pub base_median: f64,
    pub base_iqr: f64,
    pub cand_median: f64,
    pub cand_iqr: f64,
    pub delta_median: f64,
    pub delta_pct: f64,
    pub verdict: Verdict,
}

/// Per-trial throughput cells: decode each trial's `throughput` section
/// body, dropping any trial whose body doesn't decode (it then can't
/// satisfy the present-in-every-trial gate below, exactly as a missing
/// cell does for latency).
pub(super) fn decode_throughput_cells(trials: &[TrialReport]) -> Vec<Vec<ThroughputCell>> {
    trials
        .iter()
        .map(|t| {
            t.section(ThroughputSection::NAME)
                .and_then(|s| serde_json::from_value::<ThroughputSection>(s.body.clone()).ok())
                .map(|tp| tp.cells)
                .unwrap_or_default()
        })
        .collect()
}

/// Find the *measured* throughput cell matching `key` in one trial's cells
/// (a free fn so the returned borrow ties to the slice's lifetime, mirroring
/// [`find_cell`](super::metric::find_cell)). A truncated cell (`mails_per_sec == None`,
/// iamacoffeepot/aether#1226) is present in the section but is **not** a
/// measurement, so it is filtered out here: [`compare_throughput`]'s
/// "present in every trial" gate then treats the cell as absent (no
/// measurement, not a regression to zero) without any further comparator
/// branch.
pub(super) fn find_throughput_cell<'a>(cells: &'a [ThroughputCell], key: &ThroughputKey) -> Option<&'a ThroughputCell> {
    cells.iter().find(|c| c.workers == key.workers && c.topo == key.topo && c.mails_per_sec.is_some())
}

/// The throughput section's per-cell paired-delta compare
/// (iamacoffeepot/aether#1202) — mirrors [`compare_latency`](super::latency::compare_latency), but keyed by
/// (workers, topo) only (throughput is a single rate per cell, no
/// metric/percentile axis) and classified higher-is-better. A cell missing
/// from any trial of either side is dropped, exactly as in the latency
/// compare.
#[allow(clippy::cast_precision_loss)]
pub(super) fn compare_throughput(
    name: &str,
    base_cells: &[Vec<ThroughputCell>],
    cand_cells: &[Vec<ThroughputCell>],
    k: usize,
    cfg: CompareConfig,
) -> SectionReport {
    let mut cells: Vec<ThroughputComparison> = Vec::new();

    let keys: Vec<ThroughputKey> =
        base_cells.first().map(|c| c.iter().map(ThroughputCell::key).collect()).unwrap_or_default();

    for key in &keys {
        let base_hits: Vec<&ThroughputCell> =
            base_cells[..k.min(base_cells.len())].iter().filter_map(|c| find_throughput_cell(c, key)).collect();
        let cand_hits: Vec<&ThroughputCell> =
            cand_cells[..k.min(cand_cells.len())].iter().filter_map(|c| find_throughput_cell(c, key)).collect();
        if base_hits.len() != k || cand_hits.len() != k || k == 0 {
            continue; // cell not present in every trial — skip
        }

        // `find_throughput_cell` only returns measured cells, so every hit
        // carries `Some` here (iamacoffeepot/aether#1226).
        let base_vals: Vec<f64> = base_hits.iter().filter_map(|c| c.mails_per_sec).collect();
        let cand_vals: Vec<f64> = cand_hits.iter().filter_map(|c| c.mails_per_sec).collect();
        let deltas: Vec<f64> = (0..k).map(|t| cand_vals[t] - base_vals[t]).collect();

        let base_sorted = sorted(base_vals.clone());
        let cand_sorted = sorted(cand_vals.clone());
        let delta_sorted = sorted(deltas.clone());

        let base_median = median_sorted(&base_sorted);
        let cand_median = median_sorted(&cand_sorted);
        let delta_median = median_sorted(&delta_sorted);
        let delta_iqr = iqr_sorted(&delta_sorted);

        let verdict = classify(&deltas, delta_median, delta_iqr, base_median, Direction::HigherIsBetter, cfg);
        let delta_pct = if base_median > 0.0 {
            delta_median / base_median * 100.0
        } else {
            0.0
        };

        cells.push(ThroughputComparison {
            workers: key.workers,
            topo: key.topo.clone(),
            base_median,
            base_iqr: iqr_sorted(&base_sorted),
            cand_median,
            cand_iqr: iqr_sorted(&cand_sorted),
            delta_median,
            delta_pct,
            verdict,
        });
    }

    let improved = cells.iter().filter(|c| c.verdict == Verdict::Improved).count();
    let regressed = cells.iter().filter(|c| c.verdict == Verdict::Regressed).count();
    let stable = cells.len() - improved - regressed;
    SectionReport::ThroughputCompared { name: name.to_owned(), improved, stable, regressed, cells }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::perf::report::fixture::*;
    use crate::perf::report::*;

    #[test]
    fn higher_throughput_reads_improved_not_regressed() {
        // Throughput is higher-is-better: a clearly-higher candidate rate
        // is an Improvement, even though its paired delta is *positive*
        // (the opposite of a latency win).
        let base = throughput_side(&[100_000.0, 98_000.0, 102_000.0, 99_000.0, 101_000.0, 100_500.0]);
        let cand = throughput_side(&[200_000.0, 198_000.0, 202_000.0, 199_000.0, 201_000.0, 200_500.0]);
        let rep = compare(&base, &cand, CompareConfig::default());
        assert_eq!(throughput_verdict(&rep), Verdict::Improved);
    }

    #[test]
    fn lower_throughput_reads_regressed() {
        // A clearly-lower candidate rate is a regression (a negative
        // paired delta, the inverse of the latency direction).
        let base = throughput_side(&[200_000.0, 198_000.0, 202_000.0, 199_000.0, 201_000.0, 200_500.0]);
        let cand = throughput_side(&[100_000.0, 98_000.0, 102_000.0, 99_000.0, 101_000.0, 100_500.0]);
        let rep = compare(&base, &cand, CompareConfig::default());
        assert_eq!(throughput_verdict(&rep), Verdict::Regressed);
    }

    #[test]
    fn equal_throughput_reads_stable() {
        // Near-identical rates pair to δ ≈ 0 — below the noise band, so
        // stable regardless of the ns floor (neutralised for a rate).
        let base = throughput_side(&[100_000.0, 99_000.0, 101_000.0, 100_500.0, 99_500.0, 100_000.0]);
        let cand = throughput_side(&[100_200.0, 99_100.0, 101_100.0, 100_400.0, 99_600.0, 100_100.0]);
        let rep = compare(&base, &cand, CompareConfig::default());
        assert_eq!(throughput_verdict(&rep), Verdict::Stable);
    }

    #[test]
    fn report_json_round_trip_preserves_throughput_section() {
        let trials = throughput_side(&[100_000.0, 110_000.0, 120_000.0]);
        let report = &trials[0];
        let json = serde_json::to_string(report).expect("serialize trial");
        let back: TrialReport = serde_json::from_str(&json).expect("deserialize trial");
        assert_eq!(back.sections.len(), 1);
        let sec = &back.sections[0];
        assert_eq!(sec.name, ThroughputSection::NAME);
        assert_eq!(sec.version, ThroughputSection::VERSION);
        let tp: ThroughputSection = serde_json::from_value(sec.body.clone()).expect("decode throughput body");
        assert_eq!(tp.cells.len(), 1);
        let rate = tp.cells[0].mails_per_sec.expect("a measured cell round-trips its rate");
        assert!((rate - 100_000.0).abs() < f64::EPSILON);
    }

    #[test]
    fn truncated_cell_reads_as_no_measurement_in_compare() {
        // Step 4 (iamacoffeepot/aether#1226): a flagged-not-dropped truncated
        // cell must read as "no measurement" in the comparator — skipped by
        // the existing "present in every trial" gate, not scored as a rate of
        // zero. `find_throughput_cell` filters out `None`-rate cells, so the
        // cell is absent from the hit set on both sides and the gate drops it.
        let truncated_side = |k: usize| -> Vec<Vec<ThroughputCell>> {
            (0..k)
                .map(|_| vec![ThroughputCell { workers: 11, topo: "fanout-8".to_owned(), mails_per_sec: None }])
                .collect()
        };
        let k = 4;
        let report = compare_throughput(
            ThroughputSection::NAME,
            &truncated_side(k),
            &truncated_side(k),
            k,
            CompareConfig::default(),
        );
        // The only cell present is truncated on both sides, so the section
        // compares with zero scored cells (no regression-to-zero verdict).
        match report {
            SectionReport::ThroughputCompared { cells, .. } => {
                assert!(cells.is_empty(), "a truncated cell must produce no scored comparison cell, got {cells:?}");
            }
            other => panic!("expected a compared throughput section, got {other:?}"),
        }
    }
}
