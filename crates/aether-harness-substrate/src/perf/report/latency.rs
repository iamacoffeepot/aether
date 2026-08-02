//! The latency section and its paired compare: the (worker × topology ×
//! metric) percentile grid every workload tier reports, and the per-cell
//! paired-delta verdict [`compare_latency`] derives from K trials of it.

use serde::{Deserialize, Serialize};

use crate::perf::stats::{iqr_sorted, median_sorted, sorted};

use super::comparison::{CompareConfig, Direction, SectionReport, Verdict, classify};
use super::metric::{CellJson, CellKey, Metric, Pct, find_cell};
use super::trial::TrialReport;

/// The per-cell latency section: today's only section, carrying the
/// (worker × topology × metric) percentile grid. Its `version` bumps
/// whenever the metric set changes, leaving sibling sections comparable.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct LatencySection {
    pub cells: Vec<CellJson>,
}

impl LatencySection {
    /// The light tier's section name — the historical `latency`, kept
    /// verbatim so the v3 back-compat shim and the existing fixtures don't
    /// churn. The heavy / real tiers use tier-suffixed names
    /// ([`super::harness::Tier::section_name`](crate::perf::harness::Tier::section_name)).
    pub const NAME: &str = "latency";
    /// The section version. Bumped when the metric set changes; sibling
    /// sections stay comparable across the bump.
    pub const VERSION: &str = "v1";
}

/// Is `name` a latency section of *any* tier (ADR-0085 amendment)? The
/// light tier reuses the bare `latency` name; heavy / real are tier-suffixed
/// (`latency.heavy`, `latency.real`). The comparator routes all of them to
/// the same per-cell paired compare — the verdict numbers are wanted for
/// every tier; suppression is a render-time concern, not a compare-time one.
#[must_use]
pub fn is_latency_section(name: &str) -> bool {
    name == LatencySection::NAME || name == "latency.heavy" || name == "latency.real"
}

/// Whether a latency section's verdict is *rendered* (ADR-0085 amendment).
/// Only the light tier (`latency`) carries a verdict; heavy / real are
/// characterisation — numbers + direction only, no verdict column, no
/// lifted "rows that moved", no "nothing moved" note. The comparator still
/// computes the real verdict for every tier (`classify` is untouched); this
/// gates only the renderer.
#[must_use]
pub(super) fn latency_section_renders_verdict(name: &str) -> bool {
    name == LatencySection::NAME
}

/// One compared cell — display bands per side (IQR across trials) plus
/// the paired-delta verdict.
#[derive(Serialize, Clone, Debug)]
pub struct CellComparison {
    pub workers: usize,
    pub topo: String,
    pub metric: Metric,
    pub percentile: &'static str,
    /// Nanoseconds.
    pub base_median: f64,
    pub base_iqr: f64,
    pub cand_median: f64,
    pub cand_iqr: f64,
    pub delta_median: f64,
    pub delta_pct: f64,
    pub verdict: Verdict,
}

/// Per-trial latency cells for the named tier section (`latency`,
/// `latency.heavy`, or `latency.real`; ADR-0085 amendment), decoding each
/// trial's body and dropping any trial whose body doesn't decode (it then
/// can't satisfy the present-in-every-trial gate below, exactly as a missing
/// cell did).
pub(super) fn decode_latency_cells(name: &str, trials: &[TrialReport]) -> Vec<Vec<CellJson>> {
    trials
        .iter()
        .map(|t| {
            t.section(name)
                .and_then(|s| serde_json::from_value::<LatencySection>(s.body.clone()).ok())
                .map(|l| l.cells)
                .unwrap_or_default()
        })
        .collect()
}

/// Tail mass at or above which a cell counts as having booted into its slow
/// mode. Set an order of magnitude below the ~8.5% (17 of 200 frames) #4177
/// measured, so the check reads the presence of a tail rather than its exact
/// size, and well above the zero a clean cell reports.
const MODE_PRESENT_TAIL_MASS: f64 = 0.01;

