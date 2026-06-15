//! The space-time reachability kind vocabulary (issue 1908): the
//! `aether.reach.*` / `aether.corridor.*` kinds the solver core, the
//! certifier `#[transform]`s, and the corridor / counterfactual / traffic
//! passes read and write. Relocated out of `aether-kinds` so this crate
//! owns the vocabulary its logic operates over. Every `#[kind(name = …)]`
//! is byte-identical to its former home, so each `KindId` (a hash of the
//! name) is stable and no wire payload, schema, or dispatch changes.
//! Re-exported at the crate root (`pub use kinds::*`), so peers address
//! these as `aether_labyrinth::ScalarField` and the crate's own modules
//! as `crate::…`. The one type these compose with that stays in
//! `aether-kinds` is the domain-agnostic `TrajectoryLog`, referenced by
//! its full path.

use serde::{Deserialize, Serialize};

// ADR-0047/0048/0049 space-time reachability vocabulary (issue
// 1857). The shared kind contract a minimum-cost reachability solver
// over a time-varying scalar cost field reads and writes; the solver
// itself is a pair of native `#[transform]`s in this crate.
// A family of follow-on field passes (corridor extraction, windowed
// re-solve, agent populations) all consume `ScalarField`, so the
// representation lands here once. Postcard-shaped, `Vec`-bearing like
// `CreateTexture`.

/// A dense scalar field over a 2D grid and an integer tick axis — the
/// shared currency of the reachability solver (issue 1857). `values`
/// is row-major over `(tick, y, x)`: the scalar at cell `(x, y)` on
/// tick `t` is `values[t * height * width + y * width + x]`, so a
/// well-formed field has `values.len() == width * height * ticks`.
/// `u32::MAX` is the reserved sentinel — in a cost field it marks a
/// blocked / impassable cell, and in the solved cost-to-reach field it
/// marks a cell no stencil-feasible path reaches. Costs are
/// non-negative, so a `u32` field with the reserved sentinel folds
/// "blocked" in with no second mask and keeps the recurrence's `min`
/// well-defined. This is both the cost-field input (inside
/// [`ReachabilityProblem`]) and the solved cost-to-reach output of the
/// `solve` transform.
#[derive(
    aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq,
)]
#[kind(name = "aether.reach.scalar_field")]
pub struct ScalarField {
    pub width: u32,
    pub height: u32,
    pub ticks: u32,
    pub values: Vec<u32>,
}

/// One signed cell offset in a [`MovementStencil`] — a single-tick
/// step `(dx, dy)` measured in grid cells. The zero offset `(0, 0)` is
/// the "stay put" move. Not a kind on its own — only addressable
/// inside `MovementStencil.offsets`.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct StencilOffset {
    pub dx: i32,
    pub dy: i32,
}

/// The one-tick movement stencil — the set of cells reachable from any
/// cell in a single tick (issue 1857). A standalone kind so the
/// corridor-graph and windowed-replan passes share one stencil
/// representation. Each [`StencilOffset`] is a step applied to a
/// cell's position; include the zero `(0, 0)` offset for the "stay
/// put" move. The solver reads it as the predecessor set: cell `c` on
/// tick `t` is reachable from `c - offset` on tick `t - 1` for each
/// offset, so a non-symmetric stencil is interpreted in the forward
/// (reachable-from) direction.
#[derive(
    aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq,
)]
#[kind(name = "aether.reach.movement_stencil")]
pub struct MovementStencil {
    pub offsets: Vec<StencilOffset>,
}

/// The bundled input to the `solve` transform (issue 1857): a cost
/// field, the movement stencil, and the start seed. Bundling the
/// operands into one kind keeps `solve` a unary `Kind -> Kind` node,
/// the same shape [`aether_kinds::Mat4Apply`] gives `mat4_apply`. `start` is
/// a cost-valued seed slice of length `cost.width * cost.height` — the
/// per-cell initial accumulated cost at `t = 0`, with `u32::MAX`
/// marking a cell that is not a start. A plain start sets its cells to
/// `0`; the seed-slice form (rather than a bare 0-cost cell set) is
/// what lets a windowed re-solve carry its frontier and a
/// counterfactual query seed from an actual `(cell, accumulated-cost)`
/// state.
#[derive(
    aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq,
)]
#[kind(name = "aether.reach.problem")]
pub struct ReachabilityProblem {
    pub cost: ScalarField,
    pub stencil: MovementStencil,
    pub start: Vec<u32>,
}

/// A budget threshold to query a solved cost-to-reach field against
/// (issue 1857). Paired with the `V` field by the `reachability_margin`
/// transform, which compares `budget` to the minimum cost-to-reach
/// over the field's final tick.
#[derive(
    aether_data::Kind,
    aether_data::Schema,
    Serialize,
    Deserialize,
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
)]
#[kind(name = "aether.reach.budget_query")]
pub struct BudgetQuery {
    pub budget: u32,
}

/// The result of a `reachability_margin` query (issue 1857).
/// `min_cost` is the minimum cost-to-reach over the field's final-tick
/// cells (`u32::MAX` if no final-tick cell is reachable). `reachable`
/// is `min_cost < budget`. `margin` is `budget - min_cost` as a signed
/// value — positive slack when reachable under budget, negative when
/// the cheapest reachable cell still exceeds the budget (or when no
/// cell is reachable at all).
#[derive(
    aether_data::Kind,
    aether_data::Schema,
    Serialize,
    Deserialize,
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
)]
#[kind(name = "aether.reach.margin")]
pub struct ReachabilityMargin {
    pub reachable: bool,
    pub min_cost: u32,
    pub margin: i64,
}

