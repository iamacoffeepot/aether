//! Per-cell process isolation for the perf sweep (iamacoffeepot/aether#4177).
//!
//! # Why a cell needs its own process
//!
//! A sweep boots one chassis per cell, and until now it booted all of them in
//! one process. #4177 measured what that costs. On the 4-vCPU CI runner the
//! `fanout-4` / 2-worker cell is **bistable**: it settles into one of two
//! execution modes, decided at cell boot and sticky for the cell's whole
//! window, whose `drain` p99 differ by ~3.5x (~2.4 µs against ~8 µs). Every
//! in-window observable is identical between the modes — same single-worker
//! pinning, same parks and wakes, same recruit decisions, and `route_lookup` /
//! `try_seize` never implicated — so nothing the cell *executes* selects the
//! mode. What selects it is the process history the cell boots into.
//!
//! That makes cell order a hidden variable in every percentile the sweep
//! reports. It is also why #4170's "319% `drain` p99 regression" dissolved
//! under measurement: the seal's runtime path was never involved, it only
//! biased which mode the cell booted into, and the same binary produced both
//! modes at both worker counts once the boot point was varied. A paired
//! comparison whose cells inherit each other cannot separate "this change made
//! dispatch slower" from "this change moved the boot point."
//!
//! # What isolation buys
//!
//! Running each cell as a fresh `perf-trial` process makes the modes
//! independent draws instead of a correlated sequence. It does **not** remove
//! the bistability — the physical cause (allocator arena state or CPU
//! wake-depth modality; #4177 leaves it open) is untouched, and a cell can
//! still boot into the slow mode. What changes is that a mode landing is no
//! longer inherited from the cells before it, so it averages out across the K
//! trials ADR-0085 already replicates rather than tracking sweep position. The
//! per-cell mode indicator (`tail_mass`, iamacoffeepot/aether#4265) stays the
//! way a modal cell self-identifies.
//!
//! # Shape
//!
//! The parent enumerates `workers × topologies` and re-execs
//! [`current_exe`][std::env::current_exe] once per cell with
//! [`CELL_ENV`] set to that cell's selector; the child runs exactly that cell
//! via [`harness::run_cell`] and writes its [`CellSamples`] to stdout as JSON.
//! Only the selector crosses the boundary — the child rebuilds the topology
//! from the same inherited environment the parent parsed, so there is no second
//! copy of the topology vocabulary to drift.
//!
//! # What isolation must not change
//!
//! One thing has to cross the boundary besides the selector. The scheduler's
//! handoff-cost estimate is probed once per process, and the keep-local spill
//! valve is a multiple of it — so it is the operating point a cell's dispatch
//! percentiles are measured under. iamacoffeepot/aether#4180 already found
//! that letting it differ between a trial's cells makes them incomparable, and
//! fixed it by restoring the boot seed before each cell. Left alone, isolation
//! would undo that fix in a worse form: every child would run its own probe,
//! and the probe varies enough between processes to reintroduce the drift as
//! an *unreported* random draw — `boot_handoff_nanos` is carried per cell but
//! never reaches the emitted [`TrialReport`][super::report::TrialReport].
//!
//! So the parent probes once and passes its value in [`HANDOFF_SEED_ENV`];
//! each child seeds from it (`seed_handoff_cost_nanos`) before booting its
//! chassis. Every cell of one trial therefore starts from one operating point
//! — #4180's invariant — while each still refines it live from its own wakes.
//! Isolation changes what a cell inherits from its siblings and nothing else.
//!
//! Set [`ISOLATION_ENV`] to `0` to run the sweep in-process instead (the
//! pre-#4177 behaviour), which is what a debugger, a profiler, or a caller
//! that is not a re-executable binary wants.

use std::env;
use std::path::Path;

use aether_substrate::scheduler::{handoff_cost_nanos, seed_handoff_cost_nanos};

use crate::perf::harness::{self, CellSamples, SweepConfig, effective_trace_ring_cap};
use crate::perf::subprocess::run_child_json;

/// Names the one cell a child process must run, as `<topology>@<workers>` —
/// e.g. `fanout-4@2`. Set by the parent on each child; its presence is what
/// puts a `perf` bin in child mode.
pub const CELL_ENV: &str = "AETHER_PERF_CELL";

/// Set to `0` to run every cell in the parent process (the pre-#4177
/// behaviour). Unset or any other value keeps per-cell process isolation.
pub const ISOLATION_ENV: &str = "AETHER_PERF_CELL_ISOLATION";

/// Carries the parent's once-probed handoff cost (nanoseconds) to each child,
/// so a trial's cells share one operating point across the process boundary —
/// see the module docs. Set by the parent; a child seeds from it and refines
/// live, unlike `AETHER_HANDOFF_COST_NS`, which pins and freezes.
pub const HANDOFF_SEED_ENV: &str = "AETHER_PERF_HANDOFF_SEED_NANOS";

/// Which cell a child process was asked to measure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CellSelector {
    /// The [`Topology::name`][harness::Topology::name] to measure — resolved
    /// against the same [`parse_topologies`][harness::parse_topologies] set the
    /// parent enumerated.
    pub topo: String,
    /// The worker-pool size to boot the cell's chassis with.
    pub workers: usize,
}

