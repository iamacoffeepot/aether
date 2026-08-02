//! The real tier's keep-up section and its trend compare: offered /
//! completed mail counts and the paced elapsed-vs-expected ratio, reported in
//! place of per-hop spans the real tier cannot measure. Characterisation only
//! — no verdict.

use serde::{Deserialize, Serialize};

use crate::perf::stats::{median_sorted, sorted};

use super::comparison::SectionReport;
use super::trial::TrialReport;

/// One real-tier cell's keep-up characterisation (iamacoffeepot/aether#1233):
/// did the paced topology keep up at 60 Hz? The real tier reports this
/// *instead of* per-hop span percentiles — its fan-out laps the trace ring, so
/// the span tree is unmeasurable, but the plain-field offered/completed
/// counters and the paced elapsed-vs-expected timing are not. All times are
/// nanoseconds.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct KeepUpCell {
    pub workers: usize,
    pub topo: String,
    /// Total `Ping` mails the topology dispatched (`Σ sent`) — the offered
    /// load.
    pub offered: u64,
    /// Total `Ping` mails the topology handled (`Σ received`). Equals
    /// `offered` on a fully-drained run; a shortfall means mail was stranded.
    pub completed: u64,
    /// Wall-clock the paced drive loop took.
    pub elapsed_nanos: u64,
    /// Wall-clock the loop *should* have taken at the pace (`frames /
    /// pace_hz`). `elapsed / expected > 1` is the fell-behind signal.
    pub expected_nanos: u64,
}

/// The real tier's keep-up section (iamacoffeepot/aether#1233): one
/// [`KeepUpCell`] per (worker × topology) cell, emitted by a paced real-tier
/// run in place of a `latency.real` span section. Characterisation only — like
/// the heavy / real latency sections it carries no verdict; the comparator
/// renders base-vs-candidate keep-up numbers with no pass/fail.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct KeepUpSection {
    pub cells: Vec<KeepUpCell>,
}

impl KeepUpSection {
    /// The section name the comparator dispatches on — distinct from the
    /// `latency.real` span name so a base trial built before the reframe
    /// (carrying `latency.real`) and a candidate carrying `keepup.real`
    /// section independently, each landing in its own uncompared/new block
    /// rather than version-clashing.
    pub const NAME: &str = "keepup.real";
    /// The section version. Bumped when the keep-up cell shape changes.
    pub const VERSION: &str = "v1";
}

/// One compared keep-up cell (iamacoffeepot/aether#1233): the base/candidate
/// median offered / completed counts and the pace ratio (`elapsed /
/// expected`), across trials. Characterisation only — no verdict, mirroring
/// the heavy / real latency trend treatment: the real tier's variance sits
/// below the band a paired verdict needs.
#[derive(Serialize, Clone, Debug)]
pub struct KeepUpComparison {
    pub workers: usize,
    pub topo: String,
    pub base_offered: f64,
    pub cand_offered: f64,
    pub base_completed: f64,
    pub cand_completed: f64,
    /// `elapsed / expected` — `> 1` means the run fell behind the pace.
    pub base_pace_ratio: f64,
    pub cand_pace_ratio: f64,
}

/// Per-trial keep-up cells for the `keepup.real` section
/// (iamacoffeepot/aether#1233), decoding each trial's body and dropping any
/// trial whose body doesn't decode (it then can't satisfy the
/// present-in-every-trial gate, exactly as for latency / throughput).
pub(super) fn decode_keepup_cells(trials: &[TrialReport]) -> Vec<Vec<KeepUpCell>> {
    trials
        .iter()
        .map(|t| {
            t.section(KeepUpSection::NAME)
                .and_then(|s| serde_json::from_value::<KeepUpSection>(s.body.clone()).ok())
                .map(|k| k.cells)
                .unwrap_or_default()
        })
        .collect()
}

/// Collect the matching cell from each of the first `k` trials — one side
/// (base or candidate) of a keep-up comparison row.
fn keepup_trial_hits<'a>(trials: &'a [Vec<KeepUpCell>], k: usize, workers: usize, topo: &str) -> Vec<&'a KeepUpCell> {
    trials[..k.min(trials.len())].iter().filter_map(|c| find_keepup_cell(c, workers, topo)).collect()
}

/// Find the keep-up cell matching (`workers`, `topo`) in one trial's cells (a
/// free fn so the returned borrow ties to the slice's lifetime, mirroring
/// [`find_cell`](super::metric::find_cell) / [`find_throughput_cell`](super::throughput::find_throughput_cell)).
fn find_keepup_cell<'a>(cells: &'a [KeepUpCell], workers: usize, topo: &str) -> Option<&'a KeepUpCell> {
    cells.iter().find(|c| c.workers == workers && c.topo == topo)
}

