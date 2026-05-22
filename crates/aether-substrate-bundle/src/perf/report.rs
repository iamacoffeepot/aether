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

use serde::{Deserialize, Serialize};

/// Which leg of the per-hop measurement a cell reports.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Metric {
    /// `t_received − t_sent`: enqueue + worker pickup.
    Hop,
    /// `t_finished − t_received`: in-handler work.
    Handler,
}

impl Metric {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Hop => "hop",
            Self::Handler => "handler",
        }
    }
}

/// One cell's percentiles in a single trial. All latency values are
/// nanoseconds.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CellJson {
    pub workers: usize,
    pub topo: String,
    pub metric: Metric,
    pub p50: u64,
    pub p90: u64,
    pub p99: u64,
    pub max: u64,
    pub n: usize,
}

impl CellJson {
    #[allow(clippy::cast_precision_loss)]
    fn percentile(&self, p: Pct) -> f64 {
        let ns = match p {
            Pct::P50 => self.p50,
            Pct::P90 => self.p90,
            Pct::P99 => self.p99,
        };
        ns as f64
    }

    fn key(&self) -> CellKey {
        CellKey {
            workers: self.workers,
            topo: self.topo.clone(),
            metric: self.metric,
        }
    }
}

/// One fresh-process sweep run. The `perf-trial` bin emits this as JSON
/// on stdout; the `perf-compare` bin collects K of each side.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct TrialReport {
    /// Schema tag for forward-compat decode checks.
    pub schema: String,
    /// Commit the trial binary was built from, if the bin could resolve
    /// it (best-effort; `None` outside a git checkout).
    pub git_sha: Option<String>,
    /// `Some(hz)` if the sweep paced; `None` if flat-out (warm).
    pub pace_hz: Option<u64>,
    /// Frames advanced per cell.
    pub frames: u32,
    pub cells: Vec<CellJson>,
}

/// Current trial schema tag.
pub const TRIAL_SCHEMA: &str = "aether.perf.trial.v1";

impl TrialReport {
    /// Build a trial report from a sweep's [`CellResult`]s — each cell
    /// expands to two `CellJson` rows (hop + handler).
    ///
    /// [`CellResult`]: super::harness::CellResult
    #[must_use]
    pub fn from_cells(
        cells: &[super::harness::CellResult],
        frames: u32,
        pace_hz: Option<u64>,
        git_sha: Option<String>,
    ) -> Self {
        let mut out = Vec::with_capacity(cells.len() * 2);
        for c in cells {
            for (metric, s) in [(Metric::Hop, &c.hop), (Metric::Handler, &c.handler)] {
                out.push(CellJson {
                    workers: c.workers,
                    topo: c.topo.clone(),
                    metric,
                    p50: s.p50,
                    p90: s.p90,
                    p99: s.p99,
                    max: s.max,
                    n: s.n,
                });
            }
        }
        Self {
            schema: TRIAL_SCHEMA.to_owned(),
            git_sha,
            pace_hz,
            frames,
            cells: out,
        }
    }
}

#[derive(Clone, Copy)]
enum Pct {
    P50,
    P90,
    P99,
}

impl Pct {
    fn label(self) -> &'static str {
        match self {
            Self::P50 => "p50",
            Self::P90 => "p90",
            Self::P99 => "p99",
        }
    }
    const ALL: [Self; 3] = [Self::P50, Self::P90, Self::P99];
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct CellKey {
    workers: usize,
    topo: String,
    metric: Metric,
}

/// Find the cell matching `key` in one trial (a free fn, not a closure,
/// so the borrow of the returned `&CellJson` ties to `t`'s lifetime).
fn find_cell<'a>(t: &'a TrialReport, key: &CellKey) -> Option<&'a CellJson> {
    t.cells
        .iter()
        .find(|c| c.workers == key.workers && c.topo == key.topo && c.metric == key.metric)
}

/// improved / stable / regressed verdict for one (cell × percentile).
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Improved,
    Stable,
    Regressed,
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

/// Full comparison output — headline counts + per-cell rows.
#[derive(Serialize, Clone, Debug)]
pub struct ComparisonReport {
    pub trials: usize,
    pub improved: usize,
    pub stable: usize,
    pub regressed: usize,
    pub cells: Vec<CellComparison>,
}

