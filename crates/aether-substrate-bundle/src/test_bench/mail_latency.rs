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
    DescribeTree, DescribeTreeResult, MailNodeWire, TRACE_OBSERVER_MAILBOX_NAME, TraceEvent,
    TraceRingEntry, TraceTail, TraceTailResult,
};
use aether_substrate::{BootError, NativeActor, NativeCtx, NativeDispatch, NativeInitCtx, Subname};

use super::TestBench;
use crate::perf::harness::{
    CellResult, Ping, Relay, RelayConfig, Stats, SweepConfig, Topology, default_topologies,
    depth_chain, fanout, fanout_heavy, heavy_work_iters_from_env, pace_hz_from_env, relay_id,
    run_sweep, summarize, two_level_tree, wide_fanout_widths_from_env,
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

/// On each `Ping`, spawns an inherited worker thread (ADR-0080 §12) that
/// outlives the handler by a short sleep before exiting. The
/// `spawn_inherit` acquires a settlement hold before the worker starts;
/// the handler then returns (dropping `in_flight` to zero) but the root
/// must NOT settle until the worker exits and releases the hold. Used to
/// exercise the `HoldOpen` / `Release` producer hooks end-to-end against
/// the emit-time settlement counter (ADR-0086).
struct HoldRelay;

impl aether_actor::Actor for HoldRelay {
    const NAMESPACE: &'static str = "mlat.hold";
}
// Both markers: `Instanced` lets the bench's `spawn_actor` place it,
// `Singleton` satisfies `spawn_inherit`'s bound (the worker-thread hold
// primitive is singleton-oriented). A test fixture only — production
// actors pick one role.
impl aether_actor::Instanced for HoldRelay {}
impl aether_actor::Singleton for HoldRelay {}
impl aether_actor::HandlesKind<Ping> for HoldRelay {}
impl NativeActor for HoldRelay {
    type Config = ();
    fn init((): Self::Config, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self)
    }
}
impl NativeDispatch for HoldRelay {
    fn __aether_dispatch_envelope(
        &mut self,
        ctx: &mut NativeCtx<'_>,
        kind: KindId,
        _payload: &[u8],
    ) -> Option<()> {
        if kind.0 != Ping::ID.0 {
            return None;
        }
        // Hold acquired before the worker spawns; the worker sleeps so
        // the handler's `Finished` lands first (in_flight 0, held_open 1
        // — not settled), then exits to release the hold (settle).
        let _join = ctx.spawn_inherit::<Self, _>(|_inherit| {
            thread::sleep(Duration::from_millis(1));
        });
        Some(())
    }
}

const HOLD_NS: &str = "mlat.hold";

fn hold_id() -> MailboxId {
    MailboxId(mailbox_id_from_name(&format!("{HOLD_NS}:0")).0)
}

