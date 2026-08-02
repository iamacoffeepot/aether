//! Trial builders shared by the section modules' tests. Each section's own
//! assertions live beside the code they exercise; the fixtures that assemble a
//! K-trial side are common to several of them, so they sit here rather than
//! being rebuilt per module.

use super::comparison::{ComparisonReport, SectionReport, Verdict};
use super::keep_up::{KeepUpCell, KeepUpSection};
use super::latency::LatencySection;
use super::metric::{CellJson, Metric};
use super::throughput::{ThroughputCell, ThroughputSection};
use super::trial::{RawSection, TRIAL_SCHEMA, TrialReport};

/// One drain cell at `topo @ 11w` with the given `p50` (p90 / p99 / max
/// derived ×1.2 / ×1.5 / ×4 so the cell is well-formed; tests assert on
/// p50). Shared by the `fanout-8` (light) and `fanout-8-heavy` fixtures so
/// the derive lives in one place (`DuplicatedCode` guard).
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub(super) fn cell_json(topo: &str, p50: u64) -> CellJson {
    CellJson {
        workers: 11,
        topo: topo.to_owned(),
        metric: Metric::Drain,
        p50,
        p90: (p50 as f64 * 1.2) as u64,
        p99: (p50 as f64 * 1.5) as u64,
        max: p50 * 4,
        n: 1800,
        tail_mass: 0.0,
    }
}

/// Build a single-section [`TrialReport`] envelope (`DuplicatedCode` guard).
/// Every fixture trial here carries exactly one section; the envelope
/// fields (`schema` / `git_sha` / `pace_hz` / `frames`) are identical
/// across them, so factoring this stops the `TrialReport { .. }` block
/// from repeating in each builder.
pub(super) fn single_section_trial(name: &str, version: &str, body: serde_json::Value) -> TrialReport {
    TrialReport {
        schema: TRIAL_SCHEMA.to_owned(),
        git_sha: None,
        pace_hz: None,
        frames: 200,
        sections: vec![RawSection { name: name.to_owned(), version: version.to_owned(), body }],
    }
}

/// Build a K-trial side from a per-trial `p50` series for one cell
/// (`fanout-8 @ 11w`, drain). The cell rides in a single `latency` section
/// (iamacoffeepot/aether#1206).
pub(super) fn side(p50s: &[u64]) -> Vec<TrialReport> {
    p50s.iter()
        .map(|&p50| {
            let cells = vec![cell_json("fanout-8", p50)];
            let body = serde_json::to_value(LatencySection { cells }).expect("encode latency body");
            single_section_trial(LatencySection::NAME, LatencySection::VERSION, body)
        })
        .collect()
}

/// Pull the compared `latency` section out of a comparison report.
pub(super) fn latency_section(rep: &ComparisonReport) -> &SectionReport {
    rep.sections
        .iter()
        .find(|s| matches!(s, SectionReport::Compared { name, .. } if name == LatencySection::NAME))
        .expect("compared latency section present")
}

pub(super) fn p50_verdict(rep: &ComparisonReport) -> Verdict {
    let SectionReport::Compared { cells, .. } = latency_section(rep) else {
        panic!("latency section not compared");
    };
    cells.iter().find(|c| c.percentile == "p50").expect("p50 cell present").verdict
}

/// Build a K-trial side whose cell carries a per-trial `tail_mass`, with a
/// fixed `p50` — so only the mode varies and the paired delta is otherwise
/// perfectly stable.
pub(super) fn side_with_tails(tails: &[f64]) -> Vec<TrialReport> {
    tails
        .iter()
        .map(|&tail_mass| {
            let cells = vec![CellJson { tail_mass, ..cell_json("fanout-8", 1_500) }];
            let body = serde_json::to_value(LatencySection { cells }).expect("encode latency body");
            single_section_trial(LatencySection::NAME, LatencySection::VERSION, body)
        })
        .collect()
}

/// Attach an extra raw section to every trial in a side.
pub(super) fn with_extra_section(mut side: Vec<TrialReport>, name: &str, version: &str) -> Vec<TrialReport> {
    for t in &mut side {
        t.sections.push(RawSection {
            name: name.to_owned(),
            version: version.to_owned(),
            body: serde_json::json!({"opaque": true}),
        });
    }
    side
}

