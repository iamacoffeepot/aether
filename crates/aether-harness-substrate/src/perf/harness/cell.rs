//! One sweep cell — a single worker-count × topology pair: the raw per-span
//! samples it yields, the percentile collapse of those samples, and the
//! measurement itself (boot a chassis, wire the topology, drive, harvest).

use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use aether_data::{Kind, MailboxId};
use aether_kinds::trace::{TraceRingEntry, TraceTail, TraceTailResult};
use aether_kinds::{LifecycleSubscribe, LifecycleSubscribeResult, Tick};
use aether_substrate::Subname;
use aether_substrate::scheduler::{handoff_cost_nanos, reset_handoff_to_boot_seed};
use aether_trace::walk::fold_nodes;
use serde::{Deserialize, Serialize};

use super::keepup::harvest_keepup;
use super::relay::RELAY_NS;
use super::throughput::throughput_from_nodes;
use super::tick::TICKSRC_NS;
use super::{
    Drive, KeepUp, Ping, Relay, RelayConfig, Stats, TickSource, Tier, Topology, drive_for_tier, max_out_degree,
    relay_id, scheduler_tuning_from_env, summarize, ticksrc_id,
};
use crate::{DEFAULT_TICK_DELTA_MICROS, SubstrateHarness};

/// One measured cell's **raw** samples (per worker count × topology),
/// before percentile collapse. The latency spans are nanosecond
/// samples; `depth` is the scheduler ready-queue length distribution
/// (counts). [`Self::summarize`] folds these to a [`CellResult`]; the
/// `perf-plot` bin (iamacoffeepot/aether#1155) renders them directly.
/// Carried over a pipe by the per-cell subprocess in [`super::isolate`], so
/// its shape is a private transport between a `perf` bin and its own re-exec
/// — never a published artifact like [`super::report::TrialReport`], which
/// carries the versioned `schema` field instead.
///
/// [`super::isolate`]: crate::perf::isolate
/// [`super::report::TrialReport`]: crate::perf::report::TrialReport
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CellSamples {
    pub workers: usize,
    pub topo: String,
    /// The workload tier this cell's topology belongs to (ADR-0085
    /// amendment), threaded to the report builder so the renderer can
    /// suppress the verdict for a non-`light` tier.
    pub tier: Tier,
    /// The scheduler's handoff-cost estimate (nanos) this cell's chassis
    /// booted from — the input the keep-local spill valve
    /// (`worker_deque::time_budget`) is derived from, and therefore the
    /// operating point the cell's dispatch percentiles were measured under.
    /// Recorded per cell because it used to differ between them
    /// (iamacoffeepot/aether#4180); the sweep now restores the process boot
    /// seed before each cell, so every cell of one trial starts here.
    pub boot_handoff_nanos: u64,
    /// iamacoffeepot/aether#1158: `t_sent − t_construct_start` (flush-begin
    /// → blob open) — the producer building the blob.
    pub construct: Vec<u64>,
    pub queued: Vec<u64>,
    pub drain: Vec<u64>,
    pub handler: Vec<u64>,
    pub depth: Vec<u64>,
    /// iamacoffeepot/aether#1202: a steady-state mails/sec estimate under
    /// saturation — the rate over the trimmed saturated middle of the run, not
    /// a full-batch makespan average (iamacoffeepot/aether#1227). `Some` in
    /// `Drive::Saturate` (computed from the same folded nodes the latency spans
    /// come from), `None` in `Drive::Latency`. A cell whose entry ring lapped
    /// reports `None` rather than a wrong rate.
    pub throughput_mps: Option<f64>,
    /// iamacoffeepot/aether#1233: the real tier's keep-up characterisation —
    /// `Some` only for [`Tier::Real`] cells (the paced tier), `None`
    /// otherwise. The real tier reports this *instead of* the per-hop span
    /// percentiles.
    pub keepup: Option<KeepUp>,
}

