//! The certifier `#[transform]`s (ADR-0047/0048/0049). Each wraps one of
//! the subsystem's pure cores (`reachability`, `corridor`,
//! `counterfactual`, `traffic`) as a unary or multi-input `Kind -> Kind`
//! node. A `#[transform]` here links into both `aether-substrate-bundle`
//! (the headless binary's `TransformRegistry::from_inventory`) and
//! `aether-mcp` (`describe_transforms`), so the link-time inventory
//! submission populates both surfaces with no extra wiring.
//!
//! These ship in the production binaries — they are not `#[cfg(test)]`
//! like the DAG executor's `double` / `seed` fixtures. The generic
//! `mat4_apply` first-party transform (ADR-0048's first example) is
//! unrelated to reachability and stays in `aether-capabilities`.

use aether_data::transform;
use aether_kinds::TrajectoryLog;

use crate::{
    BudgetQuery, ClosureDistribution, CorridorGraph, CrossingClassification, CrossingQueryParams,
    MovementStencil, PopulationSweepProblem, ReachabilityMargin, ReachabilityProblem,
    RealizationProblem, ResolutionDepth, ScalarField, SurvivalCurve, TrafficDensity, TrajectorySet,
};

use crate::corridor::{build_corridor_graph_core, corridor_resolution_depth_core};
use crate::counterfactual::solve_counterfactual_core;
use crate::reachability::{
    UNREACHABLE, realize_single, simulate_realization, solve_cost_to_reach, solve_population_sweep,
};
use crate::traffic::aggregate_traffic_core;

/// Solve minimum-cost reachability over a time-varying scalar cost field
/// (ADR-0047/0048/0049, issue 1857). `ReachabilityProblem` bundles the
/// cost field, the movement stencil, and the start seed so the transform
/// stays a unary `Kind → Kind` node, the same shape `Mat4Apply` gives
/// `mat4_apply`. The output is the cost-to-reach field `V` — a
/// `ScalarField` of the same shape — so it is the single currency every
/// follow-on field pass consumes.
///
/// The body delegates to the pure [`solve_cost_to_reach`] core (the
/// reusable internal API the corridor-graph / windowed-replan / agent-
/// population passes call directly); the transform fn itself only
/// unbundles the operands and rewraps the result, so it clears the
/// `#[transform]` purity deny-list.
#[transform]
fn solve(problem: ReachabilityProblem) -> ScalarField {
    let ScalarField {
        width,
        height,
        ticks,
        values: costs,
    } = problem.cost;
    let values = solve_cost_to_reach(
        width as usize,
        height as usize,
        ticks as usize,
        &costs,
        &problem.stencil.offsets,
        &problem.start,
    );
    ScalarField {
        width,
        height,
        ticks,
        values,
    }
}

/// Threshold a solved cost-to-reach field against a budget (issue 1857).
/// Reads the minimum cost-to-reach over the field's final tick: the
/// cheapest cell reachable under the full horizon. `reachable` is that
/// minimum `< budget`; `margin` is `budget - minimum` as a signed value
/// (positive slack under budget, negative when even the cheapest reachable
/// cell exceeds it, or when no cell is reachable). Kept off `ScalarField`
/// itself so every downstream consumer is free of a readout it does not
/// use — a separate cached query transform composes better.
#[transform]
fn reachability_margin(field: ScalarField, query: BudgetQuery) -> ReachabilityMargin {
    let ScalarField {
        width,
        height,
        ticks,
        values,
    } = field;
    let plane = (width as usize).saturating_mul(height as usize);
    let min_cost = if ticks == 0 || plane == 0 {
        UNREACHABLE
    } else {
        let final_base = (ticks as usize - 1).saturating_mul(plane);
        values
            .get(final_base..final_base.saturating_add(plane))
            .and_then(|layer| layer.iter().copied().min())
            .unwrap_or(UNREACHABLE)
    };
    let reachable = min_cost < query.budget;
    let margin = i64::from(query.budget) - i64::from(min_cost);
    ReachabilityMargin {
        reachable,
        min_cost,
        margin,
    }
}

/// Build the time-sliced corridor graph of a solved cost-to-reach field
/// `V` under a budget (ADR-0047/0048/0049, issue 1858). A multi-input
/// transform: `V` (the `solve` output), the movement stencil shared with
/// the solver, and the budget query whose `budget` is the affordability
/// threshold `B`. The output is the connectivity skeleton of `V` — per
/// tick the connected components of `{cell : V <= B}`, the inter-tick flow
/// edges, and the intra-tick punch edges priced at the sublevel-filtration
/// threshold of `V` that fuses them — a flat DAG orders of magnitude
/// smaller than `V`.
///
/// The body delegates to the pure [`build_corridor_graph_core`] (the
/// reusable internal API the path-snap / validation passes call directly);
/// the transform fn only unbundles the operands, so it clears the
/// `#[transform]` purity deny-list.
// The `#[transform]` ABI hands the body owned kinds decoded from wire
// bytes; the core borrows them, so the owned `field` / `stencil` params
// are intentionally passed by reference rather than consumed.
#[allow(clippy::needless_pass_by_value)]
#[transform]
fn build_corridor_graph(
    field: ScalarField,
    stencil: MovementStencil,
    query: BudgetQuery,
) -> CorridorGraph {
    build_corridor_graph_core(&field, &stencil.offsets, query.budget)
}

/// Compute the fork resolution depth of a corridor graph (issue 1859): how
/// far ahead a bounded-horizon traversal must look before committing to an
/// affordable one-tick step. A unary `CorridorGraph -> ResolutionDepth`
/// node — two passes over #1858's landed graph (backward liveness over the
/// `Flow` edges, then longest dead-end path per fork), no windowed re-solve
/// and no new field.
///
/// The body delegates to the pure [`corridor_resolution_depth_core`]; the
/// transform fn only borrows the decoded graph, so it clears the
/// `#[transform]` purity deny-list (no host fn, no `Ctx`, no `std::time` /
/// `std::env`).
// The `#[transform]` ABI hands the body the owned graph decoded from wire
// bytes; the core borrows it, so the owned `graph` param is intentionally
// passed by reference rather than consumed.
#[allow(clippy::needless_pass_by_value)]
#[transform]
fn corridor_resolution_depth(graph: CorridorGraph) -> ResolutionDepth {
    corridor_resolution_depth_core(&graph)
}