/// The keep-up section's per-cell trend compare (iamacoffeepot/aether#1233) —
/// keyed by (workers, topo), it takes the across-trial median of each
/// base/candidate field. **No verdict**: keep-up is characterisation (like the
/// heavy / real latency trend), so this reports base-vs-candidate numbers and
/// the renderer prints them with no pass/fail. A cell missing from any trial
/// of either side is dropped, exactly as in the other section compares.
#[allow(clippy::cast_precision_loss)]
pub(super) fn compare_keepup(
    name: &str,
    base_cells: &[Vec<KeepUpCell>],
    cand_cells: &[Vec<KeepUpCell>],
    k: usize,
) -> SectionReport {
    let mut cells: Vec<KeepUpComparison> = Vec::new();

    let keys: Vec<(usize, String)> =
        base_cells.first().map(|c| c.iter().map(|x| (x.workers, x.topo.clone())).collect()).unwrap_or_default();

    let pace_ratio = |c: &KeepUpCell| -> f64 {
        if c.expected_nanos > 0 {
            c.elapsed_nanos as f64 / c.expected_nanos as f64
        } else {
            0.0
        }
    };

    for (workers, topo) in &keys {
        let base_hits = keepup_trial_hits(base_cells, k, *workers, topo);
        let cand_hits = keepup_trial_hits(cand_cells, k, *workers, topo);
        if base_hits.len() != k || cand_hits.len() != k || k == 0 {
            continue; // cell not present in every trial — skip
        }

        let median_of = |hits: &[&KeepUpCell], f: &dyn Fn(&KeepUpCell) -> f64| -> f64 {
            median_sorted(&sorted(hits.iter().map(|&c| f(c)).collect()))
        };

        cells.push(KeepUpComparison {
            workers: *workers,
            topo: topo.clone(),
            base_offered: median_of(&base_hits, &|c| c.offered as f64),
            cand_offered: median_of(&cand_hits, &|c| c.offered as f64),
            base_completed: median_of(&base_hits, &|c| c.completed as f64),
            cand_completed: median_of(&cand_hits, &|c| c.completed as f64),
            base_pace_ratio: median_of(&base_hits, &pace_ratio),
            cand_pace_ratio: median_of(&cand_hits, &pace_ratio),
        });
    }

    SectionReport::KeepUpCompared { name: name.to_owned(), cells }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::perf::report::fixture::*;
    use crate::perf::report::*;

    #[test]
    fn keepup_section_compares_and_renders_trend() {
        // The keep-up section pairs into a no-verdict trend
        // (iamacoffeepot/aether#1233): base ran at pace (~1.0×), candidate
        // fell behind (~1.5×); the comparison carries the numbers, the
        // headline carries no verdict, and the markdown renders the trend grid.
        let base = keepup_side(&[100_000_000, 102_000_000, 98_000_000]);
        let cand = keepup_side(&[150_000_000, 152_000_000, 148_000_000]);
        let rep = compare(&base, &cand, CompareConfig::default());

        let cells = rep
            .sections
            .iter()
            .find_map(|s| match s {
                SectionReport::KeepUpCompared { cells, .. } => Some(cells),
                _ => None,
            })
            .expect("keep-up section compared");
        assert_eq!(cells.len(), 1);
        assert!((cells[0].base_pace_ratio - 1.0).abs() < 0.05, "base ran ~at pace, got {}", cells[0].base_pace_ratio);
        assert!(cells[0].cand_pace_ratio > 1.4, "candidate fell behind the pace, got {}", cells[0].cand_pace_ratio);

        // Characterisation only — no verdict reaches the gate-signal headline.
        let (improved, _stable, regressed) = headline_counts(&rep);
        assert_eq!((improved, regressed), (0, 0), "keep-up is characterisation; no verdict leaks into the headline");

        let md = markdown(&rep, "PR 9999 vs main", "test");
        assert!(
            md.contains("keepup.real — 1 cells, keep-up (no verdict)"),
            "the keep-up section renders as a no-verdict trend"
        );
        assert!(
            md.contains("| topology | w | offered | completed | base pace | this pace |"),
            "the keep-up trend grid header is present"
        );
        assert!(
            !md.contains("<!-- aether-perf-plots: keepup.real -->"),
            "a keep-up section emits no plot anchor (no spans to plot)"
        );
    }

    #[test]
    fn report_json_round_trip_preserves_keepup_section() {
        let trials = keepup_side(&[110_000_000]);
        let report = &trials[0];
        let json = serde_json::to_string(report).expect("serialize trial");
        let back: TrialReport = serde_json::from_str(&json).expect("deserialize trial");
        assert_eq!(back.sections.len(), 1);
        assert_eq!(back.sections[0].name, KeepUpSection::NAME);
        assert_eq!(back.sections[0].version, KeepUpSection::VERSION);
        let ku: KeepUpSection = serde_json::from_value(back.sections[0].body.clone()).expect("decode keepup body");
        assert_eq!(ku.cells.len(), 1);
        assert_eq!(ku.cells[0].offered, 6400);
        assert_eq!(ku.cells[0].expected_nanos, 100_000_000);
    }
}