/// Today's per-cell paired-delta compare, extracted for the `latency`
/// section. `base_cells[t]` / `cand_cells[t]` are trial `t`'s cells;
/// cells are keyed by (workers, topo, metric) across the K trials and a
/// cell missing from any trial of either side is dropped — preserving
/// the pre-sections semantics exactly.
#[allow(clippy::cast_precision_loss)]
pub(super) fn compare_latency(
    name: &str,
    base_cells: &[Vec<CellJson>],
    cand_cells: &[Vec<CellJson>],
    k: usize,
    cfg: CompareConfig,
) -> SectionReport {
    let mut cells: Vec<CellComparison> = Vec::new();

    // Key set = cells in the first base trial; verified present across
    // all trials of both sides before comparing.
    let keys: Vec<CellKey> = base_cells.first().map(|c| c.iter().map(CellJson::key).collect()).unwrap_or_default();

    for key in &keys {
        // Per-trial lookup of this cell on each side.
        let base_hits: Vec<&CellJson> =
            base_cells[..k.min(base_cells.len())].iter().filter_map(|c| find_cell(c, key)).collect();
        let cand_hits: Vec<&CellJson> =
            cand_cells[..k.min(cand_cells.len())].iter().filter_map(|c| find_cell(c, key)).collect();
        if base_hits.len() != k || cand_hits.len() != k || k == 0 {
            continue; // cell not present in every trial — skip
        }

        // Read once per cell, not per percentile: the mode is a property of the
        // cell's sample population, and all three percentiles are drawn from it.
        let bistable = flipped_mode(&base_hits) || flipped_mode(&cand_hits);

        for p in Pct::ALL {
            let base_vals: Vec<f64> = base_hits.iter().map(|c| c.percentile(p)).collect();
            let cand_vals: Vec<f64> = cand_hits.iter().map(|c| c.percentile(p)).collect();
            let deltas: Vec<f64> = (0..k).map(|t| cand_vals[t] - base_vals[t]).collect();

            let base_sorted = sorted(base_vals.clone());
            let cand_sorted = sorted(cand_vals.clone());
            let delta_sorted = sorted(deltas.clone());

            let base_median = median_sorted(&base_sorted);
            let cand_median = median_sorted(&cand_sorted);
            let delta_median = median_sorted(&delta_sorted);
            let delta_iqr = iqr_sorted(&delta_sorted);

            // A cell that changed mode between trials has no single value to
            // compare, so the mode check precedes the paired classification
            // rather than annotating it.
            let verdict = if bistable {
                Verdict::Bistable
            } else {
                classify(&deltas, delta_median, delta_iqr, base_median, Direction::LowerIsBetter, cfg)
            };
            let delta_pct = if base_median > 0.0 {
                delta_median / base_median * 100.0
            } else {
                0.0
            };

            cells.push(CellComparison {
                workers: key.workers,
                topo: key.topo.clone(),
                metric: key.metric,
                percentile: p.label(),
                base_median,
                base_iqr: iqr_sorted(&base_sorted),
                cand_median,
                cand_iqr: iqr_sorted(&cand_sorted),
                delta_median,
                delta_pct,
                verdict,
            });
        }
    }

    let improved = cells.iter().filter(|c| c.verdict == Verdict::Improved).count();
    let regressed = cells.iter().filter(|c| c.verdict == Verdict::Regressed).count();
    let bistable = cells.iter().filter(|c| c.verdict == Verdict::Bistable).count();
    let stable = cells.len() - improved - regressed - bistable;
    SectionReport::Compared { name: name.to_owned(), improved, stable, regressed, cells }
}

