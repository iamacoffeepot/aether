//! Mail-latency measurement harness (iamacoffeepot/aether#1057).
//!
//! Measures the per-hop and end-to-end cost of inter-actor mail across
//! controlled topologies, so the mail system has a numeric regression
//! surface. The trace observer (ADR-0080) already stamps every mail
//! with `t_sent` (producer enqueue), `t_received` (dispatcher pickup),
//! and `t_finished` (handler return) from one monotonic clock, so the
//! data is recorded for free — this harness just wires synthetic relay
//! actors into a topology, injects traced root mails, reads the trace
//! tree back over the `aether.trace` mail surface, and computes
//! percentiles.
//!
//! **Worker count is the dominant variable.** Post issue #635 actors
//! are `Pooled` by default — they share a worker pool of
//! `available_parallelism() - 1` threads, not one thread each. So a
//! depth-8 chain with one root in flight serialises regardless of pool
//! size (only one slot is ever ready), but a fan-out of 8 — or many
//! concurrent roots — either parallelises across workers or queues on a
//! small pool. The harness therefore sweeps pool size as an outer axis
//! (`TestBenchBuilder::with_workers`) and reports it as a column.
//!
//! Run on demand (it is `#[ignore]`d — zero CI cost):
//!
//! ```text
//! cargo test -p aether-substrate-bundle --release lifecycle_latency_observe \
//!     -- --ignored --nocapture
//! ```
//!
//! Release matters: the numbers are dominated by enqueue + worker
//! wake, which a debug build inflates several-fold.

use std::collections::BTreeSet;
use std::env;
use std::sync::Arc;
use std::thread::{self, available_parallelism};
use std::time::{Duration, Instant};

use aether_data::{Kind, KindId, MailId, MailboxId, mailbox_id_from_name};
use aether_kinds::trace::{
    DescribeTree, DescribeTreeResult, DescribeWindow, DescribeWindowResult, MailNodeWire,
    TRACE_OBSERVER_MAILBOX_NAME, TraceWindow,
};
use aether_kinds::{SubscribeInput, SubscribeInputResult, Tick};
use aether_substrate::{BootError, NativeActor, NativeCtx, NativeDispatch, NativeInitCtx, Subname};

use super::TestBench;

/// Fire-and-forward payload the relay actors pass along. The `seq`
/// field is carried for legibility when eyeballing a trace; the relay
/// forwards the bytes verbatim, so the wire shape is irrelevant to the
/// measurement. Derived `Kind` (cast codec) — the schema-hashed `ID`
/// is what the relay matches and the trace records; the value is
/// immaterial.
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
struct Ping {
    seq: u32,
}

/// A relay forwards each inbound `Ping` to every configured downstream
/// mailbox, inheriting the trace lineage so the whole topology is one
/// causal tree. A leaf relay (empty `downstreams`) just receives and
/// returns. Pooled (the `Actor` default) — so the worker-pool size
/// gates how its fan-out and any concurrent load behave.
struct Relay {
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
        // Forward the bytes verbatim to each downstream. The loop is
        // the "one sender pushing to N inboxes" shape: each push stamps
        // its own `t_sent`, so later children in a fan-out reveal any
        // per-child enqueue skew.
        for &down in self.downstreams.iter() {
            // The minted child `MailId` isn't needed here — the trace
            // observer records it; the relay only forwards.
            let _ = ctx.send_envelope_traced(down, Ping::ID, payload);
        }
        Some(())
    }
}

const RELAY_NS: &str = "mlat.relay";

/// Self-sustaining ring actor for the multi-worker saturation profile.
/// On each `Ping{seq}` it forwards `Ping{seq-1}` to its single `next`
/// neighbour while `seq > 0`. Seeded with M tokens circulating a ring of
/// N actors, the pool stays saturated with cross-actor hand-offs and no
/// injector involvement after seeding — so a profile attributes the
/// multi-worker load tail (shared-queue contention vs actor
/// serialization vs settlement).
struct RingRelay {
    next: MailboxId,
}

