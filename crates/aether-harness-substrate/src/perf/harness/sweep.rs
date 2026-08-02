//! The sweep proper: how a cell is driven ([`Drive`]), the axes one run
//! covers ([`SweepConfig`]), and the two entry points that walk those axes
//! cell by cell.

use super::{CellResult, CellSamples, Topology, effective_trace_ring_cap, run_cell};

/// How a sweep cell drives its topology (iamacoffeepot/aether#1202). The
/// two modes measure orthogonal properties from the *same* harvested
/// trace nodes:
///
/// - `Latency` emits one `Ping` per `Tick` and measures per-hop spans
///   (construct / queued / drain / handler). `pace_hz` `Some(hz)` paces
///   one frame per period (workers park between frames → realistic
///   frame-loop latency), `None` runs flat-out (warm — isolates per-hop
///   dispatch cost). This is the harness's historical behaviour,
///   verbatim.
/// - `Saturate` emits a burst of `backlog` `Ping`s on each tick and
///   measures completed mails/sec. `SubstrateHarness::advance` drains the queue
///   to quiescence every frame (`harness.rs:630`), so one Ping per tick can
///   never build a backlog — the burst is what creates the deep ready
///   queue the throughput metric is meant to capture. Per-hop latency
///   under saturation is contended and high-variance, so a saturate cell
///   reports throughput only, not the latency spans.
#[derive(Clone, Copy, Debug)]
pub enum Drive {
    Latency { pace_hz: Option<u64> },
    Saturate { backlog: u32 },
}

/// Inputs to one sweep. `workers` is the outer axis (pool sizes);
/// `topologies` the inner. `frames` advances per cell; `drive` selects
/// the latency or saturation regime (iamacoffeepot/aether#1202).
#[derive(Clone)]
pub struct SweepConfig {
    pub workers: Vec<usize>,
    pub topologies: Vec<Topology>,
    pub frames: u32,
    pub drive: Drive,
}

/// Drive the sweep and return each cell's **raw** per-span samples
/// (un-summarized). [`run_sweep`] wraps this and collapses to
/// [`CellResult`]; the `perf-plot` bin (iamacoffeepot/aether#1155) reads
/// the raw samples to render distribution plots, which the percentiles
/// can't show.
///
/// This is the **in-process** sweep: every cell boots its chassis in the
/// caller's process, so a cell inherits whatever the preceding cells did to
/// process-global state. iamacoffeepot/aether#4177 measured that inheritance
/// deciding a cell's execution mode at boot, which is why
/// [`super::isolate::run_sweep_samples_isolated`] is what the perf bins drive
/// by default; this entry point remains for in-process callers (the on-demand
/// observe test) and as the isolated sweep's per-cell fallback.
///
/// [`super::isolate::run_sweep_samples_isolated`]: crate::perf::isolate::run_sweep_samples_isolated
#[must_use]
pub fn run_sweep_samples(cfg: &SweepConfig) -> Vec<CellSamples> {
    // Issue 1990: the effective trace-ring cap (env knob or const
    // default) governs both the sweep's `SubstrateHarness` rings and the
    // per-cell burst clamp below — resolved once so they can't drift.
    let trace_ring_cap = effective_trace_ring_cap();
    let mut rows: Vec<CellSamples> = Vec::new();

    for &workers in &cfg.workers {
        for topo in &cfg.topologies {
            if let Some(row) = run_cell(workers, topo, cfg.drive, cfg.frames, trace_ring_cap) {
                rows.push(row);
            }
        }
    }
    rows
}

/// Drive the sweep and return per-cell percentiles. Thin wrapper over
/// [`run_sweep_samples`] that collapses each cell's raw samples to
/// [`Stats`]; the historical entry point for `perf-trial` and the
/// on-demand observe table.
///
/// [`Stats`]: crate::perf::harness::Stats
#[must_use]
pub fn run_sweep(cfg: &SweepConfig) -> Vec<CellResult> {
    run_sweep_samples(cfg).into_iter().map(CellSamples::summarize).collect()
}

