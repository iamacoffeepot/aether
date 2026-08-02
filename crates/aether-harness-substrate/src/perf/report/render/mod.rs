//! The sticky-comment surface downstream tooling parses: [`markdown`] renders
//! a [`ComparisonReport`] into the PR-comment body, [`headline_counts`] /
//! [`bistable_count`] roll the verdict-carrying sections into the gate signal,
//! and each section's own grid is rendered by the sibling module named after
//! it. The shared table scaffolding and the number formatting live here so a
//! section module only decides its columns.

mod keep_up;
mod latency;
mod throughput;

use super::compare::{ComparisonReport, SectionReport, Verdict};
use super::latency::latency_section_renders_verdict;
use keep_up::push_keepup_section;
use latency::push_latency_section;
use throughput::push_throughput_section;

pub use latency::PLOT_ANCHOR_PREFIX;

pub(super) fn us(ns: f64) -> String {
    format!("{:.2}", ns / 1000.0)
}

/// A cell's paired delta, rendered so it cannot be mistaken for the
/// difference of the two medians beside it (iamacoffeepot/aether#4182).
///
/// `delta_pct` is `median(per-trial deltas) / median(base values)` — the
/// paired statistic ADR-0085 §3 is built on, and deliberately more sensitive
/// than comparing marginals. But printed as a bare percentage next to a base
/// and a candidate median it invites the wrong arithmetic: a cell reading
/// `2.19 → 2.22` carried `+20%`, and the two disagree because they measure
/// different things, not because either is wrong. Leading with the delta
/// median in µs makes the paired quantity the one being read.
pub(super) fn paired_delta_us(delta_median_nanos: f64, delta_pct: f64) -> String {
    format!("{:+.2} ({delta_pct:+.0}%)", delta_median_nanos / 1000.0)
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
    report.sections.iter().fold((0, 0, 0), |(i, s, r), sec| match sec {
        SectionReport::Compared { name, improved, stable, regressed, .. } if latency_section_renders_verdict(name) => {
            (i + improved, s + stable, r + regressed)
        }
        SectionReport::ThroughputCompared { improved, stable, regressed, .. } => {
            (i + improved, s + stable, r + regressed)
        }
        // A non-light latency section is compared (it carries counts)
        // but its verdict is suppressed — it must not reach the headline.
        // A keep-up section carries no verdict at all (characterisation),
        // so it never contributes to the gate signal either.
        SectionReport::Compared { .. } | SectionReport::KeepUpCompared { .. } | SectionReport::Uncompared { .. } => {
            (i, s, r)
        }
    })
}

/// Cells whose mode flipped between trials, over the same verdict-carrying
/// sections [`headline_counts`] sums (iamacoffeepot/aether#4265).
///
/// Reported beside the headline rather than inside it: a bistable cell is not
/// a regression and must not move the gate signal, but it is also not a stable
/// result, and letting it vanish from the rollup is how a mode-selection
/// difference gets read as a clean run.
#[must_use]
pub fn bistable_count(report: &ComparisonReport) -> usize {
    report
        .sections
        .iter()
        .filter_map(|sec| match sec {
            SectionReport::Compared { name, cells, .. } if latency_section_renders_verdict(name) => Some(cells),
            _ => None,
        })
        .flatten()
        .filter(|c| c.verdict == Verdict::Bistable)
        .count()
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
    let bistable = bistable_count(report);
    let bistable_note = if bistable > 0 {
        format!(" · **{bistable} bistable**")
    } else {
        String::new()
    };
    s.push_str(&format!(
        "**{improved} improved · {stable} stable · {regressed} regressed**{bistable_note} ({} trials/config, paired)\n\n",
        report.trials
    ));
    if bistable > 0 {
        s.push_str(
            "_A bistable cell changed mode between trials on one side, so it has no single value to compare \
             (iamacoffeepot/aether#4265). Its delta is withheld rather than reported — treat it as unmeasured, \
             not as stable._\n\n",
        );
    }

    for sec in &report.sections {
        match sec {
            SectionReport::Compared { name, cells, .. } => {
                push_latency_section(&mut s, name, cells);
            }
            SectionReport::ThroughputCompared { name, cells, .. } => {
                push_throughput_section(&mut s, name, cells);
            }
            SectionReport::KeepUpCompared { name, cells } => {
                push_keepup_section(&mut s, name, cells);
            }
            SectionReport::Uncompared { name, .. } => {
                s.push_str(&format!("_{name}: new this run — no baseline to compare_\n\n"));
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
pub(super) fn push_section_tables(s: &mut String, name: &str, header: &str, non_stable: &[String], all: &[String]) {
    if non_stable.is_empty() {
        s.push_str(&format!("_{name}: no cells moved beyond the noise band._\n\n"));
    } else {
        s.push_str(header);
        for r in non_stable {
            s.push_str(r);
        }
        s.push('\n');
    }

    s.push_str(&format!("<details><summary>{name} full grid — {} cells</summary>\n\n", all.len()));
    s.push_str(header);
    for r in all {
        s.push_str(r);
    }
    s.push_str("\n</details>\n\n");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::perf::report::fixture::*;
    use crate::perf::report::*;

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
        assert_eq!((improved, regressed), (0, 0), "the heavy tier's verdict must not reach the gate-signal headline");
    }
}