impl aether_actor::Actor for RingRelay {
    const NAMESPACE: &'static str = "mlat.ring";
}
impl aether_actor::Instanced for RingRelay {}
impl aether_actor::HandlesKind<Ping> for RingRelay {}
impl NativeActor for RingRelay {
    type Config = MailboxId;
    fn init(next: Self::Config, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self { next })
    }
}
impl NativeDispatch for RingRelay {
    fn __aether_dispatch_envelope(
        &mut self,
        ctx: &mut NativeCtx<'_>,
        kind: KindId,
        payload: &[u8],
    ) -> Option<()> {
        if kind.0 != Ping::ID.0 {
            return None;
        }
        let ping = Ping::decode_from_bytes(payload)?;
        if ping.seq > 0 {
            let bytes = Ping { seq: ping.seq - 1 }.encode_into_bytes();
            let _ = ctx.send_envelope_traced(self.next, Ping::ID, &bytes);
        }
        Some(())
    }
}

const RING_NS: &str = "mlat.ring";

fn ring_id(i: usize) -> MailboxId {
    MailboxId(mailbox_id_from_name(&format!("{RING_NS}:{i}")).0)
}

/// Deterministic `MailboxId` for relay instance `i`. Mirrors the
/// substrate's `mailbox_id_from_name("{NAMESPACE}:{subname}")` so the
/// whole topology can be wired from precomputed ids before any actor is
/// spawned (sidesteps spawn-ordering between a relay and its
/// downstreams).
fn relay_id(i: usize) -> MailboxId {
    MailboxId(mailbox_id_from_name(&format!("{RELAY_NS}:{i}")).0)
}

/// A topology is a DAG over relay indices: `downstreams[i]` lists the
/// relays that relay `i` forwards to. Relay 0 is always the entry. The
/// number of relays is `downstreams.len()`.
struct Topology {
    name: String,
    downstreams: Vec<Vec<usize>>,
}

fn depth_chain(d: usize) -> Topology {
    // 0 -> 1 -> ... -> d-1. Each relay forwards to the next; the last
    // is a leaf.
    let downstreams = (0..d)
        .map(|i| if i + 1 < d { vec![i + 1] } else { vec![] })
        .collect();
    Topology {
        name: format!("depth-{d}"),
        downstreams,
    }
}

fn fanout(b: usize) -> Topology {
    // 0 -> {1, 2, ..., b}. Entry fans to b leaves.
    let mut downstreams = vec![vec![]; b + 1];
    downstreams[0] = (1..=b).collect();
    Topology {
        name: format!("fanout-{b}"),
        downstreams,
    }
}