impl CellSamples {
    /// Collapse each span's samples to [`Stats`] percentiles.
    #[must_use]
    pub fn summarize(self) -> CellResult {
        CellResult {
            workers: self.workers,
            topo: self.topo,
            tier: self.tier,
            boot_handoff_nanos: self.boot_handoff_nanos,
            construct: summarize(self.construct),
            queued: summarize(self.queued),
            drain: summarize(self.drain),
            handler: summarize(self.handler),
            depth: summarize(self.depth),
            throughput_mps: self.throughput_mps,
            keepup: self.keepup,
        }
    }
}

/// One fully-measured cell (per worker count × topology).
#[derive(Clone, Debug)]
pub struct CellResult {
    pub workers: usize,
    pub topo: String,
    /// The workload tier this cell's topology belongs to (ADR-0085
    /// amendment). [`TrialReport::from_cells`] splits the cell list by this
    /// field into one report section per tier.
    ///
    /// [`TrialReport::from_cells`]: crate::perf::report::TrialReport::from_cells
    pub tier: Tier,
    /// The scheduler handoff-cost estimate (nanos) this cell's chassis
    /// booted from — see [`CellSamples::boot_handoff_nanos`].
    pub boot_handoff_nanos: u64,
    /// iamacoffeepot/aether#1158: `t_sent − t_construct_start` (blob open →
    /// flush-begin) — the producer-side time spent building the blob, the
    /// first leg of the four-stage lifecycle. ~0 on eager (non-buffered)
    /// paths, where construct-start *is* `t_sent`.
    pub construct: Stats,
    /// iamacoffeepot/aether#1150: `t_enqueue − t_sent` (flush-begin → the
    /// worker picks up the blob this mail rode in / the deposit lands) —
    /// wakeup + scheduling latency. ~0 on the producer's own warm worker.
    pub queued: Stats,
    /// iamacoffeepot/aether#1150: `t_received − t_enqueue` (blob pickup →
    /// this mail's handler entry) — where in the blob's drain the mail
    /// landed. The only cardinality-sensitive span: a serial fan-out's
    /// late leaf waited behind its siblings here, so it reads high by
    /// design (the scheduler's serialize-vs-recruit choice, not per-mail
    /// cost — cross-reference `handler` to judge it).
    pub drain: Stats,
    /// `t_finished − t_received` — the recipient's own handler work.
    pub handler: Stats,
    /// iamacoffeepot/aether#1134: scheduler ready-queue depth at the
    /// deposit (`enqueue_depth`), as a distribution — *counts, not
    /// nanoseconds*. p50 ≈ 0 means `queued` is wakeup-dominated (empty
    /// queue); a rising tail means wait-behind-N (offered load).
    pub depth: Stats,
    /// iamacoffeepot/aether#1202: a steady-state mails/sec estimate under
    /// saturation — the rate over the trimmed saturated middle, not a
    /// full-batch makespan average (iamacoffeepot/aether#1227). `Some` only in
    /// `Drive::Saturate` (`None` for a latency cell); `None` too when the entry
    /// ring lapped, so a truncated cell never reports a wrong rate.
    pub throughput_mps: Option<f64>,
    /// iamacoffeepot/aether#1233: the real tier's keep-up characterisation —
    /// `Some` only for [`Tier::Real`] cells. [`TrialReport::from_cells`]
    /// renders the real tier from this instead of the span percentiles.
    ///
    /// [`TrialReport::from_cells`]: crate::perf::report::TrialReport::from_cells
    pub keepup: Option<KeepUp>,
}