/// Build a K-trial side carrying a single `throughput` section cell
/// (`fanout-8 @ 11w`) whose rate follows `rates` (mails/sec). The
/// throughput analog of [`side`] (iamacoffeepot/aether#1202).
pub(super) fn throughput_side(rates: &[f64]) -> Vec<TrialReport> {
    rates
        .iter()
        .map(|&mails_per_sec| {
            let cells =
                vec![ThroughputCell { workers: 11, topo: "fanout-8".to_owned(), mails_per_sec: Some(mails_per_sec) }];
            let body = serde_json::to_value(ThroughputSection { cells }).expect("encode throughput body");
            single_section_trial(ThroughputSection::NAME, ThroughputSection::VERSION, body)
        })
        .collect()
}

/// The single throughput cell's verdict in a comparison report.
pub(super) fn throughput_verdict(rep: &ComparisonReport) -> Verdict {
    rep.sections
        .iter()
        .find_map(|s| match s {
            SectionReport::ThroughputCompared { cells, .. } => cells.first().map(|c| c.verdict),
            _ => None,
        })
        .expect("compared throughput cell present")
}

/// Build a K-trial side carrying a single latency cell under the named
/// tier section (ADR-0085 amendment) — the tier analog of [`side`]. The
/// section name selects the tier (`latency` light, `latency.heavy`,
/// `latency.real`); the cell topology is `fanout-8-heavy` throughout.
pub(super) fn tier_side(section_name: &str, p50s: &[u64]) -> Vec<TrialReport> {
    p50s.iter()
        .map(|&p50| {
            let cells = vec![cell_json("fanout-8-heavy", p50)];
            let body = serde_json::to_value(LatencySection { cells }).expect("encode tier latency body");
            single_section_trial(section_name, LatencySection::VERSION, body)
        })
        .collect()
}

/// Attach a `latency.heavy` section's cells to an existing side, so a
/// trial carries both the light `latency` section and the heavy one (the
/// realistic `AETHER_PERF_TIER=light,heavy` shape).
pub(super) fn with_heavy_section(mut side: Vec<TrialReport>, p50s: &[u64]) -> Vec<TrialReport> {
    for (t, &p50) in side.iter_mut().zip(p50s.iter()) {
        let cells = vec![cell_json("fanout-8-heavy", p50)];
        let body = serde_json::to_value(LatencySection { cells }).expect("encode heavy latency body");
        t.sections.push(RawSection {
            name: "latency.heavy".to_owned(),
            version: LatencySection::VERSION.to_owned(),
            body,
        });
    }
    side
}

/// Attach a `throughput` section (one `fanout-8 @ 11w` cell per trial,
/// rate from `rates`) to an existing side, so a trial carries both its
/// latency section(s) and the throughput one — the realistic saturate +
/// latency shape used by the anchor test.
pub(super) fn with_throughput(mut side: Vec<TrialReport>, rates: &[f64]) -> Vec<TrialReport> {
    for (t, &mails_per_sec) in side.iter_mut().zip(rates.iter()) {
        let cells =
            vec![ThroughputCell { workers: 11, topo: "fanout-8".to_owned(), mails_per_sec: Some(mails_per_sec) }];
        let body = serde_json::to_value(ThroughputSection { cells }).expect("encode throughput body");
        t.sections.push(RawSection {
            name: ThroughputSection::NAME.to_owned(),
            version: ThroughputSection::VERSION.to_owned(),
            body,
        });
    }
    side
}

/// Build a K-trial side carrying a single `keepup.real` cell
/// (`socket-server-32 @ 11w`, offered == completed) whose paced elapsed
/// follows `elapsed_nanos` against a fixed 100ms budget — the keep-up
/// analog of [`side`] (iamacoffeepot/aether#1233).
pub(super) fn keepup_side(elapsed_nanos: &[u64]) -> Vec<TrialReport> {
    elapsed_nanos
        .iter()
        .map(|&elapsed_nanos| {
            let cells = vec![KeepUpCell {
                workers: 11,
                topo: "socket-server-32".to_owned(),
                offered: 6400,
                completed: 6400,
                elapsed_nanos,
                expected_nanos: 100_000_000,
            }];
            let body = serde_json::to_value(KeepUpSection { cells }).expect("encode keepup body");
            single_section_trial(KeepUpSection::NAME, KeepUpSection::VERSION, body)
        })
        .collect()
}
