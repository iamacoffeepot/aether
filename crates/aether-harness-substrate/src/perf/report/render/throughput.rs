//! The throughput section's markdown: the higher-is-better verdict grid, in
//! thousands of mails/sec.

use super::super::comparison::Verdict;
use super::super::throughput::ThroughputComparison;
use super::push_section_tables;

/// [`paired_delta_us`](super::paired_delta_us)'s throughput sibling — same reasoning, thousands of
/// mails/sec, matching [`kps`]'s precision.
fn paired_delta_kps(delta_median_mps: f64, delta_pct: f64) -> String {
    format!("{:+.1} ({delta_pct:+.0}%)", delta_median_mps / 1000.0)
}

/// Render the throughput section (iamacoffeepot/aether#1202) — the
/// higher-is-better mails/sec analog of [`push_latency_section`](super::latency::push_latency_section):
/// non-stable rows up top, full grid collapsed, rates in thousands of
/// mails/sec.
#[allow(clippy::format_push_string)]
pub(super) fn push_throughput_section(s: &mut String, name: &str, cells: &[ThroughputComparison]) {
    let header = "| topology | w | base k/s | this k/s | paired Δ k/s | verdict |\n|---|--:|--:|--:|--:|---|\n";
    let row = |c: &ThroughputComparison| -> String {
        let verdict = match c.verdict {
            Verdict::Improved => "improved",
            Verdict::Stable => "stable",
            Verdict::Regressed => "regressed",
            Verdict::Bistable => "bistable",
        };
        format!(
            "| {} | {} | {} ±{} | {} ±{} | {} | {} |\n",
            c.topo,
            c.workers,
            kps(c.base_median),
            kps(c.base_iqr),
            kps(c.cand_median),
            kps(c.cand_iqr),
            paired_delta_kps(c.delta_median, c.delta_pct),
            verdict,
        )
    };

    let all: Vec<String> = cells.iter().map(&row).collect();
    let non_stable: Vec<String> = cells.iter().filter(|c| c.verdict != Verdict::Stable).map(&row).collect();
    push_section_tables(s, name, header, &non_stable, &all);
}

/// Format a mails/sec rate in thousands (k/s), mirroring [`us`](super::us)'s
/// scale-and-fixed-precision rendering for the latency table.
fn kps(mps: f64) -> String {
    format!("{:.1}", mps / 1000.0)
}

#[cfg(test)]
mod tests {
    use crate::perf::report::fixture::*;
    use crate::perf::report::*;

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
}