/// Tunables for the verdict rule. Defaults are conservative —
/// informational reports should under-call rather than cry wolf
/// (ADR-0085 §4).
#[derive(Clone, Copy)]
pub struct CompareConfig {
    /// Multiplier on the paired-delta IQR: the effect must exceed this
    /// many IQRs to be "large relative to its own spread".
    pub effect_floor_iqr: f64,
    /// Minimum fractional change relative to the base median (practical
    /// significance) — suppresses tiny-but-consistent deltas.
    pub rel_floor: f64,
    /// Fraction of trials whose delta must share the effect's sign.
    pub consistency: f64,
}

impl Default for CompareConfig {
    fn default() -> Self {
        Self {
            effect_floor_iqr: 1.5,
            rel_floor: 0.10,
            consistency: 0.75,
        }
    }
}

fn sorted(mut v: Vec<f64>) -> Vec<f64> {
    v.sort_by(f64::total_cmp);
    v
}

#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn quantile_sorted(s: &[f64], q: f64) -> f64 {
    if s.is_empty() {
        return 0.0;
    }
    let idx = ((s.len() - 1) as f64 * q).round() as usize;
    s[idx.min(s.len() - 1)]
}

fn median_sorted(s: &[f64]) -> f64 {
    quantile_sorted(s, 0.5)
}

fn iqr_sorted(s: &[f64]) -> f64 {
    quantile_sorted(s, 0.75) - quantile_sorted(s, 0.25)
}

