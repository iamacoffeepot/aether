//! The keep-up section's markdown: a no-verdict trend grid of offered /
//! completed counts and the pace ratio.

use super::super::keep_up::KeepUpComparison;

/// Render the real tier's keep-up section (iamacoffeepot/aether#1233) — a
/// no-verdict trend grid (like the heavy / real latency trend): offered /
/// completed mail counts and the pace ratio (`elapsed / expected`,
/// `× > 1` = fell behind the 60 Hz budget), base→candidate, no pass/fail. Emits
/// no plot anchor — `perf-plot` renders span PNGs, which a keep-up cell has
/// none of.
#[allow(clippy::format_push_string)]
pub(super) fn push_keepup_section(s: &mut String, name: &str, cells: &[KeepUpComparison]) {
    s.push_str(&format!("<details><summary>{name} — {} cells, keep-up (no verdict)</summary>\n\n", cells.len()));
    s.push_str("| topology | w | offered | completed | base pace | this pace |\n|---|--:|--:|--:|--:|--:|\n");
    for c in cells {
        s.push_str(&format!(
            "| {} | {} | {:.0}→{:.0} | {:.0}→{:.0} | {:.2}× | {:.2}× |\n",
            c.topo,
            c.workers,
            c.base_offered,
            c.cand_offered,
            c.base_completed,
            c.cand_completed,
            c.base_pace_ratio,
            c.cand_pace_ratio,
        ));
    }
    s.push_str("\n</details>\n\n");
}