/// Sweep a seeded Monte-Carlo population of reaction-delayed agents over a
/// time-varying scalar cost field, emitting the completion-rate-vs-
/// reaction-delay curve (issue 1863). `PopulationSweepProblem` bundles the
/// shared reachability operands (field, stencil, start region), the goal
/// region, and the sweep parameters (budget, population, window, seed, the
/// delay set) so the transform stays a unary `Kind → Kind` node, the same
/// shape `Mat4Apply` gives `mat4_apply`.
///
/// The body delegates to the pure [`solve_population_sweep`] core — each
/// agent's planner is #1857's exact [`solve_cost_to_reach`] degraded by a
/// reaction-delay lag, so at `delay = 0` with a full window the survival
/// fraction approaches the exact reachable-under-budget bound. The seed is
/// an explicit input and the PRNG deterministic, so the sweep is a pure
/// function of its inputs and content-addresses correctly: it clears the
/// `#[transform]` purity deny-list (no host fn, no `Ctx`, no `std::time` /
/// `std::env`; seeded PRNG only).
#[transform]
fn solve_population(input: PopulationSweepProblem) -> SurvivalCurve {
    solve_population_sweep(input)
}

/// Classify every budget-crossing in a recorded path against a windowed
/// cost field — the counterfactual reachability-from-state query
/// (ADR-0047/0048/0049, issue 1864). A three-input transform: the
/// `ReachabilityProblem` (its cost field + movement stencil; its `start`
/// seed is unused — the path supplies the seed), the recorded
/// `TrajectoryLog` (issue 1862), and the `(window, budget)` params. The
/// output is one `CrossingVerdict` per budget-crossing, each carrying
/// whether the crossing was avoidable — whether, `window` ticks earlier
/// and seeing only the field visible in that window, a within-budget
/// continuation existed from the path's actual state.
///
/// The body delegates to the pure [`solve_counterfactual_core`] (the
/// reusable internal API), which reuses [`solve_cost_to_reach`] for each
/// per-crossing windowed re-solve; the transform fn only unbundles the
/// operands and threads the params, so it clears the `#[transform]` purity
/// deny-list. Iterative throughout (a per-crossing loop, a per-window
/// dense solve).
// The `#[transform]` ABI hands the body owned kinds decoded from wire
// bytes; the core borrows them, so the owned `problem` / `path` params are
// intentionally passed by reference rather than consumed.
#[allow(clippy::needless_pass_by_value)]
#[transform]
fn solve_counterfactual(
    problem: ReachabilityProblem,
    path: TrajectoryLog,
    params: CrossingQueryParams,
) -> CrossingClassification {
    solve_counterfactual_core(
        &problem.cost,
        &problem.stencil.offsets,
        &path,
        params.window,
        params.budget,
    )
}

/// Simulate a seeded distribution of self-realizing field runs and emit the
/// closure-outcome distribution (issue 1867). `RealizationProblem` bundles
/// the shared reachability operands (base field, stencil, start region), the
/// goal region, the contribution model (`placement_period`, `lead_ticks`,
/// `covered_extent_initial`, `covered_growth_per_tick`, `contribution_cost`,
/// `max_concurrent`), the planning `window`, the run count, and the seed, so
/// the transform stays a unary `Kind → Kind` node — the same shape
/// `Mat4Apply` gives `mat4_apply`.
///
/// The body delegates to the pure [`simulate_realization`] core: per run a
/// closed feedback loop where the agent's own motion spawns the snapshot
/// contributions it then plans against via #1857's exact
/// [`solve_cost_to_reach`], so the realized field is path-dependent and the
/// closure is structural. This is the empirical complement to #1866's local
/// escapability bound — it realizes the aggregate trail along the real path
/// and detects the multi-window closure the single-instant `cover(L)` measure
/// cannot rule out. The seed is an explicit input and the PRNG
/// deterministic, so the sweep is a pure function of its inputs and
/// content-addresses correctly: it clears the `#[transform]` purity deny-list
/// (no host fn, no `Ctx`, no `std::time` / `std::env`; seeded PRNG only).
#[transform]
fn simulate_realization_sweep(input: RealizationProblem) -> ClosureDistribution {
    simulate_realization(input)
}

/// Replay a single self-realizing run (the input's seed, run index `0`) and
/// emit its realized field as a stacked `(tick, y, x)` [`ScalarField`] (issue
/// 1867). The inspection path for the counts-only [`ClosureDistribution`]:
/// leaning on the determinism contract (same seed → same realized field), any
/// run's path-dependent realized field is recoverable by replaying that seed
/// here, so the headline distribution stays tiny. The body delegates to the
/// pure [`realize_single`] core and clears the same purity deny-list as
/// `simulate_realization_sweep`.
#[transform]
fn realize_single_run(input: RealizationProblem) -> ScalarField {
    realize_single(input)
}

/// Aggregate a set of paths onto a corridor graph, producing the per-edge
/// traffic density (ADR-0047/0048/0049, issue 1865). A five-input
/// transform: the corridor graph (#1858), the field `V` it was built from
/// (#1857) re-derived for the per-tick snap, the movement stencil and the
/// budget query shared with the builder, and the trajectory set (#1862)
/// to aggregate. The output is a flat reduction keyed by the graph's node
/// / edge indices — per-edge traffic, per-node visits, the untraveled
/// (reachable-but-zero-traffic) edges, and the punch-traffic split by
/// whether crossing beat the affordable detour.
///
/// The body delegates to the pure [`aggregate_traffic_core`] (which reuses
/// #1858's per-tick component labeler for snap id parity); the transform
/// fn only unbundles the operands, so it clears the `#[transform]` purity
/// deny-list.
// The `#[transform]` ABI hands the body owned kinds decoded from wire
// bytes; the core borrows them, so the owned `graph` / `field` / `stencil`
// / `paths` params are intentionally passed by reference rather than
// consumed.
#[allow(clippy::needless_pass_by_value)]
#[transform]
fn aggregate_traffic(
    graph: CorridorGraph,
    field: ScalarField,
    stencil: MovementStencil,
    query: BudgetQuery,
    paths: TrajectorySet,
) -> TrafficDensity {
    aggregate_traffic_core(&graph, &field, &stencil.offsets, query.budget, &paths.logs)
}

