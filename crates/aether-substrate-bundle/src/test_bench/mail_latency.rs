//! Mail-latency measurement harness tests (iamacoffeepot/aether#1057).
//!
//! The reusable sweep engine (relay actors, topologies, `run_sweep`)
//! lives in [`crate::perf::harness`] so the `perf-trial` bin can drive
//! it too (iamacoffeepot/aether#1077). This module keeps the on-demand
//! `#[ignore]` measurement test ([`lifecycle_latency_observe`]), the
//! settlement regression guard, and the saturation profiling target —
//! the test-only consumers of that engine.
//!
//! Run the latency table on demand (it is `#[ignore]`d — zero CI cost):
//!
//! ```text
//! cargo test -p aether-substrate-bundle --release lifecycle_latency_observe \
//!     -- --ignored --nocapture
//! ```
//!
//! Release matters: the numbers are dominated by enqueue + worker wake,
//! which a debug build inflates several-fold.

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
use crate::perf::harness::{
    CellResult, Ping, Relay, RelayConfig, SweepConfig, default_topologies, depth_chain,
    fanout_heavy, heavy_work_iters_from_env, pace_hz_from_env, relay_id, run_sweep,
};

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
/// `mail_hop_profile`.
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

    let n = 64usize;
    for i in 0..n {
        let next = ring_id((i + 1) % n);
        let sub = i.to_string();
        tb.spawn_actor::<RingRelay>(Subname::Named(&sub), next)
            .finish()
            .expect("spawn ring relay");
    }

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
/// settle that fired on a truncated chain.
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
        let downstreams: Arc<[MailboxId]> =
            topo.downstreams[i].iter().map(|&j| relay_id(j)).collect();
        let sub = i.to_string();
        let config = RelayConfig {
            downstreams,
            work_iters: topo.work_iters[i],
        };
        tb.spawn_actor::<Relay>(Subname::Named(&sub), config)
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

const OBSERVE_FRAMES: u32 = 1000;

/// Non-perturbing latency harness: drive the substrate with its **real
/// lifecycle** (`advance` → `Tick` fan-out → a tick-reactive source →
/// the relay topology) and **harvest the trace ring after the fact**.
/// Delegates the sweep to [`run_sweep`]; this test just builds the
/// default config (the four-worker × full-topology grid) and prints the
/// tables.
///
/// Default is **flat-out** `advance` (warm — isolates per-hop dispatch
/// cost). `AETHER_LAT_PACE_HZ=60` paces one frame per period instead
/// (workers park in the gaps → realistic frame-loop latency).
///
/// `#[ignore]` — a measurement run, not a correctness gate. Skips
/// cleanly (empty result) when no wgpu adapter is available.
#[test]
#[ignore = "measurement harness — run on demand with --ignored --nocapture --release"]
#[allow(clippy::print_stdout, clippy::print_stderr)]
fn lifecycle_latency_observe() {
    let max_workers = available_parallelism().map_or(2, |n| n.get().saturating_sub(1).max(1));
    let worker_set: Vec<usize> = [1usize, 2, 4, max_workers]
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();

    let pace_hz = pace_hz_from_env();

    // The trivial default set always runs. When AETHER_LAT_HEAVY_WORK is
    // set, append CPU-heavy fan-outs so the sweep can also exhibit the
    // parallelism-wins regime (iamacoffeepot/aether#1074) — unset, the
    // grid is byte-for-byte the historical one.
    let mut topologies = default_topologies();
    let heavy = heavy_work_iters_from_env();
    if heavy > 0 {
        for b in [2usize, 4, 8] {
            topologies.push(fanout_heavy(b, heavy));
        }
    }

    let cfg = SweepConfig {
        workers: worker_set,
        topologies,
        frames: OBSERVE_FRAMES,
        pace_hz,
    };
    let rows = run_sweep(&cfg);
    if rows.is_empty() {
        eprintln!("skipping lifecycle_latency_observe: no cells measured (likely no wgpu adapter)");
        return;
    }
    print_observe_tables(&rows, pace_hz);
}

/// Print the lifecycle-harness HOP + HANDLER tables.
#[allow(clippy::print_stdout, clippy::cast_precision_loss)]
fn print_observe_tables(rows: &[CellResult], pace_hz: Option<u64>) {
    let us = |ns: u64| -> String { format!("{:.2}", ns as f64 / 1000.0) };
    let cond = if pace_hz.is_some() { "paced" } else { "warm" };

    println!();
    println!("=== lifecycle-driven mail latency (all values µs; n = sample count) ===");
    println!("driven by `advance` (real Tick fan-out → source → relay chain); harvested from the");
    println!("trace ring via one DescribeWindow — no injector, no per-root block.");
    if let Some(hz) = pace_hz {
        println!("paced @ {hz} Hz — workers park between frames (realistic frame loop)");
    } else {
        println!("flat-out advance — workers stay warm (isolates per-hop dispatch cost)");
    }
    let heavy = heavy_work_iters_from_env();
    if heavy > 0 {
        println!(
            "heavy leaves: {heavy} spin-iters/handler (*-heavy rows; read HANDLER DUR for actual µs)"
        );
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
                cond,
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