/// Measure one sweep cell — one `workers` × `topo` pair — and return its raw
/// per-span samples, or `None` if the cell could not be measured (chassis boot
/// failed, an actor spawn failed, or the trace harvest errored; each logs a
/// `warn` first). The unit both [`run_sweep_samples`] and the per-cell
/// subprocess in [`super::isolate`] drive, so an isolated cell and an
/// in-process one measure through identical code.
///
/// `trace_ring_cap` is passed in rather than read here so the sweep's rings and
/// its `Saturate` burst clamp resolve it once and cannot drift (issue 1990).
///
/// [`run_sweep_samples`]: crate::perf::harness::run_sweep_samples
/// [`super::isolate`]: crate::perf::isolate
#[allow(clippy::cast_precision_loss, clippy::too_many_lines)]
#[must_use]
pub fn run_cell(
    workers: usize,
    topo: &Topology,
    drive: Drive,
    frames: u32,
    trace_ring_cap: usize,
) -> Option<CellSamples> {
    // iamacoffeepot/aether#4180: the keep-local spill valve is a
    // multiple of the scheduler's live handoff-cost estimate, which
    // is process-global and seeded once — one chassis per process is
    // what the substrate assumes. A sweep breaks that assumption: it
    // boots a chassis per cell in one process, so without this the
    // estimate a cell starts from is whatever the *previous* cells'
    // wakes folded into it, and cell order becomes a hidden variable
    // in every dispatch percentile the valve gates. Restoring the
    // boot seed re-establishes the per-process starting state for
    // each cell — still this box's probed handoff cost, not a pinned
    // constant, and the cell's own wakes still refine it while it
    // runs. Under per-cell process isolation the seed is already at
    // boot, so this is a no-op there and the two paths agree.
    reset_handoff_to_boot_seed();
    let boot_handoff_nanos = handoff_cost_nanos();
    let Ok(mut tb) = SubstrateHarness::builder()
        .with_workers(Some(workers))
        .trace_ring_capacity(Some(trace_ring_cap))
        .scheduler_tuning(scheduler_tuning_from_env())
        .size(16, 16)
        .build()
    else {
        tracing::warn!(
            target: "aether_perf",
            topo = %topo.name, workers,
            "sweep cell skipped: SubstrateHarness boot failed (likely no wgpu adapter)",
        );
        return None;
    };

    let n = topo.downstreams.len();
    let mut spawned_ok = true;
    for i in 0..n {
        let downstreams: Arc<[MailboxId]> = topo.downstreams[i].iter().map(|&j| relay_id(j)).collect();
        let sub = i.to_string();
        let config = RelayConfig { downstreams, work_iters: topo.work_iters[i] };
        if let Err(e) = tb.spawn_actor::<Relay>(Subname::Named(&sub), config, ()).finish() {
            tracing::warn!(target: "aether_perf", topo = %topo.name, relay = i, error = ?e, "relay spawn failed");
            spawned_ok = false;
            break;
        }
    }
    if !spawned_ok {
        return None;
    }
    // The real tier is always driven paced regardless of the sweep's
    // drive (ADR-0085 amendment); light / heavy keep it verbatim.
    let drive = drive_for_tier(drive, topo.tier);
    // `burst` is the per-tick `Ping` count: 1 in `Latency` (one
    // root per frame), `backlog` in `Saturate` (a deep ready queue
    // drained in one frame, iamacoffeepot/aether#1202). The
    // `Saturate` arm is reached only by Light / Heavy cells — the
    // real tier is forced paced by `drive_for_tier` above — so the
    // clamp below governs only flooding bursts. A relay writes
    // `2 + out_degree` trace-ring slots per inbound mail, so a
    // backlog that fans out wide laps the entry relay's per-actor
    // ring once `backlog * (2 + max_out_degree) > ring_cap`; clamp
    // each cell's burst to the deepest backlog its ring allows so
    // every cell stays measurable instead of silently truncating
    // (iamacoffeepot/aether#1226). Low-fan-out cells keep full
    // depth; a wide fan-out (e.g. `fanout-8`: `4096 / 10 = 409`)
    // drops to fit, and any future wider fan-out stays measurable
    // automatically.
    let burst = match drive {
        Drive::Latency { .. } => 1,
        Drive::Saturate { backlog } => {
            let ring_cap = u32::try_from(trace_ring_cap).unwrap_or(u32::MAX);
            let out_degree = u32::try_from(max_out_degree(topo)).unwrap_or(u32::MAX);
            let fanout_divisor = out_degree.saturating_add(2);
            backlog.min(ring_cap / fanout_divisor)
        }
    };
    if let Err(e) = tb.spawn_actor::<TickSource>(Subname::Named("src"), (relay_id(0), burst), ()).finish() {
        tracing::warn!(target: "aether_perf", topo = %topo.name, error = ?e, "tick source spawn failed");
        return None;
    }

    // Subscribe the source to the `Tick` lifecycle stage so
    // `advance` broadcasts a tick to it each frame (ADR-0082).
    let sub_req = LifecycleSubscribe { stage: Tick::ID.0, mailbox: ticksrc_id().0 }.encode_into_bytes();
    match tb.send_bytes_and_await("aether.lifecycle", LifecycleSubscribe::ID, sub_req) {
        Ok(reply) => match LifecycleSubscribeResult::decode_from_bytes(&reply) {
            Some(LifecycleSubscribeResult::Ok) => {}
            other => {
                tracing::warn!(target: "aether_perf", topo = %topo.name, ?other, "Tick subscribe failed");
                return None;
            }
        },
        Err(e) => {
            tracing::warn!(target: "aether_perf", topo = %topo.name, error = ?e, "Tick subscribe send failed");
            return None;
        }
    }

    // Per-actor rings (ADR-0086 Phase 3) self-bound at their
    // capacity, so there's no central node cap to clamp against —
    // run the full frame count. A busy relay's ring laps under a
    // long wide fan-out and self-reports it (handled at harvest).

    // Drive via the real lifecycle (per-tier drive resolved above).
    // Bracket the loop so the real tier's keep-up metric can compare
    // elapsed wall-clock against the paced budget
    // (iamacoffeepot/aether#1233).
    let drive_start = Instant::now();
    match drive {
        Drive::Latency { pace_hz: Some(hz), .. } => {
            let period = Duration::from_secs_f64(1.0 / hz as f64);
            for _ in 0..frames {
                let f = Instant::now();
                let _ = tb.advance(1, DEFAULT_TICK_DELTA_MICROS);
                if let Some(rem) = period.checked_sub(f.elapsed()) {
                    thread::sleep(rem);
                }
            }
        }
        Drive::Latency { pace_hz: None } => {
            let _ = tb.advance(frames, DEFAULT_TICK_DELTA_MICROS);
        }
        // Saturate: the tick source bursts `backlog` roots onto
        // relay 0's inbox on a single tick, and one `advance(1)`
        // drains the whole burst to quiescence in that frame
        // (iamacoffeepot/aether#1202). The pool contends on a deep
        // ready queue — the load the throughput metric captures —
        // instead of the one-root-settles-per-frame latency path.
        //
        // It advances exactly once regardless of `cfg.frames`: the
        // backlog *is* the offered load, so re-bursting every frame
        // would multiply it by `frames` and lap the 4096-entry trace
        // rings, tripping the truncation gate below and nulling the
        // rate (the bug the `frames > 1` regression test guards).
        Drive::Saturate { .. } => {
            let _ = tb.advance(1, DEFAULT_TICK_DELTA_MICROS);
        }
    }
    let drive_elapsed = drive_start.elapsed();

    // Harvest each participating actor's trace ring directly
    // (ADR-0086 Phase 3, decentralized trace): we built the
    // topology, so we know the tick source + relays by name — no
    // central window query, no root enumeration. Fold every ring
    // into one node set; the `Ping`-kind filter below isolates
    // relay hops (the per-actor `aether.trace.tail` query mail
    // carries a different kind and is dropped). Rings self-report
    // truncation: a relay ring (cap 4096) laps under a long wide
    // fan-out, leaving stats from the most-recent window — valid
    // percentiles, fewer samples.
    let mut names: Vec<String> = Vec::with_capacity(n + 1);
    names.push(format!("{TICKSRC_NS}:src"));
    names.extend((0..n).map(|i| format!("{RELAY_NS}:{i}")));

    let mut entries: Vec<TraceRingEntry> = Vec::new();
    let mut truncated = false;
    let mut harvest_failed = false;
    for name in &names {
        // `max: u32::MAX` clamps to the ring capacity — pull the
        // whole ring, `root: None` across every tree in the run.
        let req = TraceTail { max: u32::MAX, since: None, root: None }.encode_into_bytes();
        match tb.send_bytes_and_await(name, TraceTail::ID, req) {
            Ok(reply) => match TraceTailResult::decode_from_bytes(&reply) {
                Some(TraceTailResult::Ok { entries: ring, truncated_before, .. }) => {
                    truncated |= truncated_before.is_some();
                    entries.extend(ring);
                }
                Some(TraceTailResult::Err { error }) => {
                    tracing::warn!(target: "aether_perf", topo = %topo.name, %name, %error, "trace.tail error");
                    harvest_failed = true;
                    break;
                }
                None => {
                    tracing::warn!(target: "aether_perf", topo = %topo.name, %name, "trace.tail decode failed");
                    harvest_failed = true;
                    break;
                }
            },
            Err(e) => {
                tracing::warn!(target: "aether_perf", topo = %topo.name, %name, error = ?e, "trace.tail send failed");
                harvest_failed = true;
                break;
            }
        }
    }
    if harvest_failed {
        return None;
    }
    if truncated {
        tracing::warn!(
            target: "aether_perf",
            topo = %topo.name, workers,
            "a relay ring lapped during the run — stats are from the most-recent window",
        );
    }
    let mails = fold_nodes(entries);

    let mut construct = Vec::new();
    let mut queued = Vec::new();
    let mut drain = Vec::new();
    let mut handler = Vec::new();
    let mut depth = Vec::new();
    for node in &mails {
        if node.kind.0 != Ping::ID.0 {
            continue;
        }
        if let Some(recv) = node.t_received {
            if let Some(fin) = node.t_finished {
                handler.push(fin.0.saturating_sub(recv.0));
            }
            // iamacoffeepot/aether#1158: `t_construct_start` (blob
            // open) rides the `Sent` event, always present. The
            // four spans are non-overlapping and cover first-send →
            // handler-done: `construct` = blob open → flush-begin;
            // iamacoffeepot/aether#1150: `t_enqueue` (blob pickup)
            // lands with `Received`, so it is present exactly when
            // `t_received` is. `queued` = flush-begin → pickup;
            // `drain` = pickup → this mail's handler entry.
            if let Some(enq) = node.t_enqueue {
                construct.push(node.t_sent.0.saturating_sub(node.t_construct_start.0));
                queued.push(enq.0.saturating_sub(node.t_sent.0));
                drain.push(recv.0.saturating_sub(enq.0));
            }
        }
        if let Some(d) = node.enqueue_depth {
            depth.push(u64::from(d));
        }
    }

    // iamacoffeepot/aether#1202: throughput rides the *same* folded
    // nodes — completed = `Ping` nodes that reached `t_finished`,
    // and the drive elapsed is `max(t_finished) − min(t_construct
    // start)` across them. Only meaningful under `Saturate` (the
    // latency modes never build a backlog), and only when the
    // harvest is complete: a lapped ring drops finished nodes, so a
    // truncated cell would report a low rate — refuse it rather than
    // mislead.
    let throughput_mps = match drive {
        Drive::Saturate { .. } if !truncated => throughput_from_nodes(&mails),
        _ => None,
    };

    // iamacoffeepot/aether#1233: the real tier reports keep-up, not
    // span percentiles. Harvest each actor's plain-field `Ping`
    // counters out-of-band (the same name-addressed `send_and_await_reply`
    // flow as the trace harvest above) and sum them: `offered =
    // Σ sent`, `completed = Σ received`. Sidesteps the trace ring
    // entirely, which the real tier's fan-out laps. Only the real
    // tier runs paced, so only it has a meaningful elapsed-vs-expected.
    let keepup = if topo.tier == Tier::Real {
        harvest_keepup(&mut tb, &names, &topo.name, drive, frames, drive_elapsed)
    } else {
        None
    };

    Some(CellSamples {
        workers,
        topo: topo.name.clone(),
        tier: topo.tier,
        boot_handoff_nanos,
        construct,
        queued,
        drain,
        handler,
        depth,
        throughput_mps,
        keepup,
    })
}
