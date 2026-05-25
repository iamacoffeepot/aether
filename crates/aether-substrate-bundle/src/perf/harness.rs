//! Lifecycle latency sweep engine (iamacoffeepot/aether#1057, #1077).
//!
//! The reusable core of the latency harness, lifted out of the
//! `#[cfg(test)]` `mail_latency` module so the `perf-trial` binary can
//! drive it (iamacoffeepot/aether#1077). [`run_sweep`] wires synthetic
//! relay actors into a topology, drives the substrate's real lifecycle
//! (`advance` → `Tick` fan-out → a tick-reactive source → the relay
//! chain), harvests the resident trace ring (ADR-0080) once per cell,
//! and returns per-cell [`CellResult`] percentiles. It performs no I/O
//! itself — callers render (the harness test prints a table; the
//! `perf-trial` bin emits JSON).
//!
//! **Worker count is the dominant variable.** Post issue #635 actors
//! are `Pooled` by default — they share a worker pool, not one thread
//! each. A depth chain with one root in flight serialises regardless of
//! pool size; a fan-out either parallelises across workers or queues on
//! a small pool. So the sweep takes the worker set as an axis.

use std::env;
use std::hint::black_box;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use aether_capabilities::trace_walk::fold_nodes;
use aether_data::{Kind, KindId, MailboxId, mailbox_id_from_name};
use aether_kinds::trace::{TraceRingEntry, TraceTail, TraceTailResult};
use aether_kinds::{SubscribeInput, SubscribeInputResult, Tick};
use aether_substrate::{BootError, NativeActor, NativeCtx, NativeDispatch, NativeInitCtx, Subname};

use crate::test_bench::TestBench;

/// Fire-and-forward payload the relay actors pass along. The `seq`
/// field is carried for legibility when eyeballing a trace; the relay
/// forwards the bytes verbatim, so the wire shape is irrelevant to the
/// measurement. The schema-hashed `ID` is what the relay matches and
/// the trace records.
#[repr(C)]
#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    bytemuck::Pod,
    bytemuck::Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "mlat.ping")]
pub struct Ping {
    pub seq: u32,
}

/// Bounded, deterministic CPU spin: an FNV-1a-style integer mix run
/// `iters` times. Real compute that occupies the worker thread for the
/// duration — deliberately **not** `thread::sleep`, which would free the
/// core and turn the measurement into park/wake latency instead of
/// compute contention (iamacoffeepot/aether#1074). `black_box` on both
/// the loop input and the accumulator stops the optimizer eliding the
/// loop or folding it to a constant. `iters == 0` is a true no-op, so
/// the trivial topologies stay byte-for-byte unchanged.
#[inline(never)]
fn busy_spin(iters: u64) {
    let mut acc: u64 = 0xcbf2_9ce4_8422_2325; // FNV-1a 64-bit offset basis
    for i in 0..iters {
        acc ^= black_box(i);
        acc = acc.wrapping_mul(0x0000_0100_0000_01b3); // FNV-1a 64-bit prime
    }
    black_box(acc);
}

/// Spawn config for a [`Relay`]: who to forward to, and how much CPU
/// work to burn per inbound `Ping` before forwarding. `work_iters == 0`
/// is the trivial relay; a non-zero count makes a leaf contend for a
/// core (the parallel-heavy regime, iamacoffeepot/aether#1074).
pub struct RelayConfig {
    pub downstreams: Arc<[MailboxId]>,
    pub work_iters: u64,
}

/// A relay forwards each inbound `Ping` to every configured downstream
/// mailbox, inheriting the trace lineage so the whole topology is one
/// causal tree. A leaf relay (empty `downstreams`) just receives and
/// returns. Before forwarding it burns `work_iters` of `busy_spin`
/// CPU — zero by default, so trivial topologies are unchanged. Pooled
/// (the `Actor` default).
pub struct Relay {
    downstreams: Arc<[MailboxId]>,
    work_iters: u64,
}