/// The canonical 64×64 grid over 1800 ticks at uniform cost 1, seeded at
/// cell 0 — the largest field the transform tests drive (issue 1908). Shared
/// by the `solve` and population-sweep encode-under-cap tests so the
/// dimensions and seed live in exactly one place.
#[cfg(test)]
fn canonical_uniform_problem() -> ReachabilityProblem {
    use crate::reachability::test_fields::stencil_4way;

    let width = 64u32;
    let height = 64u32;
    let ticks = 1800u32;
    let plane = (width * height) as usize;
    ReachabilityProblem {
        cost: ScalarField {
            width,
            height,
            ticks,
            values: vec![1u32; plane * ticks as usize],
        },
        stencil: stencil_4way(),
        start: {
            let mut s = vec![UNREACHABLE; plane];
            s[0] = 0;
            s
        },
    }
}

#[cfg(test)]
mod reachability_transform_tests {
    use super::{canonical_uniform_problem, reachability_margin, solve};
    use crate::reachability::test_fields::{UNREACHABLE, stencil_4way};
    use crate::{
        BudgetQuery, MovementStencil, ReachabilityMargin, ReachabilityProblem, ScalarField,
        StencilOffset,
    };
    use aether_data::{Kind, transforms};

    /// 3×1 uniform-cost field, start at cell 0 — the same hand-checked
    /// field the solver-core tests pin, here driven through the transform.
    fn small_problem() -> ReachabilityProblem {
        ReachabilityProblem {
            cost: ScalarField {
                width: 3,
                height: 1,
                ticks: 3,
                values: vec![1u32; 9],
            },
            stencil: stencil_4way(),
            start: vec![0, UNREACHABLE, UNREACHABLE],
        }
    }

    #[test]
    fn solve_transform_produces_the_cost_to_reach_field() {
        let v = solve(small_problem());
        assert_eq!(v.width, 3);
        assert_eq!(v.height, 1);
        assert_eq!(v.ticks, 3);
        assert_eq!(
            v.values,
            vec![0, UNREACHABLE, UNREACHABLE, 1, 1, UNREACHABLE, 2, 2, 2]
        );
    }

    #[test]
    fn solve_registered_in_link_time_inventory() {
        // Same contract as `mat4_apply`: registered, one `ReachabilityProblem`
        // input slot, `ScalarField` output.
        let entry = transforms()
            .find(|t| t.name.ends_with("::solve"))
            .expect("solve not registered in link-time inventory");
        assert_eq!(entry.input_kind_ids, [ReachabilityProblem::ID]);
        assert_eq!(entry.output_kind_id, ScalarField::ID);
    }

    #[test]
    fn reachability_margin_registered_in_link_time_inventory() {
        // A two-input transform: `(ScalarField, BudgetQuery)` in slot order,
        // `ReachabilityMargin` out.
        let entry = transforms()
            .find(|t| t.name.ends_with("::reachability_margin"))
            .expect("reachability_margin not registered in link-time inventory");
        assert_eq!(entry.input_kind_ids, [ScalarField::ID, BudgetQuery::ID]);
        assert_eq!(entry.output_kind_id, ReachabilityMargin::ID);
    }

