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
//! cargo test -p aether-substrate-bundle --release mail_latency_sweep \
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
    DescribeTree, DescribeTreeResult, MailNodeWire, TRACE_OBSERVER_MAILBOX_NAME,
};
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

/// Raw per-condition sample buckets accumulated across every measured
/// tree, summarized into [`Stats`] at the end.
#[derive(Default)]
struct Samples {
    hop: Vec<u64>,
    handler: Vec<u64>,
    e2e: Vec<u64>,
}

impl Samples {
    /// Fold one settled trace tree into the buckets: a hop sample per
    /// node (`t_received - t_sent`), a handler sample per node
    /// (`t_finished - t_received`), and one end-to-end sample for the
    /// tree (last finish minus the root's send).
    fn ingest(&mut self, mails: &[MailNodeWire]) {
        let mut tree_start: Option<u64> = None;
        let mut tree_end: Option<u64> = None;
        for node in mails {
            let sent = node.t_sent.0;
            if node.parent.is_none() {
                tree_start = Some(sent);
            }
            if let Some(recv) = node.t_received {
                self.hop.push(recv.0.saturating_sub(sent));
                if let Some(fin) = node.t_finished {
                    self.handler.push(fin.0.saturating_sub(recv.0));
                }
            }
            if let Some(fin) = node.t_finished {
                tree_end = Some(tree_end.map_or(fin.0, |e: u64| e.max(fin.0)));
            }
        }
        if let (Some(start), Some(end)) = (tree_start, tree_end) {
            self.e2e.push(end.saturating_sub(start));
        }
    }
}

/// One fully-measured cell of the sweep.
struct Row {
    workers: usize,
    topo: String,
    cond: &'static str,
    hop: Stats,
    handler: Stats,
    e2e: Stats,
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
const IDLE_SAMPLES: u32 = 120;
const LOAD_ROOTS: u32 = 200;

/// Sweep worker-pool size × topology × {idle, under-load} and print the
/// mail-latency percentile tables.
///
/// `#[ignore]` — a measurement run, not a correctness gate. Skips
/// cleanly when no wgpu adapter is available ([`TestBench`] needs an
/// offscreen render target to boot).
#[test]
#[ignore = "measurement harness — run on demand with --ignored --nocapture --release"]
#[allow(
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::cast_precision_loss
)]
fn mail_latency_sweep() {
    let max_workers = available_parallelism().map_or(2, |n| n.get().saturating_sub(1).max(1));
    let worker_set: BTreeSet<usize> = [1usize, 2, 4, max_workers].into_iter().collect();

    // Probe once so a driverless box prints a skip line instead of a
    // confusing per-cell failure storm.
    if let Err(e) = TestBench::builder().size(16, 16).build() {
        eprintln!(
            "skipping mail_latency_sweep: TestBench boot failed (likely no wgpu adapter): {e}"
        );
        return;
    }

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

            // Spawn every relay with its precomputed downstream ids.
            // Ids are deterministic, so spawn order is irrelevant.
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
            let entry = relay_id(0);

            // idle: one root in flight at a time, for clean per-hop cost.
            let mut idle = Samples::default();
            for seq in 0..IDLE_SAMPLES {
                let (root, rx) = tb.inject_root(entry, Ping::ID, Ping { seq }.encode_into_bytes());
                if rx.recv_timeout(SETTLE_TIMEOUT).is_err() {
                    eprintln!("idle settle timeout: {} @ {workers}w", topo.name);
                    break;
                }
                if let Some(mails) = describe_tree(&mut tb, root) {
                    idle.ingest(&mails);
                }
            }
            rows.push(Row {
                workers,
                topo: topo.name.clone(),
                cond: "idle",
                hop: summarize(idle.hop),
                handler: summarize(idle.handler),
                e2e: summarize(idle.e2e),
            });

            // load: many concurrent roots, to surface inbox queueing.
            let mut pending = Vec::with_capacity(LOAD_ROOTS as usize);
            for seq in 0..LOAD_ROOTS {
                pending.push(tb.inject_root(entry, Ping::ID, Ping { seq }.encode_into_bytes()));
            }
            let mut all_settled = true;
            for (_, rx) in &pending {
                if rx.recv_timeout(SETTLE_TIMEOUT).is_err() {
                    all_settled = false;
                    break;
                }
            }
            let mut load = Samples::default();
            if all_settled {
                for (root, _) in &pending {
                    if let Some(mails) = describe_tree(&mut tb, *root) {
                        load.ingest(&mails);
                    }
                }
            } else {
                eprintln!("load settle timeout: {} @ {workers}w", topo.name);
            }
            rows.push(Row {
                workers,
                topo: topo.name.clone(),
                cond: "load",
                hop: summarize(load.hop),
                handler: summarize(load.handler),
                e2e: summarize(load.e2e),
            });
        }
    }

    print_tables(&rows);
}

#[allow(clippy::print_stdout, clippy::cast_precision_loss)]
fn print_tables(rows: &[Row]) {
    let us = |ns: u64| -> String { format!("{:.2}", ns as f64 / 1000.0) };

    println!();
    println!("=== mail latency sweep (all values µs; n = sample count) ===");
    println!("idle = one root in flight; load = {LOAD_ROOTS} concurrent roots");
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
        ("END-TO-END   (last finish - root send)", 2),
    ] {
        println!("-- {label} --");
        println!(
            "{:>3}w  {:<16} {:<5} {:>9} {:>9} {:>9} {:>9} {:>7}",
            "", "topology", "cond", "p50", "p90", "p99", "max", "n"
        );
        for r in rows {
            let s = match pick {
                0 => r.hop,
                1 => r.handler,
                _ => r.e2e,
            };
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