#[cfg(test)]
#[allow(clippy::print_stderr)]
mod tests {
    use super::*;
    use crate::perf::harness::{
        DEFAULT_SATURATE_BACKLOG, REAL_UI_FOLLOWUP_STEPS, Tier, depth_chain, fanout, two_level_tree, ui_roundtrip,
    };

    /// Number of `Ping` nodes one root produces in `topo`: the entry send
    /// (source → relay 0) plus one per edge in the DAG. A saturate cell's
    /// completed count should be `backlog × this`.
    fn hops_per_root(topo: &Topology) -> usize {
        1 + topo.downstreams.iter().map(Vec::len).sum::<usize>()
    }

    /// Run a single (workers × topology) saturate cell and return its
    /// samples, or `None` when no wgpu adapter is available (the cell list
    /// comes back empty — a driverless box skips cleanly rather than
    /// failing).
    fn saturate_cell(workers: usize, topo: Topology, backlog: u32) -> Option<CellSamples> {
        let cfg = SweepConfig {
            workers: vec![workers],
            topologies: vec![topo],
            frames: 1,
            drive: Drive::Saturate { backlog },
        };
        run_sweep_samples(&cfg).into_iter().next()
    }

    /// iamacoffeepot/aether#4180: every cell of one trial must be measured
    /// under an equivalent keep-local spill valve.
    ///
    /// The valve is `BUDGET_HANDOFF_MULTIPLIER ×` the scheduler's live
    /// handoff-cost estimate, which is process-global and seeded once —
    /// correct for a production engine, which is one chassis per process.
    /// A sweep is not: it boots a chassis per cell in one process, so
    /// without the per-cell restore each cell inherits an estimate the
    /// preceding cells' wakes drove, and the valve gating every dispatch
    /// percentile becomes a function of cell order rather than of the cell.
    ///
    /// The assertion is equality among observed values, not a pinned
    /// number, so it says the same thing on any box. It has teeth: on the
    /// reference 12-core darwin box these three cells booted at
    /// 1083 → 2728 → 2899 ns before the restore landed, and the full
    /// 10-cell `max,2` sweep spanned 1083 → 25343 ns (valve 6.5 → 60 µs,
    /// pinned against its ceiling on the last cells).
    #[test]
    fn every_sweep_cell_boots_under_the_same_handoff_estimate() {
        let cfg = SweepConfig {
            workers: vec![2],
            topologies: vec![depth_chain(1), fanout(4), two_level_tree()],
            frames: 200,
            drive: Drive::Latency { pace_hz: None },
        };
        let cells = run_sweep_samples(&cfg);
        if cells.len() < cfg.topologies.len() {
            eprintln!("skipping: no wgpu adapter");
            return;
        }

        let first = &cells[0];
        for cell in &cells[1..] {
            assert_eq!(
                cell.boot_handoff_nanos, first.boot_handoff_nanos,
                "cell {} ({}w) booted under a handoff estimate the earlier cells moved — \
                 {} booted at {}ns, this one at {}ns, so the two were measured under \
                 different spill valves",
                cell.topo, cell.workers, first.topo, first.boot_handoff_nanos, cell.boot_handoff_nanos,
            );
        }
    }

    #[test]
    fn saturate_cell_drains_full_backlog_and_reports_finite_rate() {
        let topo = depth_chain(2);
        let hops = hops_per_root(&topo);
        let backlog = 64u32;
        let Some(cell) = saturate_cell(2, topo, backlog) else {
            eprintln!("skipping: no wgpu adapter");
            return;
        };
        // One frame bursts `backlog` roots; `advance(1)` drains them all.
        // Every relay hop completes (`t_received` + `t_finished`), so the
        // handler-sample count is the completed-`Ping` count.
        assert_eq!(
            cell.handler.len(),
            backlog as usize * hops,
            "saturate should drain the whole backlog × hops-per-root"
        );
        let mps = cell.throughput_mps.expect("saturate cell reports a rate");
        assert!(mps.is_finite() && mps > 0.0, "throughput must be positive and finite, got {mps}");
    }

