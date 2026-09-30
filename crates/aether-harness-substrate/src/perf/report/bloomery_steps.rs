//! The bloomery steps section and its paired compare: per-step wall time for
//! the driver's program steps, keyed by (concurrent sessions × workload shape ×
//! step) and classified lower-is-better per percentile.

use serde::{Deserialize, Serialize};

use super::comparison::{CompareConfig, Direction, SectionReport, Verdict, paired};
use super::metric::Pct;
use super::trial::TrialReport;

/// One program step's wall-time percentiles in a single bloomery trial. The
/// step is the span name the driver emits under its step tracing target
/// (`invoke`, `append`, …), kept as a string so a new step needs no report
/// change. All times are nanoseconds.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct StepCell {
    /// How many sessions ran concurrently.
    pub sessions: usize,
    /// The workload shape the sessions ran.
    pub shape: String,
    pub step: String,
    pub p50: u64,
    pub p90: u64,
    pub p99: u64,
    pub max: u64,
    pub n: usize,
}

impl StepCell {
    fn percentile(&self, p: Pct) -> f64 {
        p.pick(self.p50, self.p90, self.p99)
    }

    fn key(&self) -> StepKey {
        StepKey { sessions: self.sessions, shape: self.shape.clone(), step: self.step.clone() }
    }
}

/// The bloomery steps section: one [`StepCell`] per (sessions × shape × step).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct StepsSection {
    pub cells: Vec<StepCell>,
}

impl StepsSection {
    /// The section name the comparator dispatches on.
    pub const NAME: &str = "bloomery.steps";
    /// The section version. Bumped when the step cell shape changes.
    pub const VERSION: &str = "v1";
}

/// Pairing key for a step cell across trials.
struct StepKey {
    sessions: usize,
    shape: String,
    step: String,
}

impl StepKey {
    fn matches(&self, c: &StepCell) -> bool {
        c.sessions == self.sessions && c.shape == self.shape && c.step == self.step
    }
}

/// One compared step cell at one percentile: each side's across-trial median
/// and IQR, plus the lower-is-better paired-delta verdict. Nanoseconds.
#[derive(Serialize, Clone, Debug)]
pub struct StepComparison {
    pub sessions: usize,
    pub shape: String,
    pub step: String,
    pub percentile: &'static str,
    pub base_median: f64,
    pub base_iqr: f64,
    pub cand_median: f64,
    pub cand_iqr: f64,
    pub delta_median: f64,
    pub delta_pct: f64,
    pub verdict: Verdict,
}

/// Per-trial step cells, decoding each trial's `bloomery.steps` body and
/// leaving a trial whose body doesn't decode empty (it then fails the
/// present-in-every-trial gate, as a missing cell does).
pub(super) fn decode_steps_cells(trials: &[TrialReport]) -> Vec<Vec<StepCell>> {
    trials
        .iter()
        .map(|t| {
            t.section(StepsSection::NAME)
                .and_then(|s| serde_json::from_value::<StepsSection>(s.body.clone()).ok())
                .map(|s| s.cells)
                .unwrap_or_default()
        })
        .collect()
}

/// The cell matching `key` from each of the first `k` trials — one side of a
/// step comparison row.
fn step_hits<'a>(trials: &'a [Vec<StepCell>], k: usize, key: &StepKey) -> Vec<&'a StepCell> {
    trials[..k.min(trials.len())].iter().filter_map(|c| c.iter().find(|x| key.matches(x))).collect()
}

/// The steps section's paired compare: keys come from the first base trial,
/// a key absent from any of the K trials of either side is skipped, and each
/// remaining cell yields one lower-is-better row per percentile. `max` and `n`
/// ride along uncompared, as in the latency section.
pub(super) fn compare_steps(
    name: &str,
    base_cells: &[Vec<StepCell>],
    cand_cells: &[Vec<StepCell>],
    k: usize,
    cfg: CompareConfig,
) -> SectionReport {
    let mut cells: Vec<StepComparison> = Vec::new();

    let keys: Vec<StepKey> = base_cells.first().map(|c| c.iter().map(StepCell::key).collect()).unwrap_or_default();

    for key in &keys {
        let base_hits = step_hits(base_cells, k, key);
        let cand_hits = step_hits(cand_cells, k, key);
        if base_hits.len() != k || cand_hits.len() != k || k == 0 {
            continue; // cell not present in every trial — skip
        }

        for p in Pct::ALL {
            let stats = paired(
                base_hits.iter().map(|c| c.percentile(p)).collect(),
                cand_hits.iter().map(|c| c.percentile(p)).collect(),
                Direction::LowerIsBetter,
                cfg,
            );
            cells.push(StepComparison {
                sessions: key.sessions,
                shape: key.shape.clone(),
                step: key.step.clone(),
                percentile: p.label(),
                base_median: stats.base_median,
                base_iqr: stats.base_iqr,
                cand_median: stats.cand_median,
                cand_iqr: stats.cand_iqr,
                delta_median: stats.delta_median,
                delta_pct: stats.delta_pct,
                verdict: stats.verdict,
            });
        }
    }

    let improved = cells.iter().filter(|c| c.verdict == Verdict::Improved).count();
    let regressed = cells.iter().filter(|c| c.verdict == Verdict::Regressed).count();
    let stable = cells.len() - improved - regressed;
    SectionReport::BloomeryStepsCompared { name: name.to_owned(), improved, stable, regressed, cells }
}

#[cfg(test)]
mod tests {
    use crate::perf::report::fixture::*;
    use crate::perf::report::*;

    #[test]
    fn a_faster_step_reads_improved_while_its_unchanged_sibling_stays_stable() {
        // `invoke` drops from ~200µs to ~100µs every trial; `append`, in the
        // same (sessions, shape), is unchanged. A key that ignored the step
        // would pair `invoke` against `append` and misread one of them.
        let base = steps_side(&[200_000, 198_000, 202_000, 199_000, 201_000, 200_500]);
        let cand = steps_side(&[100_000, 99_000, 101_000, 100_500, 99_500, 100_000]);
        let rep = compare(&base, &cand, CompareConfig::default());
        assert_eq!(step_p50_verdict(&rep, "invoke"), Verdict::Improved);
        assert_eq!(step_p50_verdict(&rep, "append"), Verdict::Stable);
    }

    #[test]
    fn a_slower_step_reads_regressed() {
        let base = steps_side(&[100_000, 99_000, 101_000, 100_500, 99_500, 100_000]);
        let cand = steps_side(&[200_000, 198_000, 202_000, 199_000, 201_000, 200_500]);
        let rep = compare(&base, &cand, CompareConfig::default());
        assert_eq!(step_p50_verdict(&rep, "invoke"), Verdict::Regressed);
    }

    #[test]
    fn uniform_run_order_drift_reads_stable() {
        // Both sides drift hard across trials but track each other within
        // ~1µs per paired trial, so the paired delta is ≈ 0.
        let base = steps_side(&[100_000, 130_000, 160_000, 190_000, 220_000, 250_000]);
        let cand = steps_side(&[100_800, 129_300, 160_600, 189_200, 220_900, 249_500]);
        let rep = compare(&base, &cand, CompareConfig::default());
        assert_eq!(step_p50_verdict(&rep, "invoke"), Verdict::Stable);
    }
}