// ADR-0047/0048/0049 corridor-graph vocabulary (issue 1858). The
// time-sliced connectivity skeleton of a solved cost-to-reach field
// `V` (a [`ScalarField`]) under a budget `B`: per tick, the connected
// components of the affordable set `{cell : V(cell, tick) <= B}` (the
// nodes), the intra-tick "punch" edges a sub-budget barrier separates
// (priced at the sublevel-filtration threshold of `V` at which raising
// `B` would merge them), and the inter-tick "flow" edges between
// components an affordable one-tick stencil step links. A flat,
// postcard-friendly DAG built by the `build_corridor_graph` transform
// in this crate: nodes carry summaries, not cell sets, so
// the graph stays orders of magnitude smaller than `V`, and components
// are re-derivable from `V` + `B` + the [`MovementStencil`] (all
// content-addressed) rather than stored as per-tick label images.
// `Vec`-bearing like [`aether_kinds::DrawTexturedQuads`].

/// One node in a [`CorridorGraph`] — a single connected component of
/// the affordable set `{cell : V(cell, tick) <= budget}` at one tick.
/// A summary, not a cell set. `tick` is the time layer (the row-major
/// `(tick, y, x)` slice convention is [`ScalarField`]'s); `component`
/// is the component's per-tick id, assigned in row-major
/// first-encounter order over the tick's affordable cells, so two
/// builds of the same `V` + budget label identically; `cell_count` is
/// the number of affordable cells the component covers; `min_cost` is
/// the minimum cost-to-reach (`V`) over those cells. A node is
/// referenced from a [`CorridorEdge`] by its index into
/// [`CorridorGraph::nodes`], which is ordered by `(tick, component)`.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct CorridorNode {
    pub tick: u32,
    pub component: u32,
    pub cell_count: u32,
    pub min_cost: u32,
}

/// Whether a [`CorridorEdge`] joins two components already connected
/// under the budget (`Flow`) or a pair a sub-budget barrier currently
/// separates (`Punch`). `Flow` edges are inter-tick (a component at
/// tick `t` to one at `t + 1`); `Punch` edges are intra-tick (two
/// components at the same tick that the sublevel filtration of `V`
/// would merge above the budget). Kept as two distinct kinds, plus the
/// edge's `price`, so "connected now" never collapses into "connected
/// at higher budget."
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeKind {
    Flow,
    Punch,
}

/// One directed edge in a [`CorridorGraph`]. `from` and `to` index
/// into [`CorridorGraph::nodes`]; the graph is a time-layered DAG, so
/// a `Flow` edge always points forward in time (`from` at tick `t`,
/// `to` at tick `t + 1`) and a `Punch` edge joins two components at the
/// same tick. `price` is the punch's merge threshold — the budget at
/// which the sublevel filtration of `V` fuses the two components (the
/// minimum `V` along the separating barrier) — and is `0` for a `Flow`
/// edge. `overlap_width` is the count of distinct affordable landing
/// cells the stencil step bridges for a `Flow` edge (how wide the
/// pinch/branch is) and is `0` for a `Punch` edge. Both `price` and
/// `overlap_width` are public: a consumer recovers "what merges at
/// `B'`" by thresholding punch prices, and reads `overlap_width` to
/// render branch width.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct CorridorEdge {
    pub from: u32,
    pub to: u32,
    pub kind: EdgeKind,
    pub price: u32,
    pub overlap_width: u32,
}

/// The time-sliced corridor graph (issue 1858): the connectivity
/// skeleton of a solved cost-to-reach field `V` under a fixed budget.
/// `nodes` are the per-tick connected components of the affordable set,
/// ordered by `(tick, component)`; `edges` are the inter-tick `Flow`
/// links and the intra-tick `Punch` merges, emitted sorted by endpoints
/// then kind so the output is byte-stable and content-addressable. A
/// skeleton, not a field: it carries no cell-to-component label image,
/// so its size scales with the number of components and edges rather
/// than `width * height * ticks`.
#[derive(
    aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq,
)]
#[kind(name = "aether.corridor.graph")]
pub struct CorridorGraph {
    pub nodes: Vec<CorridorNode>,
    pub edges: Vec<CorridorEdge>,
}

/// The set of paths to aggregate over a [`CorridorGraph`] (issue 1865).
/// Bundles a `Vec` of #1862's [`TrajectoryLog`](aether_kinds::TrajectoryLog)
/// handles into one input kind so the `aggregate_traffic` transform
/// stays a fixed-arity `Kind`-slot node — embedding the top-level
/// `TrajectoryLog` Kind as a `Vec`-of-Kind field is valid because it
/// derives `Schema`, the same way [`aether_kinds::Mat4Apply`] embeds the
/// top-level Kind `Vec4`. Each log replays one moving point's tick-
/// ordered path; the aggregation snaps every sample to its corridor
/// component and accumulates per-edge traffic. `Vec`-bearing like
/// [`aether_kinds::DrawTexturedQuads`].
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.corridor.trajectory_set")]
pub struct TrajectorySet {
    /// The paths to aggregate. Each is one #1862 trajectory session;
    /// order is not significant to the aggregation (traffic is a sum).
    pub logs: Vec<aether_kinds::TrajectoryLog>,
}

