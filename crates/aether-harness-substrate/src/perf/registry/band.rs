//! K-trial replication of the registry sweep (iamacoffeepot/aether#4274).
//!
//! # Why the sweep needed replicating before anyone read it
//!
//! One run of [`super::run_registry_benchmark`] reported read throughput
//! *falling* as readers were added — sharpest between 1 and 2 — against
//! iamacoffeepot/aether#4264's reported 3.05x rise at 8 and ADR-0165's premise
//! that snapshot reads are "wait-free and approximately 30x faster at eight
//! reader threads". Two single runs disagreeing is not a result, and this arc
//! has been burned repeatedly by treating one as if it were: a sweep-position
//! artifact and a bistable cell both survived a single-run reading before.
//!
//! ADR-0085 already settled how to make a perf number trustworthy, for the
//! dispatch lane, and iamacoffeepot/aether#4176 asked for the same treatment
//! here rather than a second statistical method. So this replicates the whole
//! sweep K times, each in a fresh process (§1), and reduces each cell to the
//! median of its per-trial values with an IQR band (§2) — [`BandStats`].
//!
//! # What the band can and cannot say
//!
//! It carries no verdict. ADR-0085 §4 keeps the whole comparison informational,
//! and its amendment reserves per-cell classification for the low-variance
//! `light` tier; throughput characterisation is not that tier. What replication
//! does buy is a factual statement about the *interval*: whether a reader
//! count's scaling band sits wholly under 1.00x, wholly over, or straddles it
//! ([`BandPosition`]). "Every one of K trials put 8 readers below the
//! single-reader baseline" is a claim a single run cannot make, and it is the
//! one this sweep exists to settle.
//!
//! # Trial-per-process, not cell-per-process
//!
//! The dispatch sweep isolates each *cell* (iamacoffeepot/aether#4177) because
//! a cell there inherits the process state of the cells before it, and that
//! inheritance decides which of two execution modes it lands in. This sweep's
//! cells share something they cannot be separated from: one booted chassis
//! holding one populated route table, which *is* the fixture the readers walk.
//! Splitting them would re-populate that table per cell — a different, more
//! expensive benchmark, buying protection against an ordering effect that was
//! already excluded directly, by running the sweep in reverse and finding the
//! single-thread cell reads the same last as first.
//!
//! The handoff-cost probe is deliberately *not* seeded across trials, which is
//! the opposite of what [`super::super::isolate`] does for its cells. There the
//! seed keeps one trial's cells comparable to each other; here each trial is a
//! whole run, and its own probe is part of the between-run condition variance
//! the band is measuring. Pinning it would narrow the band by hiding a real
//! source of run-to-run spread (§1).

use std::env;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::RegistryReport;
use crate::perf::stats::{BandPosition, BandStats};
use crate::perf::subprocess::run_child_json;

/// Set on a child to make it run exactly one trial and print its
/// [`RegistryReport`] as JSON. Carries the trial index, which the child uses
/// only to label its diagnostics — every trial measures the same thing.
pub const TRIAL_ENV: &str = "AETHER_PERF_REGISTRY_TRIAL";

/// Set to `0` to run every trial in this process instead of re-exec'ing one
/// child each. Trials then share process history, which is exactly what
/// ADR-0085 §1 replicates in fresh processes to avoid — so this is for a
/// debugger or a profiler, not for a measurement anyone quotes.
pub const ISOLATION_ENV: &str = "AETHER_PERF_REGISTRY_ISOLATION";

/// Trials run when none is asked for. Odd, so the median is an observed trial
/// rather than an interpolation between two, and modest enough that the whole
/// replication stays interactive — ADR-0085 §1 leans on a modest K being
/// sufficient once the noise sources are folded in.
pub const DEFAULT_TRIALS: usize = 9;

/// The schema tag of a banded report, distinct from the single-sweep
/// `aether.perf.registry.v2` a child emits — a consumer must not mistake a
/// replication for one run of it.
pub const BAND_SCHEMA: &str = "aether.perf.registry.band.v1";