impl aether_actor::Actor for Relay {
    const NAMESPACE: &'static str = "mlat.relay";
}
impl aether_actor::Instanced for Relay {}
impl aether_actor::HandlesKind<Ping> for Relay {}
impl NativeActor for Relay {
    type Config = RelayConfig;
    fn init(config: Self::Config, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self {
            downstreams: config.downstreams,
            work_iters: config.work_iters,
        })
    }
}
impl NativeDispatch for Relay {
    fn __aether_dispatch_envelope(
        &mut self,
        ctx: &mut NativeCtx<'_>,
        kind: KindId,
        payload: &[u8],
    ) -> Option<()> {
        if kind.0 != Ping::ID.0 {
            return None;
        }
        // Burn the configured CPU budget on this worker thread before
        // forwarding. With heavy leaves and idle cores this is what makes
        // scattering children across workers pay off — the contention the
        // trivial harness can't exhibit (iamacoffeepot/aether#1074).
        busy_spin(self.work_iters);
        // Forward the bytes verbatim to each downstream. Each push
        // stamps its own `t_sent`, so later children in a fan-out reveal
        // any per-child enqueue skew.
        for &down in self.downstreams.iter() {
            let _ = ctx.send_envelope_traced(down, Ping::ID, payload);
        }
        Some(())
    }
}

const RELAY_NS: &str = "mlat.relay";

/// Deterministic `MailboxId` for relay instance `i`. Mirrors the
/// substrate's `mailbox_id_from_name("{NAMESPACE}:{subname}")` so the
/// whole topology can be wired from precomputed ids before any actor is
/// spawned (sidesteps spawn-ordering between a relay and its
/// downstreams).
#[must_use]
pub fn relay_id(i: usize) -> MailboxId {
    MailboxId(mailbox_id_from_name(&format!("{RELAY_NS}:{i}")).0)
}

/// Lifecycle bridge for the sweep: subscribed to the `Tick` input
/// stream, it emits one `Ping` into the entry relay per frame,
/// inheriting the tick's trace lineage so the whole per-frame chain is
/// one causal tree. The honest stand-in for a real tick-reactive
/// component — the substrate's own `Tick` fan-out drives the work, no
/// synthetic injector, no per-root settlement block.
pub struct TickSource {
    entry: MailboxId,
    seq: u32,
}

impl aether_actor::Actor for TickSource {
    const NAMESPACE: &'static str = "mlat.ticksrc";
}
impl aether_actor::Instanced for TickSource {}
impl aether_actor::HandlesKind<Tick> for TickSource {}
impl NativeActor for TickSource {
    type Config = MailboxId;
    fn init(entry: Self::Config, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self { entry, seq: 0 })
    }
}
impl NativeDispatch for TickSource {
    fn __aether_dispatch_envelope(
        &mut self,
        ctx: &mut NativeCtx<'_>,
        kind: KindId,
        _payload: &[u8],
    ) -> Option<()> {
        if kind.0 != Tick::ID.0 {
            return None;
        }
        let bytes = Ping { seq: self.seq }.encode_into_bytes();
        self.seq = self.seq.wrapping_add(1);
        let _ = ctx.send_envelope_traced(self.entry, Ping::ID, &bytes);
        Some(())
    }
}

const TICKSRC_NS: &str = "mlat.ticksrc";

/// Deterministic id for the single tick source (subname `"src"`).
#[must_use]
pub fn ticksrc_id() -> MailboxId {
    MailboxId(mailbox_id_from_name(&format!("{TICKSRC_NS}:src")).0)
}

/// A topology is a DAG over relay indices: `downstreams[i]` lists the
/// relays that relay `i` forwards to. Relay 0 is always the entry. The
/// number of relays is `downstreams.len()`. `work_iters[i]` is the CPU
/// spin budget relay `i` burns per inbound `Ping` (see `busy_spin`) —
/// all-zero for the trivial topologies, non-zero on the heavy ones
/// (iamacoffeepot/aether#1074). `work_iters.len() == downstreams.len()`.
#[derive(Clone)]
pub struct Topology {
    pub name: String,
    pub downstreams: Vec<Vec<usize>>,
    pub work_iters: Vec<u64>,
}

/// `0 -> 1 -> ... -> d-1`. Each relay forwards to the next; the last is
/// a leaf.
#[must_use]
pub fn depth_chain(d: usize) -> Topology {
    let downstreams: Vec<Vec<usize>> = (0..d)
        .map(|i| if i + 1 < d { vec![i + 1] } else { vec![] })
        .collect();
    Topology {
        name: format!("depth-{d}"),
        work_iters: vec![0; downstreams.len()],
        downstreams,
    }
}