/// The per-edge traffic density of a [`TrajectorySet`] snapped onto a
/// [`CorridorGraph`] (issue 1865): the discrepancy surface a set of
/// paths over a field's connectivity skeleton makes visible — which
/// edges carry the bulk of the traffic, which are reachable but
/// untraveled, and how through-boundary ("punch") traffic splits by
/// whether punching beat the affordable detour around it.
///
/// A flat, postcard-friendly reduction keyed by the graph's node / edge
/// indices so a consumer joins it back to the graph by index:
/// `edge_traffic[i]` is the traffic over `CorridorGraph.edges[i]`, and
/// `node_traffic[i]` the visits to `CorridorGraph.nodes[i]`. The output
/// is orders of magnitude smaller than the field `V` (a skeleton-sized
/// reduction over `Vec<u32>`), well under the 64MB transform output cap.
///
/// Traffic is integer-valued (counts, not a continuous density) so the
/// encoded bytes are exact for content-addressing replay; the fraction
/// `count / path_count` is a trivial derived read.
#[derive(
    aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq,
)]
#[kind(name = "aether.corridor.traffic_density")]
pub struct TrafficDensity {
    /// Number of paths aggregated (the [`TrajectorySet`] length).
    pub path_count: u32,
    /// Traffic per edge, parallel to [`CorridorGraph::edges`]:
    /// `edge_traffic[i]` is the number of path crossings attributed to
    /// `edges[i]`. A `Flow` edge's count is the number of affordable
    /// tick steps mapped onto it; a `Punch` edge's count is the number
    /// of through-boundary crossings attributed to it. An entry that
    /// stays `0` is reachable but untraveled.
    pub edge_traffic: Vec<u32>,
    /// Visits per node, parallel to [`CorridorGraph::nodes`]:
    /// `node_traffic[i]` is the number of path samples that snapped to
    /// `nodes[i]` (one per path per tick the path occupies that
    /// component).
    pub node_traffic: Vec<u32>,
    /// Indices into [`CorridorGraph::edges`] whose traffic is `0`:
    /// reachable but untraveled edges. Derivable from `edge_traffic`
    /// (its zero entries) but surfaced explicitly — it is a headline
    /// discrepancy and explicit beats derived for a machine consumer.
    pub untraveled_edges: Vec<u32>,
    /// Total punch-edge traffic over edges where punching was cheaper
    /// than the affordable detour (`price` < `detour_cost`, including
    /// the no-detour case): crossing the barrier beat going around.
    pub punch_crossing_cheaper: u32,
    /// Total punch-edge traffic over edges where an affordable detour
    /// over the flow skeleton was cheaper than or equal to the punch
    /// `price`: going around beat crossing the barrier.
    pub punch_detour_cheaper: u32,
}

/// One fork in a [`CorridorGraph`] (issue 1859): a live node with a
/// `Flow` edge into a dead-end branch, paired with the lookahead depth
/// that branch demands. `node_index` indexes into
/// [`CorridorGraph::nodes`]; `depth` is the longest `Flow`-path within
/// the dead-end subtree hanging off the fork — how many ticks the
/// dead-end region stays affordable past the fork before it terminates,
/// i.e. how far a bounded-horizon traversal must see past the fork to
/// tell the dead-end apart from a branch that reaches the final tick.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForkDepth {
    pub node_index: u32,
    pub depth: u32,
}

/// The fork resolution depth of a [`CorridorGraph`] (issue 1859): how
/// far ahead a bounded-horizon traversal must look before committing to
/// an affordable one-tick step, computed as two passes over the landed
/// corridor graph — no windowed re-solve and no new field.
///
/// A node is **live** if a `Flow`-edge path reaches a final-tick node;
/// a node in the graph (reachable-affordable) with no such continuation
/// is **dead-end**. A **fork** is a live node with a `Flow` edge into a
/// dead-end node: locally the step is affordable, yet the branch it
/// enters never reaches the end. Each fork's `depth` is the longest
/// `Flow`-path within the dead-end subtree hanging off it — the ticks
/// the dead-end stays affordable before terminating, which is exactly
/// the lookahead a traversal needs to distinguish it from a through
/// branch. `max_resolution_depth` is the `max` over `forks` (`0` when
/// the graph has no fork), so a bounded-lookahead traversal with window
/// `W >= max_resolution_depth` stops committing to dead-ends.
///
/// `forks` is the per-fork list `(node_index, depth)` for inspection,
/// emitted in ascending `node_index` order so the output is byte-stable
/// and content-addressable. A skeleton-sized reduction (`Vec`-bearing
/// like [`CorridorGraph`]), orders of magnitude smaller than the field.
#[derive(
    aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq,
)]
#[kind(name = "aether.corridor.resolution_depth")]
pub struct ResolutionDepth {
    pub max_resolution_depth: u32,
    pub forks: Vec<ForkDepth>,
}

/// A seeded Monte-Carlo population sweep over a time-varying scalar
/// cost field (issue 1863): a population of reaction-delayed agents
/// re-planning toward the cheapest goal under a fixed per-tick lag.
/// The whole sweep is a pure function of its inputs (seed + field in,
/// curve out), so it content-addresses as a `#[transform]` with no
/// executor change (ADR-0048 §4/§130).
///
/// `problem` carries the shared reachability bundle (the space-time
/// cost field, the one-tick [`MovementStencil`], and the start seed)
/// — embedding [`ReachabilityProblem`] keeps the per-agent planner
/// *literally* #1857's [`ReachabilityProblem`]-shaped solve, so
/// the headline property holds structurally rather than by
/// coincidence. `problem.start` doubles as the **start region**: the
/// cells with a finite seed value are the population's spawn cells, and
/// each agent inherits that cell's seed as its initial accumulated
/// cost.
///
/// `goal` is the goal region as flat row-major cell indices
/// (`y * width + x`); an agent **finishes** the moment it lands on a
/// goal cell with accumulated cost `< budget` before the final tick.
/// `population` is the number of agents sampled from the start region;
/// `window` is each agent's planning-window depth `W` (how far forward
/// it sees); `seed` keys the deterministic PRNG that samples the
/// population and breaks equal-cost ties; `delays` is the swept set of
/// reaction lags.
///
/// The **lag convention**: at simulation tick `t`, an agent with delay
/// `d` perceives the field tick `τ` (for each `τ` in its window) as it
/// truly was at tick `max(0, τ - d)` — it reacts to field changes `d`
/// ticks late. At `d = 0` and `window >= ticks` the perceived field is
/// the true field over the full remaining horizon, so each agent's plan
/// *is* the exact single-source solve and the survival fraction
/// approaches the exact reachable-under-budget bound. `d` (how stale the
/// view is) is orthogonal to `window` (how far forward the agent sees).
#[derive(
    aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq,
)]
#[kind(name = "aether.reach.population_problem")]
pub struct PopulationSweepProblem {
    pub problem: ReachabilityProblem,
    pub goal: Vec<u32>,
    pub budget: u32,
    pub population: u32,
    pub window: u32,
    pub seed: u64,
    pub delays: Vec<u32>,
}