/// One reader-count cell, replicated.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadBandCell {
    pub threads: u64,
    pub owner_churn: bool,
    /// Readers plus chassis workers exceed the usable core count, so this cell
    /// measures the OS timeslicing threads. Carried per cell exactly as the
    /// single sweep carries it, because replication does not make an
    /// oversubscribed cell mean anything different.
    pub oversubscribed: bool,
    pub lookups_per_sec: BandStats,
    /// Throughput over **the same trial's** single-reader cell at the same
    /// churn setting, banded across trials.
    ///
    /// The ratio is formed inside each trial before the trials are pooled, so
    /// whatever a trial's own conditions did to its absolute rate — thermal
    /// state, a co-tenant, a handoff probe that landed high — divides out
    /// instead of widening the band. That is ADR-0085 §3's pairing applied
    /// within a run rather than across two configurations.
    pub scaling_vs_one: BandStats,
    /// Where the scaling band sits relative to flat (1.00x). `Below` on a cell
    /// with more than one reader means every quartile of the replication put
    /// added readers *behind* a single reader.
    pub scaling_position: BandPosition,
    /// Owner commits observed during the cell's window, banded. A churned cell
    /// whose band centres on zero did not measure republication in any trial.
    pub owner_commits_observed: BandStats,
}

/// One owner phase, replicated.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OwnerBand {
    /// `"sequential-spawn"` (the floor) or `"staged-burst"` (the ceiling).
    pub drive: String,
    pub items_per_sec_while_draining: BandStats,
    /// Largest single drain, banded — the amortization signal. A band centred
    /// on 1 says the drainer never batched and the rate above is a floor.
    pub drain_max: BandStats,
    /// Trials whose queue actually held more than one item. A rate pooled from
    /// a mix of queued and unqueued trials is two different measurements in one
    /// band, so the count is reported rather than a single boolean asserted.
    pub queued_under_load_trials: u64,
}

/// K trials of the registry sweep, reduced to per-cell bands.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryBandReport {
    pub schema: String,
    pub trials_requested: u64,
    /// Trials that produced a report. Below `trials_requested` when a child
    /// failed to boot a chassis or died; the bands are over what survived.
    pub trials_completed: u64,
    /// Whether each trial ran in its own process. False means the trials share
    /// process history and the band understates between-run variance.
    pub isolated: bool,
    pub usable_cores: u64,
    pub chassis_workers: u64,
    /// Route table size, from the first completed trial. Constant by
    /// construction across trials; a trial that populated fewer rows would have
    /// broken well before this.
    pub populated_mailboxes: u64,
    pub read_scaling: Vec<ReadBandCell>,
    pub owner_unloaded: OwnerBand,
    pub owner_loaded: Option<OwnerBand>,
    /// Every trial's raw report. Replication that discards its inputs cannot be
    /// re-examined when the band says something surprising, and this arc has
    /// twice needed to go back to the individual runs.
    pub trials: Vec<RegistryReport>,
}

/// Whether this process was re-exec'd to run one trial, and which one.
// Dev/perf tooling: the perf bins take their run parameters from env, and this
// is the child-mode selector among them — not a capability, no config layer.
#[allow(clippy::disallowed_methods)]
#[must_use]
pub fn selected_trial() -> Option<usize> {
    env::var(TRIAL_ENV).ok().filter(|v| !v.is_empty()).and_then(|v| v.trim().parse().ok())
}

/// Whether each trial should run in its own process — true unless
/// [`ISOLATION_ENV`] is exactly `0`.
// Dev/perf tooling: see `selected_trial`.
#[allow(clippy::disallowed_methods)]
#[must_use]
pub fn isolation_enabled() -> bool {
    env::var(ISOLATION_ENV).as_deref() != Ok("0")
}

/// Run `trials` replications of the registry sweep and band them.
///
/// Each trial is a fresh child process (ADR-0085 §1) unless isolation is
/// disabled or the executable cannot be resolved, in which case the trials run
/// here and the report says so. A trial that fails is reported on stderr and
/// dropped; the band is over the trials that survived, and
/// `trials_completed` is what makes that visible rather than implicit.
// The orchestrating process boots no chassis, and the chassis boot is what
// installs the tracing subscriber — so `tracing::warn!` here would go nowhere
// and a dropped trial would be silent. Its diagnostics go straight to the
// stderr its children already share.
#[allow(clippy::print_stderr)]
#[must_use]
pub fn run_banded_benchmark(trials: usize) -> RegistryBandReport {
    let exe = isolation_enabled().then(|| env::current_exe().ok()).flatten();
    if exe.is_none() && isolation_enabled() {
        eprintln!(
            "perf-registry: cannot resolve the current executable; running trials in this process — they will share process history (ADR-0085 §1)"
        );
    }

    let mut reports = Vec::with_capacity(trials);
    for trial in 0..trials {
        eprintln!("perf-registry: trial {}/{trials}", trial + 1);
        match &exe {
            Some(exe) => match run_trial_in_subprocess(exe, trial) {
                Ok(report) => reports.push(report),
                Err(e) => eprintln!("perf-registry: trial {} dropped — {e}", trial + 1),
            },
            None => match super::run_registry_benchmark() {
                Some(report) => reports.push(report),
                None => eprintln!("perf-registry: trial {} dropped — the chassis did not boot", trial + 1),
            },
        }
    }
    summarize(&reports, trials, exe.is_some())
}

