//! The section dispatcher and the verdict vocabulary every section compare
//! shares: [`compare`] pairs base against candidate section by section and
//! routes each to its typed compare, [`classify`] turns one cell's paired
//! deltas into a [`Verdict`], and [`SectionReport`] / [`ComparisonReport`]
//! carry the result.

use serde::{Deserialize, Serialize};

use super::keep_up::{KeepUpComparison, KeepUpSection, compare_keepup, decode_keepup_cells};
use super::latency::{CellComparison, compare_latency, decode_latency_cells, is_latency_section};
use super::throughput::{ThroughputComparison, ThroughputSection, compare_throughput, decode_throughput_cells};
use super::trial::TrialReport;

/// Which direction of paired delta is the win (iamacoffeepot/aether#1202).
/// Latency is lower-is-better (a negative delta improves); throughput is
/// higher-is-better (a positive delta improves). The only verdict knob
/// that differs between the two sections.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Direction {
    LowerIsBetter,
    HigherIsBetter,
}

/// improved / stable / regressed verdict for one (cell × percentile).
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Improved,
    Stable,
    Regressed,
    /// The cell changed *mode* between trials on one side or the other, so it
    /// has no single value to compare (iamacoffeepot/aether#4265).
    ///
    /// #4177 established that a cell can boot into a fast or a slow mode via
    /// process history rather than through anything in the code under test.
    /// Comparing a side that was slow in 9 of 12 trials against one that was
    /// slow in 3 produces a confident delta for a quantity with two values —
    /// which is how a mode-selection difference reads as a regression. Saying
    /// the cell is bistable is the honest output; a delta is not available.
    Bistable,
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
    /// The real tier's keep-up section (iamacoffeepot/aether#1233):
    /// characterisation, so no improved/stable/regressed counts — the cells
    /// render as a base-vs-candidate trend with no verdict.
    KeepUpCompared {
        name: String,
        cells: Vec<KeepUpComparison>,
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
    pub abs_floor_nanos: f64,
    /// Fraction of trials whose delta must share the effect's sign.
    pub consistency: f64,
}

impl Default for CompareConfig {
    fn default() -> Self {
        Self { effect_floor_iqr: 1.5, rel_floor: 0.10, abs_floor_nanos: 300.0, consistency: 0.75 }
    }
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
                sections.push(SectionReport::Uncompared { name: name.clone(), reason: UncomparedReason::NewThisRun });
                continue;
            }
            (Some(_), None) => {
                sections.push(SectionReport::Uncompared { name: name.clone(), reason: UncomparedReason::OnlyBase });
                continue;
            }
            (None, None) => continue,
        };
        if bsec.version != csec.version {
            sections.push(SectionReport::Uncompared {
                name: name.clone(),
                reason: UncomparedReason::VersionChanged { base: bsec.version.clone(), cand: csec.version.clone() },
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
        } else if name == KeepUpSection::NAME {
            let base_cells = decode_keepup_cells(&base[..k]);
            let cand_cells = decode_keepup_cells(&cand[..k]);
            sections.push(compare_keepup(name, &base_cells, &cand_cells, k));
        } else {
            sections.push(SectionReport::Uncompared { name: name.clone(), reason: UncomparedReason::UnknownName });
        }
    }

    ComparisonReport { trials: k, sections }
}

#[allow(clippy::cast_precision_loss)]
pub(super) fn classify(
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
    let same_sign = deltas.iter().filter(|&&d| d != 0.0 && d.signum() == delta_median.signum()).count() as f64;
    let consistent = same_sign / n >= cfg.consistency;

    // The absolute floor (`abs_floor_nanos`) is a *nanosecond* resolution
    // floor — meaningful for a latency span, meaningless for a mails/sec
    // rate (iamacoffeepot/aether#1202). For a higher-is-better rate the
    // verdict rests on the IQR + relative floors only; the ns floor is
    // neutralised to zero.
    let abs_floor = match dir {
        Direction::LowerIsBetter => cfg.abs_floor_nanos,
        Direction::HigherIsBetter => 0.0,
    };
    let floor = (cfg.effect_floor_iqr * delta_iqr).max(cfg.rel_floor * base_median).max(abs_floor);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::perf::report::fixture::*;
    use crate::perf::report::*;

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
                assert_eq!(*reason, UncomparedReason::VersionChanged { base: "v1".to_owned(), cand: "v2".to_owned() });
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
}