    #[test]
    fn fanout_8_at_default_backlog_reports_finite_rate() {
        // Regression (iamacoffeepot/aether#1226): the entry relay of
        // `fanout(8)` forwards each inbound root to 8 leaves, so it records
        // `2 + 8 = 10` trace-ring slots per root. At the default backlog
        // (512) that is `512 * 10 = 5120 > 4096` (the per-actor ring cap),
        // which lapped the ring, tripped the truncation gate, and dropped
        // `fanout-8`'s throughput cell entirely. `fanout(8)` is `Tier::Light`
        // (the default tier), so the `Saturate` arm survives `drive_for_tier`
        // and this is the exact reproduction at the default depth. The
        // per-cell burst clamp (`4096 / 10 = 409`) must keep the cell
        // measurable: a finite, positive, non-truncated rate.
        let Some(cell) = saturate_cell(2, fanout(8), DEFAULT_SATURATE_BACKLOG) else {
            eprintln!("skipping: no wgpu adapter");
            return;
        };
        let mps = cell.throughput_mps.expect(
            "fanout-8 at the default backlog must report a rate, not truncate to None \
             (iamacoffeepot/aether#1226)",
        );
        assert!(mps.is_finite() && mps > 0.0, "throughput must be positive and finite, got {mps}");
    }

    #[test]
    fn throughput_rises_with_backlog_on_fixed_topology() {
        // Latency mode never reports a rate; the historical path is intact.
        let topo = fanout(4);
        let small = saturate_cell(2, topo.clone(), 32);
        let large = saturate_cell(2, topo, 256);
        let (Some(small), Some(large)) = (small, large) else {
            eprintln!("skipping: no wgpu adapter");
            return;
        };
        // A wall-clock rate compared across two independently-timed runs is
        // not robust under a contended test run: the two measurements see
        // different system load, so even a generous tolerance flakes. The
        // load-independent expression of "throughput scales with backlog" is
        // the completed-work count — `advance(1)` drains to quiescence, so a
        // larger backlog drains strictly more mails on a fixed topology
        // regardless of how busy the machine is (the handler-sample count is
        // the completed-`Ping` count). Assert that, plus that each run yields
        // a well-formed (positive, finite) rate. The rate's *magnitude*
        // relationship is the paired-delta comparator's job (ADR-0085), which
        // cancels runner drift by pairing base/candidate on one runner.
        for mps in [&small, &large].map(|c| c.throughput_mps.expect("saturate rate")) {
            assert!(mps.is_finite() && mps > 0.0, "rate must be positive + finite: {mps}");
        }
        assert!(
            large.handler.len() > small.handler.len(),
            "more backlog must drain more mails: small={}, large={}",
            small.handler.len(),
            large.handler.len(),
        );
    }

    #[test]
    fn saturate_ignores_frame_count_and_still_reports_a_rate() {
        // Regression guard (iamacoffeepot/aether#1202): `saturate_cell` above
        // hardcodes `frames: 1`, but the `perf-trial` bin builds the sweep
        // with AETHER_PERF_FRAMES (default 200). Saturate must advance
        // exactly once regardless — re-bursting `backlog` roots every frame
        // would multiply the offered load by `frames`, lap the 4096-entry
        // trace rings, and trip the truncation gate so the cell reports no
        // rate. That was the original bug: the trial emitted a throughput
        // section with zero cells. A large frame count must still yield a
        // finite rate.
        let cfg = SweepConfig {
            workers: vec![2],
            topologies: vec![fanout(4)],
            frames: 200,
            drive: Drive::Saturate { backlog: 64 },
        };
        let Some(cell) = run_sweep_samples(&cfg).into_iter().next() else {
            eprintln!("skipping: no wgpu adapter");
            return;
        };
        let mps = cell.throughput_mps.expect("frames>1 saturate must still report a rate, not truncate to None");
        assert!(mps.is_finite() && mps > 0.0, "rate must be positive + finite: {mps}");
    }

