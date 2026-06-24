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

// Dev/bench tooling: every `*_from_env` knob in this latency-sweep harness reads
// its run parameters from env (workers / topology / pacing / tiers / fan-out).
// This is a bench driver, not a capability — there is no config layer in scope,
// so the whole module opts out of the env-read ban.
#![allow(clippy::disallowed_methods)]

use std::env;
use std::hint::black_box;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use aether_actor::OutboundReply;
use aether_actor::trace_ring::DEFAULT_TRACE_RING_CAP;
use aether_capabilities::trace_walk::fold_nodes;
use aether_data::{Kind, KindId, MailboxId, mailbox_id_from_name};
use aether_kinds::trace::{MailNodeWire, TraceRingEntry, TraceTail, TraceTailResult};
use aether_kinds::{LifecycleSubscribe, LifecycleSubscribeResult, Tick};
use aether_substrate::{BootError, NativeActor, NativeCtx, NativeInitCtx, Subname};

use crate::perf::report::LatencySection;
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

/// Run-end harvest query (iamacoffeepot/aether#1233): the harness mails one
/// of these to each participating actor after the drive loop to pull its
/// plain-field `Ping` counters out-of-band. The counters live in the actor's
/// own state (no shared atomics), so the only way to read them cross-thread is
/// a mail the actor answers — matching the existing `aether.trace.tail`
/// harvest flow. The body is meaningless (the kind id is the whole signal); a
/// single field keeps it a well-formed `Pod`.
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
#[kind(name = "mlat.count_query")]
pub struct CountQuery {
    /// Unused; present only so the query carries a non-empty `Pod` body.
    pub nonce: u32,
}

/// The reply to a [`CountQuery`] (iamacoffeepot/aether#1233): one actor's
/// `Ping` throughput counters. The real tier's keep-up metric sums these
/// across the topology — `offered = Σ sent`, `completed = Σ received` — to
/// report completed-vs-offered without touching the (lapping) trace ring.
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
#[kind(name = "mlat.count_report")]
pub struct CountReport {
    /// `Ping` mails this actor dispatched downstream — the source's per-tick
    /// emissions, or a relay's per-inbound forwards.
    pub sent: u64,
    /// `Ping` mails this actor received and handled. Relays only; the source
    /// handles `Tick`, never `Ping`, so its `received` is always 0.
    pub received: u64,
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
/// (the `Addressable` default).
pub struct Relay {
    downstreams: Arc<[MailboxId]>,
    work_iters: u64,
    /// `Ping` mails handled, for the run-end keep-up harvest
    /// (iamacoffeepot/aether#1233). A plain field — the actor is
    /// single-threaded over its own state, so no atomics.
    received: u64,
    /// `Ping` mails forwarded downstream, for the same harvest.
    sent: u64,
}

impl aether_actor::Addressable for Relay {
    const NAMESPACE: &'static str = "mlat.relay";
    type Resolver = aether_actor::Many;
}
impl aether_actor::HandlesKind<Ping> for Relay {}
impl aether_actor::Lifecycle for Relay {
    type Config = RelayConfig;
    type InitError = BootError;
    type InitCtx<'a> = NativeInitCtx<'a>;
    type Ctx<'a> = NativeCtx<'a>;
    fn init(config: Self::Config, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self {
            downstreams: config.downstreams,
            work_iters: config.work_iters,
            received: 0,
            sent: 0,
        })
    }
}
impl aether_substrate::actor::native::Lifecycle<Self> for Relay {
        type Config = <Self as aether_actor::Lifecycle>::Config;
        fn init(__c: Self::Config, __ctx: &mut aether_substrate::NativeInitCtx<'_>) -> Result<Self, aether_substrate::BootError> {
            <Self as aether_actor::Lifecycle>::init(__c, __ctx)
        }
        fn wire(__s: &mut Self, __ctx: &mut aether_substrate::NativeCtx<'_>) {
            <Self as aether_actor::Lifecycle>::wire(__s, __ctx)
        }
        fn unwire(__s: &mut Self, __ctx: &mut aether_substrate::NativeCtx<'_>) {
            <Self as aether_actor::Lifecycle>::unwire(__s, __ctx)
        }
    }

    impl aether_substrate::actor::native::Dispatch<Self> for Relay {
        fn dispatch(__s: &mut Self, __ctx: &mut aether_substrate::NativeCtx<'_, aether_substrate::Manual>, __k: aether_substrate::mail::KindId, __p: &[u8]) -> Option<()> {
            Self::__aether_dispatch_envelope(__s, __ctx, __k, __p)
        }
    }

    impl aether_substrate::actor::native::NativeActor for Relay { type State = Self; }
impl Relay {
    fn __aether_dispatch_envelope(
        &mut self,
        ctx: &mut NativeCtx<'_, aether_substrate::Manual>,
        kind: KindId,
        payload: &[u8],
    ) -> Option<()> {
        // Run-end keep-up harvest (iamacoffeepot/aether#1233): answer the
        // out-of-band counter query before the `Ping` fast path.
        if kind.0 == CountQuery::ID.0 {
            ctx.reply(&CountReport {
                sent: self.sent,
                received: self.received,
            });
            return Some(());
        }
        if kind.0 != Ping::ID.0 {
            return None;
        }
        self.received += 1;
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
            self.sent += 1;
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
// Harness wires its synthetic relay topology from precomputed name-hashed ids
// before any actor spawns — id derivation, not sibling-cap addressing.
#[must_use]
#[allow(clippy::disallowed_methods)]
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
    /// `Ping` mails emitted into the entry, for the run-end keep-up harvest
    /// (iamacoffeepot/aether#1233) — the offered load. `seq` wraps at `u32`
    /// for trace legibility; this is the honest cumulative count.
    sent: u64,
}

impl aether_actor::Addressable for TickSource {
    const NAMESPACE: &'static str = "mlat.ticksrc";
    type Resolver = aether_actor::Many;
}
impl aether_actor::HandlesKind<Tick> for TickSource {}
impl aether_actor::Lifecycle for TickSource {
    /// `(entry, burst)`: the relay-0 mailbox and the number of `Ping`s to
    /// emit per `Tick` (`1` in `Latency`, `backlog` in `Saturate`).
    type Config = (MailboxId, u32);
    type InitError = BootError;
    type InitCtx<'a> = NativeInitCtx<'a>;
    type Ctx<'a> = NativeCtx<'a>;
    fn init(config: Self::Config, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        let (entry, burst) = config;
        Ok(Self {
            entry,
            burst,
            seq: 0,
            sent: 0,
        })
    }
}
impl aether_substrate::actor::native::Lifecycle<Self> for TickSource {
        type Config = <Self as aether_actor::Lifecycle>::Config;
        fn init(__c: Self::Config, __ctx: &mut aether_substrate::NativeInitCtx<'_>) -> Result<Self, aether_substrate::BootError> {
            <Self as aether_actor::Lifecycle>::init(__c, __ctx)
        }
        fn wire(__s: &mut Self, __ctx: &mut aether_substrate::NativeCtx<'_>) {
            <Self as aether_actor::Lifecycle>::wire(__s, __ctx)
        }
        fn unwire(__s: &mut Self, __ctx: &mut aether_substrate::NativeCtx<'_>) {
            <Self as aether_actor::Lifecycle>::unwire(__s, __ctx)
        }
    }