/// Spawn every relay in `topo` onto `tb` (subname = relay index), wiring
/// each relay's downstream ids. Shared by the settlement guards.
fn spawn_topology(tb: &TestBench, topo: &Topology) {
    for i in 0..topo.downstreams.len() {
        let downstreams: Arc<[MailboxId]> =
            topo.downstreams[i].iter().map(|&j| relay_id(j)).collect();
        let config = RelayConfig {
            downstreams,
            work_iters: topo.work_iters[i],
        };
        tb.spawn_actor::<Relay>(Subname::Named(&i.to_string()), config)
            .finish()
            .expect("spawn relay");
    }
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
    spawn_topology(&tb, &topo);
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

/// ADR-0086 Phase 2 emit-authority guard: the emit-time
/// `SettlementCounter` is now the *only* path that fires `Settled`
/// through the registry (the chassis-router swallows the observer's
/// superseded copy), so `inject_root`'s settlement receiver firing
/// *proves* the counter drove the zero-transition. Drives `topo` with
/// many concurrent roots through a multi-worker pool and asserts every
/// one settles within the timeout.
///
/// A stuck `in_flight` (broken lineage accounting) would leave a root's
/// cell non-zero forever, so its receiver never fires and the test times
/// out — the exactness check. Covers the topologies the trivial
/// depth-chain guard ([`sharded_trace_settles_every_root`]) doesn't:
/// fan-out and a shared (two-parent) node.
#[allow(clippy::print_stderr)]
fn emit_settlement_settles_every_root(topo: &Topology) {
    let workers = available_parallelism().map_or(2, |n| n.get().saturating_sub(1).max(1));
    let Ok(tb) = TestBench::builder()
        .with_workers(Some(workers))
        .size(16, 16)
        .build()
    else {
        eprintln!("skipping emit_settlement_settles_every_root: no wgpu adapter");
        return;
    };

    spawn_topology(&tb, topo);
    let entry = relay_id(0);

    let roots = 500u32;
    let mut pending = Vec::with_capacity(roots as usize);
    for seq in 0..roots {
        pending.push(tb.inject_root(entry, Ping::ID, Ping { seq }.encode_into_bytes()));
    }
    for (idx, (_root, rx)) in pending.iter().enumerate() {
        assert!(
            rx.recv_timeout(SETTLE_TIMEOUT).is_ok(),
            "root {idx} never settled — the emit-time counter must fire Settled"
        );
    }
}

/// Rich topology: fan-out + a shared (two-parent) node + depth.
#[test]
fn emit_settlement_settles_two_level_tree() {
    emit_settlement_settles_every_root(&two_level_tree());
}

/// Flake-soak duplicate (concurrent multi-worker dispatch; see CLAUDE.md).
#[test]
fn flaky_emit_settlement_settles_two_level_tree() {
    emit_settlement_settles_two_level_tree();
}

/// Hold path: a `spawn_inherit` worker keeps the root open past the
/// handler's `Finished`, so settlement is gated on the worker's `Release`
/// (ADR-0080 §12). Exercises the `HoldOpen` / `Release` producer hooks —
/// the only ones the topology guards don't hit — through the emit-time
/// counter. Each injected root must settle only after its hold releases.
#[test]
#[allow(clippy::print_stderr)]
fn emit_settlement_settles_with_holds() {
    let workers = available_parallelism().map_or(2, |n| n.get().saturating_sub(1).max(1));
    let Ok(tb) = TestBench::builder()
        .with_workers(Some(workers))
        .size(16, 16)
        .build()
    else {
        eprintln!("skipping emit_settlement_settles_with_holds: no wgpu adapter");
        return;
    };
    tb.spawn_actor::<HoldRelay>(Subname::Named("0"), ())
        .finish()
        .expect("spawn hold relay");
    let entry = hold_id();

    let roots = 50u32;
    let mut pending = Vec::with_capacity(roots as usize);
    for seq in 0..roots {
        pending.push(tb.inject_root(entry, Ping::ID, Ping { seq }.encode_into_bytes()));
    }
    for (idx, (_root, rx)) in pending.iter().enumerate() {
        assert!(
            rx.recv_timeout(SETTLE_TIMEOUT).is_ok(),
            "hold root {idx} never settled — the hold's Release must fire the counter"
        );
    }
}

/// Query one actor's per-actor trace ring over the mail wire
/// (`aether.trace.tail`), filtered to `root`. Returns the ring slice.
fn trace_tail(tb: &mut TestBench, mailbox_name: &str, root: MailId) -> Vec<TraceRingEntry> {
    let req = TraceTail {
        max: 0,
        since: None,
        root: Some(root),
    }
    .encode_into_bytes();
    let reply = tb
        .send_bytes_and_await(mailbox_name, TraceTail::ID, req)
        .expect("aether.trace.tail reply");
    match TraceTailResult::decode_from_bytes(&reply).expect("decode TraceTailResult") {
        TraceTailResult::Ok { entries, .. } => entries,
        TraceTailResult::Err { error } => panic!("trace.tail error: {error}"),
    }
}

/// ADR-0086 Phase 3a: the producer hooks dual-write into the per-actor
/// trace rings (and the chassis-host ring for off-actor sends) alongside
/// the central observer. Inject a single-mail root at a leaf relay,
/// settle it, then assert the rings recorded the right events on the
/// right owners: the injected `Sent` (produced off-actor on the inject
/// thread) lands in the chassis-host ring; the recipient relay's
/// `Received` + `Finished` land in its own per-actor ring, queryable via
/// `aether.trace.tail`. Validates the dispatch-arm + the `root` filter
/// too (the trace-query mail's own events carry a different root and are
/// excluded).
#[test]
#[allow(clippy::print_stderr)]
fn trace_ring_dual_write_routes_events_to_owning_rings() {
    let Ok(mut tb) = TestBench::builder()
        .with_workers(Some(2))
        .size(16, 16)
        .build()
    else {
        eprintln!("skipping trace_ring_dual_write_routes_events_to_owning_rings: no wgpu adapter");
        return;
    };

    spawn_topology(&tb, &depth_chain(1));
    let (root, rx) = tb.inject_root(relay_id(0), Ping::ID, Ping { seq: 0 }.encode_into_bytes());
    assert!(
        rx.recv_timeout(SETTLE_TIMEOUT).is_ok(),
        "injected root never settled"
    );

    // The recipient relay's own ring holds the mail's Received + Finished.
    let relay = trace_tail(&mut tb, "mlat.relay:0", root);
    assert!(
        relay
            .iter()
            .any(|e| matches!(e.event, TraceEvent::Received { .. })),
        "relay ring missing Received; got {relay:?}"
    );
    assert!(
        relay
            .iter()
            .any(|e| matches!(e.event, TraceEvent::Finished { .. })),
        "relay ring missing Finished; got {relay:?}"
    );
    assert!(
        relay.iter().all(|e| e.root == root),
        "root filter leaked other roots (e.g. the trace-query mail): {relay:?}"
    );

    // The off-actor injected Sent landed in the chassis-host ring.
    let host = match tb.chassis_host_trace_tail(&TraceTail {
        max: 0,
        since: None,
        root: Some(root),
    }) {
        TraceTailResult::Ok { entries, .. } => entries,
        TraceTailResult::Err { error } => panic!("chassis-host trace.tail error: {error}"),
    };
    assert!(
        host.iter()
            .any(|e| matches!(e.event, TraceEvent::Sent { .. }) && e.root == root),
        "chassis-host ring missing the injected Sent; got {host:?}"
    );
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
    // Wide trivial fan-outs to locate the stickiness width-crossover
    // (iamacoffeepot/aether#1075); empty unless AETHER_LAT_WIDE_FANOUT is
    // set, so the default grid is unchanged.
    for w in wide_fanout_widths_from_env() {
        topologies.push(fanout(w));
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

/// ADR-0086: measure the settlement-detection latency (inject → the
/// root's `Settled` receiver firing). The before/after vehicle for the
/// decouple.
///
/// **Before (pre-Phase-2 / `main`):** settlement rode the trace pipeline
/// — a producer's `Finished` landed in the sharded queue, the drainer
/// shipped it after a ≤1 ms park, the observer folded it, and only then
/// did `Settled` fire — so the gap was roughly the drainer interval (the
/// Phase-0 sizing: ~0.9 ms p50). **After (Phase 2, this branch):** the
/// emit-time counter fires `Settled` synchronously on the producing
/// thread's zero-transition, so the same gap collapses to one atomic plus
/// the single registry → driver notice-mail hop.
///
/// Measures it directly: inject a trivial single-mail root, time
/// inject → its settlement receiver firing. A trivial root's dispatch +
/// handler cost is sub-microsecond (see the HOP/HANDLER tables above), so
/// the measured latency is dominated by the settlement path. A small
/// pseudo-random jitter before each injection decorrelates the inject
/// phase from the (still-running, ≤1 ms) drainer cycle, so a regression
/// back onto the drained path would re-appear as the old `[0, interval]`
/// spread rather than aligning just after a drain.
///
/// `#[ignore]` — a measurement, not a gate. Skips cleanly without a wgpu
/// adapter. Run release:
///
/// ```text
/// cargo test -p aether-substrate-bundle --release \
///     settlement_detection_latency -- --ignored --nocapture
/// ```
#[test]
#[ignore = "measurement harness — run on demand with --ignored --nocapture --release"]
#[allow(
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::cast_precision_loss
)]
fn settlement_detection_latency() {
    let workers = available_parallelism().map_or(2, |n| n.get().saturating_sub(1).max(1));
    let Ok(tb) = TestBench::builder()
        .with_workers(Some(workers))
        .size(16, 16)
        .build()
    else {
        eprintln!("skipping settlement_detection_latency: TestBench boot failed (no wgpu adapter)");
        return;
    };

    // A single leaf relay: receives a `Ping`, does no work, forwards
    // nothing, returns. Its whole causal tree is the one injected mail,
    // so settlement fires on that mail's `Finished` alone.
    let topo = depth_chain(1);
    let config = RelayConfig {
        downstreams: topo.downstreams[0].iter().map(|&j| relay_id(j)).collect(),
        work_iters: 0,
    };
    tb.spawn_actor::<Relay>(Subname::Named("0"), config)
        .finish()
        .expect("spawn leaf relay");
    let entry = relay_id(0);

    let samples: usize = env::var("SETTLE_SAMPLES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1000);

    // Cheap xorshift for the decorrelating jitter (no rand dependency).
    let mut rng: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next_jitter_us = || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng % 1500 // [0, 1500) µs — wider than the 1 ms drainer cycle
    };

    let mut lat = Vec::with_capacity(samples);
    for seq in 0..samples {
        thread::sleep(Duration::from_micros(next_jitter_us()));
        let t0 = Instant::now();
        let seq = u32::try_from(seq).unwrap_or(u32::MAX);
        let (_root, rx) = tb.inject_root(entry, Ping::ID, Ping { seq }.encode_into_bytes());
        if rx.recv_timeout(SETTLE_TIMEOUT).is_err() {
            eprintln!("settlement_detection_latency: root {seq} never settled");
            return;
        }
        lat.push(u64::try_from(t0.elapsed().as_nanos()).unwrap_or(u64::MAX));
    }

    let s: Stats = summarize(lat);
    let us = |ns: u64| ns as f64 / 1000.0;
    println!();
    println!("=== settlement-detection latency (inject → Settled), trivial single-mail root ===");
    println!(
        "{workers}w, {} samples, jittered injection (decorrelated from the drainer cycle)",
        s.n
    );
    println!("Phase 2 (this branch): emit-time counter fires Settled synchronously on the");
    println!("producing thread — one atomic + the registry → driver notice hop. Compare against");
    println!(
        "main (the drained path: drainer park + observer fold + Settled mail hop, ~0.9ms p50)."
    );
    println!(
        "  p50 {:.1}µs  p90 {:.1}µs  p99 {:.1}µs  max {:.1}µs",
        us(s.p50),
        us(s.p90),
        us(s.p99),
        us(s.max)
    );
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
    let wide = wide_fanout_widths_from_env();
    if !wide.is_empty() {
        println!(
            "wide fan-outs: {wide:?} (sweep AETHER_LOCAL_STICKY_MAX=1 vs width to find W*; wide cells auto-cap frames)"
        );
    }
    println!("{OBSERVE_FRAMES} frames/cell (wide cells fewer); relay-hop (`Ping`) samples only.");
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