fn two_level_tree() -> Topology {
    // A -> {B, C} -> {D, E}, {E, F}. E (index 4) has two parents (B and
    // C) — the shared-node contention case.
    //   0=A 1=B 2=C 3=D 4=E 5=F
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

fn topologies() -> Vec<Topology> {
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
#[derive(Clone, Copy, Default)]
struct Stats {
    p50: u64,
    p90: u64,
    p99: u64,
    max: u64,
    n: usize,
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

fn summarize(mut samples: Vec<u64>) -> Stats {
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
struct Row {
    workers: usize,
    topo: String,
    cond: &'static str,
    hop: Stats,
    handler: Stats,
}

/// Query the trace observer for one root's whole tree over the mail
/// wire. Returns the node list, or `None` if the observer no longer has
/// the root (only happens if the ring lapped it — not expected at these
/// volumes).
fn describe_tree(tb: &mut TestBench, root: MailId) -> Option<Vec<MailNodeWire>> {
    let req = DescribeTree { root }.encode_into_bytes();
    let reply = tb
        .send_bytes_and_await(TRACE_OBSERVER_MAILBOX_NAME, DescribeTree::ID, req)
        .ok()?;
    match DescribeTreeResult::decode_from_bytes(&reply)? {
        DescribeTreeResult::Ok { mails, .. } => Some(mails),
        DescribeTreeResult::Err { .. } => None,
    }
}

const SETTLE_TIMEOUT: Duration = Duration::from_secs(10);


/// Multi-worker saturation profile target (samply). Boots the pool at
/// max workers, wires a ring of N `RingRelay` actors, seeds M circulating
/// tokens, and sleeps `PROFILE_SECS` while the workers churn at
/// saturation. The tokens self-sustain (each hop re-sends to the next
/// neighbour with a decremented counter), so all workers stay fed
/// without the injector bottleneck that starved the single-worker
/// `mail_hop_profile`. Profile and classify the `aether-worker-N`
/// threads to attribute the load tail: shared-queue contention
/// (crossbeam recv/send + CAS + Arc churn — work-stealing-addressable)
/// vs actor-serialization mutex wait vs settlement/trace.
///
/// ```text
/// cargo test -p aether-substrate-bundle --release mail_saturation_profile --no-run
/// samply record --rate 4000 --unstable-presymbolicate --save-only -o /tmp/sat.json.gz -- \
///     <bin> mail_saturation_profile --ignored --nocapture --test-threads=1
/// ```
#[test]
#[ignore = "profiling target — run under samply, not a correctness gate"]
#[allow(clippy::print_stderr)]
fn mail_saturation_profile() {
    let workers = available_parallelism().map_or(2, |n| n.get().saturating_sub(1).max(1));
    let Ok(tb) = TestBench::builder()
        .with_workers(Some(workers))
        .size(16, 16)
        .build()
    else {
        eprintln!("skipping mail_saturation_profile: TestBench boot failed (no wgpu adapter)");
        return;
    };

    // N actors in a cycle; actor i forwards to (i+1) % N.
    let n = 64usize;
    for i in 0..n {
        let next = ring_id((i + 1) % n);
        let sub = i.to_string();
        tb.spawn_actor::<RingRelay>(Subname::Named(&sub), next)
            .finish()
            .expect("spawn ring relay");
    }

    // Seed M tokens with a high hop budget so they outlast the run; the
    // tokens circulate without further injection. Spread across the ring
    // so multiple actors are ready at once and every worker is fed.
    let m: usize = env::var("TOKENS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(6000);
    let ttl = 100_000_000u32;
    for k in 0..m {
        let entry = ring_id(k % n);
        let _ = tb.inject_root(entry, Ping::ID, Ping { seq: ttl }.encode_into_bytes());
    }

    let secs: u64 = env::var("PROFILE_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8);
    let start = Instant::now();
    thread::sleep(Duration::from_secs(secs));
    eprintln!(
        "mail_saturation_profile: {workers}w, ring n={n}, m={m} tokens, slept {:?}",
        start.elapsed()
    );
}

/// Regression guard for the per-root trace-queue sharding
/// (iamacoffeepot/aether#1059): drive many concurrent roots through a
/// multi-worker pool and assert every one *settles*.
///
/// The sharded trace queue keeps each root's events in one FIFO shard so
/// ADR-0080's per-root `Sent`-before-`Finished` ordering holds. If that
/// ordering broke, a root's `in_flight` accounting would never balance
/// and its settlement signal would never fire — so "every injected root
/// settles within the timeout" is the exactness check. Each surviving
/// trace tree is also asserted complete (full depth chain), catching a
/// settle that fired on a truncated chain. Runs on the pool at max
/// workers so the shard fan-out is actually exercised.
#[test]
#[allow(clippy::print_stderr)]
fn sharded_trace_settles_every_root() {
    let workers = available_parallelism().map_or(2, |n| n.get().saturating_sub(1).max(1));
    let Ok(mut tb) = TestBench::builder()
        .with_workers(Some(workers))
        .size(16, 16)
        .build()
    else {
        eprintln!("skipping sharded_trace_settles_every_root: TestBench boot failed (no wgpu)");
        return;
    };

    let depth = 5;
    let topo = depth_chain(depth);
    for i in 0..topo.downstreams.len() {
        let downs: Arc<[MailboxId]> = topo.downstreams[i].iter().map(|&j| relay_id(j)).collect();
        let sub = i.to_string();
        tb.spawn_actor::<Relay>(Subname::Named(&sub), downs)
            .finish()
            .expect("spawn relay");
    }
    let entry = relay_id(0);

    // 800 roots × depth 5 = 4000 mails — well under the trace ring
    // capacity (1<<18), so nothing laps and every live root keeps its
    // settlement state.
    let roots = 800u32;
    let mut pending = Vec::with_capacity(roots as usize);
    for seq in 0..roots {
        pending.push(tb.inject_root(entry, Ping::ID, Ping { seq }.encode_into_bytes()));
    }

    for (idx, (_root, rx)) in pending.iter().enumerate() {
        assert!(
            rx.recv_timeout(SETTLE_TIMEOUT).is_ok(),
            "root {idx} never settled — per-root trace ordering may be broken (in_flight stuck)"
        );
    }

    for (root, _) in &pending {
        if let Some(mails) = describe_tree(&mut tb, *root) {
            assert_eq!(
                mails.len(),
                depth,
                "root {root:?} settled on a truncated tree ({} of {depth} hops)",
                mails.len()
            );
        }
    }
}

/// Flake-soak duplicate of [`sharded_trace_settles_every_root`] (the
/// `flaky_` prefix is the soak selector; see CLAUDE.md "Flake soak").
#[test]
#[allow(clippy::print_stderr)]
fn flaky_sharded_trace_settles_every_root() {
    sharded_trace_settles_every_root();
}

/// Lifecycle bridge for [`lifecycle_latency_observe`]: subscribed to the
/// `Tick` input stream, it emits one `Ping` into the entry relay per
/// frame, inheriting the tick's trace lineage so the whole per-frame
/// chain is one causal tree. The honest stand-in for a real tick-reactive
/// component (player, camera): the substrate's own `Tick` fan-out drives
/// the work — no synthetic injector, no per-root settlement block.
struct TickSource {
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
        // One Ping per tick into the entry relay, inheriting the tick's
        // lineage (`send_envelope_traced`) so the per-frame chain settles
        // as one tree the trace observer records. `seq` is for legibility
        // only — relays forward the bytes verbatim.
        let bytes = Ping { seq: self.seq }.encode_into_bytes();
        self.seq = self.seq.wrapping_add(1);
        let _ = ctx.send_envelope_traced(self.entry, Ping::ID, &bytes);
        Some(())
    }
}

const TICKSRC_NS: &str = "mlat.ticksrc";

/// Deterministic id for the single tick source (subname `"src"`).
fn ticksrc_id() -> MailboxId {
    MailboxId(mailbox_id_from_name(&format!("{TICKSRC_NS}:src")).0)
}

const OBSERVE_FRAMES: u32 = 1000;

/// Non-perturbing latency harness: drive the substrate with its **real
/// lifecycle** (`advance` → `Tick` fan-out → a tick-reactive source →
/// the relay topology) and **harvest the trace ring after the fact** via
/// one [`DescribeWindow`] query. The sibling to [`mail_latency_sweep`],
/// minus its two methodology hazards:
///
/// - **No synthetic injector.** The work is produced by the substrate's
///   own `Tick` delivery to an input-subscribed actor — the real
///   mechanic a component sees — not by a test thread pushing roots.
/// - **No per-root blocking.** `mail_latency_sweep`'s idle mode injects
///   one root and blocks on its settlement before the next, so the pool
///   goes cold between roots and every hop pays a fresh wakeup. Here the
///   only block is the single `advance` round-trip for the whole run, and
///   the measurement is a passive read of the resident ring afterward.
///
/// Default is **flat-out** `advance` — frames run back-to-back, workers
/// stay warm, so the numbers isolate the per-hop dispatch cost.
/// `AETHER_LAT_PACE_HZ=60` paces one frame per period instead (workers
/// park in the gaps → realistic frame-loop latency including the
/// once-per-frame wakeup).
///
/// `#[ignore]` — a measurement run, not a correctness gate. Skips cleanly
/// when no wgpu adapter is available.
#[test]
#[ignore = "measurement harness — run on demand with --ignored --nocapture --release"]
#[allow(
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::cast_precision_loss,
    clippy::too_many_lines
)]
fn lifecycle_latency_observe() {
    let max_workers = available_parallelism().map_or(2, |n| n.get().saturating_sub(1).max(1));
    let worker_set: BTreeSet<usize> = [1usize, 2, 4, max_workers].into_iter().collect();

    if let Err(e) = TestBench::builder().size(16, 16).build() {
        eprintln!(
            "skipping lifecycle_latency_observe: TestBench boot failed (likely no wgpu adapter): {e}"
        );
        return;
    }

    // `AETHER_LAT_PACE_HZ=N` → pace one frame per 1/N s (workers park
    // between frames). Unset → flat-out (warm).
    let pace_hz: Option<u64> = env::var("AETHER_LAT_PACE_HZ")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|&h| h > 0);
    let cond = if pace_hz.is_some() { "paced" } else { "warm" };

    let mut rows: Vec<Row> = Vec::new();

    for &workers in &worker_set {
        for topo in topologies() {
            let Ok(mut tb) = TestBench::builder()
                .with_workers(Some(workers))
                .size(16, 16)
                .build()
            else {
                eprintln!("skipping {} @ {workers}w: boot failed", topo.name);
                continue;
            };

            // Relay topology (entry = relay 0), then the tick source
            // pointed at it. Ids are deterministic, so spawn order is
            // irrelevant.
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
                    eprintln!("spawn failed for {} relay {i}: {e:?}", topo.name);
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
                eprintln!("tick source spawn failed for {}: {e:?}", topo.name);
                continue;
            }

            // Wire the source into the platform `Tick` stream so `advance`
            // delivers a tick to it each frame (ADR-0021 explicit
            // subscribe; auto-subscribe retired in #403 / #640).
            let sub_req = SubscribeInput {
                kind: Tick::ID,
                mailbox: ticksrc_id(),
            }
            .encode_into_bytes();
            match tb.send_bytes_and_await("aether.input", SubscribeInput::ID, sub_req) {
                Ok(reply) => match SubscribeInputResult::decode_from_bytes(&reply) {
                    Some(SubscribeInputResult::Ok) => {}
                    other => {
                        eprintln!("Tick subscribe failed for {}: {other:?}", topo.name);
                        continue;
                    }
                },
                Err(e) => {
                    eprintln!("Tick subscribe send failed for {}: {e:?}", topo.name);
                    continue;
                }
            }

            // Drive via the real lifecycle. The only block is the single
            // `advance` round-trip for the whole run (flat-out), or one
            // per paced frame.
            let t0 = Instant::now();
            match pace_hz {
                Some(hz) => {
                    let period = Duration::from_secs_f64(1.0 / hz as f64);
                    for _ in 0..OBSERVE_FRAMES {
                        let f = Instant::now();
                        let _ = tb.advance(1);
                        if let Some(rem) = period.checked_sub(f.elapsed()) {
                            thread::sleep(rem);
                        }
                    }
                }
                None => {
                    let _ = tb.advance(OBSERVE_FRAMES);
                }
            }
            let drive_ms = u64::try_from(t0.elapsed().as_millis()).unwrap_or(u64::MAX);

            // Harvest the resident ring once, after the run. The window is
            // generous; the `Ping`-kind filter below isolates the relay
            // hops regardless, so setup / Tick / query mail never counts.
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
                        eprintln!(
                            "describe_window over cap ({too_many:?}) for {} @ {workers}w — lower OBSERVE_FRAMES or raise AETHER_TRACE_RING_CAPACITY",
                            topo.name
                        );
                        continue;
                    }
                    None => {
                        eprintln!("describe_window decode failed for {}", topo.name);
                        continue;
                    }
                },
                Err(e) => {
                    eprintln!("describe_window send failed for {}: {e:?}", topo.name);
                    continue;
                }
            };

            // Per-relay-hop samples, filtered to the local `Ping` kind.
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
            rows.push(Row {
                workers,
                topo: topo.name.clone(),
                cond,
                hop: summarize(hop),
                handler: summarize(handler),
            });
        }
    }

    print_observe_tables(&rows, pace_hz);
}

