//! The bloomery session section and its compare: per-run session throughput
//! (a higher-is-better verdict), journal growth and peak memory (trend only),
//! keyed by (concurrent sessions × workload shape).

use serde::{Deserialize, Serialize};

use crate::perf::stats::{median_sorted, sorted};

use super::comparison::{CompareConfig, Direction, PairedStats, SectionReport, Verdict, paired};
use super::metric::measured;
use super::trial::TrialReport;

/// One (sessions × shape) run's session figures in a single bloomery trial.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SessionCell {
    /// How many sessions ran concurrently.
    pub sessions: usize,
    /// The workload shape the sessions ran.
    pub shape: String,
    /// Completed sessions per second; `None` when the run could not measure a
    /// rate. The cell is still emitted, so the gap is visible rather than
    /// dropped or read as zero.
    pub sessions_per_sec: Option<f64>,
    /// Journal bytes appended per session. Trend only.
    pub journal_bytes_per_session: u64,
    /// Peak resident memory of the run; `None` where the platform gives no
    /// reading. Trend only.
    pub peak_rss_bytes: Option<u64>,
}

/// The bloomery session section: one [`SessionCell`] per (sessions × shape).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SessionSection {
    pub cells: Vec<SessionCell>,
}

impl SessionSection {
    /// The section name the comparator dispatches on.
    pub const NAME: &str = "bloomery.session";
    /// The section version. Bumped when the session cell shape changes.
    pub const VERSION: &str = "v1";
}

/// One compared session cell. `rate` carries the higher-is-better sessions/sec
/// verdict, and is `None` when any trial on either side lacked a rate: the row
/// then reads unmeasured and counts toward no verdict. The journal and memory
/// fields are across-trial medians with no verdict; a side's peak memory is
/// `None` unless every trial on that side reported one.
#[derive(Serialize, Clone, Debug)]
pub struct SessionComparison {
    pub sessions: usize,
    pub shape: String,
    pub rate: Option<PairedStats>,
    pub base_journal_bytes: f64,
    pub cand_journal_bytes: f64,
    pub base_peak_rss_bytes: Option<f64>,
    pub cand_peak_rss_bytes: Option<f64>,
}

/// Per-trial session cells, decoding each trial's `bloomery.session` body and
/// leaving a trial whose body doesn't decode empty (it then fails the
/// present-in-every-trial gate, as a missing cell does).
pub(super) fn decode_session_cells(trials: &[TrialReport]) -> Vec<Vec<SessionCell>> {
    trials
        .iter()
        .map(|t| {
            t.section(SessionSection::NAME)
                .and_then(|s| serde_json::from_value::<SessionSection>(s.body.clone()).ok())
                .map(|s| s.cells)
                .unwrap_or_default()
        })
        .collect()
}

/// The cell matching (`sessions`, `shape`) from each of the first `k` trials —
/// one side of a session comparison row. Presence is the cell's, not its
/// rate's, so an unmeasured rate still yields its trend.
fn session_hits<'a>(trials: &'a [Vec<SessionCell>], k: usize, sessions: usize, shape: &str) -> Vec<&'a SessionCell> {
    trials[..k.min(trials.len())]
        .iter()
        .filter_map(|c| c.iter().find(|x| x.sessions == sessions && x.shape == shape))
        .collect()
}

/// Every hit's rate, or `None` when any hit lacks one.
fn rates(hits: &[&SessionCell]) -> Option<Vec<f64>> {
    hits.iter().map(|c| c.sessions_per_sec).collect()
}

/// The across-trial median of `values`.
fn median(values: Vec<f64>) -> f64 {
    median_sorted(&sorted(values))
}

