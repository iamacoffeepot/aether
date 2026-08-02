//! The perf lane's run parameters, resolved from the process env. Every
//! `*_from_env` knob the sweep, the `perf-trial` / `perf-plot` bins and the
//! on-demand observe test share lives here, so each parse has one spelling.

use std::env;
use std::str::FromStr;
use std::thread;

use aether_actor::trace::DEFAULT_TRACE_RING_CAP;
use aether_substrate::SchedulerTuning;

use super::{Drive, Tier};

/// Read the optional `AETHER_LATENCY_PACE_HZ` pacing override (frames/sec;
/// `None` = flat-out / warm). Shared by the on-demand harness test and
/// the `perf-trial` bin so the parse lives in one place.
#[must_use]
pub fn pace_hz_from_env() -> Option<u64> {
    env::var("AETHER_LATENCY_PACE_HZ").ok().and_then(|s| s.parse().ok()).filter(|&h| h > 0)
}

/// Default pacing for the real tier when `AETHER_LATENCY_PACE_HZ` is unset
/// (ADR-0085 amendment). The real tier is *defined* as paced — interval-fired
/// input and writer chains modelling a client talking to a server and the
/// server replying, not a saturating flood — so it never runs flat-out
/// regardless of `cfg.drive`. 60 Hz is the engine's reference frame rate. A
/// starting point, tuned per-shape in PR 3 (iamacoffeepot/aether#1222).
pub const DEFAULT_REAL_PACE_HZ: u64 = 60;

/// The [`Drive`] a cell of `tier` actually runs under, given the sweep's
/// configured `drive` (ADR-0085 amendment). This is the per-tier-drive valve:
/// the **real** tier is always driven *paced* (`Drive::Latency { pace_hz:
/// Some(..) }`) — its model is a client/server round-trip, not a flood — using
/// `AETHER_LATENCY_PACE_HZ` or [`DEFAULT_REAL_PACE_HZ`]; **light** and
/// **heavy** keep the sweep's configured `drive` verbatim (their existing flat
/// or saturate behaviour). Selecting per-tier inside [`run_sweep_samples`]
/// (mechanism (b)) — rather than running a separate sweep per tier — keeps the
/// single-`SweepConfig`, single-`run_sweep` call path that `perf-trial` and
/// the observe test already use, and leaves the emitted report shape (one
/// section per tier) untouched.
///
/// [`run_sweep_samples`]: crate::perf::harness::run_sweep_samples
#[must_use]
pub fn drive_for_tier(drive: Drive, tier: Tier) -> Drive {
    match tier {
        Tier::Real => Drive::Latency { pace_hz: Some(pace_hz_from_env().unwrap_or(DEFAULT_REAL_PACE_HZ)) },
        Tier::Light | Tier::Heavy => drive,
    }
}

/// Default per-tick `Ping` burst for a `Saturate` cell when
/// `AETHER_PERF_BACKLOG` is unset. This is the *requested* depth, not the
/// effective one: a relay writes `2 + out_degree` trace-ring slots per
/// inbound mail (`Received` + `Finished` on dispatch, plus one `Sent` per
/// downstream), so the binding constraint on the entry relay's per-actor
/// ring ([`DEFAULT_TRACE_RING_CAP`]) is `backlog * (2 + out_degree) <=
/// ring_cap`, not `backlog <= ring_cap`. At 512 a low-fan-out cell stays
/// well under cap, but a wide fan-out laps it (`fanout-8`:
/// `512 * (2 + 8) = 5120 > 4096`). [`run_sweep_samples`] therefore clamps
/// each `Saturate` cell's burst to `ring_cap / (2 + max_out_degree(topo))`
/// so every cell stays measurable regardless of fan-out
/// (iamacoffeepot/aether#1226).
///
/// [`run_sweep_samples`]: crate::perf::harness::run_sweep_samples
pub const DEFAULT_SATURATE_BACKLOG: u32 = 512;

/// Resolve the *effective* per-actor trace-ring capacity for the
/// saturation-invariant math (issue 1990). Reads `AETHER_ACTOR_TRACE_RING_SIZE`
/// (the chassis-wide knob) when set, else the `aether-actor` const
/// [`DEFAULT_TRACE_RING_CAP`]. The sweep cell ([`run_sweep_samples`])
/// pins the same value on its `SubstrateHarness` so the `backlog * (2 +
/// out_degree) <= ring_cap` clamp and the ring the relay actually writes
/// agree — bumping the knob to chase a high-volume lap (the use case that
/// motivated the issue) lifts both together instead of silently keeping
/// 4096.
///
/// [`run_sweep_samples`]: crate::perf::harness::run_sweep_samples
#[must_use]
pub fn effective_trace_ring_cap() -> usize {
    env::var("AETHER_ACTOR_TRACE_RING_SIZE")
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_TRACE_RING_CAP)
}

