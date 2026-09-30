//! The bloomery steps section's markdown: the lower-is-better per-step verdict
//! grid, in microseconds.

use super::super::bloomery_steps::StepComparison;
use super::super::comparison::Verdict;
use super::{paired_delta_us, push_section_tables, us, verdict_label};

/// Render the `bloomery.steps` section: non-stable rows up top, full grid
/// collapsed. No plot anchor — `perf-plot` renders dispatch latency cells only.
pub(super) fn push_steps_section(s: &mut String, name: &str, cells: &[StepComparison]) {
    let header = "| shape | sessions | step | pct | base µs | this µs | paired Δ µs | verdict |\n\
         |---|--:|---|---|--:|--:|--:|---|\n";
    let row = |c: &StepComparison| -> String {
        format!(
            "| {} | {} | {} | {} | {} ±{} | {} ±{} | {} | {} |\n",
            c.shape,
            c.sessions,
            c.step,
            c.percentile,
            us(c.base_median),
            us(c.base_iqr),
            us(c.cand_median),
            us(c.cand_iqr),
            paired_delta_us(c.delta_median, c.delta_pct),
            verdict_label(c.verdict),
        )
    };

    let all: Vec<String> = cells.iter().map(&row).collect();
    let non_stable: Vec<String> = cells.iter().filter(|c| c.verdict != Verdict::Stable).map(&row).collect();
    push_section_tables(s, name, header, &non_stable, &all);
}