    impl aether_substrate::actor::native::Dispatch<Self> for TickSource {
        fn dispatch(__s: &mut Self, __ctx: &mut aether_substrate::NativeCtx<'_, aether_substrate::Manual>, __k: aether_substrate::mail::KindId, __p: &[u8]) -> Option<()> {
            Self::__aether_dispatch_envelope(__s, __ctx, __k, __p)
        }
    }

    impl aether_substrate::actor::native::NativeActor for TickSource { type State = Self; }
impl TickSource {
    fn __aether_dispatch_envelope(
        &mut self,
        ctx: &mut NativeCtx<'_, aether_substrate::Manual>,
        kind: KindId,
        _payload: &[u8],
    ) -> Option<()> {
        // Run-end keep-up harvest (iamacoffeepot/aether#1233): the source
        // never receives a `Ping`, so its `received` is 0.
        if kind.0 == CountQuery::ID.0 {
            ctx.reply(&CountReport {
                sent: self.sent,
                received: 0,
            });
            return Some(());
        }
        if kind.0 != Tick::ID.0 {
            return None;
        }
        for _ in 0..self.burst {
            let bytes = Ping { seq: self.seq }.encode_into_bytes();
            self.seq = self.seq.wrapping_add(1);
            let _ = ctx.send_envelope_traced(self.entry, Ping::ID, &bytes);
            self.sent += 1;
        }
        Some(())
    }
}

const TICKSRC_NS: &str = "mlat.ticksrc";

/// Deterministic id for the single tick source (subname `"src"`).
// Harness derives the single tick-source id from its name to wire the topology
// — id derivation, not sibling-cap addressing.
#[must_use]
#[allow(clippy::disallowed_methods)]
pub fn ticksrc_id() -> MailboxId {
    MailboxId(mailbox_id_from_name(&format!("{TICKSRC_NS}:src")).0)
}

/// A workload tier (ADR-0085 amendment 2026-05-27): the three classes of
/// shape the dispatch perf comparison measures, distinguished by what each
/// isolates and how much its run-to-run variance lets the report *claim*.
/// Verdict treatment follows the variance, not the tier's importance — only
/// [`Tier::Light`] is classified pass/improved/regressed; [`Tier::Heavy`]
/// and [`Tier::Real`] are characterisation (numbers + direction + graphs, no
/// verdict). The tier rides on each [`Topology`] (and is threaded through
/// [`CellSamples`] / [`CellResult`] to the report builder), so the renderer
/// can suppress the verdict for a non-`light` section.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tier {
    /// Trivial micro-shapes (`work_iters = 0`) — isolates dispatch/routing
    /// mechanics; low variance. The regression gate.
    Light,
    /// The same shapes with a `busy_spin` CPU budget per node — exposes the
    /// parallelism-vs-locality crossover. Medium variance.
    Heavy,
    /// Application graphs at representative scale, driven paced. High,
    /// machine-dependent variance. In PR 1 this parses but yields an empty
    /// topology set — the `real` factories land in PR 2.
    Real,
}

impl Tier {
    /// The report-section name prefix for this tier. `light` reuses the
    /// historical `latency` name verbatim (preserving the v3 back-compat
    /// shim and the existing fixtures); the others are tier-suffixed.
    #[must_use]
    pub fn section_name(self) -> &'static str {
        match self {
            Self::Light => LatencySection::NAME,
            Self::Heavy => "latency.heavy",
            Self::Real => "latency.real",
        }
    }

    /// Parse one tier token (case-insensitive); `None` for an unknown token.
    #[must_use]
    pub fn parse_token(tok: &str) -> Option<Self> {
        match tok.trim().to_ascii_lowercase().as_str() {
            "light" => Some(Self::Light),
            "heavy" => Some(Self::Heavy),
            "real" => Some(Self::Real),
            _ => None,
        }
    }
}

/// A topology is a DAG over relay indices: `downstreams[i]` lists the
/// relays that relay `i` forwards to. Relay 0 is always the entry. The
/// number of relays is `downstreams.len()`. `work_iters[i]` is the CPU
/// spin budget relay `i` burns per inbound `Ping` (see `busy_spin`) —
/// all-zero for the trivial topologies, non-zero on the heavy ones
/// (iamacoffeepot/aether#1074). `work_iters.len() == downstreams.len()`.
/// `tier` carries the workload tier (ADR-0085 amendment) through the sweep
/// to the report builder, so the renderer can suppress the verdict for a
/// non-`light` tier.
#[derive(Clone)]
pub struct Topology {
    pub name: String,
    pub downstreams: Vec<Vec<usize>>,
    pub work_iters: Vec<u64>,
    pub tier: Tier,
}

/// The widest fan-out in `topo` — the largest `downstreams[i].len()` over
/// all relays (0 for a topology with no edges). A relay records
/// `2 + out_degree` trace-ring slots per inbound mail (`Received` +
/// `Finished` on dispatch, plus one `Sent` per downstream), so this is the
/// fan-out multiplier in the per-actor ring-budget bound
/// `backlog * (2 + max_out_degree) <= ring_cap` that the `Saturate` burst
/// clamp in [`run_sweep_samples`] enforces (iamacoffeepot/aether#1226).
#[must_use]
pub fn max_out_degree(topo: &Topology) -> usize {
    topo.downstreams.iter().map(Vec::len).max().unwrap_or(0)
}