    #[test]
    fn reachability_kinds_resolve_distinctly() {
        let ids = [
            ReachabilityProblem::ID,
            ScalarField::ID,
            BudgetQuery::ID,
            ReachabilityMargin::ID,
            MovementStencil::ID,
        ];
        for (i, a) in ids.iter().enumerate() {
            for b in &ids[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }

    #[test]
    fn margin_threshold_is_reachable_under_budget() {
        // Final-tick minimum is 2 (every cell reaches cost 2 at t = 2).
        let field = solve(small_problem());
        let under = reachability_margin(field.clone(), BudgetQuery { budget: 5 });
        assert_eq!(
            under,
            ReachabilityMargin {
                reachable: true,
                min_cost: 2,
                margin: 3,
            }
        );
        let exact = reachability_margin(field.clone(), BudgetQuery { budget: 2 });
        // `< budget` is strict: a budget equal to the minimum is not under.
        assert_eq!(
            exact,
            ReachabilityMargin {
                reachable: false,
                min_cost: 2,
                margin: 0,
            }
        );
        let over = reachability_margin(field, BudgetQuery { budget: 1 });
        assert_eq!(
            over,
            ReachabilityMargin {
                reachable: false,
                min_cost: 2,
                margin: -1,
            }
        );
    }

    #[test]
    fn margin_on_all_unreachable_final_tick_is_not_reachable() {
        // A stay-only stencil leaves every non-start cell unreachable; with
        // no start cell at all, the whole final tick is the sentinel.
        let field = solve(ReachabilityProblem {
            cost: ScalarField {
                width: 2,
                height: 1,
                ticks: 2,
                values: vec![1u32; 4],
            },
            stencil: MovementStencil {
                offsets: vec![StencilOffset { dx: 0, dy: 0 }],
            },
            start: vec![UNREACHABLE, UNREACHABLE],
        });
        let margin = reachability_margin(field, BudgetQuery { budget: 100 });
        assert!(!margin.reachable);
        assert_eq!(margin.min_cost, UNREACHABLE);
    }

    #[test]
    fn solve_is_deterministic_and_content_addressable() {
        // Same input -> identical output, and the content-addressing path
        // (the encoded output bytes the executor keys on) replays
        // byte-for-byte.
        let a = solve(small_problem());
        let b = solve(small_problem());
        assert_eq!(a, b);
        assert_eq!(a.encode_into_bytes(), b.encode_into_bytes());
    }

    #[test]
    fn canonical_field_encodes_under_output_cap_and_round_trips() {
        // The canonical 64×64 grid over 1800 ticks (~7.4M cells): the
        // solved `ScalarField` must fit the 64MB transform output cap
        // (ADR-0048 §6) and round-trip byte-stable through the kind codec.
        const CAP: usize = 64 * 1024 * 1024;
        let width = 64u32;
        let height = 64u32;
        let ticks = 1800u32;
        let plane = (width * height) as usize;
        let problem = canonical_uniform_problem();
        let field = solve(problem);
        assert_eq!(field.values.len(), plane * ticks as usize);

        let bytes = field.encode_into_bytes();
        assert!(
            bytes.len() < CAP,
            "encoded canonical field is {} bytes, over the {CAP}-byte cap",
            bytes.len()
        );

        let back = ScalarField::decode_from_bytes(&bytes).expect("canonical field round-trips");
        assert_eq!(field, back);
    }
}

#[cfg(test)]
mod corridor_transform_tests {
    use super::build_corridor_graph;
    use crate::reachability::test_fields::stencil_4way;
    use crate::{BudgetQuery, CorridorGraph, EdgeKind, MovementStencil, ScalarField};
    use aether_data::{Kind, transforms};

    /// 5×1 field with a sub-budget ridge at cell 2, driven through the
    /// transform: two components and a punch priced at the ridge `V`.
    fn ridge_field() -> ScalarField {
        ScalarField {
            width: 5,
            height: 1,
            ticks: 1,
            values: vec![1, 1, 7, 1, 1],
        }
    }

    #[test]
    fn transform_produces_components_and_a_punch() {
        let graph = build_corridor_graph(ridge_field(), stencil_4way(), BudgetQuery { budget: 5 });
        assert_eq!(graph.nodes.len(), 2);
        assert_eq!(graph.edges.len(), 1);
        assert_eq!(graph.edges[0].kind, EdgeKind::Punch);
        assert_eq!(graph.edges[0].price, 7);
    }

    #[test]
    fn registered_in_link_time_inventory() {
        // A three-input transform: `(ScalarField, MovementStencil,
        // BudgetQuery)` in slot order, `CorridorGraph` out — the same
        // inventory contract `solve` / `reachability_margin` satisfy.
        let entry = transforms()
            .find(|t| t.name.ends_with("::build_corridor_graph"))
            .expect("build_corridor_graph not registered in link-time inventory");
        assert_eq!(
            entry.input_kind_ids,
            [ScalarField::ID, MovementStencil::ID, BudgetQuery::ID]
        );
        assert_eq!(entry.output_kind_id, CorridorGraph::ID);
    }

    #[test]
    fn build_is_deterministic_and_content_addressable() {
        // Same input bytes -> identical output bytes: the content-addressing
        // contract the DAG executor keys transform results on.
        let a = build_corridor_graph(ridge_field(), stencil_4way(), BudgetQuery { budget: 5 });
        let b = build_corridor_graph(ridge_field(), stencil_4way(), BudgetQuery { budget: 5 });
        assert_eq!(a, b);
        assert_eq!(a.encode_into_bytes(), b.encode_into_bytes());
    }
}

#[cfg(test)]
mod resolution_depth_transform_tests {
    use super::corridor_resolution_depth;
    use crate::{CorridorEdge, CorridorGraph, CorridorNode, EdgeKind, ResolutionDepth};
    use aether_data::{Kind, transforms};

    fn node(tick: u32) -> CorridorNode {
        CorridorNode {
            tick,
            component: 0,
            cell_count: 1,
            min_cost: 1,
        }
    }

    fn flow(from: u32, to: u32) -> CorridorEdge {
        CorridorEdge {
            from,
            to,
            kind: EdgeKind::Flow,
            price: 0,
            overlap_width: 0,
        }
    }

    /// A wall-trap graph driven through the transform: a depth-2 dead-end
    /// branch beside a through spine yields `max_resolution_depth = 2`.
    fn wall_trap_graph() -> CorridorGraph {
        // Spine 0(t0) 1(t1) 2(t2) 3(t3) reaching the final tick 3; dead-end
        // branch off node 0 into node 4(t1) -> node 5(t2) that terminates
        // before the final tick and never rejoins. Fork node 0, depth 2.
        CorridorGraph {
            nodes: vec![node(0), node(1), node(2), node(3), node(1), node(2)],
            edges: vec![flow(0, 1), flow(1, 2), flow(2, 3), flow(0, 4), flow(4, 5)],
        }
    }

    #[test]
    fn transform_reports_the_fork_depth() {
        let depths = corridor_resolution_depth(wall_trap_graph());
        assert_eq!(depths.max_resolution_depth, 2);
        assert_eq!(depths.forks.len(), 1);
        assert_eq!(depths.forks[0].node_index, 0);
        assert_eq!(depths.forks[0].depth, 2);
    }

    #[test]
    fn registered_in_link_time_inventory() {
        // A unary transform: `CorridorGraph` in, `ResolutionDepth` out — the
        // same inventory contract `mat4_apply` / `build_corridor_graph`
        // satisfy.
        let entry = transforms()
            .find(|t| t.name.ends_with("::corridor_resolution_depth"))
            .expect("corridor_resolution_depth not registered in link-time inventory");
        assert_eq!(entry.input_kind_ids, [CorridorGraph::ID]);
        assert_eq!(entry.output_kind_id, ResolutionDepth::ID);
    }

    #[test]
    fn resolution_depth_id_distinct_from_corridor_graph() {
        assert_ne!(ResolutionDepth::ID, CorridorGraph::ID);
    }

    #[test]
    fn is_deterministic_and_content_addressable() {
        // Same graph bytes -> identical output bytes: the content-addressing
        // contract the DAG executor keys transform results on.
        let a = corridor_resolution_depth(wall_trap_graph());
        let b = corridor_resolution_depth(wall_trap_graph());
        assert_eq!(a, b);
        assert_eq!(a.encode_into_bytes(), b.encode_into_bytes());
    }
}

#[cfg(test)]
mod population_transform_tests {
    use super::{canonical_uniform_problem, solve_population};
    use crate::reachability::test_fields::{UNREACHABLE, stencil_4way};
    use crate::{PopulationSweepProblem, ReachabilityProblem, ScalarField, SurvivalCurve};
    use aether_data::{Kind, transforms};

    /// A small reachable sweep: a 3×1 uniform-cost field, the population
    /// spawned at cell 0, goal at cell 2.
    fn small_sweep() -> PopulationSweepProblem {
        PopulationSweepProblem {
            problem: ReachabilityProblem {
                cost: ScalarField {
                    width: 3,
                    height: 1,
                    ticks: 3,
                    values: vec![1u32; 9],
                },
                stencil: stencil_4way(),
                start: vec![0, UNREACHABLE, UNREACHABLE],
            },
            goal: vec![2],
            budget: 5,
            population: 16,
            window: 8,
            seed: 0xABCD_1234,
            delays: vec![0, 1, 2],
        }
    }

    #[test]
    fn solve_population_produces_one_sample_per_delay() {
        let curve = solve_population(small_sweep());
        assert_eq!(curve.population, 16);
        assert_eq!(curve.samples.len(), 3);
        assert_eq!(curve.samples[0].delay, 0);
        // Uniform-cost, full-window, zero-lag: the whole population clears
        // the reachable-under-budget certificate.
        assert_eq!(curve.samples[0].finished, 16);
    }

    #[test]
    fn solve_population_registered_in_link_time_inventory() {
        // Same contract as `mat4_apply` / `solve`: registered, one
        // `PopulationSweepProblem` input slot, `SurvivalCurve` output.
        let entry = transforms()
            .find(|t| t.name.ends_with("::solve_population"))
            .expect("solve_population not registered in link-time inventory");
        assert_eq!(entry.input_kind_ids, [PopulationSweepProblem::ID]);
        assert_eq!(entry.output_kind_id, SurvivalCurve::ID);
    }

    #[test]
    fn population_kinds_resolve_distinctly() {
        assert_ne!(PopulationSweepProblem::ID, SurvivalCurve::ID);
        assert_ne!(PopulationSweepProblem::ID, ScalarField::ID);
        assert_ne!(SurvivalCurve::ID, ReachabilityProblem::ID);
    }

    #[test]
    fn solve_population_is_deterministic_and_content_addressable() {
        // Same seed + field -> identical curve, and the content-addressing
        // path (the encoded output bytes the executor keys on) replays
        // byte-for-byte — the proof the seeded Monte-Carlo is pure.
        let a = solve_population(small_sweep());
        let b = solve_population(small_sweep());
        assert_eq!(a, b);
        assert_eq!(a.encode_into_bytes(), b.encode_into_bytes());
    }

    #[test]
    fn canonical_field_sweep_encodes_under_output_cap_and_replays() {
        // The canonical 64×64 grid over 1800 ticks driven through a multi-
        // delay sweep: the `SurvivalCurve` output is a handful of
        // `(delay, finished)` samples, so it sits far under the 64MB
        // transform output cap (ADR-0048 §6), and two runs of the same
        // input replay byte-identically. Uniform cost with the goal a few
        // cells inside the planning window so agents reach it in a handful
        // of ticks — the sweep exercises the canonical field dimensions
        // without grinding the full horizon.
        const CAP: usize = 64 * 1024 * 1024;
        let sweep = || PopulationSweepProblem {
            problem: canonical_uniform_problem(),
            goal: vec![5],
            budget: 1000,
            population: 4,
            window: 8,
            seed: 0x1357_9BDF,
            delays: vec![0, 2, 5],
        };

        let curve = solve_population(sweep());
        assert_eq!(curve.samples.len(), 3);

        let bytes = curve.encode_into_bytes();
        assert!(
            bytes.len() < CAP,
            "encoded survival curve is {} bytes, over the {CAP}-byte cap",
            bytes.len()
        );

        let back = SurvivalCurve::decode_from_bytes(&bytes).expect("survival curve round-trips");
        assert_eq!(curve, back);

        let replay = solve_population(sweep());
        assert_eq!(curve.encode_into_bytes(), replay.encode_into_bytes());
    }
}

#[cfg(test)]
mod counterfactual_transform_tests {
    use super::solve_counterfactual;
    use crate::reachability::test_fields::{UNREACHABLE, stencil_4way};
    use aether_data::{Kind, transforms};
    use aether_kinds::{TrajectoryEndReason, TrajectoryLog, TrajectorySampleEntry};

    use crate::{CrossingClassification, CrossingQueryParams, ReachabilityProblem, ScalarField};

    /// A 3×1 uniform-cost-1 field over `ticks` layers, wrapped in a
    /// `ReachabilityProblem` (whose `start` seed is unused — the path
    /// supplies the seed).
    fn uniform_problem(ticks: u32) -> ReachabilityProblem {
        ReachabilityProblem {
            cost: ScalarField {
                width: 3,
                height: 1,
                ticks,
                values: vec![1u32; 3 * ticks as usize],
            },
            stencil: stencil_4way(),
            // Unused by the counterfactual query; left as a non-seed.
            start: vec![UNREACHABLE, UNREACHABLE, UNREACHABLE],
        }
    }

    fn entry(tick: u32, x: u32, y: u32, value: u32) -> TrajectorySampleEntry {
        TrajectorySampleEntry { tick, x, y, value }
    }

    fn log(samples: Vec<TrajectorySampleEntry>) -> TrajectoryLog {
        TrajectoryLog {
            seed: 7,
            samples,
            end_reason: TrajectoryEndReason::Completed,
        }
    }

    /// A path over the uniform field that crosses budget 5 at tick 4.
    fn crossing_path() -> TrajectoryLog {
        log(vec![
            entry(0, 0, 0, 0),
            entry(1, 1, 0, 2),
            entry(2, 2, 0, 4),
            entry(3, 2, 0, 6),
            entry(4, 2, 0, 8),
        ])
    }

    #[test]
    fn registered_in_link_time_inventory() {
        // A three-input transform: `(ReachabilityProblem, TrajectoryLog,
        // CrossingQueryParams)` in slot order, `CrossingClassification`
        // out — the same inventory contract the other reach transforms
        // satisfy.
        let entry = transforms()
            .find(|t| t.name.ends_with("::solve_counterfactual"))
            .expect("solve_counterfactual not registered in link-time inventory");
        assert_eq!(
            entry.input_kind_ids,
            [
                ReachabilityProblem::ID,
                TrajectoryLog::ID,
                CrossingQueryParams::ID
            ]
        );
        assert_eq!(entry.output_kind_id, CrossingClassification::ID);
    }

    #[test]
    fn avoidable_crossing_under_a_large_window() {
        // The path crosses budget 5 at tick 3 (value 6). Window 4 →
        // decision tick saturates to 0, seed cost 0 at cell 0. Vloc(·, 3)
        // over 4 uniform-cost-1 layers from cost 0 reaches 3 → under budget
        // 5, so the crossing is avoidable (a within-budget continuation
        // existed from the path's state 4 ticks back).
        let out = solve_counterfactual(
            uniform_problem(5),
            crossing_path(),
            CrossingQueryParams {
                window: 4,
                budget: 5,
            },
        );
        assert_eq!(out.crossings.len(), 1);
        let v = out.crossings[0];
        assert_eq!(v.crossing_tick, 3);
        assert_eq!(v.decision_tick, 0);
        assert_eq!(v.seed_cost, 0);
        assert!(v.avoidable, "best continuation 3 < budget 5");
        assert_eq!(v.best_continuation_cost, 3);
        assert_eq!(v.margin, 2);
    }

    #[test]
    fn unavoidable_crossing_under_a_small_window() {
        // Crossing at tick 3. Window 1 → decision tick 2, seed cost 4 at
        // cell 2. Vloc(·, 3) over 2 uniform-cost-1 layers from cost 4
        // reaches 5 → not under budget 5, so the crossing is unavoidable.
        // Same field + path as the avoidable case: the shorter window seeds
        // from a costlier, later state, flipping the verdict.
        let out = solve_counterfactual(
            uniform_problem(5),
            crossing_path(),
            CrossingQueryParams {
                window: 1,
                budget: 5,
            },
        );
        assert_eq!(out.crossings.len(), 1);
        let v = out.crossings[0];
        assert_eq!(v.crossing_tick, 3);
        assert_eq!(v.decision_tick, 2);
        assert_eq!(v.seed_cost, 4);
        assert!(!v.avoidable, "best continuation 5 is not < budget 5");
        assert_eq!(v.best_continuation_cost, 5);
        assert_eq!(v.margin, 0);
    }

    #[test]
    fn no_crossing_yields_empty_classification() {
        // A path that never reaches the budget produces no verdicts.
        let path = log(vec![
            entry(0, 0, 0, 0),
            entry(1, 1, 0, 1),
            entry(2, 2, 0, 2),
        ]);
        let out = solve_counterfactual(
            uniform_problem(3),
            path,
            CrossingQueryParams {
                window: 1,
                budget: 5,
            },
        );
        assert!(out.crossings.is_empty());
    }

    #[test]
    fn is_deterministic_and_content_addressable() {
        // Same input bytes -> identical output bytes: the content-addressing
        // contract the DAG executor keys transform results on, so a query
        // replays byte-for-byte.
        let a = solve_counterfactual(
            uniform_problem(5),
            crossing_path(),
            CrossingQueryParams {
                window: 4,
                budget: 5,
            },
        );
        let b = solve_counterfactual(
            uniform_problem(5),
            crossing_path(),
            CrossingQueryParams {
                window: 4,
                budget: 5,
            },
        );
        assert_eq!(a, b);
        assert_eq!(a.encode_into_bytes(), b.encode_into_bytes());
    }

    #[test]
    fn classification_over_canonical_field_encodes_under_output_cap() {
        // A path with many crossings over the canonical 64×64×1800 field:
        // the `CrossingClassification` is a skeleton (one verdict per
        // crossing), so it stays orders of magnitude under the 64MB
        // transform output cap (ADR-0048 §6) regardless of the field size.
        const CAP: usize = 64 * 1024 * 1024;
        let width = 64u32;
        let height = 64u32;
        let ticks = 1800u32;
        let plane = (width * height) as usize;
        let problem = ReachabilityProblem {
            cost: ScalarField {
                width,
                height,
                ticks,
                values: vec![1u32; plane * ticks as usize],
            },
            stencil: stencil_4way(),
            start: vec![UNREACHABLE; plane],
        };
        // A sawtooth accumulator that crosses budget 5 repeatedly across the
        // full horizon — exercises the per-crossing loop at scale.
        let samples: Vec<_> = (0..ticks)
            .map(|t| entry(t, t % width, (t / width) % height, (t % 8) + 1))
            .collect();
        let path = log(samples);
        let out = solve_counterfactual(
            problem,
            path,
            CrossingQueryParams {
                window: 4,
                budget: 5,
            },
        );
        assert!(!out.crossings.is_empty(), "sawtooth crosses the budget");
        let bytes = out.encode_into_bytes();
        assert!(
            bytes.len() < CAP,
            "encoded classification is {} bytes, over the {CAP}-byte cap",
            bytes.len()
        );
        let back =
            CrossingClassification::decode_from_bytes(&bytes).expect("classification round-trips");
        assert_eq!(out, back);
    }
}

#[cfg(test)]
mod realization_transform_tests {
    use super::{realize_single_run, simulate_realization_sweep};
    use crate::escapability::{EscapeParams, evaluate};
    use crate::reachability::test_fields::{UNREACHABLE, stencil_4way};
    use crate::{ClosureDistribution, ReachabilityProblem, RealizationProblem, ScalarField};
    use aether_data::{Kind, transforms};

    /// A 3×1 corridor whose forward cell is base-blocked: the agent steps
    /// 0 → 1, and its own immediate over-budget contribution on cell 0 closes
    /// the realized field around it (the hand-traced closure regime from the
    /// core tests, driven through the transform).
    fn closing_problem() -> RealizationProblem {
        RealizationProblem {
            problem: ReachabilityProblem {
                cost: ScalarField {
                    width: 3,
                    height: 1,
                    ticks: 4,
                    values: vec![
                        1,
                        1,
                        UNREACHABLE,
                        1,
                        1,
                        UNREACHABLE,
                        1,
                        1,
                        UNREACHABLE,
                        1,
                        1,
                        UNREACHABLE,
                    ],
                },
                stencil: stencil_4way(),
                start: vec![0, UNREACHABLE, UNREACHABLE],
            },
            goal: vec![2],
            budget: 10,
            placement_period: 1,
            lead_ticks: 0,
            covered_extent_initial: 0.0,
            covered_growth_per_tick: 0.0,
            contribution_cost: 100,
            max_concurrent: 4,
            window: 8,
            runs: 4,
            seed: 0xABCD_4321,
        }
    }

    #[test]
    fn simulate_realization_registered_in_link_time_inventory() {
        // Same contract as `solve` / `solve_population`: registered, one
        // `RealizationProblem` input slot, `ClosureDistribution` output.
        let entry = transforms()
            .find(|t| t.name.ends_with("::simulate_realization_sweep"))
            .expect("simulate_realization_sweep not registered in link-time inventory");
        assert_eq!(entry.input_kind_ids, [RealizationProblem::ID]);
        assert_eq!(entry.output_kind_id, ClosureDistribution::ID);
    }

    #[test]
    fn realize_single_registered_in_link_time_inventory() {
        // The companion readout: one `RealizationProblem` input slot, a
        // `ScalarField` (the replayed run's realized field) output.
        let entry = transforms()
            .find(|t| t.name.ends_with("::realize_single_run"))
            .expect("realize_single_run not registered in link-time inventory");
        assert_eq!(entry.input_kind_ids, [RealizationProblem::ID]);
        assert_eq!(entry.output_kind_id, ScalarField::ID);
    }

    #[test]
    fn realization_kinds_resolve_distinctly() {
        assert_ne!(RealizationProblem::ID, ClosureDistribution::ID);
        assert_ne!(RealizationProblem::ID, ScalarField::ID);
        assert_ne!(ClosureDistribution::ID, ScalarField::ID);
    }

    #[test]
    fn simulate_and_single_are_deterministic_and_content_addressable() {
        // Same seed + field bytes -> byte-identical `ClosureDistribution` AND
        // byte-identical `realize_single` `ScalarField`: the content-
        // addressing contract and the proof that "same seed → same realized
        // field."
        let dist_a = simulate_realization_sweep(closing_problem());
        let dist_b = simulate_realization_sweep(closing_problem());
        assert_eq!(dist_a, dist_b);
        assert_eq!(dist_a.encode_into_bytes(), dist_b.encode_into_bytes());

        let field_a = realize_single_run(closing_problem());
        let field_b = realize_single_run(closing_problem());
        assert_eq!(field_a, field_b);
        assert_eq!(field_a.encode_into_bytes(), field_b.encode_into_bytes());
    }

    #[test]
    fn multi_window_closure_each_contribution_locally_certified() {
        // The headline: a regime where **every spawned contribution passes
        // #1866's local `escapable_within_lead`** (each is individually
        // escapable within its lead) yet the concurrent count along the path
        // exceeds #1866's conservative `max_concurrent` (so the local bound
        // explicitly does *not* certify the configuration) — and the
        // simulator empirically exhibits closure the O(1) local bound cannot
        // rule out.
        //
        // The certified contribution shape #1866 evaluates: r0 = 0, g = 1,
        // stencil speed s = 2 (the analytic per-tick displacement bound), lead
        // L = 4 → cover(L) = 4, reach(L) = 8. The per-contribution verdict is
        // `escapable` (4 < 8, g < s) with a *finite* conservative concurrency
        // cap `max_concurrent` — ratio = 2, ratio² = 4, N_max = 3. So #1866
        // certifies any configuration of at most 3 simultaneously-active
        // contributions of this shape, and leaves a count above 3 uncertified.
        let shape = EscapeParams {
            covered_extent_initial: 0.0,
            covered_growth_per_tick: 1.0,
            stencil_speed: 2.0,
            lead_ticks: 4,
        };
        let verdict = evaluate(&shape);
        assert!(
            verdict.escapable,
            "each contribution must be locally certified escapable"
        );
        assert_eq!(verdict.max_concurrent, 3);

        // A 1×1-wide pocket the agent walks into and seals with its own
        // trail: a long thin corridor (1×9) where the agent paces back and
        // forth dropping over-budget contributions every tick, and `lead`
        // and `cover` are tuned to the certified shape above, yet the
        // *aggregate* trail — many concurrent contributions of different
        // ages stacked along the real path — closes the field around it.
        let width = 1u32;
        let height = 9u32;
        let plane = (width * height) as usize;
        let blocked_ends = {
            // A static base field that is finite only in a short reachable
            // stub, so the agent cannot run away from its own trail; the goal
            // sits past a base-blocked cell it can never afford.
            let mut layer = vec![1u32; plane];
            // Block the far half so the agent is confined to a short segment.
            for cell in layer.iter_mut().skip(3) {
                *cell = UNREACHABLE;
            }
            let ticks = 8u32;
            let mut values = Vec::with_capacity(plane * ticks as usize);
            for _ in 0..ticks {
                values.extend_from_slice(&layer);
            }
            (values, ticks)
        };
        let (values, ticks) = blocked_ends;
        let p = RealizationProblem {
            problem: ReachabilityProblem {
                cost: ScalarField {
                    width,
                    height,
                    ticks,
                    values,
                },
                stencil: stencil_4way(),
                start: {
                    let mut s = vec![UNREACHABLE; plane];
                    s[0] = 0;
                    s
                },
            },
            // Goal in the blocked far region: unreachable, so the run never
            // finishes and the only terminal outcome is self-closure.
            goal: vec![8],
            budget: 10,
            placement_period: 1, // dense placement → many concurrent contributions
            lead_ticks: 4,       // the certified shape's lead
            covered_extent_initial: 0.0,
            covered_growth_per_tick: 1.0, // the certified shape's growth
            contribution_cost: 100,       // each covered cell goes over budget
            max_concurrent: 6,            // hold more concurrently than #1866's cap certifies
            window: 8,
            runs: 4,
            seed: 0x0BAD_F00D,
        };
        // The path holds up to `max_concurrent = 6` simultaneously-active
        // contributions — above #1866's conservative cap of 3 for this shape,
        // the multi-window regime the O(1) local check leaves uncertified.
        assert!(p.max_concurrent > verdict.max_concurrent);
        let dist = simulate_realization_sweep(p);
        assert!(
            dist.closed > 0,
            "expected the aggregate trail to close the realized field around the agent"
        );
    }

    #[test]
    fn closure_distribution_encodes_under_output_cap_and_replays() {
        // A multi-run sweep over a multi-cell start region: the
        // `ClosureDistribution` is counts plus one `(u32, bool, u32)`
        // `RunOutcome` per run, so it sits far under the 64MB transform output
        // cap (ADR-0048 §6), and two runs of the same input replay
        // byte-identically.
        const CAP: usize = 64 * 1024 * 1024;
        let width = 8u32;
        let height = 8u32;
        let ticks = 12u32;
        let plane = (width * height) as usize;
        let sweep = || RealizationProblem {
            problem: ReachabilityProblem {
                cost: ScalarField {
                    width,
                    height,
                    ticks,
                    values: vec![1u32; plane * ticks as usize],
                },
                stencil: stencil_4way(),
                start: {
                    let mut s = vec![UNREACHABLE; plane];
                    s[0] = 0;
                    s[1] = 0;
                    s[2] = 0;
                    s
                },
            },
            goal: vec![63],
            budget: 1000,
            placement_period: 2,
            lead_ticks: 3,
            covered_extent_initial: 0.0,
            covered_growth_per_tick: 0.5,
            contribution_cost: 50,
            max_concurrent: 6,
            window: 8,
            runs: 64,
            seed: 0x2468_ACE0,
        };

        let dist = simulate_realization_sweep(sweep());
        assert_eq!(dist.runs, 64);
        assert_eq!(dist.samples.len(), 64);

        let bytes = dist.encode_into_bytes();
        assert!(
            bytes.len() < CAP,
            "encoded closure distribution is {} bytes, over the {CAP}-byte cap",
            bytes.len()
        );
        let back = ClosureDistribution::decode_from_bytes(&bytes)
            .expect("closure distribution round-trips");
        assert_eq!(dist, back);

        let replay = simulate_realization_sweep(sweep());
        assert_eq!(dist.encode_into_bytes(), replay.encode_into_bytes());

        // The companion realized field also fits the cap and round-trips.
        let field = realize_single_run(sweep());
        let field_bytes = field.encode_into_bytes();
        assert!(field_bytes.len() < CAP);
        let field_back =
            ScalarField::decode_from_bytes(&field_bytes).expect("realized field round-trips");
        assert_eq!(field, field_back);
    }
}

#[cfg(test)]
mod traffic_transform_tests {
    use super::{aggregate_traffic, build_corridor_graph};
    use crate::reachability::test_fields::stencil_4way;
    use aether_data::{Kind, transforms};
    use aether_kinds::{TrajectoryEndReason, TrajectoryLog, TrajectorySampleEntry};

    use crate::{
        BudgetQuery, CorridorGraph, MovementStencil, ScalarField, TrafficDensity, TrajectorySet,
    };

    /// A 3×1 uniform-cost field held across 3 ticks — one persisting
    /// component per tick, flow edges chaining them — and a single path
    /// that rides it the whole way.
    fn held_field() -> ScalarField {
        ScalarField {
            width: 3,
            height: 1,
            ticks: 3,
            values: vec![1; 9],
        }
    }

    fn riding_path_set() -> TrajectorySet {
        TrajectorySet {
            logs: vec![TrajectoryLog {
                seed: 1,
                samples: (0..3)
                    .map(|t| TrajectorySampleEntry {
                        tick: t,
                        x: t,
                        y: 0,
                        value: 0,
                    })
                    .collect(),
                end_reason: TrajectoryEndReason::Completed,
            }],
        }
    }

    #[test]
    fn transform_produces_node_and_flow_traffic() {
        let field = held_field();
        let stencil = stencil_4way();
        let query = BudgetQuery { budget: 5 };
        let graph = build_corridor_graph(field.clone(), stencil.clone(), query);
        let density = aggregate_traffic(graph, field, stencil, query, riding_path_set());
        // One visit per tick to the single persisting component, and one
        // flow unit per consecutive-tick step.
        assert_eq!(density.path_count, 1);
        assert_eq!(density.node_traffic.iter().sum::<u32>(), 3);
        assert_eq!(density.edge_traffic.iter().sum::<u32>(), 2);
    }

    #[test]
    fn registered_in_link_time_inventory() {
        // A five-input transform: `(CorridorGraph, ScalarField,
        // MovementStencil, BudgetQuery, TrajectorySet)` in slot order,
        // `TrafficDensity` out — the same inventory contract the other
        // first-party transforms satisfy.
        let entry = transforms()
            .find(|t| t.name.ends_with("::aggregate_traffic"))
            .expect("aggregate_traffic not registered in link-time inventory");
        assert_eq!(
            entry.input_kind_ids,
            [
                CorridorGraph::ID,
                ScalarField::ID,
                MovementStencil::ID,
                BudgetQuery::ID,
                TrajectorySet::ID,
            ]
        );
        assert_eq!(entry.output_kind_id, TrafficDensity::ID);
    }

    #[test]
    fn traffic_kinds_resolve_distinctly() {
        let ids = [
            TrajectorySet::ID,
            TrafficDensity::ID,
            CorridorGraph::ID,
            ScalarField::ID,
            TrajectoryLog::ID,
        ];
        for (i, a) in ids.iter().enumerate() {
            for b in &ids[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }

    #[test]
    fn aggregate_is_deterministic_and_content_addressable() {
        // Same paths + graph + field -> identical output bytes: the
        // content-addressing contract the DAG executor keys results on.
        let field = held_field();
        let stencil = stencil_4way();
        let query = BudgetQuery { budget: 5 };
        let graph = build_corridor_graph(field.clone(), stencil.clone(), query);
        let a = aggregate_traffic(
            graph.clone(),
            field.clone(),
            stencil.clone(),
            query,
            riding_path_set(),
        );
        let b = aggregate_traffic(graph, field, stencil, query, riding_path_set());
        assert_eq!(a, b);
        assert_eq!(a.encode_into_bytes(), b.encode_into_bytes());
    }

    #[test]
    fn canonical_scale_density_encodes_under_output_cap_and_round_trips() {
        // A canonical-scale corridor graph + path set: the density is a
        // skeleton-sized handful of `Vec<u32>`, so it sits far under the
        // 64MB transform output cap (ADR-0048 §6) and round-trips
        // byte-stable through the kind codec.
        const CAP: usize = 64 * 1024 * 1024;
        let width = 64u32;
        let height = 64u32;
        let ticks = 64u32;
        let plane = (width * height) as usize;
        let field = ScalarField {
            width,
            height,
            ticks,
            values: vec![1u32; plane * ticks as usize],
        };
        let stencil = stencil_4way();
        let query = BudgetQuery { budget: 1000 };
        let graph = build_corridor_graph(field.clone(), stencil.clone(), query);
        // A handful of paths riding the single open component diagonally.
        let logs: Vec<TrajectoryLog> = (0..8)
            .map(|seed| TrajectoryLog {
                seed,
                samples: (0..ticks)
                    .map(|t| TrajectorySampleEntry {
                        tick: t,
                        x: t % width,
                        y: t % height,
                        value: 0,
                    })
                    .collect(),
                end_reason: TrajectoryEndReason::Completed,
            })
            .collect();
        let density = aggregate_traffic(graph, field, stencil, query, TrajectorySet { logs });

        let bytes = density.encode_into_bytes();
        assert!(
            bytes.len() < CAP,
            "encoded density is {} bytes, over the {CAP}-byte cap",
            bytes.len()
        );
        let back = TrafficDensity::decode_from_bytes(&bytes).expect("density round-trips");
        assert_eq!(density, back);
    }
}