    #[test]
    fn latency_mode_reports_no_throughput() {
        let cfg = SweepConfig {
            workers: vec![2],
            topologies: vec![depth_chain(1)],
            frames: 4,
            drive: Drive::Latency { pace_hz: None },
        };
        let Some(cell) = run_sweep_samples(&cfg).into_iter().next() else {
            eprintln!("skipping: no wgpu adapter");
            return;
        };
        assert!(cell.throughput_mps.is_none(), "latency mode must not report a throughput rate");
    }

    // The former `over_capacity_backlog_flags_truncation_not_a_wrong_rate`
    // lived here and fed an over-capacity backlog straight to the sweep to
    // force a lap. The per-cell burst clamp (iamacoffeepot/aether#1226) now
    // bounds every `Saturate` cell to `ring_cap / (2 + max_out_degree)`, so
    // the sweep path can no longer lap a ring — its premise is unreachable.
    // The truncation contract (a `None`-rate cell is surfaced flagged, not
    // dropped) is now report-side; the assertion moved to
    // `report::trial::tests::truncated_cell_is_flagged_not_dropped`.

    /// Run a single (workers × topology) cell under the cell's *per-tier*
    /// drive ([`drive_for_tier`]) — so a real topology runs paced — and return
    /// its samples, or `None` when no wgpu adapter is available (the driverless
    /// box skips cleanly). Mirrors [`saturate_cell`] but lets the tier select
    /// the drive, so a real cell exercises the paced path the same way the
    /// `perf-trial` bin will.
    ///
    /// [`drive_for_tier`]: crate::perf::harness::drive_for_tier
    fn real_cell(workers: usize, topo: Topology) -> Option<CellSamples> {
        let cfg = SweepConfig {
            workers: vec![workers],
            // A small frame count: paced cells sleep per frame, so keep the
            // local test fast while still settling several round-trips.
            frames: 4,
            // cfg.drive is overridden to paced for the real tier inside the
            // sweep; the value here is the light/heavy fallback, unused.
            drive: Drive::Latency { pace_hz: None },
            topologies: vec![topo],
        };
        run_sweep_samples(&cfg).into_iter().next()
    }

    #[test]
    fn paced_real_cell_yields_latency_samples_and_no_throughput() {
        // A small `ui-roundtrip` settles quickly; the larger fan shapes work
        // too but cost more per local run.
        let Some(cell) = real_cell(2, ui_roundtrip(REAL_UI_FOLLOWUP_STEPS, 500)) else {
            eprintln!("skipping: no wgpu adapter");
            return;
        };
        assert_eq!(cell.tier, Tier::Real, "the cell carries the real tier");
        assert!(!cell.handler.is_empty(), "a paced real cell must produce per-hop latency samples");
        assert!(cell.throughput_mps.is_none(), "a paced (latency) real cell reports no throughput rate");
        assert!(cell.keepup.is_some(), "a paced real cell harvests keep-up counters (iamacoffeepot/aether#1233)");
    }

    #[test]
    fn keepup_counters_match_dispatched_mail() {
        // A paced real cell harvests offered/completed counters from the
        // actors' plain fields (iamacoffeepot/aether#1233). `advance()`
        // quiesces each frame, so every dispatched `Ping` is handled within
        // its frame: offered == completed, and both equal `frames ×
        // hops-per-root` (the entry send plus one mail per DAG edge).
        let topo = ui_roundtrip(REAL_UI_FOLLOWUP_STEPS, 500);
        let hops = hops_per_root(&topo);
        let Some(cell) = real_cell(2, topo) else {
            eprintln!("skipping: no wgpu adapter");
            return;
        };
        let keepup = cell.keepup.expect("a real cell harvests keep-up counters");
        assert_eq!(keepup.offered, keepup.completed, "a drained run handles every offered mail (offered == completed)");
        // `real_cell` advances 4 frames at burst 1 → 4 roots.
        assert_eq!(keepup.offered, 4 * hops as u64, "offered = frames × hops-per-root");
        assert!(keepup.expected_nanos > 0, "a paced cell carries a positive 60 Hz budget");
    }
}