/// `0 -> {1, 2, ..., b}`. Entry fans to b leaves.
#[must_use]
pub fn fanout(b: usize) -> Topology {
    let mut downstreams = vec![vec![]; b + 1];
    downstreams[0] = (1..=b).collect();
    Topology {
        name: format!("fanout-{b}"),
        work_iters: vec![0; downstreams.len()],
        downstreams,
    }
}

/// A `fanout(b)` whose `b` leaves each burn `work_iters` of `busy_spin`
/// CPU per `Ping` (the entry stays trivial). This is the workload the
/// trivial harness cannot exhibit: with enough per-leaf work and idle
/// cores, scattering the leaves across workers (parallelism) beats
/// keeping them on the producing worker (locality). Sweeping
/// `work_iters` locates the crossover where a static keep-local policy
/// flips from win to regression (iamacoffeepot/aether#1074).
///
/// `work_iters == 0` reproduces [`fanout`] exactly (modulo the `-heavy`
/// name), so callers can include it unconditionally without perturbing
/// the trivial baseline.
#[must_use]
pub fn fanout_heavy(b: usize, work_iters: u64) -> Topology {
    let mut t = fanout(b);
    t.name = format!("fanout-{b}-heavy");
    for leaf in 1..=b {
        t.work_iters[leaf] = work_iters;
    }
    t
}

/// `A -> {B, C} -> {D, E}, {E, F}`. E (index 4) has two parents (B and
/// C) — the shared-node contention case.
#[must_use]
pub fn two_level_tree() -> Topology {
    let downstreams = vec![
        vec![1, 2], // A -> B, C
        vec![3, 4], // B -> D, E
        vec![4, 5], // C -> E, F
        vec![],     // D
        vec![],     // E
        vec![],     // F
    ];
    Topology {
        name: "tree-A-BC-DEEF".to_owned(),
        work_iters: vec![0; downstreams.len()],
        downstreams,
    }
}

/// The full default topology set (depth chains 1/2/4/8, fan-outs 2/4/8,
/// the two-level tree) — what the on-demand `lifecycle_latency_observe`
/// harness sweeps.
#[must_use]
pub fn default_topologies() -> Vec<Topology> {
    let mut t = Vec::new();
    for d in [1usize, 2, 4, 8] {
        t.push(depth_chain(d));
    }
    for b in [2usize, 4, 8] {
        t.push(fanout(b));
    }
    t.push(two_level_tree());
    t
}

/// p50 / p90 / p99 / max over a sample set, plus the sample count. All
/// values are nanoseconds.
#[derive(Clone, Copy, Default, Debug)]
pub struct Stats {
    pub p50: u64,
    pub p90: u64,
    pub p99: u64,
    pub max: u64,
    pub n: usize,
}

#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn nearest_rank(sorted: &[u64], q: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() - 1) as f64 * q).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

/// Summarise a sample set into [`Stats`] (consumes + sorts the input).
#[must_use]
pub fn summarize(mut samples: Vec<u64>) -> Stats {
    let n = samples.len();
    if n == 0 {
        return Stats::default();
    }
    samples.sort_unstable();
    Stats {
        p50: nearest_rank(&samples, 0.50),
        p90: nearest_rank(&samples, 0.90),
        p99: nearest_rank(&samples, 0.99),
        max: samples[n - 1],
        n,
    }
}

/// One fully-measured cell (per worker count × topology).
#[derive(Clone, Debug)]
pub struct CellResult {
    pub workers: usize,
    pub topo: String,
    /// `t_received − t_sent` — the whole hop. Kept as the headline /
    /// backward-compatible metric; equals `send_enqueue + residence`
    /// within clock granularity.
    pub hop: Stats,
    /// iamacoffeepot/aether#1134: `t_enqueue − t_sent` — producer-side
    /// span (rest of the sender's handler + flush + blob pickup + demux
    /// up to the recipient-inbox deposit).
    pub send_enqueue: Stats,
    /// iamacoffeepot/aether#1134: `t_received − t_enqueue` — consumer-side
    /// queue residence (deposit → the recipient's dispatcher picks it up
    /// = slot schedule + worker wakeup + recv). The span the fast-path
    /// inbox-bypass would attack.
    pub residence: Stats,
    /// `t_finished − t_received` — in-handler work.
    pub handler: Stats,
    /// iamacoffeepot/aether#1134: scheduler ready-queue depth at deposit
    /// (`enqueue_depth`), as a distribution — *counts, not nanoseconds*.
    /// p50 ≈ 0 means residence is dominated by wakeup (empty queue); a
    /// rising tail means wait-behind-N (offered load).
    pub depth: Stats,
}

