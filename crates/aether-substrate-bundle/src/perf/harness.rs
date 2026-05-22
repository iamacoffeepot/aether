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

use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use aether_data::{Kind, KindId, MailboxId, mailbox_id_from_name};
use aether_kinds::trace::{
    DescribeWindow, DescribeWindowResult, TRACE_OBSERVER_MAILBOX_NAME, TraceWindow,
};
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

/// A relay forwards each inbound `Ping` to every configured downstream
/// mailbox, inheriting the trace lineage so the whole topology is one
/// causal tree. A leaf relay (empty `downstreams`) just receives and
/// returns. Pooled (the `Actor` default).
pub struct Relay {
    downstreams: Arc<[MailboxId]>,
}

impl aether_actor::Actor for Relay {
    const NAMESPACE: &'static str = "mlat.relay";
}
impl aether_actor::Instanced for Relay {}
impl aether_actor::HandlesKind<Ping> for Relay {}
impl NativeActor for Relay {
    type Config = Arc<[MailboxId]>;
    fn init(config: Self::Config, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self {
            downstreams: config,
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
/// number of relays is `downstreams.len()`.
#[derive(Clone)]
pub struct Topology {
    pub name: String,
    pub downstreams: Vec<Vec<usize>>,
}

/// `0 -> 1 -> ... -> d-1`. Each relay forwards to the next; the last is
/// a leaf.
#[must_use]
pub fn depth_chain(d: usize) -> Topology {
    let downstreams = (0..d)
        .map(|i| if i + 1 < d { vec![i + 1] } else { vec![] })
        .collect();
    Topology {
        name: format!("depth-{d}"),
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
        downstreams,
    }
}

/// `A -> {B, C} -> {D, E}, {E, F}`. E (index 4) has two parents (B and
/// C) — the shared-node contention case.
#[must_use]
pub fn two_level_tree() -> Topology {
    Topology {
        name: "tree-A-BC-DEEF".to_owned(),
        downstreams: vec![
            vec![1, 2], // A -> B, C
            vec![3, 4], // B -> D, E
            vec![4, 5], // C -> E, F
            vec![],     // D
            vec![],     // E
            vec![],     // F
        ],
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
    pub hop: Stats,
    pub handler: Stats,
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

/// Drive the sweep and return per-cell percentiles. Each cell boots a
/// fresh [`TestBench`], wires the topology + tick source, advances, and
/// harvests the trace ring once. A cell whose bench fails to boot (no
/// wgpu adapter) or whose harvest overflows is logged via `tracing` and
/// skipped — so a driverless box returns fewer cells (possibly empty)
/// rather than panicking.
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
                let downs: Arc<[MailboxId]> =
                    topo.downstreams[i].iter().map(|&j| relay_id(j)).collect();
                let sub = i.to_string();
                if let Err(e) = tb
                    .spawn_actor::<Relay>(Subname::Named(&sub), downs)
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

            // Drive via the real lifecycle.
            let t0 = Instant::now();
            match cfg.pace_hz {
                Some(hz) => {
                    let period = Duration::from_secs_f64(1.0 / hz as f64);
                    for _ in 0..cfg.frames {
                        let f = Instant::now();
                        let _ = tb.advance(1);
                        if let Some(rem) = period.checked_sub(f.elapsed()) {
                            thread::sleep(rem);
                        }
                    }
                }
                None => {
                    let _ = tb.advance(cfg.frames);
                }
            }
            let drive_ms = u64::try_from(t0.elapsed().as_millis()).unwrap_or(u64::MAX);

            // Harvest the resident ring once, after the run. The
            // `Ping`-kind filter isolates relay hops, so setup / Tick /
            // query mail never counts.
            let req = DescribeWindow {
                window: TraceWindow::Relative {
                    last_ms: drive_ms.saturating_add(1_000),
                },
                max_mails: Some(100_000),
            }
            .encode_into_bytes();
            let mails = match tb.send_bytes_and_await(
                TRACE_OBSERVER_MAILBOX_NAME,
                DescribeWindow::ID,
                req,
            ) {
                Ok(reply) => match DescribeWindowResult::decode_from_bytes(&reply) {
                    Some(DescribeWindowResult::Ok { mails }) => mails,
                    Some(DescribeWindowResult::Err { too_many }) => {
                        tracing::warn!(
                            target: "aether_perf",
                            topo = %topo.name, workers, ?too_many,
                            "describe_window over cap — lower frames or raise AETHER_TRACE_RING_CAPACITY",
                        );
                        continue;
                    }
                    None => {
                        tracing::warn!(target: "aether_perf", topo = %topo.name, "describe_window decode failed");
                        continue;
                    }
                },
                Err(e) => {
                    tracing::warn!(target: "aether_perf", topo = %topo.name, error = ?e, "describe_window send failed");
                    continue;
                }
            };

            let mut hop = Vec::new();
            let mut handler = Vec::new();
            for node in &mails {
                if node.kind.0 != Ping::ID.0 {
                    continue;
                }
                if let Some(recv) = node.t_received {
                    hop.push(recv.0.saturating_sub(node.t_sent.0));
                    if let Some(fin) = node.t_finished {
                        handler.push(fin.0.saturating_sub(recv.0));
                    }
                }
            }
            rows.push(CellResult {
                workers,
                topo: topo.name.clone(),
                hop: summarize(hop),
                handler: summarize(handler),
            });
        }
    }
    rows
}