/// One point on a [`SurvivalCurve`] (issue 1863): for the swept
/// reaction delay `delay`, the number of agents (out of the curve's
/// shared `population`) that finished under budget. Not a kind on its
/// own — only addressable inside `SurvivalCurve.samples`. Integer
/// counts (rather than a pre-divided `f32` fraction) keep the output
/// byte-exact for content-addressing replay; the survival fraction
/// `finished / population` is a trivial derived read.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct SurvivalSample {
    pub delay: u32,
    pub finished: u32,
}

/// The output of the population sweep (issue 1863): the
/// completion-rate-vs-reaction-delay curve. `population` is the shared
/// agent count every [`SurvivalSample`] divides; `samples` carries one
/// `(delay, finished)` point per swept delay, in the input `delays`
/// order. The fraction `finished / population` is the survival rate,
/// and the knee where it collapses is the field's effective reaction
/// demand.
#[derive(
    aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq,
)]
#[kind(name = "aether.reach.survival_curve")]
pub struct SurvivalCurve {
    pub population: u32,
    pub samples: Vec<SurvivalSample>,
}

// ADR-0047/0048/0049 counterfactual reachability query (issue 1864).
// Given a recorded path through a time-varying cost field (a
// `TrajectoryLog`, issue 1862) and a tick where its accumulator
// crossed a budget `B`, the `solve_counterfactual` transform re-runs
// the reachability solver from the path's actual `(position,
// accumulated-cost)` state `W` ticks before the crossing, seeing only
// the field visible in that window, and classifies the crossing as
// avoidable (a within-budget continuation existed) or unavoidable. The
// query params are a cast-shaped scalar bundle (modeled on
// [`WindowSize`]); the classification is `Vec`-bearing (modeled on
// [`CorridorGraph`]).

/// The lookahead depth and budget for a [`solve_counterfactual`] query
/// (issue 1864). `window` is `W` — how many ticks before each
/// budget-crossing the re-solve seeds from the path's actual state, so
/// the decision tick is `crossing_tick − window`, clamped to the path's
/// first recorded tick. `budget` is `B`, the threshold a path's
/// accumulator crosses: a crossing is the first sample whose `value`
/// reaches `B` (the `prev.value < B` → `cur.value >= B` transition; a
/// first sample already `>= B` is a crossing at the path start). The
/// `value` carried in each `TrajectorySampleEntry` is the path's
/// accumulated field cost, denominated in the same `u32` currency as
/// the cost-to-reach field `V` and `budget`, which is what makes the
/// seed cost comparable to the budget.
///
/// [`solve_counterfactual`]: https://docs.rs/aether-labyrinth
#[repr(C)]
#[derive(
    Copy,
    Clone,
    Debug,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    bytemuck::Pod,
    bytemuck::Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "aether.reach.crossing_query")]
pub struct CrossingQueryParams {
    pub window: u32,
    pub budget: u32,
}

/// One budget-crossing's verdict in a [`CrossingClassification`] (issue
/// 1864). `crossing_tick` is the tick at which the path's accumulator
/// first reached the budget; `decision_tick` is `crossing_tick −
/// window` clamped to the path's first recorded tick (the tick whose
/// state seeds the windowed re-solve). `seed_x` / `seed_y` /
/// `seed_cost` are the path's actual `(x, y, value)` state at the
/// decision tick (the latest sample at or before it), echoed in full so
/// a machine consumer correlates each verdict to its seed without
/// re-deriving it. `avoidable` is true exactly when the windowed
/// re-solve reaches some cell at `crossing_tick` under the budget;
/// `best_continuation_cost` is the minimum cost-to-reach over the
/// window's final-tick cells (`u32::MAX` if none is reachable), and
/// `margin` is `budget − best_continuation_cost` as a signed value
/// (positive slack when avoidable, negative when even the cheapest
/// continuation exceeds the budget). Not a kind on its own — only
/// addressable inside `CrossingClassification.crossings`.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct CrossingVerdict {
    pub crossing_tick: u32,
    pub decision_tick: u32,
    pub seed_x: u32,
    pub seed_y: u32,
    pub seed_cost: u32,
    pub avoidable: bool,
    pub best_continuation_cost: u32,
    pub margin: i64,
}

/// The per-crossing classification produced by the
/// `solve_counterfactual` transform (issue 1864): one
/// [`CrossingVerdict`] for every budget-crossing detected in the
/// recorded path, in tick order. A path with no crossing yields an
/// empty `crossings`. A skeleton keyed to the path's crossings rather
/// than a field, so its size scales with the number of crossings, not
/// `width * height * ticks`. `Vec`-bearing like [`CorridorGraph`].
#[derive(
    aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq,
)]
#[kind(name = "aether.reach.crossing_classification")]
pub struct CrossingClassification {
    pub crossings: Vec<CrossingVerdict>,
}

