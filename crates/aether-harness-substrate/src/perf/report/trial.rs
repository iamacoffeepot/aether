//! The emitted envelope: a [`TrialReport`] is one fresh-process sweep run,
//! carrying its [`TRIAL_SCHEMA`] tag and a list of independently-versioned
//! [`RawSection`]s. The `from_*` constructors turn a sweep's cell results into
//! those sections; [`probe_schema`] reads the envelope tag without parsing the
//! rest.

use serde::{Deserialize, Serialize};

use crate::perf::harness::CellResult;

use super::keep_up::{KeepUpCell, KeepUpSection};
use super::latency::LatencySection;
use super::metric::{CellJson, Metric};
use super::throughput::{ThroughputCell, ThroughputSection};

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
    /// [`CellResult`]: crate::perf::harness::CellResult
    /// [`Tier::section_name`]: crate::perf::harness::Tier::section_name
    #[must_use]
    pub fn from_cells(cells: &[CellResult], frames: u32, pace_hz: Option<u64>, git_sha: Option<String>) -> Self {
        use crate::perf::harness::Tier;

        let mut sections = Vec::new();
        // The light and heavy tiers report per-hop span percentiles. The real
        // tier reports **keep-up** instead (iamacoffeepot/aether#1233): its
        // fan-out laps the trace ring, so the spans are unmeasurable — the
        // keep-up section below replaces them.
        for tier in [Tier::Light, Tier::Heavy] {
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
                        tail_mass: s.tail_mass,
                    });
                }
            }
            if rows.is_empty() {
                continue;
            }
            let body = serde_json::to_value(LatencySection { cells: rows }).unwrap_or(serde_json::Value::Null);
            sections.push(RawSection {
                name: tier.section_name().to_owned(),
                version: LatencySection::VERSION.to_owned(),
                body,
            });
        }

        // Real tier: keep-up characterisation (iamacoffeepot/aether#1233).
        // Only cells that harvested counters contribute; a cell whose harvest
        // failed (`keepup == None`) is dropped, as a lapped latency cell was.
        let keepup_rows: Vec<KeepUpCell> = cells
            .iter()
            .filter(|c| c.tier == Tier::Real)
            .filter_map(|c| {
                c.keepup.map(|k| KeepUpCell {
                    workers: c.workers,
                    topo: c.topo.clone(),
                    offered: k.offered,
                    completed: k.completed,
                    elapsed_nanos: k.elapsed_nanos,
                    expected_nanos: k.expected_nanos,
                })
            })
            .collect();
        if !keepup_rows.is_empty() {
            let body = serde_json::to_value(KeepUpSection { cells: keepup_rows }).unwrap_or(serde_json::Value::Null);
            sections.push(RawSection {
                name: KeepUpSection::NAME.to_owned(),
                version: KeepUpSection::VERSION.to_owned(),
                body,
            });
        }
        Self { schema: TRIAL_SCHEMA.to_owned(), git_sha, pace_hz, frames, sections }
    }

    /// Build a *saturation* trial report from a sweep's [`CellResult`]s
    /// (iamacoffeepot/aether#1202). A saturate run reports **throughput
    /// only** — per-hop latency under saturation is contended and
    /// high-variance, so pairing it would compare noise. Every cell
    /// contributes one [`ThroughputCell`]: a measured cell carries its
    /// `throughput_mps` and a truncated cell carries `None`
    /// (iamacoffeepot/aether#1226) — emitted flagged-not-dropped so a
    /// missing measurement is visible in the section rather than silently
    /// filtered away (the comparator still treats a `None`-rate cell as "no
    /// measurement"). The rows ride in a single [`ThroughputSection`].
    ///
    /// [`CellResult`]: crate::perf::harness::CellResult
    #[must_use]
    pub fn from_throughput_cells(cells: &[CellResult], frames: u32, git_sha: Option<String>) -> Self {
        let rows: Vec<ThroughputCell> = cells
            .iter()
            .map(|c| ThroughputCell { workers: c.workers, topo: c.topo.clone(), mails_per_sec: c.throughput_mps })
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
    pub(super) fn section(&self, name: &str) -> Option<&RawSection> {
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
    serde_json::from_slice::<SchemaProbe>(json).ok().map(|p| p.schema)
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn from_cells_sections_by_tier() {
        use crate::perf::harness::{CellResult, Stats, Tier};

        let cell = |topo: &str, tier: Tier| CellResult {
            workers: 4,
            topo: topo.to_owned(),
            tier,
            boot_handoff_nanos: 0,
            construct: Stats::default(),
            queued: Stats::default(),
            drain: Stats::default(),
            handler: Stats::default(),
            depth: Stats::default(),
            throughput_mps: None,
            keepup: None,
        };
        let cells = vec![cell("fanout-8", Tier::Light), cell("fanout-8-heavy", Tier::Heavy)];
        let report = TrialReport::from_cells(&cells, 200, None, None);
        // One section per tier present, light named `latency` (back-compat),
        // heavy named `latency.heavy`. No empty real section.
        let names: Vec<&str> = report.sections.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec![LatencySection::NAME, "latency.heavy"]);
    }

    #[test]
    fn truncated_cell_is_flagged_not_dropped() {
        // Steps 3 + 4 (iamacoffeepot/aether#1226): a cell whose ring lapped
        // carries `throughput_mps == None`. `from_throughput_cells` used to
        // `filter_map` that `None` away, so the cell vanished from the
        // section and from any base-vs-candidate comparison — the only trace
        // was a stderr warn. It must now appear flagged (`mails_per_sec ==
        // None`) so a missing measurement is visible in the report. (This is
        // the relocated truncation contract that
        // `over_capacity_backlog_flags_truncation_not_a_wrong_rate` once
        // tested via the sweep path, now unreachable after the per-cell
        // burst clamp.)
        use crate::perf::harness::{CellResult, Stats, Tier};

        let cell = |topo: &str, throughput_mps: Option<f64>| CellResult {
            workers: 2,
            topo: topo.to_owned(),
            tier: Tier::Light,
            boot_handoff_nanos: 0,
            construct: Stats::default(),
            queued: Stats::default(),
            drain: Stats::default(),
            handler: Stats::default(),
            depth: Stats::default(),
            throughput_mps,
            keepup: None,
        };
        let cells = vec![
            cell("depth-1", Some(123_456.0)),
            cell("fanout-8", None), // truncated — ring lapped
        ];

        let report = TrialReport::from_throughput_cells(&cells, 1, None);
        let sec = &report.sections[0];
        assert_eq!(sec.name, ThroughputSection::NAME);
        assert_eq!(sec.version, ThroughputSection::VERSION); // v2
        let tp: ThroughputSection = serde_json::from_value(sec.body.clone()).expect("decode throughput body");

        // Both cells are present — the truncated one is flagged, not dropped.
        assert_eq!(tp.cells.len(), 2, "truncated cell must not be filtered out");
        let flagged = tp
            .cells
            .iter()
            .find(|c| c.topo == "fanout-8")
            .expect("the truncated fanout-8 cell is present in the section");
        assert!(
            flagged.mails_per_sec.is_none(),
            "a truncated cell carries no rate (flagged), got {:?}",
            flagged.mails_per_sec
        );
        let measured = tp.cells.iter().find(|c| c.topo == "depth-1").expect("the measured depth-1 cell is present");
        assert_eq!(measured.mails_per_sec, Some(123_456.0));
    }

    #[test]
    fn from_cells_real_tier_emits_keepup_not_latency() {
        // The real tier reports keep-up, not per-hop spans
        // (iamacoffeepot/aether#1233): a real cell carrying a harvested
        // `keepup` produces a `keepup.real` section, and no `latency.real`.
        use crate::perf::harness::{CellResult, KeepUp, Stats, Tier};

        let cell = CellResult {
            workers: 4,
            topo: "socket-server-32".to_owned(),
            tier: Tier::Real,
            boot_handoff_nanos: 0,
            construct: Stats::default(),
            queued: Stats::default(),
            drain: Stats::default(),
            handler: Stats::default(),
            depth: Stats::default(),
            throughput_mps: None,
            keepup: Some(KeepUp {
                offered: 6400,
                completed: 6400,
                elapsed_nanos: 110_000_000,
                expected_nanos: 100_000_000,
            }),
        };
        let report = TrialReport::from_cells(&[cell], 200, Some(60), None);
        let names: Vec<&str> = report.sections.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec![KeepUpSection::NAME], "the real tier emits the keep-up section, not latency.real");
        let sec = &report.sections[0];
        assert_eq!(sec.version, KeepUpSection::VERSION);
        let ku: KeepUpSection = serde_json::from_value(sec.body.clone()).expect("decode keepup body");
        assert_eq!(ku.cells.len(), 1);
        assert_eq!(ku.cells[0].offered, 6400);
        assert_eq!(ku.cells[0].completed, 6400);
    }
}