impl CellSelector {
    /// Render as the `<topology>@<workers>` [`CELL_ENV`] value.
    #[must_use]
    pub fn to_env_value(&self) -> String {
        format!("{}@{}", self.topo, self.workers)
    }

    /// Parse a `<topology>@<workers>` selector. Splits on the **last** `@` so a
    /// topology name containing one still parses; `None` if the separator is
    /// missing or the worker count is not a number.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        let (topo, workers) = value.rsplit_once('@')?;
        Some(Self { topo: topo.to_owned(), workers: workers.trim().parse().ok()? })
    }
}

/// The cell this process was asked to measure, or `None` when it is the parent
/// (the usual case). A `perf` bin calls this first: `Some` means run that one
/// cell and print it, `None` means orchestrate the sweep.
///
/// A malformed [`CELL_ENV`] value is an error rather than a silent fall-through
/// to a full sweep — a child that quietly ran every cell would return one
/// cell's worth of JSON to a parent that then mis-attributes it.
///
/// # Errors
///
/// The [`CELL_ENV`] value if it is set but does not parse as
/// `<topology>@<workers>`.
// Dev/perf tooling: the perf bins take their run parameters from env, and this
// is the child-mode selector among them — not a capability, no config layer.
#[allow(clippy::disallowed_methods)]
pub fn selected_cell() -> Result<Option<CellSelector>, String> {
    let Ok(value) = env::var(CELL_ENV) else {
        return Ok(None);
    };
    if value.is_empty() {
        return Ok(None);
    }
    CellSelector::parse(&value).map(Some).ok_or(value)
}

/// Whether the sweep should isolate each cell in its own process — true unless
/// [`ISOLATION_ENV`] is exactly `0`.
// Dev/perf tooling: see `selected_cell`.
#[allow(clippy::disallowed_methods)]
#[must_use]
pub fn isolation_enabled() -> bool {
    env::var(ISOLATION_ENV).as_deref() != Ok("0")
}

/// Adopt the parent's probed handoff cost, if it passed one — the child half
/// of the shared-operating-point handshake the module docs describe. Silent
/// when [`HANDOFF_SEED_ENV`] is absent (this process is not a child) or
/// unparseable, in which case the child keeps its own boot probe.
// Dev/perf tooling: see `selected_cell`.
#[allow(clippy::disallowed_methods)]
fn adopt_parent_handoff_seed() {
    if let Ok(raw) = env::var(HANDOFF_SEED_ENV)
        && let Ok(nanos) = raw.trim().parse::<u64>()
        && nanos > 0
    {
        seed_handoff_cost_nanos(nanos);
    }
}

/// Measure the one cell `selector` names, resolving its topology against
/// `cfg`. `None` if no topology in `cfg` carries that name (a parent and child
/// disagreeing about the topology set, which means their environments differ)
/// or the cell itself could not be measured.
///
/// Adopts the parent's handoff seed first, so the cell boots at the trial's
/// shared operating point rather than this process's own probe.
#[must_use]
pub fn run_selected_cell(cfg: &SweepConfig, selector: &CellSelector) -> Option<CellSamples> {
    adopt_parent_handoff_seed();
    let Some(topo) = cfg.topologies.iter().find(|t| t.name == selector.topo) else {
        tracing::warn!(
            target: "aether_perf",
            topo = %selector.topo,
            "no such topology in this process's sweep set — parent and child environments differ",
        );
        return None;
    };
    harness::run_cell(selector.workers, topo, cfg.drive, cfg.frames, effective_trace_ring_cap())
}

/// The JSON a child writes to stdout for `selector` — that one cell's raw
/// samples, the private transport [`run_sweep_samples_isolated`] decodes.
/// `Ok(None)` when the cell could not be measured; the child then writes
/// nothing and the parent drops the cell, having already seen the child's
/// `warn` on the shared stderr.
///
/// Both `perf` bins are child processes of themselves, so the encode lives here
/// rather than once per bin.
///
/// # Errors
///
/// The serialization error, if a measured cell will not encode.
pub fn selected_cell_json(cfg: &SweepConfig, selector: &CellSelector) -> Result<Option<String>, String> {
    let Some(cell) = run_selected_cell(cfg, selector) else {
        return Ok(None);
    };
    serde_json::to_string(&cell).map(Some).map_err(|e| e.to_string())
}