/// A self-realizing field simulation (issue 1867): an agent whose own
/// motion through a field *spawns* the contributions it must then avoid.
/// Per run, a closed feedback loop — realize the field, plan against it,
/// step, snapshot a contribution onto the recent position, advance —
/// produces a path-dependent realized field that is distinct per seed,
/// deterministic given (seed + policy + field). Studied as a
/// distribution over many seeded runs (see [`ClosureDistribution`]), not
/// a single solve. The whole simulation is a pure function of its inputs
/// (seed + field in), so it content-addresses as a `#[transform]` with no
/// executor change (ADR-0048 §4/§130).
///
/// `problem` carries the shared reachability bundle (the base space-time
/// cost field, the one-tick [`MovementStencil`], and the start seed) —
/// embedding [`ReachabilityProblem`] keeps the per-tick planner
/// *literally* #1857's `solve_cost_to_reach` over the realized field, so
/// the path-dependence is structural rather than a bespoke trap
/// heuristic. `problem.start` doubles as the **start region**: the cells
/// with a finite seed value are the spawn cells, and each run inherits
/// its sampled cell's seed as its initial accumulated cost. `goal` is the
/// goal region as flat row-major cell indices (`y * width + x`); a run
/// **finishes** the moment it lands on a goal cell with accumulated cost
/// `< budget` before the final tick.
///
/// The **contribution model** is #1866's snapshot-placement-plus-lead.
/// Every `placement_period` ticks the run snapshots its current cell and
/// enqueues a contribution that takes effect `lead_ticks` (`L`) later. An
/// *active* contribution committed at trigger tick `t_c` (with
/// `t - t_c >= L`) adds `contribution_cost` to every base-field cell
/// within radius `cover(age) = covered_extent_initial +
/// covered_growth_per_tick * (t - t_c)` of its snapshot center
/// (saturating at the `u32::MAX` sentinel). At most `max_concurrent`
/// contributions are held active at once (oldest expires). The realized
/// field at tick `t` is the base field plus the sum of every active
/// contribution at its current age — the load-bearing path-dependent
/// quantity, since which cells are covered is a function of where the run
/// stepped at every earlier placement tick.
///
/// `window` is the planning-window depth `W` (how far forward the planner
/// sees each tick); `runs` is the number of seeded runs in the
/// distribution; `seed` keys the deterministic PRNG that samples each
/// run's start cell and breaks equal-cost stencil ties. Where the per-
/// contribution escapability bound (#1866) certifies a single snapshot in
/// O(1) but leaves `N > max_concurrent` *uncertified*, this simulator is
/// the empirical complement: it realizes the aggregate trail along the
/// real path and detects the multi-window closure the single-instant
/// `cover(L)` measure cannot see.
#[derive(
    aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq,
)]
#[kind(name = "aether.reach.realization_problem")]
pub struct RealizationProblem {
    pub problem: ReachabilityProblem,
    pub goal: Vec<u32>,
    pub budget: u32,
    pub placement_period: u32,
    pub lead_ticks: u32,
    pub covered_extent_initial: f32,
    pub covered_growth_per_tick: f32,
    pub contribution_cost: u32,
    pub max_concurrent: u32,
    pub window: u32,
    pub runs: u32,
    pub seed: u64,
}

/// One run's outcome in a [`ClosureDistribution`] (issue 1867). Not a
/// kind on its own — only addressable inside
/// `ClosureDistribution.samples`. `run` is the run index. `finished` is
/// `true` iff the agent reached a goal cell under budget before the final
/// tick. `closure_tick` is the first tick at which the realized field
/// **closed around the agent** — no stencil neighbor of the current cell
/// is both un-blocked and keeps accumulated cost `< budget`, the
/// window-independent signal that the run's own trail has covered every
/// feasible next cell. `closure_tick` is `u32::MAX` when the run never
/// closed (it exhausted ticks without the field closing), separating
/// self-accumulated closure — the headline failure — from plain
/// budget/time exhaustion. Integer-exact, so a run replays byte-stable.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunOutcome {
    pub run: u32,
    pub finished: bool,
    pub closure_tick: u32,
}

/// The output of the self-realizing field simulation (issue 1867): the
/// distribution of closure outcomes over a seeded set of runs. `runs` is
/// the shared run count; `closed` is how many runs the realized field
/// closed around (the headline failure rate is `closed / runs`);
/// `samples` carries one [`RunOutcome`] per run, in run-index order. The
/// distribution of `closure_tick` over the samples shows *when* the
/// multi-window closure happens. Counts-only and integer-exact — every
/// run's realized field is recoverable by replaying its seed through the
/// companion `realize_single` transform, so the headline output stays
/// tiny and byte-stable for content-addressing replay.
#[derive(
    aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq,
)]
#[kind(name = "aether.reach.closure_distribution")]
pub struct ClosureDistribution {
    pub runs: u32,
    pub closed: u32,
    pub samples: Vec<RunOutcome>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reach_corridor_kind_names_are_stable() {
        use aether_data::Kind;
        assert_eq!(ScalarField::NAME, "aether.reach.scalar_field");
        assert_eq!(MovementStencil::NAME, "aether.reach.movement_stencil");
        assert_eq!(ReachabilityProblem::NAME, "aether.reach.problem");
        assert_eq!(BudgetQuery::NAME, "aether.reach.budget_query");
        assert_eq!(ReachabilityMargin::NAME, "aether.reach.margin");
        assert_eq!(CorridorGraph::NAME, "aether.corridor.graph");
        assert_eq!(TrajectorySet::NAME, "aether.corridor.trajectory_set");
        assert_eq!(TrafficDensity::NAME, "aether.corridor.traffic_density");
        assert_eq!(ResolutionDepth::NAME, "aether.corridor.resolution_depth");
        assert_eq!(
            PopulationSweepProblem::NAME,
            "aether.reach.population_problem"
        );
        assert_eq!(SurvivalCurve::NAME, "aether.reach.survival_curve");
        assert_eq!(CrossingQueryParams::NAME, "aether.reach.crossing_query");
        assert_eq!(
            CrossingClassification::NAME,
            "aether.reach.crossing_classification"
        );
        assert_eq!(RealizationProblem::NAME, "aether.reach.realization_problem");
        assert_eq!(
            ClosureDistribution::NAME,
            "aether.reach.closure_distribution"
        );
    }

    mod schema {
        use super::*;
        use aether_data::{CastEligible, Schema};
        use aether_data::{Primitive, SchemaType};
        use aether_kinds::TrajectoryLog;

        #[test]
        fn reachability_kinds_resolve_distinctly() {
            use aether_data::Kind;
            // The five reachability kinds carry distinct ids — a shared id
            // would collide in the transform Ref-slot resolver (the same
            // contract `mat4_apply`'s input/output split rests on).
            let ids = [
                ScalarField::ID,
                MovementStencil::ID,
                ReachabilityProblem::ID,
                BudgetQuery::ID,
                ReachabilityMargin::ID,
            ];
            for (i, a) in ids.iter().enumerate() {
                for b in &ids[i + 1..] {
                    assert_ne!(a, b, "reachability kind ids must be distinct");
                }
            }
        }