/// The nine `AETHER_*` keys `scheduler_tuning_from_env` reads, in
/// [`SchedulerTuning`] field order. Named as a const so the chassis can pin
/// this list against `SchedulerTuningConfig`'s — the two spellings of the same
/// key set are what would otherwise drift apart.
pub const SCHEDULER_TUNING_ENV_KEYS: [&str; 9] = [
    "AETHER_SPIN_WINDOW_USEC",
    "AETHER_LOCAL_STICKY_MAX",
    "AETHER_LOCAL_TIME_BUDGET_US",
    "AETHER_PEER_STEAL",
    "AETHER_LOCAL_CHAIN_BACKSTOP",
    "AETHER_HANDOFF_COST_NS",
    "AETHER_BLOB_RECRUIT_MIN",
    "AETHER_BLOB_RECRUIT_MAX",
    "AETHER_WAKE_COST_NANOS",
];

/// Resolve the scheduler's hot-path tuning from the perf lane's process env,
/// falling back to [`SchedulerTuning::default`] per knob.
///
/// The nine keys are chassis-boot config (`aether-chassis`'s
/// `SchedulerTuningConfig`), and a `SubstrateHarness` neither takes that path
/// nor could — it resolves off a hermetic source stack (ADR-0156 §5), and
/// `aether-chassis` already depends on this crate, so the reverse edge is a
/// cycle. Left alone, that made every key inert under the perf lane while the
/// scheduler's own docs advertised them, so an A/B across two values ran the
/// same configuration twice and returned a clean null indistinguishable from a
/// real "this knob does not affect this cell" result (issue 4234).
///
/// Reading them here rather than in the harness proper keeps ordinary scenario
/// tests hermetic — a stray `AETHER_PEER_STEAL` in a developer's shell must
/// not reconfigure an unrelated substrate test — while making the perf lane's
/// documented `PERF_BASE_ENV` / `PERF_CAND_ENV` pinning real. It sits beside
/// the other `*_from_env` perf knobs for the same reason they live here.
///
/// The two adaptive knobs stay `None` when unset (measured / derived
/// behaviour) and the `nonzero` knobs coerce a `0` to their default,
/// reproducing `SchedulerTuningConfig::to_scheduler_tuning`.
#[must_use]
pub fn scheduler_tuning_from_env() -> SchedulerTuning {
    fn parsed<T: FromStr>(key: &str) -> Option<T> {
        env::var(key).ok().and_then(|value| value.trim().parse().ok())
    }

    let defaults = SchedulerTuning::default();
    SchedulerTuning {
        spin_window_micros: parsed("AETHER_SPIN_WINDOW_USEC").unwrap_or(defaults.spin_window_micros),
        local_sticky_max: parsed::<usize>("AETHER_LOCAL_STICKY_MAX")
            .filter(|&n| n > 0)
            .unwrap_or(defaults.local_sticky_max),
        // Unset auto-tunes; an explicit `0` is meaningful (it disables the
        // valve), so this one does not filter zero.
        time_budget_micros: parsed("AETHER_LOCAL_TIME_BUDGET_US").or(defaults.time_budget_micros),
        peer_steal: env::var("AETHER_PEER_STEAL")
            .ok()
            .map_or(defaults.peer_steal, |value| matches!(value.trim(), "1" | "true" | "yes")),
        local_chain_backstop: parsed::<u32>("AETHER_LOCAL_CHAIN_BACKSTOP")
            .filter(|&n| n > 0)
            .unwrap_or(defaults.local_chain_backstop),
        handoff_cost_nanos: parsed::<u64>("AETHER_HANDOFF_COST_NS").filter(|&n| n >= 1).or(defaults.handoff_cost_nanos),
        blob_recruit_min: parsed::<usize>("AETHER_BLOB_RECRUIT_MIN")
            .filter(|&n| n > 0)
            .unwrap_or(defaults.blob_recruit_min),
        blob_recruit_max: parsed::<usize>("AETHER_BLOB_RECRUIT_MAX")
            .filter(|&n| n > 0)
            .unwrap_or(defaults.blob_recruit_max),
        wake_cost_nanos: parsed::<u64>("AETHER_WAKE_COST_NANOS").filter(|&n| n >= 1).or(defaults.wake_cost_nanos),
    }
}

