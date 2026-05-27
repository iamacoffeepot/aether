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

use aether_actor::trace_ring::DEFAULT_TRACE_RING_CAP;
use aether_capabilities::trace_walk::fold_nodes;
use aether_data::{Kind, KindId, MailboxId, mailbox_id_from_name};
use aether_kinds::trace::{MailNodeWire, TraceRingEntry, TraceTail, TraceTailResult};
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
/// stream, it emits a burst of `burst` `Ping`s into the entry relay per
/// frame, each inheriting the tick's trace lineage so the whole
/// per-frame fan-out is one causal forest. The honest stand-in for a
/// real tick-reactive component — the substrate's own `Tick` fan-out
/// drives the work, no synthetic injector, no per-root settlement block.
///
/// `burst == 1` is the latency regime (one root per tick, settles within
/// its frame). A larger `burst` is the saturation regime
/// (iamacoffeepot/aether#1202): the whole burst lands on relay 0's inbox
/// in one tick, so a single `advance(1)` drains a deep ready queue — the
/// contention the per-frame `advance` quiescence otherwise prevents.
pub struct TickSource {
    entry: MailboxId,
    burst: u32,
    seq: u32,
}

impl aether_actor::Actor for TickSource {
    const NAMESPACE: &'static str = "mlat.ticksrc";
}
impl aether_actor::Instanced for TickSource {}
impl aether_actor::HandlesKind<Tick> for TickSource {}
impl NativeActor for TickSource {
    /// `(entry, burst)`: the relay-0 mailbox and the number of `Ping`s to
    /// emit per `Tick` (`1` in `Latency`, `backlog` in `Saturate`).
    type Config = (MailboxId, u32);
    fn init(config: Self::Config, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        let (entry, burst) = config;
        Ok(Self {
            entry,
            burst,
            seq: 0,
        })
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
        for _ in 0..self.burst {
            let bytes = Ping { seq: self.seq }.encode_into_bytes();
            self.seq = self.seq.wrapping_add(1);
            let _ = ctx.send_envelope_traced(self.entry, Ping::ID, &bytes);
        }
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

/// [`two_level_tree`] with **every** node (A–F) burning `work_iters` of
/// `busy_spin` CPU per `Ping` — a *uniform*-cost heavy cascade. This is the
/// multi-blob workload that exercises the keep-local **time budget**
/// (iamacoffeepot/aether#1160): the spill decision for the deepest blob
/// fires after the interior nodes (A/B/C) have run, so with heavy interiors
/// the burst's elapsed exceeds the time budget and the blob spills →
/// parallelises, matching the `cap == 1` baseline. A *mail-count-only*
/// budget keeps it local and serialises the heavy leaves — a regression the
/// time budget exists to prevent.
///
/// `work_iters == 0` reproduces [`two_level_tree`] exactly (modulo the
/// `-heavy` name).
#[must_use]
pub fn two_level_tree_heavy(work_iters: u64) -> Topology {
    let mut t = two_level_tree();
    "tree-A-BC-DEEF-heavy".clone_into(&mut t.name);
    for w in &mut t.work_iters {
        *w = work_iters;
    }
    t
}

/// [`two_level_tree`] with only the **leaves** (D, E, F) heavy and the
/// interior routers (A, B, C) trivial — a *non-uniform* "trivial router →
/// heavy worker" cascade. This is the time budget's **blind spot**
/// (iamacoffeepot/aether#1160): the spill decision fires *before* the heavy
/// leaves run, so the burst's elapsed (only the trivial interiors) never
/// exceeds the time budget, the deepest blob is kept local, and the heavy
/// leaves serialise — a regression that a *past-elapsed* budget structurally
/// cannot catch (the cost is in the blob being scheduled, i.e. the future).
/// Only a cost-aware bound (per-handler EWMA, #1128) resolves it. Included
/// so the sweep measures the blind spot honestly rather than hiding it.
#[must_use]
pub fn two_level_tree_router_heavy(work_iters: u64) -> Topology {
    let mut t = two_level_tree();
    "tree-A-BC-DEEF-routed".clone_into(&mut t.name);
    for leaf in [3usize, 4, 5] {
        t.work_iters[leaf] = work_iters;
    }
    t
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

/// One measured cell's **raw** samples (per worker count × topology),
/// before percentile collapse. The latency spans are nanosecond
/// samples; `depth` is the scheduler ready-queue length distribution
/// (counts). [`Self::summarize`] folds these to a [`CellResult`]; the
/// `perf-plot` bin (iamacoffeepot/aether#1155) renders them directly.
#[derive(Clone, Debug)]
pub struct CellSamples {
    pub workers: usize,
    pub topo: String,
    /// iamacoffeepot/aether#1158: `t_sent − t_construct_start` (flush-begin
    /// → blob open) — the producer building the blob.
    pub construct: Vec<u64>,
    pub queued: Vec<u64>,
    pub drain: Vec<u64>,
    pub handler: Vec<u64>,
    pub depth: Vec<u64>,
    /// iamacoffeepot/aether#1202: completed mails/sec under saturation —
    /// `Some` in `Drive::Saturate` (computed from the same folded nodes
    /// the latency spans come from), `None` in `Drive::Latency`. A cell
    /// whose entry ring lapped reports `None` rather than a wrong rate.
    pub throughput_mps: Option<f64>,
}

impl CellSamples {
    /// Collapse each span's samples to [`Stats`] percentiles.
    #[must_use]
    pub fn summarize(self) -> CellResult {
        CellResult {
            workers: self.workers,
            topo: self.topo,
            construct: summarize(self.construct),
            queued: summarize(self.queued),
            drain: summarize(self.drain),
            handler: summarize(self.handler),
            depth: summarize(self.depth),
            throughput_mps: self.throughput_mps,
        }
    }
}

/// One fully-measured cell (per worker count × topology).
#[derive(Clone, Debug)]
pub struct CellResult {
    pub workers: usize,
    pub topo: String,
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
    /// iamacoffeepot/aether#1202: completed mails/sec under saturation.
    /// `Some` only in `Drive::Saturate` (`None` for a latency cell);
    /// `None` too when the entry ring lapped, so a truncated cell never
    /// reports a wrong rate.
    pub throughput_mps: Option<f64>,
}

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
///   measures completed mails/sec. `TestBench::advance` drains the queue
///   to quiescence every frame (`bench.rs:630`), so one Ping per tick can
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

/// Read the optional `AETHER_LATENCY_PACE_HZ` pacing override (frames/sec;
/// `None` = flat-out / warm). Shared by the on-demand harness test and
/// the `perf-trial` bin so the parse lives in one place.
#[must_use]
pub fn pace_hz_from_env() -> Option<u64> {
    env::var("AETHER_LATENCY_PACE_HZ")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|&h| h > 0)
}

/// Default per-tick `Ping` burst for a `Saturate` cell when
/// `AETHER_PERF_BACKLOG` is unset. Sized comfortably under the per-actor
/// trace ring capacity ([`DEFAULT_TRACE_RING_CAP`]) so a default-backlog
/// run never laps the ring (see [`saturate_backlog_from_env`]).
pub const DEFAULT_SATURATE_BACKLOG: u32 = 512;

/// Read the per-tick saturation backlog from `AETHER_PERF_BACKLOG`
/// (iamacoffeepot/aether#1202), defaulting to [`DEFAULT_SATURATE_BACKLOG`]
/// when unset / unparseable / `0`. Clamped to the per-actor trace ring
/// capacity ([`DEFAULT_TRACE_RING_CAP`]): a burst larger than the ring
/// laps the entry relay's ring, which the harvest detects as `truncated`
/// and the cell then reports no rate for. Clamping up front keeps a
/// merely-large backlog measurable instead of silently truncated.
#[must_use]
pub fn saturate_backlog_from_env() -> u32 {
    let cap = u32::try_from(DEFAULT_TRACE_RING_CAP).unwrap_or(u32::MAX);
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
        Ok("saturate") => Drive::Saturate {
            backlog: saturate_backlog_from_env(),
        },
        _ => Drive::Latency {
            pace_hz: pace_hz_from_env(),
        },
    }
}

/// Read the optional heavy-leaf CPU work knob `AETHER_LATENCY_HEAVY_WORK` (a
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
    env::var("AETHER_LATENCY_HEAVY_WORK")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
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

/// Parse `AETHER_PERF_TOPOS` (`ci` — a chain/fan-out/tree subset — or
/// `full`), then append the opt-in heavy (`AETHER_LATENCY_HEAVY_WORK`) and
/// wide (`AETHER_LATENCY_WIDE_FANOUT`) fan-outs. Default `ci`. Shared by the
/// `perf-trial` and `perf-plot` bins.
#[must_use]
pub fn parse_topologies() -> Vec<Topology> {
    let mut topos = match env::var("AETHER_PERF_TOPOS").as_deref() {
        Ok("full") => default_topologies(),
        _ => vec![
            depth_chain(1),
            depth_chain(8),
            fanout(4),
            fanout(8),
            two_level_tree(),
        ],
    };
    let heavy = heavy_work_iters_from_env();
    if heavy > 0 {
        for b in [4usize, 8] {
            topos.push(fanout_heavy(b, heavy));
        }
        // The narrow-heavy multi-blob cascades that stress the keep-local
        // time budget (iamacoffeepot/aether#1160): uniform-heavy (the valve
        // fires) and trivial-router→heavy-leaf (the valve's blind spot).
        topos.push(two_level_tree_heavy(heavy));
        topos.push(two_level_tree_router_heavy(heavy));
    }
    for w in wide_fanout_widths_from_env() {
        topos.push(fanout(w));
    }
    topos
}

/// Completed mails/sec from a cell's folded trace nodes
/// (iamacoffeepot/aether#1202). Completed = `Ping` nodes that reached
/// `t_finished`; the drive elapsed is `max(t_finished) − min(t_construct
/// start)` across those nodes (construct-start is the earliest instant any
/// participating mail was built, the honest start of the burst's
/// processing). Returns `None` when nothing completed or the window
/// collapsed to zero (single sample / clock-coincident), so the caller
/// stores `None` rather than a divide-by-zero or infinite rate.
#[allow(clippy::cast_precision_loss)]
fn throughput_from_nodes(mails: &[MailNodeWire]) -> Option<f64> {
    let mut completed = 0u64;
    let mut min_start = u64::MAX;
    let mut max_finish = 0u64;
    for node in mails {
        if node.kind.0 != Ping::ID.0 {
            continue;
        }
        let Some(fin) = node.t_finished else { continue };
        completed += 1;
        min_start = min_start.min(node.t_construct_start.0);
        max_finish = max_finish.max(fin.0);
    }
    if completed == 0 || max_finish <= min_start {
        return None;
    }
    let elapsed_secs = (max_finish - min_start) as f64 / 1e9;
    if elapsed_secs <= 0.0 {
        return None;
    }
    Some(completed as f64 / elapsed_secs)
}

/// Drive the sweep and return each cell's **raw** per-span samples
/// (un-summarized). [`run_sweep`] wraps this and collapses to
/// [`CellResult`]; the `perf-plot` bin (iamacoffeepot/aether#1155) reads
/// the raw samples to render distribution plots, which the percentiles
/// can't show.
#[allow(clippy::cast_precision_loss, clippy::too_many_lines)]
#[must_use]
pub fn run_sweep_samples(cfg: &SweepConfig) -> Vec<CellSamples> {
    let mut rows: Vec<CellSamples> = Vec::new();

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
            // `burst` is the per-tick `Ping` count: 1 in `Latency` (one
            // root per frame), `backlog` in `Saturate` (a deep ready queue
            // drained in one frame, iamacoffeepot/aether#1202).
            let burst = match cfg.drive {
                Drive::Latency { .. } => 1,
                Drive::Saturate { backlog } => backlog,
            };
            if let Err(e) = tb
                .spawn_actor::<TickSource>(Subname::Named("src"), (relay_id(0), burst))
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
            match cfg.drive {
                Drive::Latency {
                    pace_hz: Some(hz), ..
                } => {
                    let period = Duration::from_secs_f64(1.0 / hz as f64);
                    for _ in 0..frames {
                        let f = Instant::now();
                        let _ = tb.advance(1);
                        if let Some(rem) = period.checked_sub(f.elapsed()) {
                            thread::sleep(rem);
                        }
                    }
                }
                Drive::Latency { pace_hz: None } => {
                    let _ = tb.advance(frames);
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
                    let _ = tb.advance(1);
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
            let throughput_mps = match cfg.drive {
                Drive::Saturate { .. } if !truncated => throughput_from_nodes(&mails),
                _ => None,
            };

            rows.push(CellSamples {
                workers,
                topo: topo.name.clone(),
                construct,
                queued,
                drain,
                handler,
                depth,
                throughput_mps,
            });
        }
    }
    rows
}

/// Drive the sweep and return per-cell percentiles. Thin wrapper over
/// [`run_sweep_samples`] that collapses each cell's raw samples to
/// [`Stats`]; the historical entry point for `perf-trial` and the
/// on-demand observe table.
#[must_use]
pub fn run_sweep(cfg: &SweepConfig) -> Vec<CellResult> {
    run_sweep_samples(cfg)
        .into_iter()
        .map(CellSamples::summarize)
        .collect()
}

#[cfg(test)]
#[allow(clippy::print_stderr)]
mod tests {
    use std::sync::Mutex;

