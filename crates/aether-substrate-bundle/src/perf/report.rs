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
    /// The light tier's section name — the historical `latency`, kept
    /// verbatim so the v3 back-compat shim and the existing fixtures don't
    /// churn. The heavy / real tiers use tier-suffixed names
    /// ([`super::harness::Tier::section_name`]).
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
fn latency_section_renders_verdict(name: &str) -> bool {
    name == LatencySection::NAME
}

/// One cell's measured throughput in a single trial
/// (iamacoffeepot/aether#1202): completed mails/sec for a (worker ×
/// topology) cell under saturation.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ThroughputCell {
    pub workers: usize,
    pub topo: String,
    pub mails_per_sec: f64,
}

impl ThroughputCell {
    fn key(&self) -> ThroughputKey {
        ThroughputKey {
            workers: self.workers,
            topo: self.topo.clone(),
        }
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
    /// (it lives only in the on-demand observe table).
    ///
    /// The cells are split **by workload tier** (ADR-0085 amendment) into
    /// one [`LatencySection`]-bodied [`RawSection`] per tier present: the
    /// light tier reuses the historical `latency` name, heavy / real are
    /// tier-suffixed ([`Tier::section_name`]). Tiers are emitted in
    /// `light → heavy → real` order so the report reads gate-first. When the
    /// sweep ran only the light tier (the historical default) the output is
    /// the single `latency` section, byte-for-byte as before.
    ///
    /// [`CellResult`]: super::harness::CellResult
    /// [`Tier::section_name`]: super::harness::Tier::section_name
    #[must_use]
    pub fn from_cells(
        cells: &[super::harness::CellResult],
        frames: u32,
        pace_hz: Option<u64>,
        git_sha: Option<String>,
    ) -> Self {
        use super::harness::Tier;

        let mut sections = Vec::new();
        for tier in [Tier::Light, Tier::Heavy, Tier::Real] {
            let mut rows = Vec::new();
            for c in cells.iter().filter(|c| c.tier == tier) {
                for (metric, s) in [
                    (Metric::Construct, &c.construct),
                    (Metric::Queued, &c.queued),
                    (Metric::Drain, &c.drain),
                    (Metric::Handler, &c.handler),
                ] {
                    rows.push(CellJson {
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
            if rows.is_empty() {
                continue;
            }
            let body = serde_json::to_value(LatencySection { cells: rows })
                .unwrap_or(serde_json::Value::Null);
            sections.push(RawSection {
                name: tier.section_name().to_owned(),
                version: LatencySection::VERSION.to_owned(),
                body,
            });
        }
        Self {
            schema: TRIAL_SCHEMA.to_owned(),
            git_sha,
            pace_hz,
            frames,
            sections,
        }
    }

    /// Build a *saturation* trial report from a sweep's [`CellResult`]s
    /// (iamacoffeepot/aether#1202). A saturate run reports **throughput
    /// only** — per-hop latency under saturation is contended and
    /// high-variance, so pairing it would compare noise. Each cell with a
    /// measured `throughput_mps` (a truncated cell carries `None` and is
    /// skipped) contributes one [`ThroughputCell`]; the rows ride in a
    /// single [`ThroughputSection`].
    ///
    /// [`CellResult`]: super::harness::CellResult
    #[must_use]
    pub fn from_throughput_cells(
        cells: &[super::harness::CellResult],
        frames: u32,
        git_sha: Option<String>,
    ) -> Self {
        let rows: Vec<ThroughputCell> = cells
            .iter()
            .filter_map(|c| {
                c.throughput_mps.map(|mps| ThroughputCell {
                    workers: c.workers,
                    topo: c.topo.clone(),
                    mails_per_sec: mps,
                })
            })
            .collect();
        let throughput = ThroughputSection { cells: rows };
        let body = serde_json::to_value(&throughput).unwrap_or(serde_json::Value::Null);
        Self {
            schema: TRIAL_SCHEMA.to_owned(),
            git_sha,
            // Saturation isn't paced — the backlog drains flat-out per
            // frame — so `pace_hz` is `None`.
            pace_hz: None,
            frames,
            sections: vec![RawSection {
                name: ThroughputSection::NAME.to_owned(),
                version: ThroughputSection::VERSION.to_owned(),
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

/// Pairing key for a throughput cell (iamacoffeepot/aether#1202) — a
/// (worker × topology) cell, no metric/percentile axis since throughput
/// is a single rate per cell.
#[derive(Clone, PartialEq, Eq, Hash)]
struct ThroughputKey {
    workers: usize,
    topo: String,
}

/// Which direction of paired delta is the win (iamacoffeepot/aether#1202).
/// Latency is lower-is-better (a negative delta improves); throughput is
/// higher-is-better (a positive delta improves). The only verdict knob
/// that differs between the two sections.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Direction {
    LowerIsBetter,
    HigherIsBetter,
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

/// One compared throughput cell (iamacoffeepot/aether#1202): the
/// base/candidate median rate (mails/sec) with its across-trial IQR band,
/// plus the higher-is-better paired-delta verdict. The throughput analog
/// of [`CellComparison`] — no metric/percentile axis, since throughput is
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

/// One section's outcome in a [`ComparisonReport`]: a typed verdict grid
/// (`Compared` for latency, `ThroughputCompared` for the saturation rate,
/// iamacoffeepot/aether#1202) or a reasoned skip (`Uncompared`). The two
/// compared variants carry the same headline counts so the rollup sums
/// over both, but distinct cell payloads so each renders with its own
/// table.
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
    ThroughputCompared {
        name: String,
        improved: usize,
        stable: usize,
        regressed: usize,
        cells: Vec<ThroughputComparison>,
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

        // A latency section of any tier (light = `latency`, heavy / real
        // tier-suffixed; ADR-0085 amendment) routes to the same per-cell
        // paired compare — the verdict numbers are computed identically for
        // every tier. Verdict *suppression* for non-light tiers is a
        // render-time concern (see `push_latency_section`), not here.
        if is_latency_section(name) {
            let base_cells = decode_latency_cells(name, &base[..k]);
            let cand_cells = decode_latency_cells(name, &cand[..k]);
            sections.push(compare_latency(name, &base_cells, &cand_cells, k, cfg));
        } else if name == ThroughputSection::NAME {
            let base_cells = decode_throughput_cells(&base[..k]);
            let cand_cells = decode_throughput_cells(&cand[..k]);
            sections.push(compare_throughput(name, &base_cells, &cand_cells, k, cfg));
        } else {
            sections.push(SectionReport::Uncompared {
                name: name.clone(),
                reason: UncomparedReason::UnknownName,
            });
        }
    }

    ComparisonReport {
        trials: k,
        sections,
    }
}

/// Per-trial latency cells for the named tier section (`latency`,
/// `latency.heavy`, or `latency.real`; ADR-0085 amendment), decoding each
/// trial's body and dropping any trial whose body doesn't decode (it then
/// can't satisfy the present-in-every-trial gate below, exactly as a missing
/// cell did).
fn decode_latency_cells(name: &str, trials: &[TrialReport]) -> Vec<Vec<CellJson>> {
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

            let verdict = classify(
                &deltas,
                delta_median,
                delta_iqr,
                base_median,
                Direction::LowerIsBetter,
                cfg,
            );
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

/// Per-trial throughput cells: decode each trial's `throughput` section
/// body, dropping any trial whose body doesn't decode (it then can't
/// satisfy the present-in-every-trial gate below, exactly as a missing
/// cell does for latency).
fn decode_throughput_cells(trials: &[TrialReport]) -> Vec<Vec<ThroughputCell>> {
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

/// Find the throughput cell matching `key` in one trial's cells (a free
/// fn so the returned borrow ties to the slice's lifetime, mirroring
/// [`find_cell`]).
fn find_throughput_cell<'a>(
    cells: &'a [ThroughputCell],
    key: &ThroughputKey,
) -> Option<&'a ThroughputCell> {
    cells
        .iter()
        .find(|c| c.workers == key.workers && c.topo == key.topo)
}

/// The throughput section's per-cell paired-delta compare
/// (iamacoffeepot/aether#1202) — mirrors [`compare_latency`], but keyed by
/// (workers, topo) only (throughput is a single rate per cell, no
/// metric/percentile axis) and classified higher-is-better. A cell missing
/// from any trial of either side is dropped, exactly as in the latency
/// compare.
#[allow(clippy::cast_precision_loss)]
fn compare_throughput(
    name: &str,
    base_cells: &[Vec<ThroughputCell>],
    cand_cells: &[Vec<ThroughputCell>],
    k: usize,
    cfg: CompareConfig,
) -> SectionReport {
    let mut cells: Vec<ThroughputComparison> = Vec::new();

    let keys: Vec<ThroughputKey> = base_cells
        .first()
        .map(|c| c.iter().map(ThroughputCell::key).collect())
        .unwrap_or_default();

    for key in &keys {
        let base_hits: Vec<&ThroughputCell> = base_cells[..k.min(base_cells.len())]
            .iter()
            .filter_map(|c| find_throughput_cell(c, key))
            .collect();
        let cand_hits: Vec<&ThroughputCell> = cand_cells[..k.min(cand_cells.len())]
            .iter()
            .filter_map(|c| find_throughput_cell(c, key))
            .collect();
        if base_hits.len() != k || cand_hits.len() != k || k == 0 {
            continue; // cell not present in every trial — skip
        }

        let base_vals: Vec<f64> = base_hits.iter().map(|c| c.mails_per_sec).collect();
        let cand_vals: Vec<f64> = cand_hits.iter().map(|c| c.mails_per_sec).collect();
        let deltas: Vec<f64> = (0..k).map(|t| cand_vals[t] - base_vals[t]).collect();

        let base_sorted = sorted(base_vals.clone());
        let cand_sorted = sorted(cand_vals.clone());
        let delta_sorted = sorted(deltas.clone());

        let base_median = median_sorted(&base_sorted);
        let cand_median = median_sorted(&cand_sorted);
        let delta_median = median_sorted(&delta_sorted);
        let delta_iqr = iqr_sorted(&delta_sorted);

        let verdict = classify(
            &deltas,
            delta_median,
            delta_iqr,
            base_median,
            Direction::HigherIsBetter,
            cfg,
        );
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

    let improved = cells
        .iter()
        .filter(|c| c.verdict == Verdict::Improved)
        .count();
    let regressed = cells
        .iter()
        .filter(|c| c.verdict == Verdict::Regressed)
        .count();
    let stable = cells.len() - improved - regressed;
    SectionReport::ThroughputCompared {
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
    dir: Direction,
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

    // The absolute floor (`abs_floor_ns`) is a *nanosecond* resolution
    // floor — meaningful for a latency span, meaningless for a mails/sec
    // rate (iamacoffeepot/aether#1202). For a higher-is-better rate the
    // verdict rests on the IQR + relative floors only; the ns floor is
    // neutralised to zero.
    let abs_floor = match dir {
        Direction::LowerIsBetter => cfg.abs_floor_ns,
        Direction::HigherIsBetter => 0.0,
    };
    let floor = (cfg.effect_floor_iqr * delta_iqr)
        .max(cfg.rel_floor * base_median)
        .max(abs_floor);
    let large = delta_median.abs() > floor;

    if !(consistent && large) {
        return Verdict::Stable;
    }
    // A negative paired delta means the candidate's value fell; whether
    // that reads `Improved` depends on the metric's direction.
    let value_fell = delta_median < 0.0;
    let improved = match dir {
        Direction::LowerIsBetter => value_fell,
        Direction::HigherIsBetter => !value_fell,
    };
    if improved {
        Verdict::Improved
    } else {
        Verdict::Regressed
    }
}

fn us(ns: f64) -> String {
    format!("{:.2}", ns / 1000.0)
}

/// Hidden marker so the CI poster (PR 2) can find-and-update its sticky
/// comment in place rather than spamming new ones.
pub const STICKY_MARKER: &str = "<!-- aether-perf-report -->";

/// The headline `N improved · N stable · N regressed` rollup — the
/// **gate-signal** count, so it sums **only** the verdict-carrying sections
/// (ADR-0085 amendment): the light tier's `latency` section and the
/// throughput section. Heavy / real latency sections are characterisation —
/// `compare_latency` still populates their improved/regressed counts (the
/// numbers are wanted), but their verdict is suppressed at render time, so
/// summing them into the headline would leak a no-verdict tier into the
/// signal a reviewer reads as "did this change regress". Shared by
/// [`markdown`] here and `perf-compare`'s `roll_up` so the two never drift.
#[must_use]
pub fn headline_counts(report: &ComparisonReport) -> (usize, usize, usize) {
    report
        .sections
        .iter()
        .fold((0, 0, 0), |(i, s, r), sec| match sec {
            SectionReport::Compared {
                name,
                improved,
                stable,
                regressed,
                ..
            } if latency_section_renders_verdict(name) => (i + improved, s + stable, r + regressed),
            SectionReport::ThroughputCompared {
                improved,
                stable,
                regressed,
                ..
            } => (i + improved, s + stable, r + regressed),
            // A non-light latency section is compared (it carries counts)
            // but its verdict is suppressed — it must not reach the headline.
            SectionReport::Compared { .. } | SectionReport::Uncompared { .. } => (i, s, r),
        })
}

/// Render the comparison as a sticky PR-comment markdown body: headline
/// counts (the verdict-carrying sections only — see [`headline_counts`]),
/// then per section — a light `latency` verdict (non-stable rows up top,
/// full grid collapsed), a heavy / real latency *trend grid* (no verdict,
/// ADR-0085 amendment), or a one-line "new this run" note for an uncompared
/// section (iamacoffeepot/aether#1206).
#[must_use]
#[allow(clippy::format_push_string)]
pub fn markdown(report: &ComparisonReport, title: &str, subtitle: &str) -> String {
    let mut s = String::new();
    s.push_str(STICKY_MARKER);
    s.push('\n');
    s.push_str(&format!("## dispatch perf — {title}\n"));
    s.push_str(&format!("{subtitle}\n\n"));

    let (improved, stable, regressed) = headline_counts(report);
    s.push_str(&format!(
        "**{improved} improved · {stable} stable · {regressed} regressed** ({} trials/config, paired)\n\n",
        report.trials
    ));

    for sec in &report.sections {
        match sec {
            SectionReport::Compared { name, cells, .. } => {
                push_latency_section(&mut s, name, cells);
            }
            SectionReport::ThroughputCompared { name, cells, .. } => {
                push_throughput_section(&mut s, name, cells);
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

/// Shared tail for a compared section's markdown: the non-stable rows (or
/// a "no cells moved" note when none did), then the collapsed full grid.
/// `push_latency_section` and `push_throughput_section` each build their
/// own header + per-row rendering and hand the rendered rows here, so the
/// table scaffolding lives in one place.
#[allow(clippy::format_push_string)]
fn push_section_tables(
    s: &mut String,
    name: &str,
    header: &str,
    non_stable: &[String],
    all: &[String],
) {
    if non_stable.is_empty() {
        s.push_str(&format!(
            "_{name}: no cells moved beyond the noise band._\n\n"
        ));
    } else {
        s.push_str(header);
        for r in non_stable {
            s.push_str(r);
        }
        s.push('\n');
    }

    s.push_str(&format!(
        "<details><summary>{name} full grid — {} cells</summary>\n\n",
        all.len()
    ));
    s.push_str(header);
    for r in all {
        s.push_str(r);
    }
    s.push_str("\n</details>\n\n");
}

/// Render a latency section. The renderer learns its tier from the section
/// name (ADR-0085 amendment): the light tier (`latency`) renders the full
/// verdict treatment — non-stable rows lifted up top, the verdict column,
/// the "nothing moved" note. A non-light tier (`latency.heavy` /
/// `latency.real`) renders a **no-verdict trend grid**: every cell in one
/// table, no verdict column, no lifted rows, no noise-band note. `classify`
/// still produced a verdict for these cells (the numbers + direction are
/// wanted); this just declines to *display* it, since the tier's variance
/// sits below the band a verdict needs.
#[allow(clippy::format_push_string)]
fn push_latency_section(s: &mut String, name: &str, cells: &[CellComparison]) {
    if latency_section_renders_verdict(name) {
        push_latency_verdict_section(s, name, cells);
    } else {
        push_latency_trend_section(s, name, cells);
    }
}

#[allow(clippy::format_push_string)]
fn push_latency_verdict_section(s: &mut String, name: &str, cells: &[CellComparison]) {
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

    let all: Vec<String> = cells.iter().map(&row).collect();
    let non_stable: Vec<String> = cells
        .iter()
        .filter(|c| c.verdict != Verdict::Stable)
        .map(&row)
        .collect();
    push_section_tables(s, name, header, &non_stable, &all);
}

/// The no-verdict trend grid for a heavy / real latency section: one table,
/// every cell, no verdict column, base/this/Δ only — characterisation, not
/// classification (ADR-0085 amendment).
#[allow(clippy::format_push_string)]
fn push_latency_trend_section(s: &mut String, name: &str, cells: &[CellComparison]) {
    let header =
        "| topology | w | metric | pct | base µs | this µs | Δ |\n|---|--:|---|---|--:|--:|--:|\n";
    s.push_str(&format!(
        "<details><summary>{name} trend (no verdict — characterisation) — {} cells</summary>\n\n",
        cells.len()
    ));
    s.push_str(header);
    for c in cells {
        s.push_str(&format!(
            "| {} | {} | {} | {} | {} ±{} | {} ±{} | {:+.0}% |\n",
            c.topo,
            c.workers,
            c.metric.label(),
            c.percentile,
            us(c.base_median),
            us(c.base_iqr),
            us(c.cand_median),
            us(c.cand_iqr),
            c.delta_pct,
        ));
    }
    s.push_str("\n</details>\n\n");
}

/// Render the throughput section (iamacoffeepot/aether#1202) — the
/// higher-is-better mails/sec analog of [`push_latency_section`]:
/// non-stable rows up top, full grid collapsed, rates in thousands of
/// mails/sec.
#[allow(clippy::format_push_string)]
fn push_throughput_section(s: &mut String, name: &str, cells: &[ThroughputComparison]) {
    let header =
        "| topology | w | base k/s | this k/s | Δ | verdict |\n|---|--:|--:|--:|--:|---|\n";
    let row = |c: &ThroughputComparison| -> String {
        let verdict = match c.verdict {
            Verdict::Improved => "improved",
            Verdict::Stable => "stable",
            Verdict::Regressed => "regressed",
        };
        format!(
            "| {} | {} | {} ±{} | {} ±{} | {:+.0}% | {} |\n",
            c.topo,
            c.workers,
            kps(c.base_median),
            kps(c.base_iqr),
            kps(c.cand_median),
            kps(c.cand_iqr),
            c.delta_pct,
            verdict,
        )
    };

    let all: Vec<String> = cells.iter().map(&row).collect();
    let non_stable: Vec<String> = cells
        .iter()
        .filter(|c| c.verdict != Verdict::Stable)
        .map(&row)
        .collect();
    push_section_tables(s, name, header, &non_stable, &all);
}

/// Format a mails/sec rate in thousands (k/s), mirroring [`us`]'s
/// scale-and-fixed-precision rendering for the latency table.
fn kps(mps: f64) -> String {
    format!("{:.1}", mps / 1000.0)
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
            _ => panic!("throughput should not be compared"),
        }
    }

    #[test]
    fn unknown_section_on_both_sides_reads_unknown_name() {
        // Present on both sides at an agreed version but with no typed
        // compare — that's the UnknownName reason, distinct from
        // NewThisRun. (`throughput` is now a *known* section — this guard
        // needs a name the comparator still has no compare for, so it uses
        // `experimental`. iamacoffeepot/aether#1202.)
        let base = with_extra_section(side(&[1000, 1000, 1000]), "experimental", "v1");
        let cand = with_extra_section(side(&[1000, 1000, 1000]), "experimental", "v1");
        let rep = compare(&base, &cand, CompareConfig::default());
        let unknown = rep
            .sections
            .iter()
            .find(|s| matches!(s, SectionReport::Uncompared { name, .. } if name == "experimental"))
            .expect("uncompared experimental section present");
        match unknown {
            SectionReport::Uncompared { reason, .. } => {
                assert_eq!(*reason, UncomparedReason::UnknownName);
            }
            _ => panic!("experimental should not be compared"),
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
            _ => panic!("latency should not compare across versions"),
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

    /// Build a K-trial side carrying a single `throughput` section cell
    /// (`fanout-8 @ 11w`) whose rate follows `rates` (mails/sec). The
    /// throughput analog of [`side`] (iamacoffeepot/aether#1202).
    fn throughput_side(rates: &[f64]) -> Vec<TrialReport> {
        rates
            .iter()
            .map(|&mails_per_sec| {
                let cells = vec![ThroughputCell {
                    workers: 11,
                    topo: "fanout-8".to_owned(),
                    mails_per_sec,
                }];
                let body = serde_json::to_value(ThroughputSection { cells })
                    .expect("encode throughput body");
                TrialReport {
                    schema: TRIAL_SCHEMA.to_owned(),
                    git_sha: None,
                    pace_hz: None,
                    frames: 200,
                    sections: vec![RawSection {
                        name: ThroughputSection::NAME.to_owned(),
                        version: ThroughputSection::VERSION.to_owned(),
                        body,
                    }],
                }
            })
            .collect()
    }

    /// The single throughput cell's verdict in a comparison report.
    fn throughput_verdict(rep: &ComparisonReport) -> Verdict {
        rep.sections
            .iter()
            .find_map(|s| match s {
                SectionReport::ThroughputCompared { cells, .. } => cells.first().map(|c| c.verdict),
                _ => None,
            })
            .expect("compared throughput cell present")
    }

    #[test]
    fn higher_throughput_reads_improved_not_regressed() {
        // Throughput is higher-is-better: a clearly-higher candidate rate
        // is an Improvement, even though its paired delta is *positive*
        // (the opposite of a latency win).
        let base = throughput_side(&[
            100_000.0, 98_000.0, 102_000.0, 99_000.0, 101_000.0, 100_500.0,
        ]);
        let cand = throughput_side(&[
            200_000.0, 198_000.0, 202_000.0, 199_000.0, 201_000.0, 200_500.0,
        ]);
        let rep = compare(&base, &cand, CompareConfig::default());
        assert_eq!(throughput_verdict(&rep), Verdict::Improved);
    }

    #[test]
    fn lower_throughput_reads_regressed() {
        // A clearly-lower candidate rate is a regression (a negative
        // paired delta, the inverse of the latency direction).
        let base = throughput_side(&[
            200_000.0, 198_000.0, 202_000.0, 199_000.0, 201_000.0, 200_500.0,
        ]);
        let cand = throughput_side(&[
            100_000.0, 98_000.0, 102_000.0, 99_000.0, 101_000.0, 100_500.0,
        ]);
        let rep = compare(&base, &cand, CompareConfig::default());
        assert_eq!(throughput_verdict(&rep), Verdict::Regressed);
    }

    #[test]
    fn equal_throughput_reads_stable() {
        // Near-identical rates pair to δ ≈ 0 — below the noise band, so
        // stable regardless of the ns floor (neutralised for a rate).
        let base = throughput_side(&[
            100_000.0, 99_000.0, 101_000.0, 100_500.0, 99_500.0, 100_000.0,
        ]);
        let cand = throughput_side(&[
            100_200.0, 99_100.0, 101_100.0, 100_400.0, 99_600.0, 100_100.0,
        ]);
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
        let tp: ThroughputSection =
            serde_json::from_value(sec.body.clone()).expect("decode throughput body");
        assert_eq!(tp.cells.len(), 1);
        assert!((tp.cells[0].mails_per_sec - 100_000.0).abs() < f64::EPSILON);
    }

    #[test]
    fn throughput_verdict_renders_in_markdown() {
        // Step 4 round-trip: a report carrying a throughput section flows
        // through `compare` → `markdown` and the higher-is-better verdict
        // shows in the rendered body (the per-section dispatch routes it,
        // no perf-compare change needed).
        let base = throughput_side(&[100_000.0, 98_000.0, 102_000.0, 99_000.0]);
        let cand = throughput_side(&[200_000.0, 198_000.0, 202_000.0, 199_000.0]);
        let rep = compare(&base, &cand, CompareConfig::default());
        let md = markdown(&rep, "PR 9999 vs main", "test");
        assert!(md.contains(STICKY_MARKER));
        // The throughput table header (k/s units), the headline rollup
        // counting the win, and the improved verdict are all present.
        assert!(md.contains("| topology | w | base k/s |"));
        assert!(md.contains("improved"));
        assert!(md.contains("throughput full grid"));
    }

    /// Build a K-trial side carrying a single latency cell under the named
    /// tier section (ADR-0085 amendment) — the tier analog of [`side`]. The
    /// section name selects the tier (`latency` light, `latency.heavy`,
    /// `latency.real`).
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    fn tier_side(section_name: &str, p50s: &[u64]) -> Vec<TrialReport> {
        p50s.iter()
            .map(|&p50| {
                let cells = vec![CellJson {
                    workers: 11,
                    topo: "fanout-8-heavy".to_owned(),
                    metric: Metric::Drain,
                    p50,
                    p90: (p50 as f64 * 1.2) as u64,
                    p99: (p50 as f64 * 1.5) as u64,
                    max: p50 * 4,
                    n: 1800,
                }];
                let body = serde_json::to_value(LatencySection { cells })
                    .expect("encode tier latency body");
                TrialReport {
                    schema: TRIAL_SCHEMA.to_owned(),
                    git_sha: None,
                    pace_hz: None,
                    frames: 200,
                    sections: vec![RawSection {
                        name: section_name.to_owned(),
                        version: LatencySection::VERSION.to_owned(),
                        body,
                    }],
                }
            })
            .collect()
    }

    /// Attach a `latency.heavy` section's cells to an existing side, so a
    /// trial carries both the light `latency` section and the heavy one (the
    /// realistic `AETHER_PERF_TIER=light,heavy` shape).
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    fn with_heavy_section(mut side: Vec<TrialReport>, p50s: &[u64]) -> Vec<TrialReport> {
        for (t, &p50) in side.iter_mut().zip(p50s.iter()) {
            let cells = vec![CellJson {
                workers: 11,
                topo: "fanout-8-heavy".to_owned(),
                metric: Metric::Drain,
                p50,
                p90: (p50 as f64 * 1.2) as u64,
                p99: (p50 as f64 * 1.5) as u64,
                max: p50 * 4,
                n: 1800,
            }];
            let body =
                serde_json::to_value(LatencySection { cells }).expect("encode heavy latency body");
            t.sections.push(RawSection {
                name: "latency.heavy".to_owned(),
                version: LatencySection::VERSION.to_owned(),
                body,
            });
        }
        side
    }

    #[test]
    fn from_cells_sections_by_tier() {
        use crate::perf::harness::{CellResult, Stats, Tier};

        let cell = |topo: &str, tier: Tier| CellResult {
            workers: 4,
            topo: topo.to_owned(),
            tier,
            construct: Stats::default(),
            queued: Stats::default(),
            drain: Stats::default(),
            handler: Stats::default(),
            depth: Stats::default(),
            throughput_mps: None,
        };
        let cells = vec![
            cell("fanout-8", Tier::Light),
            cell("fanout-8-heavy", Tier::Heavy),
        ];
        let report = TrialReport::from_cells(&cells, 200, None, None);
        // One section per tier present, light named `latency` (back-compat),
        // heavy named `latency.heavy`. No empty real section.
        let names: Vec<&str> = report.sections.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec![LatencySection::NAME, "latency.heavy"]);
    }

    #[test]
    fn heavy_section_renders_no_verdict() {
        // A `latency.heavy` section is compared (it carries counts), but the
        // renderer must suppress the verdict: a no-verdict trend grid, no
        // verdict column, no "no cells moved" note (ADR-0085 amendment). Use
        // a base/cand that *would* flag a verdict for the light tier so the
        // suppression is the thing under test, not a coincidentally-stable
        // cell.
        let base = tier_side("latency.heavy", &[167_000, 165_000, 169_000, 166_000]);
        let cand = tier_side("latency.heavy", &[33_000, 34_000, 32_000, 33_500]);
        let rep = compare(&base, &cand, CompareConfig::default());

        // The heavy section is Compared (the numbers/direction are wanted) ...
        let heavy = rep
            .sections
            .iter()
            .find(|s| matches!(s, SectionReport::Compared { name, .. } if name == "latency.heavy"))
            .expect("heavy latency section compared");
        // ... and it did compute a non-stable verdict internally.
        let SectionReport::Compared {
            improved, cells, ..
        } = heavy
        else {
            panic!("heavy section should be compared");
        };
        assert!(*improved > 0, "heavy compare still computes the verdict");
        assert!(
            cells.iter().any(|c| c.verdict == Verdict::Improved),
            "the per-cell verdict is still computed (just not rendered)"
        );

        // But the rendered markdown carries no verdict column / value and no
        // noise-band note for the heavy section — only the trend grid.
        let md = markdown(&rep, "PR 9999 vs main", "test");
        assert!(
            md.contains("latency.heavy trend (no verdict"),
            "heavy section renders as a no-verdict trend grid"
        );
        assert!(
            !md.contains("latency.heavy: no cells moved beyond the noise band"),
            "the noise-band note is suppressed for a no-verdict tier"
        );
        // The trend grid's header omits the verdict column the light table has.
        assert!(md.contains("| topology | w | metric | pct | base µs | this µs | Δ |\n"));
    }

    #[test]
    fn headline_rollup_excludes_heavy_and_real() {
        // CRITICAL guard (ADR-0085 amendment, #1222): the headline rollup is
        // the gate signal, so a suppressed-verdict heavy / real tier must not
        // leak into it. Build a light tier that's all-stable and a heavy tier
        // with a big swing that classify *would* call improved/regressed; the
        // headline must reflect the light tier only.
        let light_base = side(&[1000, 1000, 1000, 1000]);
        let light_cand = side(&[1010, 990, 1005, 995]); // δ ≈ 0 → stable
        let base = with_heavy_section(light_base, &[167_000, 165_000, 169_000, 166_000]);
        let cand = with_heavy_section(light_cand, &[33_000, 34_000, 32_000, 33_500]);
        let rep = compare(&base, &cand, CompareConfig::default());

        // Sanity: the heavy section *did* compute a non-stable verdict.
        let heavy_improved = rep.sections.iter().any(|s| {
            matches!(s, SectionReport::Compared { name, improved, .. }
                if name == "latency.heavy" && *improved > 0)
        });
        assert!(heavy_improved, "heavy section computed an improvement");

        // The headline counts the light tier only — its three p50/p90/p99
        // cells are all stable, so zero improved / regressed from the heavy
        // swing leaks in.
        let (improved, _stable, regressed) = headline_counts(&rep);
        assert_eq!(
            (improved, regressed),
            (0, 0),
            "the heavy tier's verdict must not reach the gate-signal headline"
        );
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
            rep.sections.iter().any(
                |s| matches!(s, SectionReport::Compared { name, .. } if name == "latency.real")
            ),
            "real latency section routes to the per-cell compare"
        );
        let (improved, _stable, regressed) = headline_counts(&rep);
        assert_eq!(
            (improved, regressed),
            (0, 0),
            "the real tier's verdict is excluded from the headline too"
        );
    }
}
