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

use aether_substrate::scheduler::SchedulerCountersSnapshot;
use serde::{Deserialize, Serialize};

/// Which per-mail span a cell reports (iamacoffeepot/aether#1150). Each
/// measures one property, so a regression points at a mechanism rather
/// than a smeared rollup.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Metric {
    /// iamacoffeepot/aether#1158: `t_sent − t_construct_start`: blob open →
    /// flush-begin — the producer-side time spent building the blob, the
    /// first leg of the four-stage lifecycle. ~0 on eager (non-buffered)
    /// paths.
    Construct,
    /// `t_enqueue − t_sent`: flush-begin → the worker picks up the blob —
    /// wakeup / scheduling latency. Tight on a warm worker.
    Queued,
    /// `t_received − t_enqueue`: blob pickup → this mail's handler entry —
    /// where in the blob's drain it landed. The only cardinality-sensitive
    /// span (a serial fan-out's late leaf waited behind its siblings), so
    /// high-variance by design.
    Drain,
    /// `t_finished − t_received`: the recipient's own handler work.
    Handler,
}

impl Metric {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Construct => "construct",
            Self::Queued => "queued",
            Self::Drain => "drain",
            Self::Handler => "handler",
        }
    }
}

/// Which scheduler mechanism counter a count-cell reports
/// (iamacoffeepot/aether#1129). Each is a per-cell delta — a count, not a
/// latency — and near-deterministic for a fixed workload, so the
/// comparator gives them a deterministic (non-noise-banded) verdict
/// rather than the latency [`Metric`]'s paired IQR test.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[serde(rename_all = "snake_case")]
pub enum CounterMetric {
    /// `SpinPark::notify` slow-path futex unparks — a producer woke a
    /// parked worker (no spinner was available to route to). The ~4.3µs
    /// handoff events the route-to-spinner fast path exists to avoid.
    NotifySlowUnparks,
    /// Recruiter-suppressed wakeups — a recruit asked for more siblings
    /// than the pool could field (`workers − 1`); the clamped difference
    /// is the suppressed count.
    RecruitSuppressed,
    /// Steals that pulled work from the shared injector (off-worker
    /// producers, spilled fan-out, requeued yields).
    StealsInjector,
    /// Steals that raided a sibling worker's deque tail (only when
    /// `AETHER_PEER_STEAL` is opted in).
    StealsSibling,
    /// Inline-runs — a wake kept its slot on the producing worker's own
    /// deque (the affinity warm path), so it ran without a futex wakeup.
    InlineRuns,
}