/// Read the per-tick saturation backlog from `AETHER_PERF_BACKLOG`
/// (iamacoffeepot/aether#1202), defaulting to [`DEFAULT_SATURATE_BACKLOG`]
/// when unset / unparseable / `0`. The `min(cap)` here is the *env ceiling*
/// only — it bounds the parsed value against the per-actor trace ring
/// capacity ([`DEFAULT_TRACE_RING_CAP`]) so a wildly-large
/// `AETHER_PERF_BACKLOG` can't request a depth no topology could ever fit.
/// It does **not** account for fan-out: a relay records `2 + out_degree`
/// ring slots per inbound mail, so the tighter per-topology bound
/// (`backlog * (2 + out_degree) <= ring_cap`) lives at the cell in
/// [`run_sweep_samples`], which clamps each `Saturate` burst to
/// `ring_cap / (2 + max_out_degree(topo))` (iamacoffeepot/aether#1226).
///
/// [`run_sweep_samples`]: crate::perf::harness::run_sweep_samples
#[must_use]
pub fn saturate_backlog_from_env() -> u32 {
    let cap = u32::try_from(effective_trace_ring_cap()).unwrap_or(u32::MAX);
    env::var("AETHER_PERF_BACKLOG")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .filter(|&b| b > 0)
        .unwrap_or(DEFAULT_SATURATE_BACKLOG)
        .min(cap)
}

/// Parse the `Drive` mode from `AETHER_PERF_DRIVE` (`latency` |
/// `saturate`; default `latency`), composing `pace_hz_from_env` /
/// `saturate_backlog_from_env` for the mode's own knob
/// (iamacoffeepot/aether#1202). Shared by the `perf-trial` and `perf-plot`
/// bins and the on-demand observe test so the parse lives in one place.
#[must_use]
pub fn drive_from_env() -> Drive {
    match env::var("AETHER_PERF_DRIVE").as_deref() {
        Ok("saturate") => Drive::Saturate { backlog: saturate_backlog_from_env() },
        _ => Drive::Latency { pace_hz: pace_hz_from_env() },
    }
}

/// Default per-leaf `busy_spin` iteration count for the heavy tier when
/// `AETHER_LATENCY_HEAVY_WORK` is unset (ADR-0085 amendment). The tier
/// selector ([`tiers_from_env`]) now gates *whether* heavy shapes run; this
/// var supplies only the spin magnitude, so an active heavy tier needs a
/// sensible non-zero default rather than silently degenerating to the
/// trivial shapes. Sized to give a heavy leaf a clearly-non-trivial
/// per-handler cost (tens of µs at the harness's measured rate) so the
/// parallelism-vs-locality crossover the heavy tier exists to expose is
/// actually present — read the HANDLER DUR column to convert to wall-clock.
pub const DEFAULT_HEAVY_WORK_ITERS: u64 = 50_000;

/// The heavy-leaf CPU work *magnitude* — a raw `busy_spin` iteration count
/// per heavy node (see [`fanout_heavy`]). Read from
/// `AETHER_LATENCY_HEAVY_WORK`; unset / unparseable / `0` falls back to
/// [`DEFAULT_HEAVY_WORK_ITERS`].
///
/// This var no longer *gates* the heavy shapes — that is the tier selector's
/// job ([`tiers_from_env`]) since the ADR-0085 amendment. It now carries
/// only the spin count, so the calibration workflow still works: set a count
/// and read the actual per-leaf microseconds off the harness's HANDLER DUR
/// column (it measures `t_finished - t_received`), then adjust. A raw
/// iteration count, not a microsecond budget, keeps the work *identical*
/// across processes so a paired base-vs-candidate comparison (ADR-0085)
/// isn't confounded by per-run calibration drift.
///
/// [`fanout_heavy`]: crate::perf::harness::fanout_heavy
#[must_use]
pub fn heavy_work_iters_from_env() -> u64 {
    env::var("AETHER_LATENCY_HEAVY_WORK")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&w| w > 0)
        .unwrap_or(DEFAULT_HEAVY_WORK_ITERS)
}

/// Parse `AETHER_PERF_TIER` — a comma list of workload tiers (`light`,
/// `heavy`, `real`; e.g. `"light,heavy"`), default `light` when unset /
/// empty / all-unparseable (ADR-0085 amendment). This is the *tier* axis,
/// orthogonal to `AETHER_PERF_TOPOS` (`ci` / `full`), which selects the
/// shape *breadth* within each tier. Unknown tokens are dropped; the result
/// is order-preserving and de-duplicated. Shared by the `perf-trial` and
/// `perf-plot` bins and the on-demand observe test.
#[must_use]
pub fn tiers_from_env() -> Vec<Tier> {
    let spec = env::var("AETHER_PERF_TIER").unwrap_or_default();
    let mut out: Vec<Tier> = Vec::new();
    for tok in spec.split(',') {
        if let Some(tier) = Tier::parse_token(tok)
            && !out.contains(&tier)
        {
            out.push(tier);
        }
    }
    if out.is_empty() {
        out.push(Tier::Light);
    }
    out
}