/// Drive the sweep with **each cell in its own process** and return their raw
/// samples — the #4177 entry point the `perf` bins use, and the isolation this
/// module exists for.
///
/// Falls back to [`harness::run_sweep_samples`] when [`isolation_enabled`] is
/// false or [`current_exe`][std::env::current_exe] cannot be resolved. A cell
/// whose child fails is warned about and dropped, matching how the in-process
/// sweep drops a cell it could not measure.
// The orchestrating process boots no chassis, and the chassis boot is what
// installs the tracing subscriber — so `tracing::warn!` here would go nowhere
// and a dropped cell would be silent. Its diagnostics go straight to the
// stderr its children already share.
#[allow(clippy::print_stderr)]
#[must_use]
pub fn run_sweep_samples_isolated(cfg: &SweepConfig) -> Vec<CellSamples> {
    if !isolation_enabled() {
        eprintln!("perf: per-cell process isolation disabled; measuring every cell in this process");
        return harness::run_sweep_samples(cfg);
    }
    let exe = match env::current_exe() {
        Ok(exe) => exe,
        Err(e) => {
            eprintln!(
                "perf: cannot resolve the current executable ({e}); falling back to an in-process sweep — cells will not be independent (iamacoffeepot/aether#4177)"
            );
            return harness::run_sweep_samples(cfg);
        }
    };

    // Probe once, here, and hand the value to every child: one operating point
    // per trial rather than one per cell (see the module docs). Reading it is
    // what runs this process's boot probe, so this call *is* the trial's probe.
    let handoff_seed_nanos = handoff_cost_nanos();
    let cells = cfg.workers.len() * cfg.topologies.len();
    eprintln!("perf: measuring {cells} cells, one process each; handoff seed {handoff_seed_nanos}ns");

    let mut rows = Vec::new();
    for &workers in &cfg.workers {
        for topo in &cfg.topologies {
            let selector = CellSelector { topo: topo.name.clone(), workers };
            match run_cell_in_subprocess(&exe, &selector, handoff_seed_nanos) {
                Ok(row) => rows.push(row),
                Err(e) => eprintln!("perf: cell {} skipped — {e}", selector.to_env_value()),
            }
        }
    }
    rows
}

/// Run one cell as a child of `exe` and decode its result.
///
/// Beyond the two variables here the child inherits this process's whole
/// environment — every `AETHER_PERF_*` / `AETHER_LATENCY_*` knob the parent
/// parsed — which is what lets only the selector cross the boundary.
fn run_cell_in_subprocess(exe: &Path, selector: &CellSelector, handoff_seed_nanos: u64) -> Result<CellSamples, String> {
    run_child_json(exe, &[(CELL_ENV, selector.to_env_value()), (HANDOFF_SEED_ENV, handoff_seed_nanos.to_string())])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tripwire: the selector round-trips through its env-value form. The
    /// parent writes `to_env_value` and the child reads `parse`, so a change to
    /// either side that is not mirrored on the other silently mis-addresses
    /// every cell of every trial (iamacoffeepot/aether#4177).
    #[test]
    fn selector_round_trips_through_its_env_value() {
        let selector = CellSelector { topo: "tree-A-BC-DEEF-routed".to_owned(), workers: 3 };
        assert_eq!(CellSelector::parse(&selector.to_env_value()), Some(selector));
    }

    /// Tripwire: the split is on the *last* `@`, so a topology name containing
    /// one still resolves. Nothing in today's vocabulary has an `@`, which is
    /// exactly why the wrong split would go unnoticed until one did.
    #[test]
    fn selector_splits_on_the_last_separator() {
        assert_eq!(
            CellSelector::parse("weird@name@2"),
            Some(CellSelector { topo: "weird@name".to_owned(), workers: 2 })
        );
    }

    #[test]
    fn a_malformed_selector_does_not_parse() {
        assert_eq!(CellSelector::parse("fanout-4"), None, "no separator");
        assert_eq!(CellSelector::parse("fanout-4@"), None, "no worker count");
        assert_eq!(CellSelector::parse("fanout-4@many"), None, "non-numeric worker count");
    }

    /// Tripwire: a topology name addresses exactly one shape across the whole
    /// vocabulary — the light shapes and the heavy variants derived from them
    /// together (iamacoffeepot/aether#4177).
    ///
    /// The child resolves its cell by name alone, so this is the property that
    /// makes the selector well-defined. It is not automatic: each heavy factory
    /// *derives* from a light one and re-labels the clone, so dropping a
    /// `-heavy` / `-routed` suffix would collide two shapes whose measurements
    /// differ by a `busy_spin` budget, and the child would silently measure the
    /// wrong one rather than fail.
    #[test]
    fn a_topology_name_addresses_exactly_one_shape() {
        let work = 1;
        let vocabulary = [
            harness::depth_chain(1),
            harness::depth_chain(8),
            harness::fanout(4),
            harness::fanout(8),
            harness::two_level_tree(),
            harness::fanout_heavy(4, work),
            harness::fanout_heavy(8, work),
            harness::two_level_tree_heavy(work),
            harness::two_level_tree_router_heavy(work),
            harness::socket_server(2, work, work),
            harness::tick_broadcast(2, work, work),
            harness::ui_roundtrip(2, work),
        ];

        for topo in &vocabulary {
            let parsed = CellSelector::parse(&CellSelector { topo: topo.name.clone(), workers: 2 }.to_env_value())
                .expect("a topology name renders a parseable selector");
            let matches = vocabulary.iter().filter(|t| t.name == parsed.topo).count();
            assert_eq!(matches, 1, "`{}` must address exactly one shape", topo.name);
        }
    }
}