/// Did this side's trials disagree about which mode the cell was in?
///
/// True when some trials carry a tail and others carry essentially none —
/// evidence the cell booted differently between runs rather than that the code
/// changed. Compares against [`MODE_PRESENT_TAIL_MASS`] rather than looking for
/// a spread, so a cell that is *consistently* tailed (a genuinely bimodal
/// workload, same in every trial) is not flagged: that one has a stable
/// distribution and its percentiles do compare.
fn flipped_mode(trials: &[&CellJson]) -> bool {
    let tailed = trials.iter().filter(|c| c.tail_mass >= MODE_PRESENT_TAIL_MASS).count();
    tailed > 0 && tailed < trials.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::perf::report::fixture::*;
    use crate::perf::report::*;

    /// Tripwire: a cell that changed mode between trials is withheld rather
    /// than compared (iamacoffeepot/aether#4265).
    ///
    /// Both sides here have an identical, perfectly stable `p50`, so the paired
    /// classifier would call this `stable` with full confidence. It is not
    /// stable — the base booted into its slow mode in half its trials. That is
    /// exactly the shape #4177's comparison had, and reporting a confident
    /// delta across it is how mode selection reads as a code change.
    #[test]
    fn a_cell_that_flipped_mode_is_withheld_not_compared() {
        let base = side_with_tails(&[0.0, 0.085, 0.0, 0.085, 0.0, 0.085]);
        let cand = side_with_tails(&[0.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
        let rep = compare(&base, &cand, CompareConfig::default());

        assert_eq!(p50_verdict(&rep), Verdict::Bistable, "a flipped cell has no single value to compare");
        assert_eq!(bistable_count(&rep), 3, "all three percentiles of the cell are withheld");

        let SectionReport::Compared { stable, regressed, improved, .. } = latency_section(&rep) else {
            panic!("latency section not compared");
        };
        assert_eq!(
            (*improved, *stable, *regressed),
            (0, 0, 0),
            "a withheld cell must not be counted as stable — that is how it would disappear",
        );
    }

    /// A cell that is *consistently* tailed has a stable distribution, so its
    /// percentiles do compare. Only disagreement between trials is the signal.
    #[test]
    fn a_consistently_tailed_cell_still_compares() {
        let base = side_with_tails(&[0.085, 0.085, 0.085, 0.085, 0.085, 0.085]);
        let cand = side_with_tails(&[0.085, 0.085, 0.085, 0.085, 0.085, 0.085]);
        let rep = compare(&base, &cand, CompareConfig::default());
        assert_eq!(p50_verdict(&rep), Verdict::Stable, "a steadily bimodal workload is still measurable");
        assert_eq!(bistable_count(&rep), 0);
    }

    #[test]
    fn consistent_win_reads_improved() {
        // base ~167µs, cand ~33µs, every trial — the fan-out win.
        let base = side(&[167_000, 165_000, 169_000, 166_000, 168_000, 170_000, 164_000, 167_000]);
        let cand = side(&[33_000, 34_000, 32_000, 33_500, 33_000, 31_000, 34_000, 33_000]);
        let rep = compare(&base, &cand, CompareConfig::default());
        assert_eq!(p50_verdict(&rep), Verdict::Improved);
    }

    #[test]
    fn consistent_regression_reads_regressed() {
        // base ~1.0µs, cand ~1.4µs every trial (+40%, the depth-8 example).
        let base = side(&[1000, 960, 1040, 980, 1010, 990, 1020, 1000]);
        let cand = side(&[1400, 1360, 1440, 1380, 1410, 1390, 1420, 1400]);
        let rep = compare(&base, &cand, CompareConfig::default());
        assert_eq!(p50_verdict(&rep), Verdict::Regressed);
    }

    #[test]
    fn uniform_run_order_drift_reads_stable() {
        // Both sides drift hard across trials (thermal/background), but
        // the candidate tracks the baseline within ~30ns per paired
        // trial. Unpaired this is two wide clouds; paired, δ ≈ 0.
        let base = side(&[1000, 1300, 1600, 1900, 2200, 2500, 2800, 3100]);
        let cand = side(&[1030, 1330, 1570, 1930, 2170, 2530, 2770, 3130]);
        let rep = compare(&base, &cand, CompareConfig::default());
        assert_eq!(p50_verdict(&rep), Verdict::Stable);
    }

    #[test]
    fn one_off_outlier_reads_stable() {
        // Steady ~1µs both sides, except one candidate trial spikes —
        // the median of paired deltas shrugs it off.
        let base = side(&[1000, 1000, 1000, 1000, 1000, 1000, 1000, 1000]);
        let cand = side(&[1010, 990, 1000, 600_000, 1000, 1005, 995, 1000]);
        let rep = compare(&base, &cand, CompareConfig::default());
        assert_eq!(p50_verdict(&rep), Verdict::Stable);
    }

    #[test]
    fn tiny_consistent_change_is_below_practical_floor() {
        // +30ns on a 1µs base is perfectly consistent but only 3% —
        // below the 10% relative floor, so it stays stable rather than
        // crying wolf on a sub-noise dispatch-glue change.
        let base = side(&[1000, 1000, 1000, 1000, 1000, 1000, 1000, 1000]);
        let cand = side(&[1030, 1030, 1030, 1030, 1030, 1030, 1030, 1030]);
        let rep = compare(&base, &cand, CompareConfig::default());
        assert_eq!(p50_verdict(&rep), Verdict::Stable);
    }

    #[test]
    fn sub_microsecond_consistent_shift_is_below_absolute_floor() {
        // A consistent 170ns -> 120ns shift (50ns) on a sub-µs handler
        // cell is a 30% relative change but below the harness's
        // resolution — must read stable, not "improved". (Regression
        // guard for the dry-run finding where identical binaries
        // differed ~50ns on depth-1 handler and flagged a false win.)
        let base = side(&[170, 170, 165, 172, 168, 170, 169, 171]);
        let cand = side(&[120, 122, 118, 121, 119, 120, 123, 120]);
        let rep = compare(&base, &cand, CompareConfig::default());
        assert_eq!(p50_verdict(&rep), Verdict::Stable);
    }

    #[test]
    fn report_json_round_trip_preserves_latency_section() {
        let trials = side(&[1000, 1100, 1200]);
        let report = &trials[0];
        let json = serde_json::to_string(report).expect("serialize trial");
        let back: TrialReport = serde_json::from_str(&json).expect("deserialize trial");
        assert_eq!(back.schema, TRIAL_SCHEMA);
        assert_eq!(back.sections.len(), 1);
        let sec = &back.sections[0];
        assert_eq!(sec.name, LatencySection::NAME);
        assert_eq!(sec.version, LatencySection::VERSION);
        let latency: LatencySection = serde_json::from_value(sec.body.clone()).expect("decode latency body");
        assert_eq!(latency.cells.len(), 1);
        assert_eq!(latency.cells[0].metric, Metric::Drain);
        assert_eq!(latency.cells[0].p50, 1000);
    }

    #[test]
    fn real_tier_section_compares_and_is_suppressed() {
        // The real tier parses and sections in PR 1 (its factories are empty
        // until PR 2), so a `latency.real` section — if present — routes to
        // the same compare and is verdict-suppressed at render, exactly like
        // heavy.
        let base = tier_side("latency.real", &[167_000, 165_000, 169_000, 166_000]);
        let cand = tier_side("latency.real", &[33_000, 34_000, 32_000, 33_500]);
        let rep = compare(&base, &cand, CompareConfig::default());
        assert!(
            rep.sections.iter().any(|s| matches!(s, SectionReport::Compared { name, .. } if name == "latency.real")),
            "real latency section routes to the per-cell compare"
        );
        let (improved, _stable, regressed) = headline_counts(&rep);
        assert_eq!((improved, regressed), (0, 0), "the real tier's verdict is excluded from the headline too");
    }
}