fn run_trial_in_subprocess(exe: &Path, trial: usize) -> Result<RegistryReport, String> {
    run_child_json(exe, &[(TRIAL_ENV, trial.to_string())])
}

/// Reduce K raw sweeps to per-cell bands.
///
/// Cells pair across trials by `(threads, owner_churn)` rather than by
/// position, so a trial that dropped a cell contributes its remaining cells
/// instead of shifting every later one onto the wrong key. The key order comes
/// from the first completed trial, which is the order the sweep ran them in.
#[must_use]
pub fn summarize(reports: &[RegistryReport], requested: usize, isolated: bool) -> RegistryBandReport {
    let first = reports.first();
    let read_scaling = first.map(|first| band_read_cells(first, reports)).unwrap_or_default();

    RegistryBandReport {
        schema: BAND_SCHEMA.to_owned(),
        trials_requested: requested as u64,
        trials_completed: reports.len() as u64,
        isolated,
        usable_cores: first.map_or(0, |r| r.usable_cores),
        chassis_workers: first.map_or(0, |r| r.chassis_workers),
        populated_mailboxes: first.map_or(0, |r| r.populated_mailboxes),
        read_scaling,
        owner_unloaded: band_owner("sequential-spawn", reports, |r| Some(&r.owner_unloaded)),
        owner_loaded: reports
            .iter()
            .any(|r| r.owner_loaded.is_some())
            .then(|| band_owner("staged-burst", reports, |r| r.owner_loaded.as_ref())),
        trials: reports.to_vec(),
    }
}

#[allow(clippy::cast_precision_loss, reason = "a commit count is exact in f64 far past what a cell's window retires")]
fn band_read_cells(first: &RegistryReport, reports: &[RegistryReport]) -> Vec<ReadBandCell> {
    first
        .read_scaling
        .iter()
        .map(|key| {
            let matching: Vec<_> = reports
                .iter()
                .filter_map(|r| {
                    r.read_scaling.iter().find(|c| c.threads == key.threads && c.owner_churn == key.owner_churn)
                })
                .collect();
            let scaling = BandStats::of(
                &reports.iter().filter_map(|r| trial_scaling(r, key.threads, key.owner_churn)).collect::<Vec<_>>(),
            );
            ReadBandCell {
                threads: key.threads,
                owner_churn: key.owner_churn,
                oversubscribed: key.oversubscribed,
                lookups_per_sec: BandStats::of(&matching.iter().map(|c| c.lookups_per_sec).collect::<Vec<_>>()),
                scaling_position: scaling.position_against(1.0),
                scaling_vs_one: scaling,
                owner_commits_observed: BandStats::of(
                    &matching.iter().map(|c| c.owner_commits_observed as f64).collect::<Vec<_>>(),
                ),
            }
        })
        .collect()
}

/// One trial's own scaling ratio for a cell: its rate over *its* single-reader
/// rate at the same churn setting.
///
/// Formed here rather than read off `ReadScalingCell::scaling_vs_one` so the
/// single-reader cell itself reads 1.00x instead of the `None` the sweep leaves
/// on the row it uses as its baseline, and so the pairing is stated where the
/// band that depends on it is computed.
#[allow(clippy::cast_precision_loss, reason = "throughput is a trend ratio, exact in f64 at these magnitudes")]
fn trial_scaling(report: &RegistryReport, threads: u64, churn: bool) -> Option<f64> {
    let cell = report.read_scaling.iter().find(|c| c.threads == threads && c.owner_churn == churn)?;
    let one = report.read_scaling.iter().find(|c| c.threads == 1 && c.owner_churn == churn)?;
    (one.lookups_per_sec > 0.0).then(|| cell.lookups_per_sec / one.lookups_per_sec)
}

