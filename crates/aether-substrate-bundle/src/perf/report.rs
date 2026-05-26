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

/// One versioned, opaque slice of a [`TrialReport`]. The comparator
/// pairs sections by `name`, decodes `body` to a typed payload only when
/// both sides agree on `name` *and* `version`, and otherwise leaves the
/// section uncompared (iamacoffeepot/aether#1206). Keeping `body` as a
/// [`serde_json::Value`] is load-bearing: a section the comparator can't
/// read stays opaque instead of failing the whole decode.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct RawSection {
    pub name: String,
    pub version: String,
    pub body: serde_json::Value,
}

/// The per-cell latency section: today's only section, carrying the
/// (worker × topology × metric) percentile grid. Its `version` bumps
/// whenever the metric set changes, leaving sibling sections comparable.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct LatencySection {
    pub cells: Vec<CellJson>,
}

impl LatencySection {
    /// The section name the comparator dispatches on.
    pub const NAME: &str = "latency";
    /// The section version. Bumped when the metric set changes; sibling
    /// sections stay comparable across the bump.
    pub const VERSION: &str = "v1";
}

/// One fresh-process sweep run. The `perf-trial` bin emits this as JSON
/// on stdout; the `perf-compare` bin collects K of each side.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct TrialReport {
    /// Envelope schema tag (iamacoffeepot/aether#1206): guards only the
    /// *container* shape — "a report is a list of named, versioned
    /// sections". Per-metric evolution rides each section's own
    /// `version`, not this tag.
    pub schema: String,
    /// Commit the trial binary was built from, if the bin could resolve
    /// it (best-effort; `None` outside a git checkout).
    pub git_sha: Option<String>,
    /// `Some(hz)` if the sweep paced; `None` if flat-out (warm).
    pub pace_hz: Option<u64>,
    /// Frames advanced per cell.
    pub frames: u32,
    /// The independently-versioned sections of this run.
    pub sections: Vec<RawSection>,
}

/// Current envelope schema tag. Bumped to `v2` by
/// iamacoffeepot/aether#1150 when `hop` / `send_enqueue` / `residence`
/// gave way to the `queued` / `drain` / `handler` span model; to `v3` by
/// iamacoffeepot/aether#1158 when `construct` joined as the producer-side
/// first leg; and to `v4` by iamacoffeepot/aether#1206 when the flat
/// top-level `cells` array became a list of named, independently-versioned
/// sections (so a metric-set change bumps a section's `version`, not this
/// envelope tag).
pub const TRIAL_SCHEMA: &str = "aether.perf.trial.v4";

impl TrialReport {
    /// Build a trial report from a sweep's [`CellResult`]s — each cell
    /// expands to four `CellJson` rows (`construct` + `queued` + `drain` +
    /// `handler`, in lifecycle order; iamacoffeepot/aether#1158). `depth`
    /// is a count, not a latency, so it is omitted from the latency compare
    /// (it lives only in the on-demand observe table). The rows are wrapped
    /// in a single [`LatencySection`] — the lone section today
    /// (iamacoffeepot/aether#1206).
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
        }
        let latency = LatencySection { cells: out };
        let body = serde_json::to_value(&latency).unwrap_or(serde_json::Value::Null);
        Self {
            schema: TRIAL_SCHEMA.to_owned(),
            git_sha,
            pace_hz,
            frames,
            sections: vec![RawSection {
                name: LatencySection::NAME.to_owned(),
                version: LatencySection::VERSION.to_owned(),
                body,
            }],
        }
    }

    /// The section with the given name, if present.
    fn section(&self, name: &str) -> Option<&RawSection> {
        self.sections.iter().find(|s| s.name == name)
    }
}

/// Read just the `schema` (envelope) tag from a trial's JSON, ignoring
/// the rest. The comparator uses this to detect an unreadable envelope
/// — a pre-sections report on the wrong envelope tag can't be sectioned
/// — *before* the full [`TrialReport`] parse. Probing first also dodges
/// serde's unknown-`Metric`-variant hard-fail when an older base trial
/// still carries the retired `hop` / `send_enqueue` / `residence` names
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