impl CounterMetric {
    /// The four-plus counters in report order.
    pub const ALL: [Self; 5] = [
        Self::NotifySlowUnparks,
        Self::RecruitSuppressed,
        Self::StealsInjector,
        Self::StealsSibling,
        Self::InlineRuns,
    ];

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::NotifySlowUnparks => "notify_slow_unparks",
            Self::RecruitSuppressed => "recruit_suppressed",
            Self::StealsInjector => "steals_injector",
            Self::StealsSibling => "steals_sibling",
            Self::InlineRuns => "inline_runs",
        }
    }

    /// Read this counter's field out of a snapshot delta.
    #[must_use]
    pub fn read(self, snap: &SchedulerCountersSnapshot) -> u64 {
        match self {
            Self::NotifySlowUnparks => snap.notify_slow_unparks,
            Self::RecruitSuppressed => snap.recruit_suppressed,
            Self::StealsInjector => snap.steals_injector,
            Self::StealsSibling => snap.steals_sibling,
            Self::InlineRuns => snap.inline_runs,
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

/// One scheduler mechanism counter's per-cell delta in a single trial
/// (iamacoffeepot/aether#1129). `count` is a count, not a latency — the
/// number of events of `counter` kind the worker pool produced over this
/// cell's `advance`.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CounterCellJson {
    pub workers: usize,
    pub topo: String,
    pub counter: CounterMetric,
    pub count: u64,
}

impl CounterCellJson {
    fn key(&self) -> CounterCellKey {
        CounterCellKey {
            workers: self.workers,
            topo: self.topo.clone(),
            counter: self.counter,
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
    /// iamacoffeepot/aether#1129: the scheduler mechanism-counter cells,
    /// one row per (workers × topology × counter). A count class, separate
    /// from the latency `cells` because counts get a deterministic verdict,
    /// not the noise-aware paired test.
    pub counters: Vec<CounterCellJson>,
}

/// Current trial schema tag. Bumped to `v2` by iamacoffeepot/aether#1150
/// when `hop` / `send_enqueue` / `residence` gave way to the
/// `queued` / `drain` / `handler` span model; to `v3` by
/// iamacoffeepot/aether#1158 when `construct` joined as the producer-side
/// first leg, completing the four-stage lifecycle; to `v4` by
/// iamacoffeepot/aether#1129 when the scheduler mechanism counters joined
/// as the `counters` count-cell list.
pub const TRIAL_SCHEMA: &str = "aether.perf.trial.v4";

impl TrialReport {
    /// Build a trial report from a sweep's [`CellResult`]s — each cell
    /// expands to four `CellJson` rows (`construct` + `queued` + `drain` +
    /// `handler`, in lifecycle order; iamacoffeepot/aether#1158). `depth`
    /// is a count, not a latency, so it is omitted from the latency compare
    /// (it lives only in the on-demand observe table).
    ///
    /// [`CellResult`]: super::harness::CellResult
    #[must_use]
    pub fn from_cells(
        cells: &[super::harness::CellResult],
        frames: u32,
        pace_hz: Option<u64>,
        git_sha: Option<String>,
    ) -> Self {
        let mut out = Vec::with_capacity(cells.len() * 4);
        let mut counter_rows = Vec::with_capacity(cells.len() * CounterMetric::ALL.len());
        for c in cells {
            for (metric, s) in [
                (Metric::Construct, &c.construct),
                (Metric::Queued, &c.queued),
                (Metric::Drain, &c.drain),
                (Metric::Handler, &c.handler),
            ] {
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
            // iamacoffeepot/aether#1129: one count-cell per mechanism counter.
            for counter in CounterMetric::ALL {
                counter_rows.push(CounterCellJson {
                    workers: c.workers,
                    topo: c.topo.clone(),
                    counter,
                    count: counter.read(&c.counters),
                });
            }
        }
        Self {
            schema: TRIAL_SCHEMA.to_owned(),
            git_sha,
            pace_hz,
            frames,
            cells: out,
            counters: counter_rows,
        }
    }
}

/// Read just the `schema` tag from a trial's JSON, ignoring the rest.
/// The comparator uses this to detect a base-vs-candidate schema
/// transition (a changed metric set) *before* the full [`TrialReport`]
/// parse — which would otherwise hard-fail on serde's
/// unknown-`Metric`-variant error when an older base trial still carries
/// the retired `hop` / `send_enqueue` / `residence` names
/// (iamacoffeepot/aether#1151). `None` if the bytes aren't a JSON object
/// carrying a string `schema` field.
#[must_use]
pub fn probe_schema(json: &[u8]) -> Option<String> {
    #[derive(Deserialize)]
    struct SchemaProbe {
        schema: String,
    }
    serde_json::from_slice::<SchemaProbe>(json)
        .ok()
        .map(|p| p.schema)
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

#[derive(Clone, PartialEq, Eq, Hash)]
struct CounterCellKey {
    workers: usize,
    topo: String,
    counter: CounterMetric,
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

/// changed / stable verdict for one counter cell (iamacoffeepot/aether#1129).
/// Deterministic — the counts are near-deterministic for a fixed workload,
/// so a change above a small absolute tolerance is real, not noise. No
/// improved/regressed direction: whether more steals or fewer unparks is
/// "better" is mechanism-dependent and the reader's call; the verdict only
/// asserts *whether the mechanism behaviour moved*.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum CounterVerdict {
    Stable,
    Changed,
}

/// One compared counter cell — the median count per side (across the K
/// trials) plus the deterministic [`CounterVerdict`].
#[derive(Serialize, Clone, Debug)]
pub struct CounterComparison {
    pub workers: usize,
    pub topo: String,
    pub counter: CounterMetric,
    pub base_count: u64,
    pub cand_count: u64,
    /// `cand_count − base_count` (signed).
    pub delta: i64,
    pub verdict: CounterVerdict,
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
    /// iamacoffeepot/aether#1129: how many counter cells moved past the
    /// absolute tolerance (deterministic verdict, not noise-banded).
    pub counters_changed: usize,
    /// The per-counter-cell comparison rows.
    pub counters: Vec<CounterComparison>,
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
    /// Absolute floor in nanoseconds — a change smaller than this is
    /// below the harness's resolution (sub-microsecond dispatch-glue
    /// differences read as noise; see the latency-sweep finding that
    /// ~100ns deltas are unresolvable). Without it, a 50ns shift on a
    /// 170ns sub-µs handler cell reads as a 30% "regression".
    pub abs_floor_ns: f64,
    /// Fraction of trials whose delta must share the effect's sign.
    pub consistency: f64,
    /// iamacoffeepot/aether#1129: absolute tolerance (in events) for the
    /// mechanism counters' deterministic verdict. A counter cell flags
    /// `Changed` only when the median-count delta strictly exceeds this.
    /// Small but non-zero: the counts are near-deterministic, but a lost
    /// relaxed increment or a racing wakeup can jitter a count by one or
    /// two across trials, and the median across K trials already absorbs
    /// most of that — the tolerance is the final guard against a 1-event
    /// wobble reading as a behaviour change. No IQR / noise band, by
    /// design: the whole point of the counters is that they are stable
    /// where latency is not.
    pub counter_abs_tolerance: u64,
}

impl Default for CompareConfig {
    fn default() -> Self {
        Self {
            effect_floor_iqr: 1.5,
            rel_floor: 0.10,
            abs_floor_ns: 300.0,
            consistency: 0.75,
            counter_abs_tolerance: 2,
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

    let counters = compare_counters(base, cand, k, cfg);
    let counters_changed = counters
        .iter()
        .filter(|c| c.verdict == CounterVerdict::Changed)
        .count();

    ComparisonReport {
        trials: k,
        improved,
        stable,
        regressed,
        cells,
        counters_changed,
        counters,
    }
}

/// Find the counter cell matching `key` in one trial.
fn find_counter<'a>(t: &'a TrialReport, key: &CounterCellKey) -> Option<&'a CounterCellJson> {
    t.counters
        .iter()
        .find(|c| c.workers == key.workers && c.topo == key.topo && c.counter == key.counter)
}

/// Median of an unsorted `u64` slice (nearest-rank, like the latency
/// percentiles). `0` on an empty slice.
fn median_counts(counts: &[u64]) -> u64 {
    if counts.is_empty() {
        return 0;
    }
    let mut s = counts.to_vec();
    s.sort_unstable();
    s[(s.len() - 1) / 2]
}

/// Deterministic counter comparison (iamacoffeepot/aether#1129). Per
/// counter cell present in every trial of both sides, take the median
/// count per side across the K trials and flag `Changed` when their
/// absolute difference exceeds [`CompareConfig::counter_abs_tolerance`].
/// No IQR / noise band — the counts are near-deterministic, so a change
/// past the (tiny) tolerance is real.
fn compare_counters(
    base: &[TrialReport],
    cand: &[TrialReport],
    k: usize,
    cfg: CompareConfig,
) -> Vec<CounterComparison> {
    if k == 0 {
        return Vec::new();
    }
    let keys: Vec<CounterCellKey> = base
        .first()
        .map(|t| t.counters.iter().map(CounterCellJson::key).collect())
        .unwrap_or_default();

    let mut out = Vec::new();
    for key in &keys {
        let base_counts: Vec<u64> = base[..k]
            .iter()
            .filter_map(|t| find_counter(t, key))
            .map(|c| c.count)
            .collect();
        let cand_counts: Vec<u64> = cand[..k]
            .iter()
            .filter_map(|t| find_counter(t, key))
            .map(|c| c.count)
            .collect();
        if base_counts.len() != k || cand_counts.len() != k {
            continue; // cell not present in every trial — skip
        }

        let base_count = median_counts(&base_counts);
        let cand_count = median_counts(&cand_counts);
        let delta = i64::try_from(cand_count).unwrap_or(i64::MAX)
            - i64::try_from(base_count).unwrap_or(i64::MAX);
        let verdict = if delta.unsigned_abs() > cfg.counter_abs_tolerance {
            CounterVerdict::Changed
        } else {
            CounterVerdict::Stable
        };
        out.push(CounterComparison {
            workers: key.workers,
            topo: key.topo.clone(),
            counter: key.counter,
            base_count,
            cand_count,
            delta,
            verdict,
        });
    }
    out
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

    let floor = (cfg.effect_floor_iqr * delta_iqr)
        .max(cfg.rel_floor * base_median)
        .max(cfg.abs_floor_ns);
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

    s.push_str(&counters_markdown(report));
    s
}

/// Render the mechanism-counter (iamacoffeepot/aether#1129) section: a
/// headline change count, the changed rows up top, and the full counter
/// grid collapsed. Counts are deterministic, so a non-zero delta past the
/// tolerance is a real behaviour change (no noise band).
#[allow(clippy::format_push_string)]
fn counters_markdown(report: &ComparisonReport) -> String {
    if report.counters.is_empty() {
        return String::new();
    }
    let mut s = String::new();
    s.push_str(&format!(
        "\n### scheduler mechanism counters\n\n**{} of {} counter cells changed** (deterministic, abs-tolerance verdict)\n\n",
        report.counters_changed,
        report.counters.len()
    ));

    let header =
        "| topology | w | counter | base | this | Δ | verdict |\n|---|--:|---|--:|--:|--:|---|\n";
    let row = |c: &CounterComparison| -> String {
        let verdict = match c.verdict {
            CounterVerdict::Stable => "stable",
            CounterVerdict::Changed => "changed",
        };
        format!(
            "| {} | {} | {} | {} | {} | {:+} | {} |\n",
            c.topo,
            c.workers,
            c.counter.label(),
            c.base_count,
            c.cand_count,
            c.delta,
            verdict,
        )
    };

    let changed: Vec<&CounterComparison> = report
        .counters
        .iter()
        .filter(|c| c.verdict == CounterVerdict::Changed)
        .collect();
    if changed.is_empty() {
        s.push_str("_No counter cell moved past the tolerance._\n\n");
    } else {
        s.push_str(header);
        for c in changed {
            s.push_str(&row(c));
        }
        s.push('\n');
    }

    s.push_str(&format!(
        "<details><summary>full counter grid — {} cells</summary>\n\n",
        report.counters.len()
    ));
    s.push_str(header);
    for c in &report.counters {
        s.push_str(&row(c));
    }
    s.push_str("\n</details>\n");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a K-trial side from a per-trial `p50` series for one cell
    /// (`fanout-8 @ 11w`, drain). Other percentiles track p50 ×1.2 / ×1.5
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
                    metric: Metric::Drain,
                    p50,
                    p90: (p50 as f64 * 1.2) as u64,
                    p99: (p50 as f64 * 1.5) as u64,
                    max: p50 * 4,
                    n: 1800,
                }],
                counters: Vec::new(),
            })
            .collect()
    }

    /// Build a K-trial side carrying one counter cell (`fanout-8 @ 11w`,
    /// `inline_runs`) with the per-trial counts in `series`. The latency
    /// `cells` are left empty — the counter-verdict tests assert only on
    /// the counter comparison.
    fn counter_side(series: &[u64]) -> Vec<TrialReport> {
        series
            .iter()
            .map(|&count| TrialReport {
                schema: TRIAL_SCHEMA.to_owned(),
                git_sha: None,
                pace_hz: None,
                frames: 200,
                cells: Vec::new(),
                counters: vec![CounterCellJson {
                    workers: 11,
                    topo: "fanout-8".to_owned(),
                    counter: CounterMetric::InlineRuns,
                    count,
                }],
            })
            .collect()
    }

    fn inline_runs_verdict(rep: &ComparisonReport) -> CounterVerdict {
        rep.counters
            .iter()
            .find(|c| c.counter == CounterMetric::InlineRuns)
            .expect("inline_runs counter cell present")
            .verdict
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
    fn probe_schema_reads_tag_past_unknown_metric_variants() {
        // An older base trial carries the retired `hop` variant; the probe
        // must read its schema tag without choking on it (a full parse
        // would hard-fail on the unknown `Metric` variant — that is the
        // whole point of probing first, iamacoffeepot/aether#1151).
        let v1 = br#"{"schema":"aether.perf.trial.v1","cells":[{"metric":"hop","p50":1}]}"#;
        assert_eq!(probe_schema(v1).as_deref(), Some("aether.perf.trial.v1"));
        assert_eq!(probe_schema(b"not json"), None);
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
    fn markdown_includes_marker_and_counts() {
        let base = side(&[167_000, 165_000, 169_000, 166_000]);
        let cand = side(&[33_000, 34_000, 32_000, 33_500]);
        let rep = compare(&base, &cand, CompareConfig::default());
        let md = markdown(&rep, "PR 9999 vs main", "test");
        assert!(md.contains(STICKY_MARKER));
        assert!(md.contains("improved"));
        assert!(md.contains("full grid"));
    }

    #[test]
    fn schema_tag_is_v4() {
        // iamacoffeepot/aether#1129: the count-cell list bumped the schema.
        assert_eq!(TRIAL_SCHEMA, "aether.perf.trial.v4");
    }

    #[test]
    fn trial_report_round_trips_with_counter_cells() {
        // iamacoffeepot/aether#1129: a v4 report carries both latency cells
        // and counter cells; both survive a JSON round-trip, and the schema
        // probe reads the v4 tag.
        let mut t = side(&[1000]).remove(0);
        t.counters = CounterMetric::ALL
            .iter()
            .map(|&counter| CounterCellJson {
                workers: 11,
                topo: "fanout-8".to_owned(),
                counter,
                count: 42,
            })
            .collect();
        let json = serde_json::to_vec(&t).expect("serialize v4 trial");
        assert_eq!(probe_schema(&json).as_deref(), Some(TRIAL_SCHEMA));
        let back: TrialReport = serde_json::from_slice(&json).expect("round-trip v4 trial");
        assert_eq!(back.counters.len(), CounterMetric::ALL.len());
        assert!(back.counters.iter().all(|c| c.count == 42));
        // The snake_case counter labels survive the enum serde round-trip.
        assert!(
            back.counters
                .iter()
                .any(|c| c.counter == CounterMetric::NotifySlowUnparks)
        );
    }

    #[test]
    fn equal_counts_read_stable() {
        // Identical counts both sides — deterministic stable.
        let base = counter_side(&[500, 500, 500, 500]);
        let cand = counter_side(&[500, 500, 500, 500]);
        let rep = compare(&base, &cand, CompareConfig::default());
        assert_eq!(inline_runs_verdict(&rep), CounterVerdict::Stable);
        assert_eq!(rep.counters_changed, 0);
    }

    #[test]
    fn one_event_jitter_is_within_tolerance() {
        // A 1-event wobble (a lost relaxed increment / racing wakeup) is
        // inside the default abs-tolerance of 2 — must read stable.
        let base = counter_side(&[500, 500, 501, 500]);
        let cand = counter_side(&[501, 500, 500, 502]);
        let rep = compare(&base, &cand, CompareConfig::default());
        assert_eq!(inline_runs_verdict(&rep), CounterVerdict::Stable);
    }

    #[test]
    fn clear_count_delta_reads_changed() {
        // A workload-level shift (500 -> 50 inline-runs) is far past the
        // tolerance — deterministically flagged, no noise band.
        let base = counter_side(&[500, 500, 500, 500]);
        let cand = counter_side(&[50, 48, 52, 50]);
        let rep = compare(&base, &cand, CompareConfig::default());
        assert_eq!(inline_runs_verdict(&rep), CounterVerdict::Changed);
        assert_eq!(rep.counters_changed, 1);
        let cell = rep
            .counters
            .iter()
            .find(|c| c.counter == CounterMetric::InlineRuns)
            .expect("inline_runs cell");
        assert_eq!(cell.base_count, 500);
        assert_eq!(cell.cand_count, 50);
        assert_eq!(cell.delta, -450);
    }

    #[test]
    fn markdown_renders_counter_section() {
        let base = counter_side(&[500, 500, 500, 500]);
        let cand = counter_side(&[50, 48, 52, 50]);
        let rep = compare(&base, &cand, CompareConfig::default());
        let md = markdown(&rep, "PR 9999 vs main", "test");
        assert!(md.contains("scheduler mechanism counters"));
        assert!(md.contains("inline_runs"));
        assert!(md.contains("changed"));
    }
}