    use super::*;

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
        assert!(
            mps.is_finite() && mps > 0.0,
            "throughput must be positive and finite, got {mps}"
        );
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
            assert!(
                mps.is_finite() && mps > 0.0,
                "rate must be positive + finite: {mps}"
            );
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
        let mps = cell
            .throughput_mps
            .expect("frames>1 saturate must still report a rate, not truncate to None");
        assert!(
            mps.is_finite() && mps > 0.0,
            "rate must be positive + finite: {mps}"
        );
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
        assert!(
            cell.throughput_mps.is_none(),
            "latency mode must not report a throughput rate"
        );
    }

    #[test]
    fn over_capacity_backlog_flags_truncation_not_a_wrong_rate() {
        // A backlog past the per-actor ring capacity laps the entry
        // source's ring (it holds one `Sent` per root). The harvest detects
        // the lap and the cell reports no rate, rather than dividing an
        // undercounted completed-count by the wall window
        // (iamacoffeepot/aether#1202). depth-1 is the cheapest topology, so
        // the lap is on the source ring's root count, not a busy relay's
        // fan-out. The normal env path clamps below the cap, so this
        // over-cap value is fed straight to the sweep.
        let cap = u32::try_from(DEFAULT_TRACE_RING_CAP).unwrap_or(u32::MAX);
        let Some(cell) = saturate_cell(2, depth_chain(1), cap + 1) else {
            eprintln!("skipping: no wgpu adapter");
            return;
        };
        assert!(
            cell.throughput_mps.is_none(),
            "an over-capacity backlog must flag truncation (no rate), not report a wrong one"
        );
    }

    #[test]
    fn over_capacity_backlog_is_clamped_by_env_parse() {
        // The env parse clamps a backlog past the trace ring capacity so a
        // merely-large `AETHER_PERF_BACKLOG` stays measurable. Serialised
        // against the other env-reading test via a shared lock, since
        // nextest runs tests in one process across threads.
        let _guard = ENV_LOCK.lock().expect("env lock");
        let cap = u32::try_from(DEFAULT_TRACE_RING_CAP).unwrap_or(u32::MAX);
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
        assert_eq!(
            parsed, cap,
            "an over-capacity backlog clamps to the ring cap"
        );
    }

    /// Serialises the `AETHER_PERF_BACKLOG`-mutating test against any other
    /// env-reading test in this module.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn throughput_from_nodes_handles_degenerate_windows() {
        // No completed nodes → no rate (rather than a divide-by-zero).
        assert!(throughput_from_nodes(&[]).is_none());
    }
}