/// Find the cell matching `key` in one trial's latency cells (a free fn,
/// not a closure, so the borrow of the returned `&CellJson` ties to the
/// slice's lifetime).
fn find_cell<'a>(cells: &'a [CellJson], key: &CellKey) -> Option<&'a CellJson> {
    cells
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

/// Why a section couldn't be paired into a verdict
/// (iamacoffeepot/aether#1206). Picked per case so the markdown note and
/// the JSON report both spell out the reason rather than a bare "skipped".
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UncomparedReason {
    /// Present on the candidate but absent from the base — new this run,
    /// no baseline to compare against.
    NewThisRun,
    /// Present on both sides but the versions differ — the section's own
    /// shape changed, so a paired comparison isn't meaningful this run.
    VersionChanged { base: String, cand: String },
    /// Present on the base but absent from the candidate — the section
    /// was dropped this run.
    OnlyBase,
    /// Present on both sides at an agreed version, but the comparator has
    /// no typed compare for this section name.
    UnknownName,
}

/// One section's outcome in a [`ComparisonReport`]: either a typed
/// verdict grid (`Compared`) or a reasoned skip (`Uncompared`).
#[derive(Serialize, Clone, Debug)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SectionReport {
    Compared {
        name: String,
        improved: usize,
        stable: usize,
        regressed: usize,
        cells: Vec<CellComparison>,
    },
    Uncompared {
        name: String,
        reason: UncomparedReason,
    },
}

/// Full comparison output — the trial count plus one entry per section.
#[derive(Serialize, Clone, Debug)]
pub struct ComparisonReport {
    pub trials: usize,
    pub sections: Vec<SectionReport>,
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
}