        #[test]
        fn scalar_field_schema_is_struct() {
            let SchemaType::Struct { fields, .. } = &<ScalarField as Schema>::SCHEMA else {
                panic!("expected Struct");
            };
            assert_eq!(fields.len(), 4);
            assert_eq!(fields[0].name, "width");
            assert_eq!(fields[0].ty, SchemaType::Scalar(Primitive::U32));
            assert_eq!(fields[3].name, "values");
            let SchemaType::Vec(element) = &fields[3].ty else {
                panic!("expected Vec");
            };
            assert_eq!(**element, SchemaType::Scalar(Primitive::U32));
        }

        #[test]
        fn reachability_problem_schema_nests_field_and_stencil() {
            let SchemaType::Struct { fields, .. } = &<ReachabilityProblem as Schema>::SCHEMA else {
                panic!("expected Struct");
            };
            assert_eq!(fields.len(), 3);
            assert_eq!(fields[0].name, "cost");
            assert!(matches!(fields[0].ty, SchemaType::Struct { .. }));
            assert_eq!(fields[1].name, "stencil");
            assert!(matches!(fields[1].ty, SchemaType::Struct { .. }));
            assert_eq!(fields[2].name, "start");
            assert!(matches!(fields[2].ty, SchemaType::Vec(_)));
        }

        #[test]
        fn margin_schema_is_struct() {
            let SchemaType::Struct { fields, .. } = &<ReachabilityMargin as Schema>::SCHEMA else {
                panic!("expected Struct");
            };
            assert_eq!(fields.len(), 3);
            assert_eq!(fields[0].name, "reachable");
            assert_eq!(fields[0].ty, SchemaType::Bool);
            assert_eq!(fields[1].name, "min_cost");
            assert_eq!(fields[1].ty, SchemaType::Scalar(Primitive::U32));
            assert_eq!(fields[2].name, "margin");
            assert_eq!(fields[2].ty, SchemaType::Scalar(Primitive::I64));
        }

        #[test]
        fn corridor_graph_schema_is_struct_of_two_vecs() {
            // The corridor graph is a flat DAG: a `Vec<CorridorNode>` and a
            // `Vec<CorridorEdge>`, both recursing into their element
            // structs. A `Struct` schema (not a cast) is what the DAG
            // validator and the hub codec read for the transform output.
            let SchemaType::Struct { fields, .. } = &<CorridorGraph as Schema>::SCHEMA else {
                panic!("expected Struct");
            };
            assert_eq!(fields.len(), 2);
            assert_eq!(fields[0].name, "nodes");
            let SchemaType::Vec(node) = &fields[0].ty else {
                panic!("expected Vec of nodes");
            };
            assert!(matches!(**node, SchemaType::Struct { .. }));
            assert_eq!(fields[1].name, "edges");
            let SchemaType::Vec(edge) = &fields[1].ty else {
                panic!("expected Vec of edges");
            };
            assert!(matches!(**edge, SchemaType::Struct { .. }));
        }

        #[test]
        fn resolution_depth_schema_is_max_plus_vec_of_forks() {
            // The resolution-depth result is a flat reduction: a scalar
            // `max_resolution_depth` and a `Vec<ForkDepth>`, the fork struct
            // recursing into two `u32` fields. A `Struct` schema (not a
            // cast) is what the DAG validator and the hub codec read for the
            // transform output.
            let SchemaType::Struct { fields, .. } = &<ResolutionDepth as Schema>::SCHEMA else {
                panic!("expected Struct");
            };
            assert_eq!(fields.len(), 2);
            assert_eq!(fields[0].name, "max_resolution_depth");
            assert_eq!(fields[0].ty, SchemaType::Scalar(Primitive::U32));
            assert_eq!(fields[1].name, "forks");
            let SchemaType::Vec(fork) = &fields[1].ty else {
                panic!("expected Vec of forks");
            };
            let SchemaType::Struct {
                fields: fork_fields,
                ..
            } = &**fork
            else {
                panic!("expected nested fork Struct");
            };
            assert_eq!(fork_fields.len(), 2);
            assert_eq!(fork_fields[0].name, "node_index");
            assert_eq!(fork_fields[0].ty, SchemaType::Scalar(Primitive::U32));
            assert_eq!(fork_fields[1].name, "depth");
            assert_eq!(fork_fields[1].ty, SchemaType::Scalar(Primitive::U32));
        }

        #[test]
        fn resolution_depth_id_distinct_from_corridor_kinds() {
            use aether_data::Kind;
            // The resolution-depth output shares no id with the corridor /
            // reach kinds it composes with in a DAG — a collision would
            // alias the transform's `Ref<ResolutionDepth>` output slot.
            let ids = [
                ResolutionDepth::ID,
                CorridorGraph::ID,
                TrafficDensity::ID,
                ScalarField::ID,
            ];
            for (i, a) in ids.iter().enumerate() {
                for b in &ids[i + 1..] {
                    assert_ne!(a, b, "resolution-depth id must be distinct");
                }
            }
        }

        #[test]
        fn corridor_graph_id_distinct_from_reach_kinds() {
            use aether_data::Kind;
            // The corridor output shares no id with the reachability kinds
            // it composes with in a DAG — a collision would alias the
            // transform's `Ref<CorridorGraph>` output slot.
            let ids = [
                CorridorGraph::ID,
                ScalarField::ID,
                MovementStencil::ID,
                BudgetQuery::ID,
                ReachabilityMargin::ID,
            ];
            for (i, a) in ids.iter().enumerate() {
                for b in &ids[i + 1..] {
                    assert_ne!(a, b, "corridor / reach kind ids must be distinct");
                }
            }
        }

        #[test]
        fn population_kinds_resolve_distinctly_from_reach_kinds() {
            use aether_data::Kind;
            // The two population kinds carry ids distinct from each other
            // and from the #1857 reachability kinds they build on — a
            // shared id would collide in the transform Ref-slot resolver.
            let ids = [
                PopulationSweepProblem::ID,
                SurvivalCurve::ID,
                ScalarField::ID,
                ReachabilityProblem::ID,
            ];
            for (i, a) in ids.iter().enumerate() {
                for b in &ids[i + 1..] {
                    assert_ne!(a, b, "population kind ids must be distinct");
                }
            }
        }

