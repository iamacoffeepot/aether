//! The shapes the sweep measures: the workload [`Tier`] vocabulary, the
//! [`Topology`] DAG the relays are wired into, the per-shape factories, and
//! the per-tier shape sets the env-selected sweep is built from.

use std::env;

use serde::{Deserialize, Serialize};

use super::{heavy_work_iters_from_env, tiers_from_env, wide_fanout_widths_from_env};
use crate::perf::report::LatencySection;

/// A workload tier (ADR-0085 amendment 2026-05-27): the three classes of
/// shape the dispatch perf comparison measures, distinguished by what each
/// isolates and how much its run-to-run variance lets the report *claim*.
/// Verdict treatment follows the variance, not the tier's importance — only
/// [`Tier::Light`] is classified pass/improved/regressed; [`Tier::Heavy`]
/// and [`Tier::Real`] are characterisation (numbers + direction + graphs, no
/// verdict). The tier rides on each [`Topology`] (and is threaded through
/// [`CellSamples`] / [`CellResult`] to the report builder), so the renderer
/// can suppress the verdict for a non-`light` section.
///
/// [`CellSamples`]: crate::perf::harness::CellSamples
/// [`CellResult`]: crate::perf::harness::CellResult
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
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
///
/// [`run_sweep_samples`]: crate::perf::harness::run_sweep_samples
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
        .map(|i| {
            if i + 1 < d {
                vec![i + 1]
            } else {
                vec![]
            }
        })
        .collect()
}

/// `0 -> 1 -> ... -> d-1`. Each relay forwards to the next; the last is
/// a leaf.
#[must_use]
pub fn depth_chain(d: usize) -> Topology {
    let downstreams = forward_chain_edges(d);
    Topology { name: format!("depth-{d}"), work_iters: vec![0; downstreams.len()], tier: Tier::Light, downstreams }
}

/// `0 -> {1, 2, ..., b}`. Entry fans to b leaves.
#[must_use]
pub fn fanout(b: usize) -> Topology {
    let mut downstreams = vec![vec![]; b + 1];
    downstreams[0] = (1..=b).collect();
    Topology { name: format!("fanout-{b}"), work_iters: vec![0; downstreams.len()], tier: Tier::Light, downstreams }
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
///
/// [`Relay`]: crate::perf::harness::Relay
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

    Topology { name: format!("socket-server-{n}"), downstreams, work_iters, tier: Tier::Real }
}

/// `tick-broadcast-N` (ADR-0085 amendment): a tick-paced source feeding a
/// single **sim** node (medium cost) that broadcasts to `N` **encoder** nodes
/// (heavy codec cost), each forwarding to **one writer** leaf. Models a
/// per-frame simulation step fanning state out to `N` connected clients. Pure
/// fan — broadcast-to-all fits [`Relay`] unchanged.
///
/// Node layout (indices): `0` = source; `1` = sim; `2..=N+1` = encoders;
/// `N+2..=2N+1` = writers. Total `2N + 2` nodes.
///
/// [`Relay`]: crate::perf::harness::Relay
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

    Topology { name: format!("tick-broadcast-{n}"), downstreams, work_iters, tier: Tier::Real }
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
///
/// [`Relay`]: crate::perf::harness::Relay
#[must_use]
pub fn ui_roundtrip(followup_steps: usize, handler_work: u64) -> Topology {
    let total = 3 + followup_steps;
    // A straight chain: each node forwards to the next; the last is a leaf.
    let downstreams = forward_chain_edges(total);
    // 1: handler does the medium-cost work; everything else is trivial.
    let mut work_iters = vec![0u64; total];
    work_iters[1] = handler_work;

    Topology { name: "ui-roundtrip".to_owned(), downstreams, work_iters, tier: Tier::Real }
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
        vec![depth_chain(1), depth_chain(8), fanout(4), fanout(8), two_level_tree()]
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A `Topology`'s structural invariants hold for any factory: the two
    /// per-node vectors are the same length, every downstream index is in
    /// range, and the DAG is acyclic (every edge points forward — all our
    /// real shapes wire strictly increasing indices). Factored so each
    /// real-shape test asserts the same invariants without copy-pasting the
    /// checks (keeps the duplicate-code check quiet).
    fn assert_well_formed_real(topo: &Topology, expected_nodes: usize) {
        assert_eq!(topo.tier, Tier::Real, "real factory must tag Tier::Real");
        assert_eq!(topo.downstreams.len(), expected_nodes, "node count for {}", topo.name);
        assert_eq!(topo.work_iters.len(), topo.downstreams.len(), "work_iters must be one-per-node for {}", topo.name);
        for (i, downs) in topo.downstreams.iter().enumerate() {
            for &j in downs {
                assert!(j < topo.downstreams.len(), "{} edge {i}->{j} out of range", topo.name);
                assert!(j > i, "{} edge {i}->{j} is not forward — the DAG must stay acyclic", topo.name);
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
            assert_eq!(t.downstreams[n + i], vec![2 * n + i], "logic {i} → its own encoder");
            assert_eq!(t.downstreams[2 * n + i], vec![3 * n + i], "encoder {i} → its own writer");
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
        assert!(topos.iter().all(|t| t.tier == Tier::Real), "every real shape must be tagged Tier::Real");
    }
}