/// The downstream adjacency of a `d`-node forward chain `0 -> 1 -> ... ->
/// d-1`: each node forwards to its successor; the last is a leaf. Shared by
/// the [`depth_chain`] (light) and [`ui_roundtrip`] (real) factories so the
/// chain-build lives in one place.
fn forward_chain_edges(d: usize) -> Vec<Vec<usize>> {
    (0..d)
        .map(|i| if i + 1 < d { vec![i + 1] } else { vec![] })
        .collect()
}

/// `0 -> 1 -> ... -> d-1`. Each relay forwards to the next; the last is
/// a leaf.
#[must_use]
pub fn depth_chain(d: usize) -> Topology {
    let downstreams = forward_chain_edges(d);
    Topology {
        name: format!("depth-{d}"),
        work_iters: vec![0; downstreams.len()],
        tier: Tier::Light,
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
        tier: Tier::Light,
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
    t.tier = Tier::Heavy;
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
        tier: Tier::Light,
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
    t.tier = Tier::Heavy;
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
    t.tier = Tier::Heavy;
    for leaf in [3usize, 4, 5] {
        t.work_iters[leaf] = work_iters;
    }
    t
}

/// Starting fan-out width for the real tier's `socket-server` /
/// `tick-broadcast` shapes (ADR-0085 amendment). A modest value so local
/// `cargo test` cells stay fast; the empirically-settled per-shape `N` and
/// the per-PR fidelity cap (~64–128) land in PR 3 (iamacoffeepot/aether#1222).
pub const REAL_FANOUT_N: usize = 32;

/// Starting per-codec `busy_spin` budget for the real tier's heavy
/// decode/encode nodes (ADR-0085 amendment) — sized for a tens-of-µs
/// per-node cost at the harness's measured rate (read the HANDLER DUR column
/// to convert to wall-clock). A starting point, tuned + capped in PR 3.
pub const REAL_CODEC_WORK_ITERS: u64 = 20_000;

/// Starting per-node `busy_spin` budget for a real-tier *medium*-cost logic /
/// sim node (the join / broadcast hub) — lighter than a codec, heavier than a
/// trivial router. A starting point, tuned in PR 3.
pub const REAL_LOGIC_WORK_ITERS: u64 = 5_000;

/// The depth of the `ui-roundtrip` follow-up chain — the bounded, **unrolled**
/// sequence of post-response steps (ADR-0085 amendment: bounded UI loops are
/// unrolled to a finite depth, never introduced as cycles, so the trace stays
/// a DAG). A small fixed count; the real magnitude is tuned in PR 3.
pub const REAL_UI_FOLLOWUP_STEPS: usize = 4;

/// `socket-server-N` (ADR-0085 amendment; reshaped in
/// iamacoffeepot/aether#1233): a single-entry DAG modelling an N-connection
/// server as **N independent request→response chains** — the routing a real
/// socket server has, where each of the N requests flows to its *own* client's
/// response, not an N→N broadcast. The entry source fans each paced request to
/// `N` **decoder** nodes (heavy codec cost); each decoder forwards to its own
/// **logic** node (medium cost); each logic node to its own **encoder** node
/// (heavy codec cost); each encoder to its own **writer** leaf (the server
/// replying, trivial). No node is shared between chains, so the per-frame mail
/// volume is `O(N)` (`1 + 4N` `Ping` mails per root) rather than the `N²` a
/// shared broadcast join produced — every [`Relay`] forwards to exactly one
/// downstream past the source's fan, so it never amplifies. `Relay`'s
/// broadcast-to-all forwarding still fits unchanged (no conditional routing):
/// the source's single broadcast *is* the per-connection fan, and every
/// interior node has a single downstream.
///
/// Node layout (indices): `0` = source; `1..=N` = decoders; `N+1..=2N` =
/// logic; `2N+1..=3N` = encoders; `3N+1..=4N` = writers. Chain `i`
/// (`1 ≤ i ≤ N`) is `decoder i → logic N+i → encoder 2N+i → writer 3N+i`.
/// Total `4N + 1` nodes.
#[must_use]
pub fn socket_server(n: usize, codec_work: u64, logic_work: u64) -> Topology {
    let total = 4 * n + 1;
    let mut downstreams = vec![vec![]; total];
    let mut work_iters = vec![0u64; total];

    // 0: source → all N decoders (the connection fan).
    downstreams[0] = (1..=n).collect();
    for i in 1..=n {
        // Chain `i`: decoder → logic → encoder → writer, all private to this
        // connection. Indices step by `n` so the DAG stays strictly forward
        // (every edge points to a higher index → acyclic).
        let decoder = i;
        let logic = n + i;
        let encoder = 2 * n + i;
        let writer = 3 * n + i;
        downstreams[decoder] = vec![logic];
        downstreams[logic] = vec![encoder];
        downstreams[encoder] = vec![writer];
        work_iters[decoder] = codec_work;
        work_iters[logic] = logic_work;
        work_iters[encoder] = codec_work;
        // writers stay trivial leaves (downstreams empty, work 0).
    }

    Topology {
        name: format!("socket-server-{n}"),
        downstreams,
        work_iters,
        tier: Tier::Real,
    }
}

/// `tick-broadcast-N` (ADR-0085 amendment): a tick-paced source feeding a
/// single **sim** node (medium cost) that broadcasts to `N` **encoder** nodes
/// (heavy codec cost), each forwarding to **one writer** leaf. Models a
/// per-frame simulation step fanning state out to `N` connected clients. Pure
/// fan — broadcast-to-all fits [`Relay`] unchanged.
///
/// Node layout (indices): `0` = source; `1` = sim; `2..=N+1` = encoders;
/// `N+2..=2N+1` = writers. Total `2N + 2` nodes.
#[must_use]
pub fn tick_broadcast(n: usize, codec_work: u64, sim_work: u64) -> Topology {
    let total = 2 * n + 2;
    let mut downstreams = vec![vec![]; total];
    let mut work_iters = vec![0u64; total];

    // 0: source → sim.
    downstreams[0] = vec![1];
    // 1: sim (medium) → all N encoders.
    work_iters[1] = sim_work;
    let first_enc = 2;
    downstreams[1] = (first_enc..first_enc + n).collect();
    // encoders (heavy) → one writer each.
    let first_writer = first_enc + n; // N+2
    for k in 0..n {
        let enc = first_enc + k;
        let writer = first_writer + k;
        downstreams[enc] = vec![writer];
        work_iters[enc] = codec_work;
    }

    Topology {
        name: format!("tick-broadcast-{n}"),
        downstreams,
        work_iters,
        tier: Tier::Real,
    }
}

/// `ui-roundtrip` (ADR-0085 amendment): request → handler → response → a
/// **bounded, unrolled** follow-up chain of `followup_steps` nodes. The whole
/// shape is a finite-depth chain (NOT a cycle — the DAG stays acyclic), each
/// node forwarding the same payload to its single successor, so [`Relay`]'s
/// broadcast-to-(one) fits unchanged. Models a UI request/response with a
/// finite settle of follow-up work.
///
/// Node layout (indices): `0` = request (entry); `1` = handler (medium cost);
/// `2` = response; `3..` = the unrolled follow-up steps; the last is a leaf.
/// Total `3 + followup_steps` nodes.
#[must_use]
pub fn ui_roundtrip(followup_steps: usize, handler_work: u64) -> Topology {
    let total = 3 + followup_steps;
    // A straight chain: each node forwards to the next; the last is a leaf.
    let downstreams = forward_chain_edges(total);
    // 1: handler does the medium-cost work; everything else is trivial.
    let mut work_iters = vec![0u64; total];
    work_iters[1] = handler_work;

    Topology {
        name: "ui-roundtrip".to_owned(),
        downstreams,
        work_iters,
        tier: Tier::Real,
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

/// The real tier's keep-up characterisation (iamacoffeepot/aether#1233): a
/// sustained-paced run answers "does it keep up at 60 Hz", not the per-hop
/// span tree (whose volume laps the trace ring at real-tier fan-out). The
/// counters are harvested from the harness actors' plain fields at run end;
/// the timings bracket the paced drive loop. `Some` only for [`Tier::Real`]
/// cells (the only paced tier); `None` otherwise.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeepUp {
    /// Total `Ping` mails dispatched across the topology (`Σ sent`) — the
    /// offered load.
    pub offered: u64,
    /// Total `Ping` mails handled across the topology (`Σ received`) — the
    /// work completed. Equals `offered` when the pool drained everything the
    /// pace offered (the drain-integrity check); a shortfall means mail was
    /// left in flight.
    pub completed: u64,
    /// Wall-clock nanoseconds the paced drive loop took.
    pub elapsed_nanos: u64,
    /// Wall-clock nanoseconds the loop *should* have taken at the pace
    /// (`frames / pace_hz`). `elapsed / expected > 1` means the run fell
    /// behind the 60 Hz budget — the keep-up signal.
    pub expected_nanos: u64,
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
    /// The workload tier this cell's topology belongs to (ADR-0085
    /// amendment), threaded to the report builder so the renderer can
    /// suppress the verdict for a non-`light` tier.
    pub tier: Tier,
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
    /// [`TrialReport::from_cells`]: super::report::TrialReport::from_cells
    pub tier: Tier,
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
    /// [`TrialReport::from_cells`]: super::report::TrialReport::from_cells
    pub keepup: Option<KeepUp>,
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
#[must_use]
pub fn drive_for_tier(drive: Drive, tier: Tier) -> Drive {
    match tier {
        Tier::Real => Drive::Latency {
            pace_hz: Some(pace_hz_from_env().unwrap_or(DEFAULT_REAL_PACE_HZ)),
        },
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
pub const DEFAULT_SATURATE_BACKLOG: u32 = 512;

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
/// Resolve the *effective* per-actor trace-ring capacity for the
/// saturation-invariant math (issue 1990). Reads `AETHER_ACTOR_TRACE_RING_SIZE`
/// (the chassis-wide knob) when set, else the `aether-actor` const
/// [`DEFAULT_TRACE_RING_CAP`]. The sweep cell ([`run_sweep_samples`])
/// pins the same value on its `TestBench` so the `backlog * (2 +
/// out_degree) <= ring_cap` clamp and the ring the relay actually writes
/// agree — bumping the knob to chase a high-volume lap (the use case that
/// motivated the issue) lifts both together instead of silently keeping
/// 4096.
#[must_use]
pub fn effective_trace_ring_cap() -> usize {
    env::var("AETHER_ACTOR_TRACE_RING_SIZE")
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_TRACE_RING_CAP)
}

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
        Ok("saturate") => Drive::Saturate {
            backlog: saturate_backlog_from_env(),
        },
        _ => Drive::Latency {
            pace_hz: pace_hz_from_env(),
        },
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

/// Read `AETHER_PERF_TOPOS` (`full` → the whole [`default_topologies`] set;
/// anything else → the `ci` chain/fan-out/tree subset). This is the breadth
/// knob *within* a tier — the shape set the light tier sweeps and the heavy
/// tier mirrors with CPU burn — orthogonal to the [`tiers_from_env`] tier
/// axis.
#[must_use]
fn topos_full() -> bool {
    matches!(env::var("AETHER_PERF_TOPOS").as_deref(), Ok("full"))
}

/// The light tier's shapes: the trivial micro-topologies the breadth knob
/// selects, plus any opt-in wide fan-outs (`AETHER_LATENCY_WIDE_FANOUT`).
/// All carry [`Tier::Light`] from their factories.
#[must_use]
fn light_topologies() -> Vec<Topology> {
    let mut topos = if topos_full() {
        default_topologies()
    } else {
        vec![
            depth_chain(1),
            depth_chain(8),
            fanout(4),
            fanout(8),
            two_level_tree(),
        ]
    };
    for w in wide_fanout_widths_from_env() {
        topos.push(fanout(w));
    }
    topos
}

/// The heavy tier's shapes: the light fan-outs / two-level trees, each node
/// burning `work_iters` of `busy_spin` CPU. The narrow-heavy cascades stress
/// the keep-local time budget (iamacoffeepot/aether#1160): uniform-heavy (the
/// valve fires) and trivial-router→heavy-leaf (the valve's blind spot). All
/// carry [`Tier::Heavy`].
#[must_use]
fn heavy_topologies(work_iters: u64) -> Vec<Topology> {
    let mut topos = Vec::new();
    for b in [4usize, 8] {
        topos.push(fanout_heavy(b, work_iters));
    }
    topos.push(two_level_tree_heavy(work_iters));
    topos.push(two_level_tree_router_heavy(work_iters));
    topos
}

/// The real tier's shapes (ADR-0085 amendment): application graphs at a
/// representative — modest, local-test-fast — scale, driven **paced** by the
/// sweep (see [`drive_for_tier`]). All carry [`Tier::Real`] from their
/// factories. `N` / `work_iters` / `pace_hz` are starting points
/// ([`REAL_FANOUT_N`] / [`REAL_CODEC_WORK_ITERS`] / [`REAL_LOGIC_WORK_ITERS`]);
/// they are tuned + fidelity-capped in PR 3 (iamacoffeepot/aether#1222), which
/// also wires the env so the tier runs in CI.
#[must_use]
fn real_topologies() -> Vec<Topology> {
    vec![
        socket_server(REAL_FANOUT_N, REAL_CODEC_WORK_ITERS, REAL_LOGIC_WORK_ITERS),
        tick_broadcast(REAL_FANOUT_N, REAL_CODEC_WORK_ITERS, REAL_LOGIC_WORK_ITERS),
        ui_roundtrip(REAL_UI_FOLLOWUP_STEPS, REAL_LOGIC_WORK_ITERS),
    ]
}

/// Build the sweep's topology set from the selected tiers
/// ([`tiers_from_env`]) and the breadth knob (`AETHER_PERF_TOPOS`). Each tier
/// contributes its own shapes, tagged with its [`Tier`] so the report
/// sections by tier. Shared by the `perf-trial` and `perf-plot` bins.
#[must_use]
pub fn parse_topologies() -> Vec<Topology> {
    let mut topos = Vec::new();
    for tier in tiers_from_env() {
        match tier {
            Tier::Light => topos.extend(light_topologies()),
            Tier::Heavy => topos.extend(heavy_topologies(heavy_work_iters_from_env())),
            Tier::Real => topos.extend(real_topologies()),
        }
    }
    topos
}

/// Minimum completion count for the trimmed-window throughput estimate
/// (iamacoffeepot/aether#1227). Below this, dropping 10% off each end would
/// leave too few completions to characterise the saturated regime, so the cell
/// falls back to the full window — the ramp/tail are then an unavoidable but
/// small fraction of a short run.
const THROUGHPUT_TRIM_FLOOR: usize = 50;

/// Completed mails/sec from a cell's folded trace nodes — a **steady-state
/// estimate** over the saturated middle of the run
/// (iamacoffeepot/aether#1202, refined by iamacoffeepot/aether#1227).
///
/// A makespan average — `completed / (max(t_finished) − min(t_construct
/// start))` — folds the unsaturated **ramp-up** (the source still building
/// roots while most workers are cold) and the **drain-down tail** (a few mails
/// left, the pool under-utilised) into the denominator, so it systematically
/// understates the rate the pool sustains while the queue is actually deep,
/// with topology-dependent contamination. Instead, order the completions by
/// `t_finished`, trim the first and last 10% (the ramp and the tail), and take
/// the inter-completion rate over the inner window:
/// `completions_in_window / (t_finished_high − t_finished_low)`. Both sides of
/// the paired ADR-0085 comparison compute the same statistic, so the
/// higher-is-better delta semantics are unchanged.
///
/// A cell with fewer than [`THROUGHPUT_TRIM_FLOOR`] completions falls back to
/// the full window (no trim). Returns `None` when fewer than two `Ping`s
/// completed, or the window collapsed to a single instant (clock-coincident),
/// so the caller stores `None` rather than a divide-by-zero or infinite rate.
#[allow(clippy::cast_precision_loss)]
fn throughput_from_nodes(mails: &[MailNodeWire]) -> Option<f64> {
    let mut finishes: Vec<u64> = mails
        .iter()
        .filter(|node| node.kind.0 == Ping::ID.0)
        .filter_map(|node| node.t_finished.map(|fin| fin.0))
        .collect();
    if finishes.len() < 2 {
        return None;
    }
    finishes.sort_unstable();
    let n = finishes.len();

    // Trim 10% off each end to drop the fill ramp and the drain tail — but
    // only when enough completions remain for the inner window to still
    // represent the deep-queue regime. `n / 10` is `floor(0.10 · n)`.
    let (lo, hi) = if n >= THROUGHPUT_TRIM_FLOOR {
        let cut = n / 10;
        (cut, n - cut)
    } else {
        (0, n)
    };
    let window = &finishes[lo..hi];
    let completions = window.len();
    if completions < 2 {
        return None;
    }
    let span_nanos = window[completions - 1] - window[0];
    if span_nanos == 0 {
        return None;
    }
    let span_secs = span_nanos as f64 / 1e9;
    Some(completions as f64 / span_secs)
}

/// Harvest the real tier's keep-up counters (iamacoffeepot/aether#1233).
/// Mails a [`CountQuery`] to every participating actor (by the same names the
/// trace harvest used), sums `offered = Σ sent` and `completed = Σ received`,
/// and brackets them with the paced elapsed-vs-expected timing. Returns `None`
/// (logged) if any actor's reply fails to arrive or decode, so a botched
/// harvest yields no keep-up cell rather than a wrong one — mirroring the
/// trace harvest's fail-closed posture.
fn harvest_keepup(
    tb: &mut TestBench,
    names: &[String],
    topo_name: &str,
    drive: Drive,
    frames: u32,
    drive_elapsed: Duration,
) -> Option<KeepUp> {
    let mut offered = 0u64;
    let mut completed = 0u64;
    for name in names {
        let req = CountQuery::default().encode_into_bytes();
        let reply = match tb.send_bytes_and_await(name, CountQuery::ID, req) {
            Ok(reply) => reply,
            Err(e) => {
                tracing::warn!(target: "aether_perf", topo = %topo_name, %name, error = ?e, "count_query send failed");
                return None;
            }
        };
        let Some(report) = CountReport::decode_from_bytes(&reply) else {
            tracing::warn!(target: "aether_perf", topo = %topo_name, %name, "count_report decode failed");
            return None;
        };
        offered = offered.saturating_add(report.sent);
        completed = completed.saturating_add(report.received);
    }
    // Only a paced run has a budget to measure against; an unpaced cell never
    // reaches here (the real tier is always paced), so the `_ => 0` arm is a
    // belt-and-braces guard.
    let expected_nanos = match drive {
        Drive::Latency { pace_hz: Some(hz) } if hz > 0 => {
            u64::from(frames).saturating_mul(1_000_000_000 / hz)
        }
        _ => 0,
    };
    Some(KeepUp {
        offered,
        completed,
        elapsed_nanos: u64::try_from(drive_elapsed.as_nanos()).unwrap_or(u64::MAX),
        expected_nanos,
    })
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

    // Issue 1990: the effective trace-ring cap (env knob or const
    // default) governs both the sweep's `TestBench` rings and the
    // per-cell burst clamp below — resolved once so they can't drift.
    let trace_ring_cap = effective_trace_ring_cap();
    for &workers in &cfg.workers {
        for topo in &cfg.topologies {
            let Ok(mut tb) = TestBench::builder()
                .with_workers(Some(workers))
                .trace_ring_capacity(Some(trace_ring_cap))
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
            // The real tier is always driven paced regardless of `cfg.drive`
            // (ADR-0085 amendment); light / heavy keep the configured drive.
            let drive = drive_for_tier(cfg.drive, topo.tier);
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
            if let Err(e) = tb
                .spawn_actor::<TickSource>(Subname::Named("src"), (relay_id(0), burst))
                .finish()
            {
                tracing::warn!(target: "aether_perf", topo = %topo.name, error = ?e, "tick source spawn failed");
                continue;
            }

            // Subscribe the source to the `Tick` lifecycle stage so
            // `advance` broadcasts a tick to it each frame (ADR-0082).
            let sub_req = LifecycleSubscribe {
                stage: Tick::ID.0,
                mailbox: ticksrc_id().0,
            }
            .encode_into_bytes();
            match tb.send_bytes_and_await("aether.lifecycle", LifecycleSubscribe::ID, sub_req) {
                Ok(reply) => match LifecycleSubscribeResult::decode_from_bytes(&reply) {
                    Some(LifecycleSubscribeResult::Ok) => {}
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

            // Drive via the real lifecycle (per-tier drive resolved above).
            // Bracket the loop so the real tier's keep-up metric can compare
            // elapsed wall-clock against the paced budget
            // (iamacoffeepot/aether#1233).
            let drive_start = Instant::now();
            match drive {
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
            let throughput_mps = match drive {
                Drive::Saturate { .. } if !truncated => throughput_from_nodes(&mails),
                _ => None,
            };

            // iamacoffeepot/aether#1233: the real tier reports keep-up, not
            // span percentiles. Harvest each actor's plain-field `Ping`
            // counters out-of-band (the same name-addressed `send_and_await`
            // flow as the trace harvest above) and sum them: `offered =
            // Σ sent`, `completed = Σ received`. Sidesteps the trace ring
            // entirely, which the real tier's fan-out laps. Only the real
            // tier runs paced, so only it has a meaningful elapsed-vs-expected.
            let keepup = if topo.tier == Tier::Real {
                harvest_keepup(&mut tb, &names, &topo.name, drive, frames, drive_elapsed)
            } else {
                None
            };

            rows.push(CellSamples {
                workers,
                topo: topo.name.clone(),
                tier: topo.tier,
                construct,
                queued,
                drain,
                handler,
                depth,
                throughput_mps,
                keepup,
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
    fn fanout_8_at_default_backlog_reports_finite_rate() {
        // Regression (iamacoffeepot/aether#1226): the entry relay of
        // `fanout(8)` forwards each inbound root to 8 leaves, so it records
        // `2 + 8 = 10` trace-ring slots per root. At the default backlog
        // (512) that is `512 * 10 = 5120 > 4096` (the per-actor ring cap),
        // which lapped the ring, tripped the truncation gate, and dropped
        // `fanout-8`'s throughput cell entirely. `fanout(8)` is `Tier::Light`
        // (the default tier), so the `Saturate` arm survives `drive_for_tier`
        // and this is the exact reproduction at the default depth. The
        // per-cell burst clamp (`4096 / 10 = 409`) must keep the cell
        // measurable: a finite, positive, non-truncated rate.
        let Some(cell) = saturate_cell(2, fanout(8), DEFAULT_SATURATE_BACKLOG) else {
            eprintln!("skipping: no wgpu adapter");
            return;
        };
        let mps = cell.throughput_mps.expect(
            "fanout-8 at the default backlog must report a rate, not truncate to None \
             (iamacoffeepot/aether#1226)",
        );
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

    // The former `over_capacity_backlog_flags_truncation_not_a_wrong_rate`
    // lived here and fed an over-capacity backlog straight to the sweep to
    // force a lap. The per-cell burst clamp (iamacoffeepot/aether#1226) now
    // bounds every `Saturate` cell to `ring_cap / (2 + max_out_degree)`, so
    // the sweep path can no longer lap a ring — its premise is unreachable.
    // The truncation contract (a `None`-rate cell is surfaced flagged, not
    // dropped) is now report-side; the assertion moved to
    // `report::tests::truncated_cell_is_flagged_not_dropped`.

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

    /// A completed `Ping` node finishing at `t_finished_nanos`. Only `kind`
    /// (must be `Ping`) and `t_finished` drive `throughput_from_nodes`; every
    /// other field is filler.
    fn finished_ping(correlation: u64, t_finished_nanos: u64) -> MailNodeWire {
        use aether_data::MailId;
        use aether_kinds::trace::Nanos;
        MailNodeWire {
            mail_id: MailId {
                sender: MailboxId(0),
                correlation_id: correlation,
            },
            parent: None,
            sender: MailboxId(0),
            recipient: MailboxId(0),
            kind: Ping::ID,
            t_construct_start: Nanos(0),
            t_sent: Nanos(0),
            t_enqueue: None,
            enqueue_depth: None,
            t_received: None,
            t_finished: Some(Nanos(t_finished_nanos)),
            thread_name: None,
        }
    }

    #[test]
    #[allow(clippy::cast_precision_loss)]
    fn throughput_trims_ramp_and_tail_to_recover_steady_rate() {
        // iamacoffeepot/aether#1227: a node set with a slow fill ramp and a
        // slow drain tail bracketing an evenly-paced saturated middle. The
        // makespan average (whole-batch span) understates the deep-queue rate;
        // trimming 10% off each end must recover the injected steady rate.
        //
        // Steady middle: `STEADY` completions at a 1 ms spacing → 1000/sec.
        // Ramp + tail: `TRIM` completions each, spaced 20 ms (slow), so the
        // 10% trim drops exactly the ramp and the tail, leaving the steady run.
        const STEADY: u64 = 208;
        const TRIM: u64 = 26; // floor(0.10 × (208 + 26 + 26)) = 26
        const STEADY_DT_NANOS: u64 = 1_000_000; // 1 ms → 1000 completions/sec
        const SLOW_DT_NANOS: u64 = 20_000_000; // 20 ms ramp/tail spacing

        let mut nodes = Vec::new();
        let mut corr = 0u64;
        // Ramp: earliest finishes, far apart (cold pool filling).
        for i in 0..TRIM {
            nodes.push(finished_ping(corr, i * SLOW_DT_NANOS));
            corr += 1;
        }
        // Steady middle: evenly paced, starting after the ramp.
        let steady_base = TRIM * SLOW_DT_NANOS + SLOW_DT_NANOS;
        for i in 0..STEADY {
            nodes.push(finished_ping(corr, steady_base + i * STEADY_DT_NANOS));
            corr += 1;
        }
        // Tail: latest finishes, far apart (drain under-utilised).
        let tail_base = steady_base + STEADY * STEADY_DT_NANOS + SLOW_DT_NANOS;
        for i in 0..TRIM {
            nodes.push(finished_ping(corr, tail_base + i * SLOW_DT_NANOS));
            corr += 1;
        }

        let rate = throughput_from_nodes(&nodes).expect("a populated cell reports a rate");
        // The trimmed inner window is the evenly-paced steady run; the rate is
        // `completions / span = STEADY / ((STEADY-1)·dt)` ≈ 1000/sec.
        assert!(
            (rate - 1000.0).abs() < 20.0,
            "trimmed rate must recover the ~1000/sec steady rate, got {rate}"
        );
        // The full-batch makespan average is dragged far lower by the slow
        // ramp/tail — the contamination this fix removes.
        let total = STEADY + 2 * TRIM;
        let makespan_secs = (tail_base + (TRIM - 1) * SLOW_DT_NANOS) as f64 / 1e9;
        let makespan_rate = total as f64 / makespan_secs;
        assert!(
            makespan_rate < 600.0 && rate > makespan_rate * 1.5,
            "the trimmed rate ({rate}) must exceed the contaminated makespan rate ({makespan_rate})"
        );
    }

    #[test]
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    fn throughput_small_cell_falls_back_to_full_window() {
        // Below the trim floor, a cell uses the full window (no trim): a
        // handful of evenly-paced completions reports `completions / span`
        // over all of them, not a trimmed subset.
        let dt_nanos = 1_000_000u64; // 1 ms
        let count = 5u64;
        let nodes: Vec<MailNodeWire> = (0..count).map(|i| finished_ping(i, i * dt_nanos)).collect();
        assert!(
            (count as usize) < THROUGHPUT_TRIM_FLOOR,
            "this fixture must sit below the trim floor to exercise the fallback"
        );
        let rate = throughput_from_nodes(&nodes).expect("a small cell still reports a rate");
        // Full window: 5 completions over (5-1)×1 ms = 4 ms → 1250/sec.
        let expected = count as f64 / ((count - 1) * dt_nanos) as f64 * 1e9;
        assert!(
            (rate - expected).abs() < 1.0,
            "small-cell fallback rate {rate} must match the full-window rate {expected}"
        );
    }

    #[test]
    fn throughput_one_completion_is_none() {
        // A single completion has no window to measure a rate over → `None`,
        // not a divide-by-zero (the existing degenerate guard, at count 1).
        assert!(throughput_from_nodes(&[finished_ping(0, 42)]).is_none());
    }

    #[test]
    fn throughput_clock_coincident_completions_are_none() {
        // Many completions, all at the same instant → zero span → `None`
        // rather than an infinite rate.
        let nodes: Vec<MailNodeWire> = (0..8).map(|i| finished_ping(i, 5_000)).collect();
        assert!(throughput_from_nodes(&nodes).is_none());
    }

    /// A `Topology`'s structural invariants hold for any factory: the two
    /// per-node vectors are the same length, every downstream index is in
    /// range, and the DAG is acyclic (every edge points forward — all our
    /// real shapes wire strictly increasing indices). Factored so each
    /// real-shape test asserts the same invariants without copy-pasting the
    /// checks (keeps Qodana's `DuplicatedCode` quiet).
    fn assert_well_formed_real(topo: &Topology, expected_nodes: usize) {
        assert_eq!(topo.tier, Tier::Real, "real factory must tag Tier::Real");
        assert_eq!(
            topo.downstreams.len(),
            expected_nodes,
            "node count for {}",
            topo.name
        );
        assert_eq!(
            topo.work_iters.len(),
            topo.downstreams.len(),
            "work_iters must be one-per-node for {}",
            topo.name
        );
        for (i, downs) in topo.downstreams.iter().enumerate() {
            for &j in downs {
                assert!(
                    j < topo.downstreams.len(),
                    "{} edge {i}->{j} out of range",
                    topo.name
                );
                assert!(
                    j > i,
                    "{} edge {i}->{j} is not forward — the DAG must stay acyclic",
                    topo.name
                );
            }
        }
    }

    /// Total `Ping` mails one root drives through `topo` — the per-frame mail
    /// volume (iamacoffeepot/aether#1233). A single forward pass: the source
    /// receives one root, every node forwards each of its inbound mails to all
    /// its downstreams (a [`Relay`] broadcasts), so a downstream's inbound
    /// count is the sum of its parents'. Relies on the DAG being strictly
    /// forward (every edge `i → j` has `j > i`), which all real shapes are. A
    /// fan-in→fan-out join (the reshaped-away `socket_server` bug) would make
    /// this quadratic in N; the independent-chain shape keeps it `1 + 4N`.
    fn runtime_ping_volume(topo: &Topology) -> usize {
        let nodes = topo.downstreams.len();
        let mut received = vec![0usize; nodes];
        received[0] = 1; // the source's single inbound root
        for i in 0..nodes {
            for &j in &topo.downstreams[i] {
                received[j] += received[i];
            }
        }
        received.iter().sum()
    }

    #[test]
    fn socket_server_models_independent_chains_at_linear_volume() {
        let n = 8;
        let t = socket_server(n, 1_000, 500);
        // source + N decoders + N logic + N encoders + N writers = 4N + 1.
        assert_well_formed_real(&t, 4 * n + 1);
        // Source fans to all N decoders (the connection accept-fan).
        assert_eq!(t.downstreams[0].len(), n, "source fans to N decoders");
        // Each connection is its own chain — decoder i → logic N+i → encoder
        // 2N+i → writer 3N+i — every interior node with exactly one
        // downstream and no node shared between chains (no broadcast join).
        for i in 1..=n {
            assert_eq!(t.downstreams[i], vec![n + i], "decoder {i} → its own logic");
            assert_eq!(
                t.downstreams[n + i],
                vec![2 * n + i],
                "logic {i} → its own encoder"
            );
            assert_eq!(
                t.downstreams[2 * n + i],
                vec![3 * n + i],
                "encoder {i} → its own writer"
            );
            assert!(t.downstreams[3 * n + i].is_empty(), "writer {i} is a leaf");
        }
        // The per-frame mail volume is O(N) — `1 + 4N` Ping mails per root,
        // never the N² a shared broadcast join produced (the bug this reshape
        // fixes). Pin it at two widths so a regression to a fan-in→fan-out
        // join (quadratic) trips the test.
        assert_eq!(runtime_ping_volume(&socket_server(n, 0, 0)), 1 + 4 * n);
        assert_eq!(runtime_ping_volume(&socket_server(2 * n, 0, 0)), 1 + 8 * n);
    }

    #[test]
    fn tick_broadcast_has_expected_node_count_and_shape() {
        let n = 8;
        let t = tick_broadcast(n, 1_000, 500);
        // source + sim + N encoders + N writers = 2N + 2.
        assert_well_formed_real(&t, 2 * n + 2);
        assert_eq!(t.downstreams[0], vec![1], "source feeds the sim node");
        assert_eq!(t.downstreams[1].len(), n, "sim broadcasts to N encoders");
    }

    #[test]
    fn ui_roundtrip_is_a_finite_acyclic_chain() {
        let steps = REAL_UI_FOLLOWUP_STEPS;
        let t = ui_roundtrip(steps, 500);
        // request + handler + response + followup steps = 3 + steps.
        assert_well_formed_real(&t, 3 + steps);
        // A pure chain: every non-leaf node has exactly one downstream, the
        // last is a leaf — bounded, unrolled, never a cycle.
        let leaves = t.downstreams.iter().filter(|d| d.is_empty()).count();
        assert_eq!(leaves, 1, "a chain has a single leaf");
    }

    #[test]
    fn real_topologies_carry_the_real_tier() {
        let topos = real_topologies();
        assert_eq!(topos.len(), 3, "three real shapes");
        assert!(
            topos.iter().all(|t| t.tier == Tier::Real),
            "every real shape must be tagged Tier::Real"
        );
    }

    #[test]
    fn drive_for_tier_paces_real_and_passes_others_through() {
        // Real is always paced, even when the sweep was configured saturate.
        let sat = Drive::Saturate { backlog: 64 };
        assert!(
            matches!(
                drive_for_tier(sat, Tier::Real),
                Drive::Latency { pace_hz: Some(_) }
            ),
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

    /// Run a single (workers × topology) cell under the cell's *per-tier*
    /// drive ([`drive_for_tier`]) — so a real topology runs paced — and return
    /// its samples, or `None` when no wgpu adapter is available (the driverless
    /// box skips cleanly). Mirrors [`saturate_cell`] but lets the tier select
    /// the drive, so a real cell exercises the paced path the same way the
    /// `perf-trial` bin will.
    fn real_cell(workers: usize, topo: Topology) -> Option<CellSamples> {
        let cfg = SweepConfig {
            workers: vec![workers],
            // A small frame count: paced cells sleep per frame, so keep the
            // local test fast while still settling several round-trips.
            frames: 4,
            // cfg.drive is overridden to paced for the real tier inside the
            // sweep; the value here is the light/heavy fallback, unused.
            drive: Drive::Latency { pace_hz: None },
            topologies: vec![topo],
        };
        run_sweep_samples(&cfg).into_iter().next()
    }

    #[test]
    fn paced_real_cell_yields_latency_samples_and_no_throughput() {
        // A small `ui-roundtrip` settles quickly; the larger fan shapes work
        // too but cost more per local run.
        let Some(cell) = real_cell(2, ui_roundtrip(REAL_UI_FOLLOWUP_STEPS, 500)) else {
            eprintln!("skipping: no wgpu adapter");
            return;
        };
        assert_eq!(cell.tier, Tier::Real, "the cell carries the real tier");
        assert!(
            !cell.handler.is_empty(),
            "a paced real cell must produce per-hop latency samples"
        );
        assert!(
            cell.throughput_mps.is_none(),
            "a paced (latency) real cell reports no throughput rate"
        );
        assert!(
            cell.keepup.is_some(),
            "a paced real cell harvests keep-up counters (iamacoffeepot/aether#1233)"
        );
    }

    #[test]
    fn keepup_counters_match_dispatched_mail() {
        // A paced real cell harvests offered/completed counters from the
        // actors' plain fields (iamacoffeepot/aether#1233). `advance()`
        // quiesces each frame, so every dispatched `Ping` is handled within
        // its frame: offered == completed, and both equal `frames ×
        // hops-per-root` (the entry send plus one mail per DAG edge).
        let topo = ui_roundtrip(REAL_UI_FOLLOWUP_STEPS, 500);
        let hops = hops_per_root(&topo);
        let Some(cell) = real_cell(2, topo) else {
            eprintln!("skipping: no wgpu adapter");
            return;
        };
        let keepup = cell.keepup.expect("a real cell harvests keep-up counters");
        assert_eq!(
            keepup.offered, keepup.completed,
            "a drained run handles every offered mail (offered == completed)"
        );
        // `real_cell` advances 4 frames at burst 1 → 4 roots.
        assert_eq!(
            keepup.offered,
            4 * hops as u64,
            "offered = frames × hops-per-root"
        );
        assert!(
            keepup.expected_nanos > 0,
            "a paced cell carries a positive 60 Hz budget"
        );
    }
}