/// Compare K interleaved base/candidate trials. Trials pair by index:
/// `base[t]` against `cand[t]`. Cells present in every trial of both
/// sides are compared; a cell missing from any trial (a skipped boot)
/// is dropped.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn compare(base: &[TrialReport], cand: &[TrialReport], cfg: CompareConfig) -> ComparisonReport {
    let k = base.len().min(cand.len());
    let mut cells: Vec<CellComparison> = Vec::new();

    // Key set = cells in the first base trial; verified present across
    // all trials of both sides before comparing.
    let keys: Vec<CellKey> = base
        .first()
        .map(|t| t.cells.iter().map(CellJson::key).collect())
        .unwrap_or_default();

    for key in &keys {
        // Per-trial lookup of this cell on each side.
        let base_cells: Vec<&CellJson> =
            base[..k].iter().filter_map(|t| find_cell(t, key)).collect();
        let cand_cells: Vec<&CellJson> =
            cand[..k].iter().filter_map(|t| find_cell(t, key)).collect();
        if base_cells.len() != k || cand_cells.len() != k || k == 0 {
            continue; // cell not present in every trial — skip
        }

        for p in Pct::ALL {
            let base_vals: Vec<f64> = base_cells.iter().map(|c| c.percentile(p)).collect();
            let cand_vals: Vec<f64> = cand_cells.iter().map(|c| c.percentile(p)).collect();
            let deltas: Vec<f64> = (0..k).map(|t| cand_vals[t] - base_vals[t]).collect();

            let base_sorted = sorted(base_vals.clone());
            let cand_sorted = sorted(cand_vals.clone());
            let delta_sorted = sorted(deltas.clone());

            let base_median = median_sorted(&base_sorted);
            let cand_median = median_sorted(&cand_sorted);
            let delta_median = median_sorted(&delta_sorted);
            let delta_iqr = iqr_sorted(&delta_sorted);

            let verdict = classify(&deltas, delta_median, delta_iqr, base_median, cfg);
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

    let improved = cells
        .iter()
        .filter(|c| c.verdict == Verdict::Improved)
        .count();
    let regressed = cells
        .iter()
        .filter(|c| c.verdict == Verdict::Regressed)
        .count();
    let stable = cells.len() - improved - regressed;
    ComparisonReport {
        trials: k,
        improved,
        stable,
        regressed,
        cells,
    }
}

#[allow(clippy::cast_precision_loss)]
fn classify(
    deltas: &[f64],
    delta_median: f64,
    delta_iqr: f64,
    base_median: f64,
    cfg: CompareConfig,
) -> Verdict {
    if deltas.is_empty() || delta_median == 0.0 {
        return Verdict::Stable;
    }
    let n = deltas.len() as f64;
    let same_sign = deltas
        .iter()
        .filter(|&&d| d != 0.0 && d.signum() == delta_median.signum())
        .count() as f64;
    let consistent = same_sign / n >= cfg.consistency;

    let floor = (cfg.effect_floor_iqr * delta_iqr).max(cfg.rel_floor * base_median);
    let large = delta_median.abs() > floor;

    if consistent && large {
        if delta_median < 0.0 {
            Verdict::Improved
        } else {
            Verdict::Regressed
        }
    } else {
        Verdict::Stable
    }
}

fn us(ns: f64) -> String {
    format!("{:.2}", ns / 1000.0)
}

/// Hidden marker so the CI poster (PR 2) can find-and-update its sticky
/// comment in place rather than spamming new ones.
pub const STICKY_MARKER: &str = "<!-- aether-perf-report -->";

/// Render the comparison as a sticky PR-comment markdown body: headline
/// counts, the non-stable rows up top, and the full grid collapsed.
#[must_use]
#[allow(clippy::format_push_string)]
pub fn markdown(report: &ComparisonReport, title: &str, subtitle: &str) -> String {
    let mut s = String::new();
    s.push_str(STICKY_MARKER);
    s.push('\n');
    s.push_str(&format!("## dispatch perf — {title}\n"));
    s.push_str(&format!("{subtitle}\n\n"));
    s.push_str(&format!(
        "**{} improved · {} stable · {} regressed** ({} trials/config, paired)\n\n",
        report.improved, report.stable, report.regressed, report.trials
    ));

    let header = "| topology | w | metric | pct | base µs | this µs | Δ | verdict |\n|---|--:|---|---|--:|--:|--:|---|\n";
    let row = |c: &CellComparison| -> String {
        let verdict = match c.verdict {
            Verdict::Improved => "improved",
            Verdict::Stable => "stable",
            Verdict::Regressed => "regressed",
        };
        format!(
            "| {} | {} | {} | {} | {} ±{} | {} ±{} | {:+.0}% | {} |\n",
            c.topo,
            c.workers,
            c.metric.label(),
            c.percentile,
            us(c.base_median),
            us(c.base_iqr),
            us(c.cand_median),
            us(c.cand_iqr),
            c.delta_pct,
            verdict,
        )
    };

    let non_stable: Vec<&CellComparison> = report
        .cells
        .iter()
        .filter(|c| c.verdict != Verdict::Stable)
        .collect();
    if non_stable.is_empty() {
        s.push_str("_No cells moved beyond the noise band._\n\n");
    } else {
        s.push_str(header);
        for c in non_stable {
            s.push_str(&row(c));
        }
        s.push('\n');
    }

    s.push_str(&format!(
        "<details><summary>full grid — {} cells</summary>\n\n",
        report.cells.len()
    ));
    s.push_str(header);
    for c in &report.cells {
        s.push_str(&row(c));
    }
    s.push_str("\n</details>\n");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a K-trial side from a per-trial `p50` series for one cell
    /// (`fanout-8 @ 11w`, hop). Other percentiles track p50 ×1.2 / ×1.5
    /// so the cell is well-formed; tests assert on p50.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    fn side(p50s: &[u64]) -> Vec<TrialReport> {
        p50s.iter()
            .map(|&p50| TrialReport {
                schema: TRIAL_SCHEMA.to_owned(),
                git_sha: None,
                pace_hz: None,
                frames: 200,
                cells: vec![CellJson {
                    workers: 11,
                    topo: "fanout-8".to_owned(),
                    metric: Metric::Hop,
                    p50,
                    p90: (p50 as f64 * 1.2) as u64,
                    p99: (p50 as f64 * 1.5) as u64,
                    max: p50 * 4,
                    n: 1800,
                }],
            })
            .collect()
    }

    fn p50_verdict(rep: &ComparisonReport) -> Verdict {
        rep.cells
            .iter()
            .find(|c| c.percentile == "p50")
            .expect("p50 cell present")
            .verdict
    }

    #[test]
    fn consistent_win_reads_improved() {
        // base ~167µs, cand ~33µs, every trial — the fan-out win.
        let base = side(&[
            167_000, 165_000, 169_000, 166_000, 168_000, 170_000, 164_000, 167_000,
        ]);
        let cand = side(&[
            33_000, 34_000, 32_000, 33_500, 33_000, 31_000, 34_000, 33_000,
        ]);
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
    fn markdown_includes_marker_and_counts() {
        let base = side(&[167_000, 165_000, 169_000, 166_000]);
        let cand = side(&[33_000, 34_000, 32_000, 33_500]);
        let rep = compare(&base, &cand, CompareConfig::default());
        let md = markdown(&rep, "PR 9999 vs main", "test");
        assert!(md.contains(STICKY_MARKER));
        assert!(md.contains("improved"));
        assert!(md.contains("full grid"));
    }
}