/// Parse the optional `AETHER_LATENCY_WIDE_FANOUT` knob — a comma list of
/// *extra* trivial fan-out widths to append to the sweep, e.g.
/// `"16,32,64,128"`. Unset or empty appends nothing, so the default
/// sweep is unchanged (iamacoffeepot/aether#1075). Widths should exceed
/// the default `≤8` set; values are sorted and de-duplicated.
///
/// The point is to push past the default widths and locate the
/// stickiness width-crossover `W*` — the width at which keeping a
/// fan-out's children on the producing worker (`AETHER_LOCAL_STICKY_MAX`
/// `≥ width`) stops winning, because draining `N` children serially on
/// one worker overtakes the cross-worker handoff that keeping-local
/// avoided. Sweep this against `AETHER_LOCAL_STICKY_MAX` (`1` vs width)
/// and the win should invert somewhere past `W* ≈ handoff / per-child`.
#[must_use]
pub fn wide_fanout_widths_from_env() -> Vec<usize> {
    let Ok(spec) = env::var("AETHER_LATENCY_WIDE_FANOUT") else {
        return Vec::new();
    };
    let mut out: Vec<usize> =
        spec.split(',').filter_map(|t| t.trim().parse::<usize>().ok()).filter(|&w| w > 0).collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// Parse `AETHER_PERF_WORKERS` — a comma list of pool sizes; the token
/// `max` resolves to `available_parallelism() - 1`. Default `max`.
/// Shared by the `perf-trial` and `perf-plot` bins so their sweeps cover
/// the identical worker axis.
#[must_use]
pub fn parse_workers() -> Vec<usize> {
    let max = thread::available_parallelism().map_or(2, |n| n.get().saturating_sub(1).max(1));
    let spec = env::var("AETHER_PERF_WORKERS").unwrap_or_else(|_| "max".to_owned());
    let mut out: Vec<usize> = spec
        .split(',')
        .filter_map(|tok| {
            let t = tok.trim();
            if t.eq_ignore_ascii_case("max") {
                Some(max)
            } else {
                t.parse::<usize>().ok().map(|w| w.max(1))
            }
        })
        .collect();
    out.sort_unstable();
    out.dedup();
    if out.is_empty() {
        out.push(max);
    }
    out
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[test]
    fn over_capacity_backlog_is_clamped_by_env_parse() {
        // The env parse clamps a backlog past the trace ring capacity so a
        // merely-large `AETHER_PERF_BACKLOG` stays measurable. Serialised
        // against the other env-reading test via a shared lock, since
        // nextest runs tests in one process across threads.
        let _guard = ENV_LOCK.lock().expect("env lock");
        // Re-pointed at the effective cap (issue 1990): with the trace
        // ring knob unset it equals the const default, so the clamp
        // target is unchanged.
        let cap = u32::try_from(effective_trace_ring_cap()).unwrap_or(u32::MAX);
        // Safety: process-wide env mutation, serialised by `ENV_LOCK` and
        // restored before the guard drops.
        unsafe {
            env::set_var("AETHER_PERF_BACKLOG", (cap + 10_000).to_string());
        }
        let parsed = saturate_backlog_from_env();
        // Safety: same serialised env mutation — restore the cleared state.
        unsafe {
            env::remove_var("AETHER_PERF_BACKLOG");
        }
        assert_eq!(parsed, cap, "an over-capacity backlog clamps to the ring cap");
    }

    /// Serialises the `AETHER_PERF_BACKLOG`-mutating test against any other
    /// env-reading test in this module.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn drive_for_tier_paces_real_and_passes_others_through() {
        // Real is always paced, even when the sweep was configured saturate.
        let sat = Drive::Saturate { backlog: 64 };
        assert!(
            matches!(drive_for_tier(sat, Tier::Real), Drive::Latency { pace_hz: Some(_) }),
            "real tier must be driven paced regardless of cfg.drive"
        );
        // Light / heavy keep the configured drive verbatim.
        for tier in [Tier::Light, Tier::Heavy] {
            assert!(
                matches!(drive_for_tier(sat, tier), Drive::Saturate { backlog: 64 }),
                "{tier:?} must keep the configured drive"
            );
        }
    }
}