/// The session section's compare: keys come from the first base trial and a
/// key absent from any of the K trials of either side is skipped. The rate
/// verdict is computed only when every trial on both sides measured a rate;
/// the counts cover only those rows.
pub(super) fn compare_session(
    name: &str,
    base_cells: &[Vec<SessionCell>],
    cand_cells: &[Vec<SessionCell>],
    k: usize,
    cfg: CompareConfig,
) -> SectionReport {
    let mut cells: Vec<SessionComparison> = Vec::new();

    let keys: Vec<(usize, String)> =
        base_cells.first().map(|c| c.iter().map(|x| (x.sessions, x.shape.clone())).collect()).unwrap_or_default();

    for (sessions, shape) in &keys {
        let base_hits = session_hits(base_cells, k, *sessions, shape);
        let cand_hits = session_hits(cand_cells, k, *sessions, shape);
        if base_hits.len() != k || cand_hits.len() != k || k == 0 {
            continue; // cell not present in every trial — skip
        }

        let rate = rates(&base_hits)
            .zip(rates(&cand_hits))
            .map(|(base_vals, cand_vals)| paired(base_vals, cand_vals, Direction::HigherIsBetter, cfg));
        let journal =
            |hits: &[&SessionCell]| median(hits.iter().map(|c| measured(c.journal_bytes_per_session)).collect());
        let peak_rss = |hits: &[&SessionCell]| {
            hits.iter().map(|c| c.peak_rss_bytes.map(measured)).collect::<Option<Vec<f64>>>().map(median)
        };

        cells.push(SessionComparison {
            sessions: *sessions,
            shape: shape.clone(),
            rate,
            base_journal_bytes: journal(&base_hits),
            cand_journal_bytes: journal(&cand_hits),
            base_peak_rss_bytes: peak_rss(&base_hits),
            cand_peak_rss_bytes: peak_rss(&cand_hits),
        });
    }

    let verdicts = || cells.iter().filter_map(|c| c.rate.as_ref().map(|r| r.verdict));
    let improved = verdicts().filter(|v| *v == Verdict::Improved).count();
    let regressed = verdicts().filter(|v| *v == Verdict::Regressed).count();
    let stable = verdicts().count() - improved - regressed;
    SectionReport::BloomerySessionCompared { name: name.to_owned(), improved, stable, regressed, cells }
}

#[cfg(test)]
mod tests {
    use crate::perf::report::fixture::*;
    use crate::perf::report::*;

    #[test]
    fn a_higher_session_rate_reads_improved() {
        let base = session_side(&[Some(50.0), Some(49.0), Some(51.0), Some(50.5), Some(49.5), Some(50.0)]);
        let cand = session_side(&[Some(100.0), Some(99.0), Some(101.0), Some(100.5), Some(99.5), Some(100.0)]);
        let rep = compare(&base, &cand, CompareConfig::default());
        let row = session_row(&rep);
        assert_eq!(row.rate.as_ref().expect("both sides measured a rate").verdict, Verdict::Improved);
    }

    #[test]
    fn a_lower_session_rate_reads_regressed() {
        let base = session_side(&[Some(100.0), Some(99.0), Some(101.0), Some(100.5), Some(99.5), Some(100.0)]);
        let cand = session_side(&[Some(50.0), Some(49.0), Some(51.0), Some(50.5), Some(49.5), Some(50.0)]);
        let rep = compare(&base, &cand, CompareConfig::default());
        let row = session_row(&rep);
        assert_eq!(row.rate.as_ref().expect("both sides measured a rate").verdict, Verdict::Regressed);
    }

    #[test]
    fn an_unmeasured_rate_keeps_its_row_and_trend_but_counts_no_verdict() {
        // One candidate trial measured no rate. The row must survive with its
        // journal and memory trend, carry no rate verdict, and add nothing to
        // the section's counts, rather than vanish or read as a zero rate.
        let base = session_side(&[Some(50.0), Some(49.0), Some(51.0), Some(50.5)]);
        let cand = session_side(&[Some(100.0), None, Some(101.0), Some(100.5)]);
        let rep = compare(&base, &cand, CompareConfig::default());

        let SectionReport::BloomerySessionCompared { improved, stable, regressed, cells, .. } = session_section(&rep)
        else {
            panic!("session section not compared");
        };
        assert_eq!(cells.len(), 1, "the cell with an unmeasured rate is kept");
        assert!(cells[0].rate.is_none(), "a missing rate yields no verdict");
        assert!(cells[0].cand_journal_bytes > 0.0, "the journal trend is still reported");
        assert!(cells[0].cand_peak_rss_bytes.is_some(), "the memory trend is still reported");
        assert_eq!((*improved, *stable, *regressed), (0, 0, 0), "an unmeasured row counts toward no verdict");
    }
}
