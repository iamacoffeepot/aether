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

// Test-only (`#[cfg(test)]`): the on-demand `#[ignore]` profiling tests in this
// module read their tuning knobs (WORKERS / TOKENS / PROFILE_SECS /
// SETTLE_SAMPLES) from env so a run can be parameterised from the shell. No cap,
// no config layer — the whole module opts out of the env-read ban.
#![allow(clippy::disallowed_methods)]

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::sync::Arc;
use std::thread::{self, available_parallelism};
use std::time::{Duration, Instant};

use aether_data::{Kind, KindId, MailId, MailboxId, mailbox_id_from_name};
use aether_kinds::trace::{
    DescribeTreeResult, MailNodeWire, TraceEvent, TraceRingEntry, TraceTail, TraceTailResult,
};
use aether_substrate::chassis::settlement::{
    TerminalDisposition, WaitOutcome, await_internal_signal,
};
use aether_substrate::{BootError, NativeActor, NativeCtx, NativeDispatch, NativeInitCtx, Subname};

use super::TestBench;
use crate::perf::harness::{
    CellResult, Drive, Ping, Relay, RelayConfig, Stats, SweepConfig, Tier, Topology,
    default_topologies, depth_chain, fanout, fanout_heavy, heavy_work_iters_from_env,
    pace_hz_from_env, relay_id, run_sweep, summarize, tiers_from_env, two_level_tree,
    wide_fanout_widths_from_env,
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
        ctx: &mut NativeCtx<'_, aether_substrate::Manual>,
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

// Harness wires its synthetic relay-ring topology from precomputed name-hashed
// ids before any actor spawns — id derivation, not sibling-cap addressing.
#[allow(clippy::disallowed_methods)]
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
        ctx: &mut NativeCtx<'_, aether_substrate::Manual>,
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

// Harness derives the hold-relay's id from its name to wire the topology —
// id derivation, not sibling-cap addressing.
#[allow(clippy::disallowed_methods)]
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

/// Per-round settlement patience (the log cadence of the escalating
/// wait, issue #1305).
const SETTLE_TIMEOUT: Duration = Duration::from_secs(10);

/// Cumulative patience cap before a settlement gate panics attributably
/// (issue #1305). A starved-but-healthy chain settles before this cap; a
/// genuine wedge exhausts it and the helper `panic!`s at the gate site.
const SETTLE_CAP: Duration = Duration::from_secs(50);

/// Panic-on-wedge settlement wait shared by the mail-latency assertions:
/// escalating patience under the wait, attributable panic at the gate on
/// a genuine wedge instead of a downstream assertion. `Panic` diverges
/// inside the helper, so a return means the chain settled.
fn assert_settled(rx: &crossbeam_channel::Receiver<()>, gate: &str) {
    match await_internal_signal(
        rx,
        gate,
        SETTLE_TIMEOUT,
        SETTLE_CAP,
        TerminalDisposition::Panic,
    ) {
        WaitOutcome::Settled => {}
        WaitOutcome::Wedged(_) => unreachable!("Panic disposition diverges on a wedge"),
    }
}

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
    // `WORKERS=1` isolates the warm per-hop dispatch glue with zero
    // cross-worker contention (the inline demux keeps every ring hop on
    // the one worker, which is always fed by the circulating tokens) —
    // the clean profile for the warm-floor decomposition. Unset, the
    // default saturates the whole pool (the throughput/contention view).
    let workers = env::var("WORKERS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|&w| w >= 1)
        .unwrap_or_else(|| available_parallelism().map_or(2, |n| n.get().saturating_sub(1).max(1)));
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

/// Settlement regression guard over a depth chain
/// (iamacoffeepot/aether#1059): drive 800 concurrent roots through a
/// multi-worker pool and assert every one *settles*. Post-ADR-0086 the
/// emit-time `SettlementCounter` is the settlement authority — a stuck
/// `in_flight` (broken lineage accounting) leaves a root's cell non-zero
/// forever, so its receiver never fires and the test times out. The
/// depth-chain topology + high root count complement
/// [`emit_settlement_settles_every_root`]'s fan-out / two-parent
/// coverage. (Pre-3c this also asserted each settled tree was complete,
/// guarding the observer fold against settling on a truncated chain;
/// that failure mode retired with the fold — the counter never settles
/// on a partial chain.)
#[allow(clippy::print_stderr)]
fn depth_chain_settles_every_root() {
    let workers = available_parallelism().map_or(2, |n| n.get().saturating_sub(1).max(1));
    let Ok(tb) = TestBench::builder()
        .with_workers(Some(workers))
        .size(16, 16)
        .build()
    else {
        eprintln!("skipping depth_chain_settles_every_root: TestBench boot failed (no wgpu)");
        return;
    };

    let depth = 5;
    let topo = depth_chain(depth);
    spawn_topology(&tb, &topo);
    let entry = relay_id(0);

    let roots = 800u32;
    let mut pending = Vec::with_capacity(roots as usize);
    for seq in 0..roots {
        pending.push(tb.inject_root(entry, Ping::ID, Ping { seq }.encode_into_bytes()));
    }

    for (idx, (_root, rx)) in pending.iter().enumerate() {
        assert_settled(rx, &format!("mlat.per_root_lineage[{idx}]"));
    }
}

/// Contention/backoff-sensitive tests live in `mod heavy`: these settlement
/// scenarios drive a multi-worker pool whose park/wake backoff path
/// oversubscribes cores under the full suite, so they are serialized into the
/// `serial-heavy` nextest group (`.config/nextest.toml`). Each delegates
/// to the scenario body declared at module scope.
mod heavy {
    #[test]
    fn depth_chain_settles_every_root() {
        super::depth_chain_settles_every_root();
    }

    #[test]
    fn emit_settlement_settles_two_level_tree() {
        super::emit_settlement_settles_two_level_tree();
    }

    #[test]
    fn emit_settlement_settles_wide_fanout() {
        super::emit_settlement_settles_wide_fanout();
    }

    #[test]
    fn emit_settlement_settles_under_chunked_demux() {
        super::emit_settlement_settles_under_chunked_demux();
    }
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
        assert_settled(rx, &format!("mlat.emit_time_counter[{idx}]"));
    }
}

/// Rich topology: fan-out + a shared (two-parent) node + depth.
fn emit_settlement_settles_two_level_tree() {
    emit_settlement_settles_every_root(&two_level_tree());
}

/// ADR-0087 Phase 3b focused guard: a wide single-source fan-out — one
/// handler emits to N free recipients in **one blob**, the exact shape
/// the blob demux runs (claim-or-deposit per recipient, free ones inline
/// on the demuxing worker). Every injected root must settle, proving the
/// inline-demux path balances `Sent`/`Finished` for every fanned mail —
/// a dropped or double-counted demux mail would wedge `in_flight` and
/// the root would never settle.
fn emit_settlement_settles_wide_fanout() {
    emit_settlement_settles_every_root(&fanout(8));
}

/// ADR-0087 Phase 3c: with the inline-demux chunk cap forced small
/// (`K=2`), a wide fan-out splits into stealable sub-blobs that cascade
/// across workers (`split_off` moves the mail handles — zero ring-byte
/// copy). Settlement must stay **exact** across the split: the per-mail
/// eager `Sent` (at buffer time) + dispatch-time `Finished` accounting is
/// split-invariant, so a dropped or double-counted chunk mail would wedge
/// `in_flight` and a root would never settle. Also drives the
/// steal-mid-demux race (a sibling steals a remainder sub-blob while the
/// producer runs its own chunk).
#[allow(clippy::print_stderr)]
fn emit_settlement_settles_under_chunked_demux() {
    // Force chunking on for this process. SAFETY: set before any actor is
    // booted (so before any blob flushes / `demux_chunk` is first read),
    // and nextest isolates each test in its own process — the memoised
    // first read picks up this value. No restore needed: the OnceLock is
    // process-lived and the process ends with the test.
    unsafe {
        env::set_var("AETHER_BLOB_DEMUX_CHUNK", "2");
    }
    emit_settlement_settles_every_root(&fanout(8));
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
        assert_settled(rx, &format!("mlat.hold_release[{idx}]"));
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
    assert_settled(&rx, "mlat.trace_ring_dual_write");

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

/// Issue 1990: a non-default `trace_ring_capacity` set on the
/// `TestBenchBuilder` is honoured by the chassis-host trace ring — drive
/// more off-actor injected roots than the small cap and the unfiltered
/// tail reports `truncated_before` (the FIFO-eviction gap cursor). The
/// chassis-host ring is outside the `Spawner`/builder slot path, so this
/// directly exercises the explicit `set_chassis_host_ring_capacity`
/// wiring at boot.
#[test]
#[allow(clippy::print_stderr)]
fn small_trace_ring_cap_laps_chassis_host_ring() {
    const CAP: usize = 4;
    const INJECTS: usize = CAP + 6;
    let Ok(tb) = TestBench::builder()
        .with_workers(Some(2))
        .trace_ring_capacity(Some(CAP))
        .size(16, 16)
        .build()
    else {
        eprintln!("skipping small_trace_ring_cap_laps_chassis_host_ring: no wgpu adapter");
        return;
    };

    // No actor at this id: each inject pushes exactly one `Sent` into the
    // chassis-host ring (off-actor producer) and nothing else, so the
    // ring's depth is deterministic. The mail warn-drops with no
    // recipient; we don't await settlement.
    let orphan = relay_id(0);
    for seq in 0..INJECTS {
        let _ = tb.inject_root(
            orphan,
            Ping::ID,
            Ping {
                seq: u32::try_from(seq).unwrap_or(u32::MAX),
            }
            .encode_into_bytes(),
        );
    }

    // Unfiltered tail from the start cursor: the ring holds at most CAP
    // entries, so the earliest surviving sequence is past `since + 1` and
    // `truncated_before` flags the evicted prefix.
    let (entries, truncated_before) = match tb.chassis_host_trace_tail(&TraceTail {
        max: 0,
        since: None,
        root: None,
    }) {
        TraceTailResult::Ok {
            entries,
            truncated_before,
            ..
        } => (entries, truncated_before),
        TraceTailResult::Err { error } => panic!("chassis-host trace.tail error: {error}"),
    };
    assert!(
        entries.len() <= CAP,
        "ring retained more than its {CAP}-entry cap: {} entries",
        entries.len()
    );
    assert!(
        truncated_before.is_some(),
        "expected a truncated_before gap after lapping a {CAP}-cap ring with {INJECTS} \
         injects; got entries={entries:?}",
    );
}

/// Issue 1990: the configured `trace_ring_capacity` reaches a spawned
/// actor's *per-actor* trace ring (the `Spawner` spawn funnel seeds it),
/// not just the chassis-host ring. Drive a single relay (no downstream)
/// enough roots that its ring — two slots per inbound mail (`Received` +
/// `Finished`) — laps the small cap, then a root-unfiltered
/// `aether.trace.tail` against the relay reports `truncated_before`.
#[test]
#[allow(clippy::print_stderr)]
fn small_trace_ring_cap_laps_per_actor_ring() {
    const CAP: usize = 6;
    // Two trace slots per settled inbound mail (Received + Finished), so
    // enough roots to comfortably overrun CAP. Each is settled before the
    // next so the ring fills deterministically.
    const INJECTS: usize = CAP * 3;
    let Ok(mut tb) = TestBench::builder()
        .with_workers(Some(2))
        .trace_ring_capacity(Some(CAP))
        .size(16, 16)
        .build()
    else {
        eprintln!("skipping small_trace_ring_cap_laps_per_actor_ring: no wgpu adapter");
        return;
    };

    // Single leaf relay (depth-1 chain: relay 0, no downstream) — each
    // inbound mail writes exactly Received + Finished into its own ring
    // and nothing fans out.
    spawn_topology(&tb, &depth_chain(1));
    for seq in 0..INJECTS {
        let (_root, rx) = tb.inject_root(
            relay_id(0),
            Ping::ID,
            Ping {
                seq: u32::try_from(seq).unwrap_or(u32::MAX),
            }
            .encode_into_bytes(),
        );
        assert_settled(&rx, "mlat.small_trace_ring_cap_laps_per_actor_ring");
    }

    // The relay's ring holds at most CAP entries, so its earliest
    // surviving sequence is past the start cursor — `truncated_before`
    // flags the evicted prefix. Query without a root filter (it would
    // also drop the trace-query mail's own Received/Finished, but the gap
    // cursor is computed over the whole ring regardless of the filter).
    let req = TraceTail {
        max: 0,
        since: None,
        root: None,
    }
    .encode_into_bytes();
    let reply = tb
        .send_bytes_and_await("mlat.relay:0", TraceTail::ID, req)
        .expect("aether.trace.tail reply");
    let truncated_before =
        match TraceTailResult::decode_from_bytes(&reply).expect("decode TraceTailResult") {
            TraceTailResult::Ok {
                entries,
                truncated_before,
                ..
            } => {
                assert!(
                    entries.len() <= CAP,
                    "per-actor ring retained more than its {CAP}-entry cap: {} entries",
                    entries.len()
                );
                truncated_before
            }
            TraceTailResult::Err { error } => panic!("trace.tail error: {error}"),
        };
    assert!(
        truncated_before.is_some(),
        "expected a truncated_before gap after lapping a {CAP}-cap per-actor ring",
    );
}

/// ADR-0086 Phase 3: the decentralized guided walk reconstructs a
/// causally-coherent tree for a settled root over the per-actor rings —
/// the rings are the source of truth post-3c (the central observer this
/// once cross-checked against retired with the fold). Drive a branching
/// topology (the diamond `two_level_tree`, where relay 4 has two
/// parents), settle one injected root, walk it, and assert the node
/// count + causal ordering ([`assert_causal_order`]).
///
/// The rings are written synchronously at the producer hooks, so a
/// single walk right after settlement sees the complete tree — no poll
/// loop. (A single walk also avoids the query self-pollution that a
/// tight repeated walk would cause: each `trace.tail` query mail records
/// its own `Received`/`Finished` into the queried actor's bounded ring.)
#[test]
#[allow(clippy::print_stderr)]
fn guided_walk_reconstructs_causal_tree() {
    let Ok(mut tb) = TestBench::builder()
        .with_workers(Some(2))
        .size(16, 16)
        .build()
    else {
        eprintln!("skipping guided_walk_reconstructs_causal_tree: no wgpu adapter");
        return;
    };

    spawn_topology(&tb, &two_level_tree());
    let (root, rx) = tb.inject_root(relay_id(0), Ping::ID, Ping { seq: 0 }.encode_into_bytes());
    assert_settled(&rx, "mlat.guided_walk");

    let mails = match tb.describe_tree_walked(root) {
        DescribeTreeResult::Ok { mails, .. } => mails,
        DescribeTreeResult::Err { not_found } => panic!("guided walk lost root {not_found:?}"),
    };
    // root (chassis -> relay 0) + 0->{1,2} + 1->{3,4} + 2->{4,5} = 7
    // mails (relay 4 receives two).
    assert_eq!(mails.len(), 7, "two_level_tree under one root is 7 mails");
    assert_causal_order(&mails);
}

/// Assert the causal invariants a correct trace tree must satisfy on its
/// own timestamps:
///
/// - per node, `t_sent <= t_enqueue <= t_received <= t_finished` (sent,
///   deposited into the recipient inbox, received, then the handler
///   returns — iamacoffeepot/aether#1134 inserts the deposit instant);
/// - per parent->child edge, `parent.t_received <= child.t_sent <=
///   parent.t_finished` — a child is sent from inside its parent's
///   handler, and all three reads land on the parent's dispatch thread
///   (one monotonic clock), so this holds even under the multi-worker
///   pool. (`child.t_received` is deliberately *not* bounded by
///   `parent.t_finished`: parallelism lets a child be received before
///   its parent's handler returns.)
fn assert_causal_order(mails: &[MailNodeWire]) {
    let by_id: BTreeMap<MailId, &MailNodeWire> = mails.iter().map(|n| (n.mail_id, n)).collect();
    for n in mails {
        if let Some(received) = n.t_received {
            assert!(
                n.t_sent.0 <= received.0,
                "node {:?}: t_sent {} > t_received {}",
                n.mail_id,
                n.t_sent.0,
                received.0
            );
            // iamacoffeepot/aether#1134: the deposit stamp rides the same
            // `Received` event, so it must be present whenever a node is
            // received, and must fall between send and receive (monotonic
            // process clock; deposit happens-after send, before pickup).
            let enq = n
                .t_enqueue
                .expect("a received node must carry t_enqueue (#1134)");
            assert!(
                n.t_sent.0 <= enq.0,
                "node {:?}: t_sent {} > t_enqueue {}",
                n.mail_id,
                n.t_sent.0,
                enq.0
            );
            assert!(
                enq.0 <= received.0,
                "node {:?}: t_enqueue {} > t_received {}",
                n.mail_id,
                enq.0,
                received.0
            );
            if let Some(finished) = n.t_finished {
                assert!(
                    received.0 <= finished.0,
                    "node {:?}: t_received {} > t_finished {}",
                    n.mail_id,
                    received.0,
                    finished.0
                );
            }
        }
        if let Some(parent_id) = n.parent {
            let parent = by_id
                .get(&parent_id)
                .unwrap_or_else(|| panic!("parent {parent_id:?} of {:?} absent", n.mail_id));
            if let Some(parent_received) = parent.t_received {
                assert!(
                    parent_received.0 <= n.t_sent.0,
                    "child {:?} sent at {} before parent {:?} received at {}",
                    n.mail_id,
                    n.t_sent.0,
                    parent_id,
                    parent_received.0
                );
            }
            if let Some(parent_finished) = parent.t_finished {
                assert!(
                    n.t_sent.0 <= parent_finished.0,
                    "child {:?} sent at {} after parent {:?} finished at {}",
                    n.mail_id,
                    n.t_sent.0,
                    parent_id,
                    parent_finished.0
                );
            }
        }
    }
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
/// cost). `AETHER_LATENCY_PACE_HZ=60` paces one frame per period instead
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

    // The trivial (light-tier) default set always runs. With the heavy tier
    // selected (AETHER_PERF_TIER=light,heavy; ADR-0085 amendment), append
    // CPU-heavy fan-outs so the sweep can also exhibit the parallelism-wins
    // regime (iamacoffeepot/aether#1074); the heavy spin magnitude comes from
    // AETHER_LATENCY_HEAVY_WORK (now magnitude-only, with a non-zero
    // default). Without the heavy tier the grid is the historical light one.
    let mut topologies = default_topologies();
    if tiers_from_env().contains(&Tier::Heavy) {
        let heavy = heavy_work_iters_from_env();
        for b in [2usize, 4, 8] {
            topologies.push(fanout_heavy(b, heavy));
        }
    }
    // Wide trivial fan-outs to locate the stickiness width-crossover
    // (iamacoffeepot/aether#1075); empty unless AETHER_LATENCY_WIDE_FANOUT is
    // set, so the default grid is unchanged.
    for w in wide_fanout_widths_from_env() {
        topologies.push(fanout(w));
    }

    let cfg = SweepConfig {
        workers: worker_set,
        topologies,
        frames: OBSERVE_FRAMES,
        drive: Drive::Latency { pace_hz },
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
        assert_settled(&rx, &format!("mlat.settlement_detection_latency[{seq}]"));
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
    println!(
        "per-actor trace rings (aether.trace.tail per relay) — no injector, no per-root block."
    );
    if let Some(hz) = pace_hz {
        println!("paced @ {hz} Hz — workers park between frames (realistic frame loop)");
    } else {
        println!("flat-out advance — workers stay warm (isolates per-hop dispatch cost)");
    }
    if tiers_from_env().contains(&Tier::Heavy) {
        let heavy = heavy_work_iters_from_env();
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

    // iamacoffeepot/aether#1158: the per-mail lifecycle decomposes into
    // four non-overlapping single-property spans covering first-send →
    // handler-done. CONSTRUCT + QUEUED + DRAIN sum to the producer→pickup
    // span; each measures one thing (blob build vs wakeup vs in-blob
    // serialization vs handler work).
    for (label, pick) in [
        (
            "CONSTRUCT    (t_sent - t_construct_start: blob open → flush-begin = producer builds the blob)",
            0usize,
        ),
        (
            "QUEUED       (t_enqueue - t_sent: flush-begin → worker picks up the blob = wakeup/schedule)",
            1,
        ),
        (
            "DRAIN        (t_received - t_enqueue: pickup → handler entry = where in the blob's drain it landed)",
            2,
        ),
        (
            "HANDLER DUR  (t_finished - t_received: relay forward work)",
            3,
        ),
    ] {
        println!("-- {label} --");
        println!(
            "{:>3}w  {:<16} {:<5} {:>9} {:>9} {:>9} {:>9} {:>7}",
            "", "topology", "cond", "p50", "p90", "p99", "max", "n"
        );
        for r in rows {
            let s = match pick {
                0 => r.construct,
                1 => r.queued,
                2 => r.drain,
                _ => r.handler,
            };
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

    // iamacoffeepot/aether#1134: enqueue depth is a *count* (scheduler
    // ready-queue len at deposit), printed raw — not µs. p50 ≈ 0 means
    // `queued` is wakeup-dominated (empty queue); a rising tail is
    // wait-behind-N offered load (the fan-out queueing signal).
    println!(
        "-- ENQUEUE DEPTH (scheduler ready-queue len at deposit; counts, not µs: 0 = wakeup, n = behind-n) --"
    );
    println!(
        "{:>3}w  {:<16} {:<5} {:>9} {:>9} {:>9} {:>9} {:>7}",
        "", "topology", "cond", "p50", "p90", "p99", "max", "n"
    );
    for r in rows {
        let s = r.depth;
        println!(
            "{:>3}   {:<16} {:<5} {:>9} {:>9} {:>9} {:>9} {:>7}",
            r.workers, r.topo, cond, s.p50, s.p90, s.p99, s.max, s.n
        );
    }
    println!();
}