        #[test]
        fn crossing_query_params_is_repr_c_struct_of_two_u32() {
            // A cast-shaped scalar bundle (modeled on `WindowSize`): two
            // `u32` fields in a `repr(C)` struct so the cheap cast path
            // carries it on the wire.
            const { assert!(<CrossingQueryParams as CastEligible>::ELIGIBLE) };
            let SchemaType::Struct { repr_c, fields } = &<CrossingQueryParams as Schema>::SCHEMA
            else {
                panic!("expected Struct");
            };
            assert!(*repr_c);
            assert_eq!(fields.len(), 2);
            assert_eq!(fields[0].name, "window");
            assert_eq!(fields[0].ty, SchemaType::Scalar(Primitive::U32));
            assert_eq!(fields[1].name, "budget");
            assert_eq!(fields[1].ty, SchemaType::Scalar(Primitive::U32));
        }

        #[test]
        fn crossing_classification_schema_is_struct_of_one_vec() {
            // A flat Vec-bearing struct (modeled on `CorridorGraph`): one
            // `Vec<CrossingVerdict>`, recursing into the verdict struct. A
            // `Struct` schema (not a cast) is what the DAG validator and the
            // hub codec read for the transform output.
            let SchemaType::Struct { fields, .. } = &<CrossingClassification as Schema>::SCHEMA
            else {
                panic!("expected Struct");
            };
            assert_eq!(fields.len(), 1);
            assert_eq!(fields[0].name, "crossings");
            let SchemaType::Vec(verdict) = &fields[0].ty else {
                panic!("expected Vec of verdicts");
            };
            let SchemaType::Struct {
                fields: verdict_fields,
                ..
            } = &**verdict
            else {
                panic!("expected nested verdict Struct");
            };
            assert_eq!(verdict_fields.len(), 8);
            assert_eq!(verdict_fields[0].name, "crossing_tick");
            assert_eq!(verdict_fields[1].name, "decision_tick");
            assert_eq!(verdict_fields[5].name, "avoidable");
            assert_eq!(verdict_fields[5].ty, SchemaType::Bool);
            assert_eq!(verdict_fields[7].name, "margin");
            assert_eq!(verdict_fields[7].ty, SchemaType::Scalar(Primitive::I64));
        }

        #[test]
        fn crossing_kinds_resolve_distinctly_from_reach_kinds() {
            use aether_data::Kind;
            // The two new crossing-query kinds share no id with the
            // reachability / trajectory kinds they compose with in a DAG — a
            // collision would alias the transform's Ref slots.
            let ids = [
                CrossingQueryParams::ID,
                CrossingClassification::ID,
                ReachabilityProblem::ID,
                ScalarField::ID,
                TrajectoryLog::ID,
            ];
            for (i, a) in ids.iter().enumerate() {
                for b in &ids[i + 1..] {
                    assert_ne!(a, b, "crossing / reach / trajectory ids must be distinct");
                }
            }
        }

        #[test]
        fn population_problem_schema_embeds_reachability_problem() {
            let SchemaType::Struct { fields, .. } = &<PopulationSweepProblem as Schema>::SCHEMA
            else {
                panic!("expected Struct");
            };
            assert_eq!(fields.len(), 7);
            assert_eq!(fields[0].name, "problem");
            assert!(matches!(fields[0].ty, SchemaType::Struct { .. }));
            assert_eq!(fields[1].name, "goal");
            assert!(matches!(fields[1].ty, SchemaType::Vec(_)));
            assert_eq!(fields[2].name, "budget");
            assert_eq!(fields[2].ty, SchemaType::Scalar(Primitive::U32));
            assert_eq!(fields[5].name, "seed");
            assert_eq!(fields[5].ty, SchemaType::Scalar(Primitive::U64));
            assert_eq!(fields[6].name, "delays");
            assert!(matches!(fields[6].ty, SchemaType::Vec(_)));
        }

        #[test]
        fn survival_curve_schema_is_struct_of_samples() {
            let SchemaType::Struct { fields, .. } = &<SurvivalCurve as Schema>::SCHEMA else {
                panic!("expected Struct");
            };
            assert_eq!(fields.len(), 2);
            assert_eq!(fields[0].name, "population");
            assert_eq!(fields[0].ty, SchemaType::Scalar(Primitive::U32));
            assert_eq!(fields[1].name, "samples");
            let SchemaType::Vec(element) = &fields[1].ty else {
                panic!("expected Vec");
            };
            let SchemaType::Struct { fields: sample, .. } = &**element else {
                panic!("expected nested SurvivalSample struct");
            };
            assert_eq!(sample.len(), 2);
            assert_eq!(sample[0].name, "delay");
            assert_eq!(sample[1].name, "finished");
        }

        #[test]
        fn crossing_classification_postcard_round_trips() {
            let cls = CrossingClassification {
                crossings: vec![
                    CrossingVerdict {
                        crossing_tick: 12,
                        decision_tick: 4,
                        seed_x: 3,
                        seed_y: 7,
                        seed_cost: 18,
                        avoidable: true,
                        best_continuation_cost: 22,
                        margin: 3,
                    },
                    CrossingVerdict {
                        crossing_tick: 30,
                        decision_tick: 30,
                        seed_x: 0,
                        seed_y: 0,
                        seed_cost: 99,
                        avoidable: false,
                        best_continuation_cost: u32::MAX,
                        margin: -1,
                    },
                ],
            };
            let bytes = postcard::to_allocvec(&cls)
                .expect("test setup: postcard encodes CrossingClassification");
            let back: CrossingClassification = postcard::from_bytes(&bytes)
                .expect("test setup: postcard decodes CrossingClassification");
            assert_eq!(back, cls);
        }

