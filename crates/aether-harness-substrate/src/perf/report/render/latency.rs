//! The latency section's markdown: the light tier's verdict grid, the heavy /
//! real tiers' no-verdict trend grid, and the per-section plot anchor the plot
//! publisher find-replaces.

use super::super::comparison::Verdict;
use super::super::latency::{CellComparison, latency_section_renders_verdict};
use super::{paired_delta_us, push_section_tables, us};

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
pub(super) fn push_latency_section(s: &mut String, name: &str, cells: &[CellComparison]) {
    if latency_section_renders_verdict(name) {
        push_latency_verdict_section(s, name, cells);
    } else {
        push_latency_trend_section(s, name, cells);
    }
    push_plot_anchor(s, name);
}

/// Emit the per-section plot anchor (iamacoffeepot/aether#1228): an HTML
/// comment `perf-publish-plots.sh` find-replaces with this section's
/// candidate span-distribution plots (grouped by the matching `{tier}__`
/// filename prefix). Only latency sections get plots — `perf-plot` renders
/// one PNG per latency cell and nothing for the throughput section — so this
/// is the only place an anchor is emitted. The marker carries the section
/// name verbatim (`latency` / `latency.heavy` / `latency.real`) so the script
/// matches a plot's tier prefix to its anchor exactly.
#[allow(clippy::format_push_string)]
fn push_plot_anchor(s: &mut String, name: &str) {
    s.push_str(&format!("<!-- aether-perf-plots: {name} -->\n\n"));
}

/// Marker prefix the plot publisher (iamacoffeepot/aether#1228) scans for to
/// co-locate each section's plots. One `<!-- aether-perf-plots: TIER -->`
/// comment is emitted after each latency section by `push_plot_anchor`.
pub const PLOT_ANCHOR_PREFIX: &str = "<!-- aether-perf-plots:";

#[allow(clippy::format_push_string)]
fn push_latency_verdict_section(s: &mut String, name: &str, cells: &[CellComparison]) {
    let header = "| topology | w | metric | pct | base µs | this µs | paired Δ µs | verdict |\n\
         |---|--:|---|---|--:|--:|--:|---|\n";
    let row = |c: &CellComparison| -> String {
        let verdict = match c.verdict {
            Verdict::Improved => "improved",
            Verdict::Stable => "stable",
            Verdict::Regressed => "regressed",
            Verdict::Bistable => "bistable",
        };
        format!(
            "| {} | {} | {} | {} | {} ±{} | {} ±{} | {} | {} |\n",
            c.topo,
            c.workers,
            c.metric.label(),
            c.percentile,
            us(c.base_median),
            us(c.base_iqr),
            us(c.cand_median),
            us(c.cand_iqr),
            paired_delta_us(c.delta_median, c.delta_pct),
            verdict,
        )
    };

    let all: Vec<String> = cells.iter().map(&row).collect();
    let non_stable: Vec<String> = cells.iter().filter(|c| c.verdict != Verdict::Stable).map(&row).collect();
    push_section_tables(s, name, header, &non_stable, &all);
}

/// The no-verdict trend grid for a heavy / real latency section: one table,
/// every cell, no verdict column, base/this/Δ only — characterisation, not
/// classification (ADR-0085 amendment).
#[allow(clippy::format_push_string)]
fn push_latency_trend_section(s: &mut String, name: &str, cells: &[CellComparison]) {
    let header = "| topology | w | metric | pct | base µs | this µs | paired Δ µs |\n\
         |---|--:|---|---|--:|--:|--:|\n";
    // A plain trend label — cell count + an explicit "no verdict"
    // (iamacoffeepot/aether#1228). The improved/stable/regressed tally is
    // reserved for the verdict-carrying sections (light latency + throughput);
    // a no-verdict tier showing one would read as a misleading gate signal.
    s.push_str(&format!("<details><summary>{name} — {} cells, trend (no verdict)</summary>\n\n", cells.len()));
    s.push_str(header);
    for c in cells {
        s.push_str(&format!(
            "| {} | {} | {} | {} | {} ±{} | {} ±{} | {} |\n",
            c.topo,
            c.workers,
            c.metric.label(),
            c.percentile,
            us(c.base_median),
            us(c.base_iqr),
            us(c.cand_median),
            us(c.cand_iqr),
            paired_delta_us(c.delta_median, c.delta_pct),
        ));
    }
    s.push_str("\n</details>\n\n");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::perf::report::fixture::*;
    use crate::perf::report::*;

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
        let SectionReport::Compared { improved, cells, .. } = heavy else {
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
            md.contains("latency.heavy — 3 cells, trend (no verdict)"),
            "heavy section renders as a plain no-verdict trend grid"
        );
        assert!(
            !md.contains("latency.heavy: no cells moved beyond the noise band"),
            "the noise-band note is suppressed for a no-verdict tier"
        );
        // The trend grid's header omits the verdict column the light table has.
        assert!(md.contains("| topology | w | metric | pct | base µs | this µs | paired Δ µs |\n"));
    }

    #[test]
    fn latency_sections_emit_plot_anchors_throughput_does_not() {
        // iamacoffeepot/aether#1228: each latency section (any tier) emits a
        // per-section plot anchor `perf-publish-plots.sh` find-replaces; the
        // throughput section emits none (perf-plot renders no throughput PNGs).
        // Build a report carrying light + heavy latency sections *and* a
        // throughput section so all three render in one body.
        let light_base =
            with_heavy_section(side(&[167_000, 165_000, 169_000, 166_000]), &[167_000, 165_000, 169_000, 166_000]);
        let light_cand = with_heavy_section(side(&[33_000, 34_000, 32_000, 33_500]), &[33_000, 34_000, 32_000, 33_500]);
        // Splice a throughput section onto each side.
        let base = with_throughput(light_base, &[100_000.0, 98_000.0, 102_000.0, 99_000.0]);
        let cand = with_throughput(light_cand, &[200_000.0, 198_000.0, 202_000.0, 199_000.0]);
        let rep = compare(&base, &cand, CompareConfig::default());
        let md = markdown(&rep, "PR 9999 vs main", "test");

        assert!(md.contains("<!-- aether-perf-plots: latency -->"), "light latency section emits its plot anchor");
        assert!(
            md.contains("<!-- aether-perf-plots: latency.heavy -->"),
            "heavy latency section emits its plot anchor"
        );
        // The throughput section renders a verdict table but no plot anchor.
        assert!(md.contains("| topology | w | base k/s |"), "throughput table is present");
        assert!(
            !md.contains("<!-- aether-perf-plots: throughput -->"),
            "throughput section emits no plot anchor (perf-plot is latency-only)"
        );
    }

    #[test]
    fn trend_section_carries_no_verdict_tally() {
        // iamacoffeepot/aether#1228 secondary: a non-light latency section's
        // summary is a plain trend label (cell count + "no verdict"), never an
        // improved/stable/regressed tally — the tally would read as a gate
        // signal for a section that carries none.
        let base = tier_side("latency.heavy", &[167_000, 165_000, 169_000, 166_000]);
        let cand = tier_side("latency.heavy", &[33_000, 34_000, 32_000, 33_500]);
        let rep = compare(&base, &cand, CompareConfig::default());
        let md = markdown(&rep, "PR 9999 vs main", "test");
        assert!(
            md.contains("latency.heavy — 3 cells, trend (no verdict)"),
            "the trend summary is a plain cell-count + no-verdict label"
        );
        assert!(
            !md.contains("latency.heavy: improved") && !md.contains("latency.heavy improved"),
            "no verdict tally leaks into the trend section header"
        );
    }
}