#[allow(clippy::cast_precision_loss, reason = "a drain count is exact in f64 far past what a burst retires")]
fn band_owner(
    drive: &str,
    reports: &[RegistryReport],
    pick: impl Fn(&RegistryReport) -> Option<&super::owner::OwnerCeiling>,
) -> OwnerBand {
    let samples: Vec<_> = reports.iter().filter_map(&pick).collect();
    OwnerBand {
        drive: drive.to_owned(),
        items_per_sec_while_draining: BandStats::of(
            &samples.iter().filter_map(|s| s.items_per_sec_while_draining).collect::<Vec<_>>(),
        ),
        drain_max: BandStats::of(&samples.iter().map(|s| s.drain_max as f64).collect::<Vec<_>>()),
        queued_under_load_trials: samples.iter().filter(|s| s.queued_under_load).count() as u64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::perf::registry::owner::OwnerCeiling;
    use crate::perf::registry::read::ReadScalingCell;

    fn cell(threads: u64, churn: bool, rate: f64) -> ReadScalingCell {
        ReadScalingCell {
            threads,
            owner_churn: churn,
            owner_commits_observed: 0,
            lookups: 0,
            elapsed_nanos: 1,
            lookups_per_sec: rate,
            scaling_vs_one: None,
            oversubscribed: false,
        }
    }

    fn owner(drive: &str, rate: f64) -> OwnerCeiling {
        OwnerCeiling {
            drive: drive.to_owned(),
            commits_driven: 0,
            elapsed_nanos: 1,
            admitted: 0,
            drained: 0,
            drains: 0,
            drain_max: 1,
            depth_max: 1,
            over_capacity: 0,
            shed: 0,
            busy_nanos: 1,
            items_per_sec_while_draining: Some(rate),
            queued_under_load: false,
            drive_duty_cycle: None,
        }
    }

    fn report(cells: Vec<ReadScalingCell>) -> RegistryReport {
        RegistryReport {
            schema: "aether.perf.registry.v2".to_owned(),
            populated_mailboxes: 256,
            usable_cores: 12,
            chassis_workers: 2,
            read_scaling: cells,
            owner_unloaded: owner("sequential-spawn", 57_000.0),
            owner_loaded: None,
        }
    }

    /// Tripwire (iamacoffeepot/aether#4274): each trial's scaling ratio is
    /// formed against **its own** single-reader cell before the trials pool.
    ///
    /// The two trials here are the same shape scaled by 10x — a trial that ran
    /// on a hot box and one that did not. Ratios formed per trial are identical
    /// (0.2x), so the band is tight. Pooling the absolute rates first and
    /// dividing medians afterwards would let the between-trial spread leak into
    /// a number that is supposed to have divided it out, and nothing downstream
    /// would show that it had.
    #[test]
    fn scaling_is_paired_inside_each_trial_before_pooling() {
        let fast = report(vec![cell(1, false, 80_000_000.0), cell(2, false, 16_000_000.0)]);
        let slow = report(vec![cell(1, false, 8_000_000.0), cell(2, false, 1_600_000.0)]);

        let banded = summarize(&[fast, slow], 2, true);
        let two = banded.read_scaling.iter().find(|c| c.threads == 2).expect("the 2-reader cell");
        assert!((two.scaling_vs_one.median - 0.2).abs() < 1e-9, "median ratio was {}", two.scaling_vs_one.median);
        assert!(two.scaling_vs_one.iqr < 1e-9, "pairing must leave no spread; iqr was {}", two.scaling_vs_one.iqr);
    }

    /// Tripwire: the single-reader cell bands at exactly 1.00x rather than
    /// inheriting the `None` the sweep leaves on its own baseline row. A cell
    /// banding at zero trials there would read as "unmeasured" for the one row
    /// every other row is stated against.
    #[test]
    fn the_single_reader_cell_bands_at_one() {
        let banded = summarize(&[report(vec![cell(1, false, 80_000_000.0), cell(2, false, 16_000_000.0)])], 1, true);
        let one = banded.read_scaling.iter().find(|c| c.threads == 1).expect("the 1-reader cell");
        assert_eq!(one.scaling_vs_one.trials, 1);
        assert!((one.scaling_vs_one.median - 1.0).abs() < 1e-9);
    }

    /// Tripwire: cells pair by `(threads, churn)`, not by position. The second
    /// trial here is missing its 2-reader uncontended cell, so a positional
    /// pairing would band the *churned* 2-reader rate against the uncontended
    /// one — silently mixing the two columns the sweep exists to separate.
    #[test]
    fn cells_pair_by_key_not_by_position() {
        let full = report(vec![cell(1, false, 80.0), cell(2, false, 16.0), cell(2, true, 4.0)]);
        let gappy = report(vec![cell(1, false, 80.0), cell(2, true, 4.0)]);

        let banded = summarize(&[full, gappy], 2, true);
        let uncontended = banded
            .read_scaling
            .iter()
            .find(|c| c.threads == 2 && !c.owner_churn)
            .expect("the uncontended 2-reader cell");
        assert_eq!(uncontended.lookups_per_sec.trials, 1, "only one trial measured this cell");
        assert!((uncontended.lookups_per_sec.median - 16.0).abs() < 1e-9);
    }
}