/// Inputs to one sweep. `workers` is the outer axis (pool sizes);
/// `topologies` the inner. `frames` advances per cell; `pace_hz`
/// `Some(hz)` paces one frame per period (workers park between frames →
/// realistic frame-loop latency), `None` runs flat-out (warm — isolates
/// per-hop dispatch cost).
#[derive(Clone)]
pub struct SweepConfig {
    pub workers: Vec<usize>,
    pub topologies: Vec<Topology>,
    pub frames: u32,
    pub pace_hz: Option<u64>,
}

/// Read the optional `AETHER_LAT_PACE_HZ` pacing override (frames/sec;
/// `None` = flat-out / warm). Shared by the on-demand harness test and
/// the `perf-trial` bin so the parse lives in one place.
#[must_use]
pub fn pace_hz_from_env() -> Option<u64> {
    env::var("AETHER_LAT_PACE_HZ")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|&h| h > 0)
}

/// Read the optional heavy-leaf CPU work knob `AETHER_LAT_HEAVY_WORK` (a
/// raw `busy_spin` iteration count per heavy leaf handler; see
/// [`fanout_heavy`]). Unset, unparseable, or `0` means no heavy work —
/// callers omit the heavy topology entirely, so the trivial sweep is
/// byte-for-byte unchanged (iamacoffeepot/aether#1074).
///
/// A raw iteration count, not a microsecond budget, keeps the work
/// *identical* across processes so a paired base-vs-candidate comparison
/// (ADR-0085) isn't confounded by per-run calibration drift. To target a
/// wall-clock budget, set a count and read the actual per-leaf
/// microseconds off the harness's HANDLER DUR column (it already
/// measures `t_finished - t_received`), then adjust.
#[must_use]
pub fn heavy_work_iters_from_env() -> u64 {
    env::var("AETHER_LAT_HEAVY_WORK")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

/// Parse the optional `AETHER_LAT_WIDE_FANOUT` knob — a comma list of
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
    let Ok(spec) = env::var("AETHER_LAT_WIDE_FANOUT") else {
        return Vec::new();
    };
    let mut out: Vec<usize> = spec
        .split(',')
        .filter_map(|t| t.trim().parse::<usize>().ok())
        .filter(|&w| w > 0)
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// Drive the sweep and return per-cell percentiles. Each cell boots a
/// fresh [`TestBench`], wires the topology + tick source, advances, then
/// harvests every participating actor's per-actor trace ring (ADR-0086
/// Phase 3) directly by name and folds them into one node set. A cell
/// whose bench fails to boot (no wgpu adapter) or whose ring harvest
/// errors is logged via `tracing` and skipped — so a driverless box
/// returns fewer cells (possibly empty) rather than panicking.
///
/// The full frame count runs unclamped: the per-actor rings self-bound
/// at their capacity and self-report truncation, so a long wide fan-out
/// laps a busy relay's ring and the cell's stats come from the
/// most-recent window (valid percentiles, fewer samples) with a logged
/// note, instead of being capped up front.
#[allow(clippy::cast_precision_loss, clippy::too_many_lines)]
#[must_use]
pub fn run_sweep(cfg: &SweepConfig) -> Vec<CellResult> {
    let mut rows: Vec<CellResult> = Vec::new();

    for &workers in &cfg.workers {
        for topo in &cfg.topologies {
            let Ok(mut tb) = TestBench::builder()
                .with_workers(Some(workers))
                .size(16, 16)
                .build()
            else {
                tracing::warn!(
                    target: "aether_perf",
                    topo = %topo.name, workers,
                    "sweep cell skipped: TestBench boot failed (likely no wgpu adapter)",
                );
                continue;
            };

            let n = topo.downstreams.len();
            let mut spawned_ok = true;
            for i in 0..n {
                let downstreams: Arc<[MailboxId]> =
                    topo.downstreams[i].iter().map(|&j| relay_id(j)).collect();
                let sub = i.to_string();
                let config = RelayConfig {
                    downstreams,
                    work_iters: topo.work_iters[i],
                };
                if let Err(e) = tb
                    .spawn_actor::<Relay>(Subname::Named(&sub), config)
                    .finish()
                {
                    tracing::warn!(target: "aether_perf", topo = %topo.name, relay = i, error = ?e, "relay spawn failed");
                    spawned_ok = false;
                    break;
                }
            }
            if !spawned_ok {
                continue;
            }
            if let Err(e) = tb
                .spawn_actor::<TickSource>(Subname::Named("src"), relay_id(0))
                .finish()
            {
                tracing::warn!(target: "aether_perf", topo = %topo.name, error = ?e, "tick source spawn failed");
                continue;
            }

            // Wire the source into the platform `Tick` stream so
            // `advance` delivers a tick to it each frame (ADR-0021).
            let sub_req = SubscribeInput {
                kind: Tick::ID,
                mailbox: ticksrc_id(),
            }
            .encode_into_bytes();
            match tb.send_bytes_and_await("aether.input", SubscribeInput::ID, sub_req) {
                Ok(reply) => match SubscribeInputResult::decode_from_bytes(&reply) {
                    Some(SubscribeInputResult::Ok) => {}
                    other => {
                        tracing::warn!(target: "aether_perf", topo = %topo.name, ?other, "Tick subscribe failed");
                        continue;
                    }
                },
                Err(e) => {
                    tracing::warn!(target: "aether_perf", topo = %topo.name, error = ?e, "Tick subscribe send failed");
                    continue;
                }
            }

            // Per-actor rings (ADR-0086 Phase 3) self-bound at their
            // capacity, so there's no central node cap to clamp against —
            // run the full frame count. A busy relay's ring laps under a
            // long wide fan-out and self-reports it (handled at harvest).
            let frames = cfg.frames;

            // Drive via the real lifecycle.
            match cfg.pace_hz {
                Some(hz) => {
                    let period = Duration::from_secs_f64(1.0 / hz as f64);
                    for _ in 0..frames {
                        let f = Instant::now();
                        let _ = tb.advance(1);
                        if let Some(rem) = period.checked_sub(f.elapsed()) {
                            thread::sleep(rem);
                        }
                    }
                }
                None => {
                    let _ = tb.advance(frames);
                }
            }

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
                let req = TraceTail {
                    max: u32::MAX,
                    since: None,
                    root: None,
                }
                .encode_into_bytes();
                match tb.send_bytes_and_await(name, TraceTail::ID, req) {
                    Ok(reply) => match TraceTailResult::decode_from_bytes(&reply) {
                        Some(TraceTailResult::Ok {
                            entries: ring,
                            truncated_before,
                            ..
                        }) => {
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
                continue;
            }
            if truncated {
                tracing::warn!(
                    target: "aether_perf",
                    topo = %topo.name, workers,
                    "a relay ring lapped during the run — stats are from the most-recent window",
                );
            }
            let mails = fold_nodes(entries);

            let mut hop = Vec::new();
            let mut send_enqueue = Vec::new();
            let mut residence = Vec::new();
            let mut handler = Vec::new();
            let mut depth = Vec::new();
            for node in &mails {
                if node.kind.0 != Ping::ID.0 {
                    continue;
                }
                if let Some(recv) = node.t_received {
                    hop.push(recv.0.saturating_sub(node.t_sent.0));
                    if let Some(fin) = node.t_finished {
                        handler.push(fin.0.saturating_sub(recv.0));
                    }
                    // iamacoffeepot/aether#1134: split the hop at the
                    // deposit instant. `t_enqueue` lands with `Received`,
                    // so it is present exactly when `t_received` is.
                    if let Some(enq) = node.t_enqueue {
                        send_enqueue.push(enq.0.saturating_sub(node.t_sent.0));
                        residence.push(recv.0.saturating_sub(enq.0));
                    }
                }
                if let Some(d) = node.enqueue_depth {
                    depth.push(u64::from(d));
                }
            }
            rows.push(CellResult {
                workers,
                topo: topo.name.clone(),
                hop: summarize(hop),
                send_enqueue: summarize(send_enqueue),
                residence: summarize(residence),
                handler: summarize(handler),
                depth: summarize(depth),
            });
        }
    }
    rows
}
