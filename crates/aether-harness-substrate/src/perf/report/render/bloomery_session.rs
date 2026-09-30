//! The bloomery session section's markdown: the higher-is-better sessions/sec
//! verdict beside the journal-growth and peak-memory trend.

use super::super::bloomery_session::SessionComparison;
use super::super::comparison::Verdict;
use super::{push_section_tables, verdict_label};

/// Bytes per mebibyte, for the peak-memory column.
const MEBIBYTE: f64 = 1024.0 * 1024.0;

/// Render the `bloomery.session` section: rows whose rate moved up top, full
/// grid collapsed. A row whose rate went unmeasured on either side prints
/// `unmeasured` in its rate columns and still shows its journal and memory
/// trend; a side with no memory reading prints `—`.
pub(super) fn push_session_section(s: &mut String, name: &str, cells: &[SessionComparison]) {
    let header = "| shape | sessions | base /s | this /s | paired Δ /s | verdict | journal B/session | peak RSS MiB |\n\
         |---|--:|--:|--:|--:|---|--:|--:|\n";
    let rss = |bytes: Option<f64>| bytes.map_or_else(|| "—".to_owned(), |b| format!("{:.1}", b / MEBIBYTE));
    let row = |c: &SessionComparison| -> String {
        let rate = c.rate.as_ref().map_or_else(
            || "unmeasured | unmeasured | unmeasured | unmeasured".to_owned(),
            |r| {
                format!(
                    "{:.2} ±{:.2} | {:.2} ±{:.2} | {:+.2} ({:+.0}%) | {}",
                    r.base_median,
                    r.base_iqr,
                    r.cand_median,
                    r.cand_iqr,
                    r.delta_median,
                    r.delta_pct,
                    verdict_label(r.verdict),
                )
            },
        );
        format!(
            "| {} | {} | {rate} | {:.0}→{:.0} | {}→{} |\n",
            c.shape,
            c.sessions,
            c.base_journal_bytes,
            c.cand_journal_bytes,
            rss(c.base_peak_rss_bytes),
            rss(c.cand_peak_rss_bytes),
        )
    };

    let moved = |c: &&SessionComparison| c.rate.as_ref().is_some_and(|r| r.verdict != Verdict::Stable);
    let all: Vec<String> = cells.iter().map(&row).collect();
    let non_stable: Vec<String> = cells.iter().filter(moved).map(&row).collect();
    push_section_tables(s, name, header, &non_stable, &all);
}

#[cfg(test)]
mod tests {
    use crate::perf::report::fixture::*;
    use crate::perf::report::*;

    #[test]
    fn an_unmeasured_rate_renders_unmeasured_with_its_trend() {
        let base = session_side(&[Some(50.0), Some(49.0), Some(51.0), Some(50.5)]);
        let cand = session_side(&[Some(100.0), None, Some(101.0), Some(100.5)]);
        let rep = compare(&base, &cand, CompareConfig::default());
        let md = markdown(&rep, "PR 9999 vs main", "test");
        assert!(
            md.contains("| small | 4 | unmeasured | unmeasured | unmeasured | unmeasured | 8192→8192 | 64.0→64.0 |"),
            "the unmeasured row renders with its journal and memory trend:\n{md}"
        );
    }

    #[test]
    fn a_bloomery_verdict_reaches_the_headline() {
        let base = session_side(&[Some(50.0), Some(49.0), Some(51.0), Some(50.5), Some(49.5), Some(50.0)]);
        let cand = session_side(&[Some(100.0), Some(99.0), Some(101.0), Some(100.5), Some(99.5), Some(100.0)]);
        let rep = compare(&base, &cand, CompareConfig::default());
        let (improved, _stable, regressed) = headline_counts(&rep);
        assert_eq!((improved, regressed), (1, 0), "the session rate verdict is counted in the headline");
    }
}