        #[test]
        fn realization_kinds_resolve_distinctly_from_reach_kinds() {
            use aether_data::Kind;
            // The two realization kinds carry ids distinct from each other
            // and from the #1857 / #1863 reachability kinds they build on — a
            // shared id would collide in the transform Ref-slot resolver.
            let ids = [
                RealizationProblem::ID,
                ClosureDistribution::ID,
                ScalarField::ID,
                ReachabilityProblem::ID,
                PopulationSweepProblem::ID,
            ];
            for (i, a) in ids.iter().enumerate() {
                for b in &ids[i + 1..] {
                    assert_ne!(a, b, "realization kind ids must be distinct");
                }
            }
        }

        #[test]
        fn trajectory_set_schema_is_vec_of_trajectory_logs() {
            // The path-set bundle is a single `Vec<TrajectoryLog>` field;
            // embedding the top-level `TrajectoryLog` Kind as a Vec element
            // recurses into its struct (the same Kind-as-Schema-field shape
            // `Mat4Apply` uses for `Vec4`).
            let SchemaType::Struct { fields, .. } = &<TrajectorySet as Schema>::SCHEMA else {
                panic!("expected Struct");
            };
            assert_eq!(fields.len(), 1);
            assert_eq!(fields[0].name, "logs");
            let SchemaType::Vec(log) = &fields[0].ty else {
                panic!("expected Vec of logs");
            };
            assert!(matches!(**log, SchemaType::Struct { .. }));
        }

        #[test]
        fn traffic_density_schema_is_struct() {
            // The density is a flat reduction: three `Vec<u32>` plus three
            // scalar `u32`s — a `Struct` schema the hub codec and DAG
            // validator read for the transform output.
            let SchemaType::Struct { fields, .. } = &<TrafficDensity as Schema>::SCHEMA else {
                panic!("expected Struct");
            };
            assert_eq!(fields.len(), 6);
            assert_eq!(fields[0].name, "path_count");
            assert_eq!(fields[0].ty, SchemaType::Scalar(Primitive::U32));
            assert_eq!(fields[1].name, "edge_traffic");
            assert!(matches!(fields[1].ty, SchemaType::Vec(_)));
            assert_eq!(fields[2].name, "node_traffic");
            assert!(matches!(fields[2].ty, SchemaType::Vec(_)));
            assert_eq!(fields[3].name, "untraveled_edges");
            assert!(matches!(fields[3].ty, SchemaType::Vec(_)));
            assert_eq!(fields[4].name, "punch_crossing_cheaper");
            assert_eq!(fields[4].ty, SchemaType::Scalar(Primitive::U32));
            assert_eq!(fields[5].name, "punch_detour_cheaper");
            assert_eq!(fields[5].ty, SchemaType::Scalar(Primitive::U32));
        }

        #[test]
        fn traffic_kinds_resolve_distinctly_from_corridor_kinds() {
            use aether_data::Kind;
            // The aggregation's input bundle and output density carry ids
            // distinct from each other and from the corridor / field / log
            // kinds they compose with — a shared id would alias a transform
            // Ref slot.
            let ids = [
                TrajectorySet::ID,
                TrafficDensity::ID,
                CorridorGraph::ID,
                ScalarField::ID,
                TrajectoryLog::ID,
            ];
            for (i, a) in ids.iter().enumerate() {
                for b in &ids[i + 1..] {
                    assert_ne!(a, b, "traffic / corridor kind ids must be distinct");
                }
            }
        }

        #[test]
        fn realization_problem_schema_embeds_reachability_problem() {
            let SchemaType::Struct { fields, .. } = &<RealizationProblem as Schema>::SCHEMA else {
                panic!("expected Struct");
            };
            assert_eq!(fields.len(), 12);
            assert_eq!(fields[0].name, "problem");
            assert!(matches!(fields[0].ty, SchemaType::Struct { .. }));
            assert_eq!(fields[1].name, "goal");
            assert!(matches!(fields[1].ty, SchemaType::Vec(_)));
            assert_eq!(fields[2].name, "budget");
            assert_eq!(fields[2].ty, SchemaType::Scalar(Primitive::U32));
            assert_eq!(fields[5].name, "covered_extent_initial");
            assert_eq!(fields[5].ty, SchemaType::Scalar(Primitive::F32));
            assert_eq!(fields[6].name, "covered_growth_per_tick");
            assert_eq!(fields[6].ty, SchemaType::Scalar(Primitive::F32));
            assert_eq!(fields[11].name, "seed");
            assert_eq!(fields[11].ty, SchemaType::Scalar(Primitive::U64));
        }

        #[test]
        fn closure_distribution_schema_is_struct_of_samples() {
            let SchemaType::Struct { fields, .. } = &<ClosureDistribution as Schema>::SCHEMA else {
                panic!("expected Struct");
            };
            assert_eq!(fields.len(), 3);
            assert_eq!(fields[0].name, "runs");
            assert_eq!(fields[0].ty, SchemaType::Scalar(Primitive::U32));
            assert_eq!(fields[1].name, "closed");
            assert_eq!(fields[1].ty, SchemaType::Scalar(Primitive::U32));
            assert_eq!(fields[2].name, "samples");
            let SchemaType::Vec(element) = &fields[2].ty else {
                panic!("expected Vec");
            };
            let SchemaType::Struct { fields: sample, .. } = &**element else {
                panic!("expected nested RunOutcome struct");
            };
            assert_eq!(sample.len(), 3);
            assert_eq!(sample[0].name, "run");
            assert_eq!(sample[1].name, "finished");
            assert_eq!(sample[1].ty, SchemaType::Bool);
            assert_eq!(sample[2].name, "closure_tick");
        }

        #[test]
        fn traffic_density_postcard_round_trips() {
            use aether_data::Kind;
            let density = TrafficDensity {
                path_count: 3,
                edge_traffic: vec![5, 0, 2],
                node_traffic: vec![3, 3, 1, 0],
                untraveled_edges: vec![1],
                punch_crossing_cheaper: 4,
                punch_detour_cheaper: 1,
            };
            let bytes = density.encode_into_bytes();
            let back =
                TrafficDensity::decode_from_bytes(&bytes).expect("traffic density round-trips");
            assert_eq!(density, back);
        }
    }
}