/// Print the lifecycle-harness HOP + HANDLER tables.
#[allow(clippy::print_stdout, clippy::cast_precision_loss)]
fn print_observe_tables(rows: &[Row], pace_hz: Option<u64>) {
    let us = |ns: u64| -> String { format!("{:.2}", ns as f64 / 1000.0) };

    println!();
    println!("=== lifecycle-driven mail latency (all values µs; n = sample count) ===");
    println!(
        "driven by `advance` (real Tick fan-out → source → relay chain); harvested from the"
    );
    println!("trace ring via one DescribeWindow — no injector, no per-root block.");
    match pace_hz {
        Some(hz) => println!("paced @ {hz} Hz — workers park between frames (realistic frame loop)"),
        None => println!("flat-out advance — workers stay warm (isolates per-hop dispatch cost)"),
    }
    println!("{OBSERVE_FRAMES} frames/cell; relay-hop (`Ping`) samples only.");
    println!();

    for (label, pick) in [
        (
            "HOP LATENCY  (t_received - t_sent: enqueue + worker pickup)",
            0usize,
        ),
        (
            "HANDLER DUR  (t_finished - t_received: relay forward work)",
            1,
        ),
    ] {
        println!("-- {label} --");
        println!(
            "{:>3}w  {:<16} {:<5} {:>9} {:>9} {:>9} {:>9} {:>7}",
            "", "topology", "cond", "p50", "p90", "p99", "max", "n"
        );
        for r in rows {
            let s = if pick == 0 { r.hop } else { r.handler };
            println!(
                "{:>3}   {:<16} {:<5} {:>9} {:>9} {:>9} {:>9} {:>7}",
                r.workers,
                r.topo,
                r.cond,
                us(s.p50),
                us(s.p90),
                us(s.p99),
                us(s.max),
                s.n
            );
        }
        println!();
    }
}