impl Default for CompareConfig {
    fn default() -> Self {
        Self {
            effect_floor_iqr: 1.5,
            rel_floor: 0.10,
            abs_floor_ns: 300.0,
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

/// Compare K interleaved base/candidate trials, section by section.
/// Trials pair by index: `base[t]` against `cand[t]`. A section is
/// dispatched on its `name`: present on both sides at an agreed version
/// with a known name decodes both bodies and runs that section's typed
/// compare; otherwise it lands in the report as an `Uncompared` block
/// with the reason (new this run / version changed / only base / unknown
/// name) so the comparable sections still get verdicts
/// (iamacoffeepot/aether#1206).
#[must_use]
pub fn compare(base: &[TrialReport], cand: &[TrialReport], cfg: CompareConfig) -> ComparisonReport {
    let k = base.len().min(cand.len());

    // Section names present on either side, base-first then any
    // candidate-only names, de-duplicated while preserving order.
    let mut names: Vec<String> = Vec::new();
    for t in base.iter().chain(cand.iter()) {
        for sec in &t.sections {
            if !names.contains(&sec.name) {
                names.push(sec.name.clone());
            }
        }
    }

    let base_sec = |name: &str| base.first().and_then(|t| t.section(name));
    let cand_sec = |name: &str| cand.first().and_then(|t| t.section(name));

    let mut sections: Vec<SectionReport> = Vec::with_capacity(names.len());
    for name in &names {
        let on_base = base_sec(name);
        let on_cand = cand_sec(name);
        let (bsec, csec) = match (on_base, on_cand) {
            (Some(b), Some(c)) => (b, c),
            (None, Some(_)) => {
                sections.push(SectionReport::Uncompared {
                    name: name.clone(),
                    reason: UncomparedReason::NewThisRun,
                });
                continue;
            }
            (Some(_), None) => {
                sections.push(SectionReport::Uncompared {
                    name: name.clone(),
                    reason: UncomparedReason::OnlyBase,
                });
                continue;
            }
            (None, None) => continue,
        };
        if bsec.version != csec.version {
            sections.push(SectionReport::Uncompared {
                name: name.clone(),
                reason: UncomparedReason::VersionChanged {
                    base: bsec.version.clone(),
                    cand: csec.version.clone(),
                },
            });
            continue;
        }

        match name.as_str() {
            LatencySection::NAME => {
                let base_cells = decode_latency_cells(&base[..k]);
                let cand_cells = decode_latency_cells(&cand[..k]);
                sections.push(compare_latency(name, &base_cells, &cand_cells, k, cfg));
            }
            _ => sections.push(SectionReport::Uncompared {
                name: name.clone(),
                reason: UncomparedReason::UnknownName,
            }),
        }
    }

    ComparisonReport {
        trials: k,
        sections,
    }
}

/// Per-trial latency cells: decode each trial's `latency` section body,
/// dropping any trial whose body doesn't decode (it then can't satisfy
/// the present-in-every-trial gate below, exactly as a missing cell did).
fn decode_latency_cells(trials: &[TrialReport]) -> Vec<Vec<CellJson>> {
    trials
        .iter()
        .map(|t| {
            t.section(LatencySection::NAME)
                .and_then(|s| serde_json::from_value::<LatencySection>(s.body.clone()).ok())
                .map(|l| l.cells)
                .unwrap_or_default()
        })
        .collect()
}

/// Today's per-cell paired-delta compare, extracted for the `latency`
/// section. `base_cells[t]` / `cand_cells[t]` are trial `t`'s cells;
/// cells are keyed by (workers, topo, metric) across the K trials and a
/// cell missing from any trial of either side is dropped — preserving
/// the pre-sections semantics exactly.
#[allow(clippy::cast_precision_loss)]
fn compare_latency(
    name: &str,
    base_cells: &[Vec<CellJson>],
    cand_cells: &[Vec<CellJson>],
    k: usize,
    cfg: CompareConfig,
) -> SectionReport {
    let mut cells: Vec<CellComparison> = Vec::new();

    // Key set = cells in the first base trial; verified present across
    // all trials of both sides before comparing.
    let keys: Vec<CellKey> = base_cells
        .first()
        .map(|c| c.iter().map(CellJson::key).collect())
        .unwrap_or_default();

    for key in &keys {
        // Per-trial lookup of this cell on each side.
        let base_hits: Vec<&CellJson> = base_cells[..k.min(base_cells.len())]
            .iter()
            .filter_map(|c| find_cell(c, key))
            .collect();
        let cand_hits: Vec<&CellJson> = cand_cells[..k.min(cand_cells.len())]
            .iter()
            .filter_map(|c| find_cell(c, key))
            .collect();
        if base_hits.len() != k || cand_hits.len() != k || k == 0 {
            continue; // cell not present in every trial — skip
        }

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
    SectionReport::Compared {
        name: name.to_owned(),
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
/// counts (summed across the compared sections), then per section — a
/// `latency` verdict (non-stable rows up top, full grid collapsed) or a
/// one-line "new this run" note for an uncompared section
/// (iamacoffeepot/aether#1206).
#[must_use]
#[allow(clippy::format_push_string)]
pub fn markdown(report: &ComparisonReport, title: &str, subtitle: &str) -> String {
    let mut s = String::new();
    s.push_str(STICKY_MARKER);
    s.push('\n');
    s.push_str(&format!("## dispatch perf — {title}\n"));
    s.push_str(&format!("{subtitle}\n\n"));

    let (mut improved, mut stable, mut regressed) = (0usize, 0usize, 0usize);
    for sec in &report.sections {
        if let SectionReport::Compared {
            improved: i,
            stable: st,
            regressed: r,
            ..
        } = sec
        {
            improved += i;
            stable += st;
            regressed += r;
        }
    }
    s.push_str(&format!(
        "**{improved} improved · {stable} stable · {regressed} regressed** ({} trials/config, paired)\n\n",
        report.trials
    ));

    for sec in &report.sections {
        match sec {
            SectionReport::Compared { name, cells, .. } => {
                push_latency_section(&mut s, name, cells);
            }
            SectionReport::Uncompared { name, .. } => {
                s.push_str(&format!(
                    "_{name}: new this run — no baseline to compare_\n\n"
                ));
            }
        }
    }
    s
}

#[allow(clippy::format_push_string)]
fn push_latency_section(s: &mut String, name: &str, cells: &[CellComparison]) {
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

    let non_stable: Vec<&CellComparison> = cells
        .iter()
        .filter(|c| c.verdict != Verdict::Stable)
        .collect();
    if non_stable.is_empty() {
        s.push_str(&format!(
            "_{name}: no cells moved beyond the noise band._\n\n"
        ));
    } else {
        s.push_str(header);
        for c in non_stable {
            s.push_str(&row(c));
        }
        s.push('\n');
    }

    s.push_str(&format!(
        "<details><summary>{name} full grid — {} cells</summary>\n\n",
        cells.len()
    ));
    s.push_str(header);
    for c in cells {
        s.push_str(&row(c));
    }
    s.push_str("\n</details>\n\n");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a K-trial side from a per-trial `p50` series for one cell
    /// (`fanout-8 @ 11w`, drain). Other percentiles track p50 ×1.2 / ×1.5
    /// so the cell is well-formed; tests assert on p50. The cell rides in
    /// a single `latency` section (iamacoffeepot/aether#1206).
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    fn side(p50s: &[u64]) -> Vec<TrialReport> {
        p50s.iter()
            .map(|&p50| {
                let cells = vec![CellJson {
                    workers: 11,
                    topo: "fanout-8".to_owned(),
                    metric: Metric::Drain,
                    p50,
                    p90: (p50 as f64 * 1.2) as u64,
                    p99: (p50 as f64 * 1.5) as u64,
                    max: p50 * 4,
                    n: 1800,
                }];
                let body =
                    serde_json::to_value(LatencySection { cells }).expect("encode latency body");
                TrialReport {
                    schema: TRIAL_SCHEMA.to_owned(),
                    git_sha: None,
                    pace_hz: None,
                    frames: 200,
                    sections: vec![RawSection {
                        name: LatencySection::NAME.to_owned(),
                        version: LatencySection::VERSION.to_owned(),
                        body,
                    }],
                }
            })
            .collect()
    }

    /// Pull the compared `latency` section out of a comparison report.
    fn latency_section(rep: &ComparisonReport) -> &SectionReport {
        rep.sections
            .iter()
            .find(|s| matches!(s, SectionReport::Compared { name, .. } if name == LatencySection::NAME))
            .expect("compared latency section present")
    }

    fn p50_verdict(rep: &ComparisonReport) -> Verdict {
        let SectionReport::Compared { cells, .. } = latency_section(rep) else {
            panic!("latency section not compared");
        };
        cells
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
        let latency: LatencySection =
            serde_json::from_value(sec.body.clone()).expect("decode latency body");
        assert_eq!(latency.cells.len(), 1);
        assert_eq!(latency.cells[0].metric, Metric::Drain);
        assert_eq!(latency.cells[0].p50, 1000);
    }

    /// Attach an extra raw section to every trial in a side.
    fn with_extra_section(
        mut side: Vec<TrialReport>,
        name: &str,
        version: &str,
    ) -> Vec<TrialReport> {
        for t in &mut side {
            t.sections.push(RawSection {
                name: name.to_owned(),
                version: version.to_owned(),
                body: serde_json::json!({"opaque": true}),
            });
        }
        side
    }

    #[test]
    fn unknown_section_on_candidate_does_not_blind_latency() {
        // iamacoffeepot/aether#1205 core guard: a section the comparator
        // doesn't recognise (here only on the candidate) survives decode
        // and yields an Uncompared block, while the latency section
        // present on both sides still produces a Compared verdict.
        let base = side(&[167_000, 165_000, 169_000, 166_000]);
        let cand = with_extra_section(side(&[33_000, 34_000, 32_000, 33_500]), "throughput", "v1");
        let rep = compare(&base, &cand, CompareConfig::default());

        // Latency still compared, and the win still reads.
        assert_eq!(p50_verdict(&rep), Verdict::Improved);

        // The unknown section is present and uncompared (new this run,
        // since the base lacks it).
        let unknown = rep
            .sections
            .iter()
            .find(|s| matches!(s, SectionReport::Uncompared { name, .. } if name == "throughput"))
            .expect("uncompared throughput section present");
        match unknown {
            SectionReport::Uncompared { reason, .. } => {
                assert_eq!(*reason, UncomparedReason::NewThisRun);
            }
            SectionReport::Compared { .. } => panic!("throughput should not be compared"),
        }
    }

    #[test]
    fn unknown_section_on_both_sides_reads_unknown_name() {
        // Present on both sides at an agreed version but with no typed
        // compare — that's the UnknownName reason, distinct from
        // NewThisRun.
        let base = with_extra_section(side(&[1000, 1000, 1000]), "throughput", "v1");
        let cand = with_extra_section(side(&[1000, 1000, 1000]), "throughput", "v1");
        let rep = compare(&base, &cand, CompareConfig::default());
        let unknown = rep
            .sections
            .iter()
            .find(|s| matches!(s, SectionReport::Uncompared { name, .. } if name == "throughput"))
            .expect("uncompared throughput section present");
        match unknown {
            SectionReport::Uncompared { reason, .. } => {
                assert_eq!(*reason, UncomparedReason::UnknownName);
            }
            SectionReport::Compared { .. } => panic!("throughput should not be compared"),
        }
    }

    #[test]
    fn version_mismatch_does_not_blind_other_sections() {
        // The latency section on the base is v1; on the candidate it is
        // v2. That section reads VersionChanged; a second section present
        // at an agreed version on both sides still compares.
        let mut base = with_extra_section(side(&[1000, 1000, 1000]), "extra", "v1");
        let mut cand = with_extra_section(side(&[1000, 1000, 1000]), "extra", "v1");
        for t in &mut cand {
            for sec in &mut t.sections {
                if sec.name == LatencySection::NAME {
                    sec.version = "v2".to_owned();
                }
            }
        }
        // Keep base's latency at v1 explicitly (it already is).
        for t in &mut base {
            for sec in &mut t.sections {
                if sec.name == LatencySection::NAME {
                    sec.version = "v1".to_owned();
                }
            }
        }
        let rep = compare(&base, &cand, CompareConfig::default());

        let latency = rep
            .sections
            .iter()
            .find(|s| matches!(s, SectionReport::Uncompared { name, .. } if name == LatencySection::NAME))
            .expect("latency uncompared");
        match latency {
            SectionReport::Uncompared { reason, .. } => {
                assert_eq!(
                    *reason,
                    UncomparedReason::VersionChanged {
                        base: "v1".to_owned(),
                        cand: "v2".to_owned(),
                    }
                );
            }
            SectionReport::Compared { .. } => panic!("latency should not compare across versions"),
        }

        // The `extra` section (v1 on both) still resolves — to UnknownName,
        // proving the version mismatch on latency didn't abort the loop.
        assert!(rep.sections.iter().any(|s| matches!(
            s,
            SectionReport::Uncompared { name, reason }
                if name == "extra" && *reason == UncomparedReason::UnknownName
        )));
    }

    #[test]
    fn markdown_renders_both_compared_table_and_uncompared_note() {
        let base = side(&[167_000, 165_000, 169_000, 166_000]);
        let cand = with_extra_section(side(&[33_000, 34_000, 32_000, 33_500]), "throughput", "v1");
        let rep = compare(&base, &cand, CompareConfig::default());
        let md = markdown(&rep, "PR 9999 vs main", "test");
        // The latency table is present (not blinded) ...
        assert!(md.contains("full grid"));
        assert!(md.contains("| topology | w | metric |"));
        // ... and the uncompared section's note rides alongside it.
        assert!(md.contains("throughput: new this run"));
    }
}
