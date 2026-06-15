//! Minimum-cost reachability over a time-varying scalar field (issue
//! 1857). The pure dynamic-programming core the `solve` /
//! `reachability_margin` transforms (ADR-0048) wrap, kept in its own
//! module so the follow-on field passes (corridor extraction, windowed
//! re-solve, agent populations, counterfactual queries) call
//! [`solve_cost_to_reach`] directly without going through the transform
//! encode/decode boundary.
//!
//! The graph is a time-layered DAG — ticks only advance — so the
//! cost-to-reach field `V` is a single forward sweep:
//! `V(c, t) = C(c, t) + min over stencil-predecessors c' of V(c', t-1)`,
//! seeded at `t = 0` from the start slice. Accumulated cost is the
//! objective the sweep minimizes, so it never enters the node coordinate
//! — nodes stay `(cell, tick)`, with no accumulator dimension. The pass
//! is dense and iterative (no recursion, per the load-bearing-code rule),
//! deterministic, and integer-exact, so a given field replays
//! identically.

use aether_kinds::{
    MovementStencil, PopulationSweepProblem, ReachabilityProblem, ScalarField, StencilOffset,
    SurvivalCurve, SurvivalSample,
};

/// The reserved sentinel shared by cost fields and the solved
/// cost-to-reach field. In a cost field it marks a blocked / impassable
/// cell; in `V` it marks a cell no stencil-feasible path reaches. Costs
/// are non-negative, so a `u32` field with this sentinel folds "blocked"
/// in with no second mask, and saturating arithmetic keeps the `min`
/// recurrence well-defined (a blocked cell or a missing predecessor
/// absorbs to the sentinel).
pub const UNREACHABLE: u32 = u32::MAX;

/// Shift `coord` by `-delta` (the reachable-from direction) and keep it
/// in `0..bound`. Returns `None` when the predecessor falls off the grid.
/// Stays in `usize` arithmetic (no `i64` round-trip) so the bound check
/// is a single `checked_add` / `checked_sub` per axis.
fn predecessor_coord(coord: usize, delta: i32, bound: usize) -> Option<usize> {
    let magnitude = delta.unsigned_abs() as usize;
    let pred = if delta >= 0 {
        coord.checked_sub(magnitude)?
    } else {
        coord.checked_add(magnitude)?
    };
    (pred < bound).then_some(pred)
}

/// Solve the cost-to-reach field `V` over a `width × height` grid across
/// `ticks` integer time layers (issue 1857).
///
/// `costs` is the row-major `(tick, y, x)` cost field — the scalar at
/// cell `(x, y)` on tick `t` is `costs[t * height * width + y * width + x]`
/// — with [`UNREACHABLE`] marking a blocked cell. `stencil` is the
/// one-tick movement offset set (include the zero offset for the "stay
/// put" move); cell `c` on tick `t` draws from `c - offset` on tick
/// `t - 1` for each offset, so a non-symmetric stencil is read in the
/// reachable-from direction. `start` is the cost-valued seed slice of
/// length `width * height` — the per-cell initial accumulated cost at
/// `t = 0`, with [`UNREACHABLE`] marking a non-start cell. The cost
/// field's `t = 0` layer is never charged; the seed *is* the accumulated
/// value at `t = 0`, which is what lets a windowed re-solve carry its
/// frontier without double-counting the boundary tick.
///
/// Returns the row-major `V` field of the same shape (`width * height *
/// ticks` values). Malformed lengths never panic: a too-short `costs` or
/// `start` reads as [`UNREACHABLE`] past its end, and a `width × height ×
/// ticks` product that overflows `usize` yields an empty field.
pub fn solve_cost_to_reach(
    width: usize,
    height: usize,
    ticks: usize,
    costs: &[u32],
    stencil: &[StencilOffset],
    start: &[u32],
) -> Vec<u32> {
    let plane = width.saturating_mul(height);
    let Some(total) = plane.checked_mul(ticks) else {
        return Vec::new();
    };
    let mut v = vec![UNREACHABLE; total];
    if total == 0 {
        return v;
    }

    // Seed t = 0 from the cost-valued start slice (UNREACHABLE = not
    // seeded). A short slice leaves the tail at the sentinel default.
    let seed_len = plane.min(start.len());
    v[..seed_len].copy_from_slice(&start[..seed_len]);

    // Forward sweep. Each layer reads only the immediately preceding one,
    // so a split borrow gives a shared previous layer and a mutable
    // current layer with no overlap.
    for t in 1..ticks {
        let (head, tail) = v.split_at_mut(t * plane);
        let prev = &head[(t - 1) * plane..];
        let curr = &mut tail[..plane];
        let cost_base = t * plane;
        for y in 0..height {
            for x in 0..width {
                let idx = y * width + x;
                let mut best = UNREACHABLE;
                for offset in stencil {
                    let Some(px) = predecessor_coord(x, offset.dx, width) else {
                        continue;
                    };
                    let Some(py) = predecessor_coord(y, offset.dy, height) else {
                        continue;
                    };
                    best = best.min(prev[py * width + px]);
                }
                let cost = costs.get(cost_base + idx).copied().unwrap_or(UNREACHABLE);
                curr[idx] = cost.saturating_add(best);
            }
        }
    }

    v
}

/// A hand-rolled xorshift64 PRNG (the established in-tree pattern —
/// `audio.rs`'s per-voice xorshift32, `aether-kit`'s `arena.rs`
/// xorshift64 — so the population sweep needs no `rand` dependency). The
/// sequence is a pure function of the seed, so seeding it from the
/// transform's `seed` *input* keeps the whole sweep content-addressable
/// (ADR-0048 §4/§130): same seed + field → byte-identical curve.
struct Xorshift64 {
    state: u64,
}

impl Xorshift64 {
    /// Seed the generator, forcing the state non-zero (xorshift64 is
    /// stuck at zero), the same guard `audio.rs` applies to its
    /// voice-keyed noise PRNG.
    fn seeded(seed: u64) -> Self {
        Self {
            state: if seed == 0 {
                0x9E37_79B9_7F4A_7C15
            } else {
                seed
            },
        }
    }

    /// One xorshift64 step (the `13 / 7 / 17` triple `arena.rs` uses).
    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }

    /// A draw in `0..bound` (`bound >= 1`).
    fn next_bounded(&mut self, bound: usize) -> usize {
        let reduced = self.next_u64() % bound as u64;
        // `reduced < bound`, so it always fits a `usize` (even where the
        // pointer is 32-bit and `bound` was widened to `u64`).
        usize::try_from(reduced).unwrap_or(0)
    }
}

/// Derive a per-agent seed from the sweep seed and the agent index — the
/// splitmix64 finalizer, so each agent gets its own well-mixed tie-break
/// stream. Folding *only* the seed and the index (not the delay) in is
/// deliberate: agent `i`'s PRNG starts identically at every swept delay,
/// so the only thing that changes the agent's fate across the sweep is
/// the reaction lag, not a reshuffled tie-break stream.
fn agent_seed(seed: u64, index: u64) -> u64 {
    let mut z = seed.wrapping_add(index.wrapping_mul(0x9E37_79B9_7F4A_7C15));
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Pick one candidate, breaking a tie with the PRNG (a single draw only
/// when there is an actual tie, so a tie-free field consumes no entropy
/// and the stream stays predictable).
fn pick_tie(rng: &mut Xorshift64, candidates: &[usize]) -> usize {
    match candidates.len() {
        0 => usize::MAX,
        1 => candidates[0],
        n => candidates[rng.next_bounded(n)],
    }
}

/// The per-agent receding-horizon planner (issue 1863). The agent sits at
/// flat cell `current` on real tick `now`; it solves the **exact**
/// [`solve_cost_to_reach`] core over its perceived window — the field as
/// it truly was `delay` ticks earlier for each window layer — and returns
/// the first cell of the optimal path to the cheapest perceived goal.
/// Committing that one step and re-planning next tick is #1859's `S = 1`
/// receding horizon, per agent. Returns `current` (stay put) when no goal
/// is visible inside the window.
///
/// Binding the planner to the same core the exact solver uses is the load-
/// bearing move: at `delay = 0` and `window >= ticks` the perceived field
/// is the true field over the full remaining horizon, so the plan *is* the
/// exact single-source solve and the greedy traces an optimal path —
/// reaching the cheapest goal with exactly its certified cost.
#[allow(clippy::too_many_arguments)]
fn plan_next_cell(
    width: usize,
    height: usize,
    ticks: usize,
    costs: &[u32],
    stencil: &[StencilOffset],
    goal: &[usize],
    current: usize,
    now: usize,
    window: usize,
    delay: usize,
    rng: &mut Xorshift64,
) -> usize {
    let plane = width.saturating_mul(height);
    if plane == 0 || now + 1 >= ticks {
        return current;
    }
    // The window spans real ticks [now ..= horizon_end]; layer k is real
    // tick now + k, perceived as the true field at tick max(0, real - d).
    let horizon_end = (now + window).min(ticks - 1);
    let layers = horizon_end - now + 1;
    if layers < 2 {
        return current;
    }

    let mut perceived = vec![UNREACHABLE; plane * layers];
    for k in 0..layers {
        let real_tick = now + k;
        let src_tick = real_tick.saturating_sub(delay);
        let src_base = src_tick * plane;
        let dst_base = k * plane;
        for idx in 0..plane {
            perceived[dst_base + idx] = costs.get(src_base + idx).copied().unwrap_or(UNREACHABLE);
        }
    }

    let mut seed = vec![UNREACHABLE; plane];
    seed[current] = 0;
    let v = solve_cost_to_reach(width, height, layers, &perceived, stencil, &seed);

    // Cheapest perceived goal arrival over layers 1..layers (layer 0 is
    // the seed). Among equal-cost arrivals, the PRNG breaks the tie.
    let mut best_cost = UNREACHABLE;
    let mut best_targets: Vec<(usize, usize)> = Vec::new();
    for k in 1..layers {
        let layer_base = k * plane;
        for &g in goal {
            if g >= plane {
                continue;
            }
            let cost = v[layer_base + g];
            if cost == UNREACHABLE {
                continue;
            }
            if cost < best_cost {
                best_cost = cost;
                best_targets.clear();
                best_targets.push((g, k));
            } else if cost == best_cost {
                best_targets.push((g, k));
            }
        }
    }
    if best_targets.is_empty() {
        return current;
    }
    let pick = if best_targets.len() == 1 {
        0
    } else {
        rng.next_bounded(best_targets.len())
    };
    let (mut cur, mut layer) = best_targets[pick];

    // Backtrack along an optimal predecessor chain to layer 1; that cell
    // is the agent's next step. `cur(layer)` drew from `cur - offset` on
    // `layer - 1`, mirroring the forward sweep's predecessor read.
    while layer > 1 {
        let x = cur % width;
        let y = cur / width;
        let target = v[layer * plane + cur];
        let landed = perceived[layer * plane + cur];
        let mut preds: Vec<usize> = Vec::new();
        for offset in stencil {
            let Some(px) = predecessor_coord(x, offset.dx, width) else {
                continue;
            };
            let Some(py) = predecessor_coord(y, offset.dy, height) else {
                continue;
            };
            let p = py * width + px;
            let pv = v[(layer - 1) * plane + p];
            if pv != UNREACHABLE && landed.saturating_add(pv) == target {
                preds.push(p);
            }
        }
        if preds.is_empty() {
            // No reconstructable predecessor (degenerate perceived field) —
            // stay rather than step blind.
            return current;
        }
        cur = pick_tie(rng, &preds);
        layer -= 1;
    }
    cur
}

/// Simulate one reaction-delayed agent from `start` under reaction lag
/// `delay`, returning whether it finishes (lands on a goal cell with
/// accumulated cost `< budget`) before the final tick. Accumulates the
/// **true** (un-lagged) cost of every cell it lands on — the lag only
/// clouds the agent's *plan*, never the cost it actually pays.
#[allow(clippy::too_many_arguments)]
fn simulate_agent(
    width: usize,
    height: usize,
    ticks: usize,
    costs: &[u32],
    stencil: &[StencilOffset],
    goal: &[usize],
    start: usize,
    start_cost: u32,
    budget: u32,
    window: usize,
    delay: usize,
    rng: &mut Xorshift64,
) -> bool {
    let plane = width.saturating_mul(height);
    let is_goal = |cell: usize| goal.contains(&cell);

    let mut current = start;
    let mut acc = start_cost;
    if acc < budget && is_goal(current) {
        return true;
    }

    for now in 0..ticks.saturating_sub(1) {
        let next = plan_next_cell(
            width, height, ticks, costs, stencil, goal, current, now, window, delay, rng,
        );
        let landed_tick = now + 1;
        let true_cost = costs
            .get(landed_tick * plane + next)
            .copied()
            .unwrap_or(UNREACHABLE);
        acc = acc.saturating_add(true_cost);
        current = next;
        if acc < budget && is_goal(current) {
            return true;
        }
    }
    false
}

/// Run a seeded Monte-Carlo population of reaction-delayed agents over a
/// time-varying scalar cost field and emit the completion-rate-vs-
/// reaction-delay curve (issue 1863). Each agent's planner is #1857's
/// exact [`solve_cost_to_reach`] degraded by a per-tick perceived-field
/// lag, so at `delay = 0` with a full window the survival fraction
/// approaches the exact reachable-under-budget bound.
///
/// The whole sweep is a pure function of `problem` (seed + field in,
/// curve out): the PRNG samples the population once from the start region,
/// then every swept delay re-simulates that same population, so the only
/// variable across the curve is the reaction lag. Iterative throughout
/// (the per-agent window solve and the simulation loop are bounded; no
/// recursion, per the load-bearing-code rule).
pub fn solve_population_sweep(problem: PopulationSweepProblem) -> SurvivalCurve {
    let PopulationSweepProblem {
        problem:
            ReachabilityProblem {
                cost:
                    ScalarField {
                        width,
                        height,
                        ticks,
                        values: costs,
                    },
                stencil: MovementStencil { offsets: stencil },
                start: start_seed,
            },
        goal: goal_indices,
        budget,
        population,
        window,
        seed,
        delays,
    } = problem;
    let width = width as usize;
    let height = height as usize;
    let ticks = ticks as usize;
    let window = window as usize;
    let plane = width.saturating_mul(height);

    let goal: Vec<usize> = goal_indices
        .iter()
        .map(|&g| g as usize)
        .filter(|&g| g < plane)
        .collect();

    // The start region is every cell with a finite seed value; each agent
    // inherits its sampled cell's seed as its initial accumulated cost.
    let start_cells: Vec<usize> = (0..plane)
        .filter(|&c| start_seed.get(c).copied().unwrap_or(UNREACHABLE) != UNREACHABLE)
        .collect();

    // Sample the population once (so the same agents are replayed at every
    // delay) — a no-op when the start region is empty.
    let mut spawn_rng = Xorshift64::seeded(seed);
    let agents: Vec<usize> = if start_cells.is_empty() {
        Vec::new()
    } else {
        (0..population)
            .map(|_| start_cells[spawn_rng.next_bounded(start_cells.len())])
            .collect()
    };

    let mut samples = Vec::with_capacity(delays.len());
    for &delay in &delays {
        let delay_ticks = delay as usize;
        let mut finished: u32 = 0;
        for (i, &start) in agents.iter().enumerate() {
            let start_cost = start_seed.get(start).copied().unwrap_or(UNREACHABLE);
            let mut rng = Xorshift64::seeded(agent_seed(seed, i as u64 + 1));
            if simulate_agent(
                width,
                height,
                ticks,
                &costs,
                &stencil,
                &goal,
                start,
                start_cost,
                budget,
                window,
                delay_ticks,
                &mut rng,
            ) {
                finished += 1;
            }
        }
        samples.push(SurvivalSample { delay, finished });
    }

    SurvivalCurve {
        population,
        samples,
    }
}

/// Shared cost-field fixtures for this crate's `#[cfg(test)]` modules: the
/// `stencil_4way` movement set lives here once (returned both as raw
/// [`StencilOffset`]s for the solver-core tests and wrapped in a
/// [`MovementStencil`] for the transform tests) so the reachability,
/// population, corridor, and population-transform test modules reuse one
/// definition instead of redeclaring it.
#[cfg(test)]
pub mod test_fields {
    pub use super::UNREACHABLE;
    use aether_kinds::{MovementStencil, StencilOffset};

    /// Stay + the four orthogonal one-cell moves, as raw offsets.
    pub fn stencil_offsets() -> Vec<StencilOffset> {
        vec![
            StencilOffset { dx: 0, dy: 0 },
            StencilOffset { dx: 1, dy: 0 },
            StencilOffset { dx: -1, dy: 0 },
            StencilOffset { dx: 0, dy: 1 },
            StencilOffset { dx: 0, dy: -1 },
        ]
    }

    /// The same 4-way move set wrapped in a [`MovementStencil`] — the form
    /// the `ReachabilityProblem` / transform tests bundle.
    pub fn stencil_4way() -> MovementStencil {
        MovementStencil {
            offsets: stencil_offsets(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_fields::stencil_offsets as stencil_4way;
    use super::{UNREACHABLE, solve_cost_to_reach};
    use aether_kinds::StencilOffset;

    #[test]
    fn single_start_cell_seeds_t0_others_unreachable() {
        // One tick: only the seeded cell carries a value, and the tick-0
        // cost layer is never charged (the seed is the value).
        let costs = vec![5, 5, 5];
        let start = vec![0, UNREACHABLE, UNREACHABLE];
        let v = solve_cost_to_reach(3, 1, 1, &costs, &stencil_4way(), &start);
        assert_eq!(v, vec![0, UNREACHABLE, UNREACHABLE]);
    }

    #[test]
    fn accumulated_cost_spreads_with_uniform_field() {
        // 3×1 grid, uniform cost 1, start at cell 0. Hand-checked layers:
        //   t0: [0,   MAX, MAX]
        //   t1: [1,   1,   MAX]  (cell 2 has no reachable predecessor yet)
        //   t2: [2,   2,   2]
        let plane = 3;
        let ticks = 3;
        let costs = vec![1u32; plane * ticks];
        let start = vec![0, UNREACHABLE, UNREACHABLE];
        let v = solve_cost_to_reach(3, 1, ticks, &costs, &stencil_4way(), &start);
        assert_eq!(&v[0..3], &[0, UNREACHABLE, UNREACHABLE]);
        assert_eq!(&v[3..6], &[1, 1, UNREACHABLE]);
        assert_eq!(&v[6..9], &[2, 2, 2]);
    }

    #[test]
    fn nonzero_seed_offsets_the_whole_field() {
        // Seeding cell 0 at accumulated cost 10 shifts every reachable
        // value up by 10 — the seed is the initial accumulated cost.
        let plane = 3;
        let ticks = 3;
        let costs = vec![1u32; plane * ticks];
        let start = vec![10, UNREACHABLE, UNREACHABLE];
        let v = solve_cost_to_reach(3, 1, ticks, &costs, &stencil_4way(), &start);
        assert_eq!(&v[0..3], &[10, UNREACHABLE, UNREACHABLE]);
        assert_eq!(&v[3..6], &[11, 11, UNREACHABLE]);
        assert_eq!(&v[6..9], &[12, 12, 12]);
    }

    #[test]
    fn blocked_cell_blocks_propagation_through_it() {
        // Cell 1 blocked (cost MAX) every tick. It never carries a finite
        // value, and cell 2 — only reachable through cell 1 — stays
        // unreachable, while cell 0 keeps accumulating via the stay move.
        let plane = 3;
        let ticks = 3;
        let mut costs = vec![1u32; plane * ticks];
        for t in 0..ticks {
            costs[t * plane + 1] = UNREACHABLE;
        }
        let start = vec![0, UNREACHABLE, UNREACHABLE];
        let v = solve_cost_to_reach(3, 1, ticks, &costs, &stencil_4way(), &start);
        for t in 0..ticks {
            assert_eq!(v[t * plane + 1], UNREACHABLE, "blocked cell stays MAX");
            assert_eq!(
                v[t * plane + 2],
                UNREACHABLE,
                "cell behind the block is unreachable"
            );
        }
        assert_eq!(v[0], 0);
        assert_eq!(v[plane], 1);
        assert_eq!(v[2 * plane], 2);
    }

    #[test]
    fn unreachable_without_a_path_stays_max() {
        // Stay-only stencil on a 2×2 grid: a non-start cell can never be
        // reached, so it holds the sentinel for the whole horizon.
        let stay = vec![StencilOffset { dx: 0, dy: 0 }];
        let plane = 4;
        let ticks = 5;
        let costs = vec![1u32; plane * ticks];
        let start = vec![0, UNREACHABLE, UNREACHABLE, UNREACHABLE];
        let v = solve_cost_to_reach(2, 2, ticks, &costs, &stay, &start);
        for t in 0..ticks {
            let expected = u32::try_from(t).expect("tick index fits u32");
            assert_eq!(v[t * plane], expected, "stay cell accumulates each tick");
            for cell in 1..plane {
                assert_eq!(v[t * plane + cell], UNREACHABLE, "no path -> stays MAX");
            }
        }
    }
}

#[cfg(test)]
mod population_tests {
    use super::test_fields::stencil_offsets as stencil_4way;
    use super::{UNREACHABLE, solve_cost_to_reach, solve_population_sweep};
    use aether_kinds::{MovementStencil, PopulationSweepProblem, ReachabilityProblem, ScalarField};

    /// Build a sweep problem from a single start cell (the rest of the
    /// start region is the sentinel, so the population is sampled entirely
    /// from `start_cell`).
    #[allow(clippy::too_many_arguments)]
    fn single_start_problem(
        width: u32,
        height: u32,
        ticks: u32,
        values: Vec<u32>,
        start_cell: usize,
        goal: Vec<u32>,
        budget: u32,
        population: u32,
        window: u32,
        seed: u64,
        delays: Vec<u32>,
    ) -> PopulationSweepProblem {
        let plane = (width * height) as usize;
        let mut start = vec![UNREACHABLE; plane];
        start[start_cell] = 0;
        PopulationSweepProblem {
            problem: ReachabilityProblem {
                cost: ScalarField {
                    width,
                    height,
                    ticks,
                    values,
                },
                stencil: MovementStencil {
                    offsets: stencil_4way(),
                },
                start,
            },
            goal,
            budget,
            population,
            window,
            seed,
            delays,
        }
    }

    /// The exact certificate: the minimum cost to reach any goal cell from
    /// `start_cell` over the full horizon (#1857's machinery — a single-
    /// source `solve_cost_to_reach` plus a goal-region min readout).
    fn certificate_min(
        width: usize,
        height: usize,
        ticks: usize,
        values: &[u32],
        start_cell: usize,
        goal: &[u32],
    ) -> u32 {
        let plane = width * height;
        let mut seed = vec![UNREACHABLE; plane];
        seed[start_cell] = 0;
        let v = solve_cost_to_reach(width, height, ticks, values, &stencil_4way(), &seed);
        let mut best = UNREACHABLE;
        for t in 0..ticks {
            for &g in goal {
                best = best.min(v[t * plane + g as usize]);
            }
        }
        best
    }

    #[test]
    fn zero_delay_full_window_matches_exact_reachable_under_budget() {
        // 3×1 uniform-cost field, start at cell 0, goal = {cell 2}. The
        // exact min cost to reach the goal is 2 (cell 2 first reachable at
        // t = 2). A full-window (W >= ticks), zero-lag population must
        // finish iff the budget strictly clears that certificate — and a
        // d = 0 wipe against a *reachable* certificate is exactly the
        // planner gap this guards.
        let values = vec![1u32; 9];
        let goal = vec![2u32];
        let cert = certificate_min(3, 1, 3, &values, 0, &goal);
        assert_eq!(cert, 2, "hand-checked exact cost-to-reach the goal");

        // budget 5 > cert -> reachable -> the whole population finishes.
        let reachable = solve_population_sweep(single_start_problem(
            3,
            1,
            3,
            values.clone(),
            0,
            goal.clone(),
            5,
            8,
            8,
            0xA17E,
            vec![0],
        ));
        assert_eq!(reachable.population, 8);
        assert_eq!(
            reachable.samples[0].finished, 8,
            "d=0 must finish from a reachable-under-budget start (planner-gap guard)"
        );

        // budget == cert -> strict `< budget` fails -> nobody finishes.
        let at_threshold = solve_population_sweep(single_start_problem(
            3,
            1,
            3,
            values,
            0,
            goal,
            2,
            8,
            8,
            0xA17E,
            vec![0],
        ));
        assert_eq!(at_threshold.samples[0].finished, 0);
    }

    #[test]
    fn survival_is_non_increasing_across_a_delay_sweep() {
        // A false-corridor field on a 2×3 grid. The left column cell (0,1)
        // (index 2) is open at t = 0 but blocks at every later tick; the
        // right column stays open. A zero-lag agent sees the future block
        // and routes right to the goal row; a lagged agent reacts to the
        // stale (open) left corridor, steps onto (0,1), and dies when the
        // true field has it blocked. So survival is 1 at d = 0 and 0 once
        // the lag clouds the block — strictly non-increasing.
        let width = 2u32;
        let height = 3u32;
        let ticks = 4u32;
        let plane = (width * height) as usize;
        let mut values = vec![1u32; plane * ticks as usize];
        for t in 1..ticks as usize {
            values[t * plane + 2] = UNREACHABLE; // (0,1) blocks from t = 1 on
        }
        // d = 0: the certificate says the goal row is reachable.
        let cert = certificate_min(2, 3, 4, &values, 0, &[4, 5]);
        assert_eq!(cert, 3, "right-column route to (1,2) costs 3");

        let curve = solve_population_sweep(single_start_problem(
            width,
            height,
            ticks,
            values,
            0,
            vec![4, 5],
            100,
            1,
            8,
            0x00C0_FFEE,
            vec![0, 1, 2, 3],
        ));
        let finished: Vec<u32> = curve.samples.iter().map(|s| s.finished).collect();
        assert_eq!(finished, vec![1, 0, 0, 0], "lag strictly hurts past d = 0");
        for pair in finished.windows(2) {
            assert!(
                pair[1] <= pair[0],
                "survival must be non-increasing in delay"
            );
        }
    }

    #[test]
    fn unreachable_certificate_yields_zero_finishers_at_every_delay() {
        // 3×1 grid with cell 1 walled off every tick; the goal (cell 2)
        // sits behind the wall, so no path reaches it at any delay — no
        // false positives.
        let mut values = vec![1u32; 9];
        for t in 0..3 {
            values[t * 3 + 1] = UNREACHABLE;
        }
        let cert = certificate_min(3, 1, 3, &values, 0, &[2]);
        assert_eq!(cert, UNREACHABLE, "goal is walled off");

        let curve = solve_population_sweep(single_start_problem(
            3,
            1,
            3,
            values,
            0,
            vec![2],
            1000,
            16,
            8,
            0xBEEF,
            vec![0, 1, 2],
        ));
        for sample in &curve.samples {
            assert_eq!(
                sample.finished, 0,
                "an unreachable certificate finishes nobody at any delay"
            );
        }
    }

    #[test]
    fn population_fraction_is_stable_under_doubling() {
        // A 5×1 grid with a wall at cell 3 and a start region {cell 0,
        // cell 4} either side of the goal (cell 2). Agents sampled at
        // cell 0 reach the goal; those at cell 4 are walled off — so the
        // survival fraction tracks the sampler's split of the region, and
        // is stable (the same fraction in expectation) when the population
        // doubles.
        let mut values = vec![1u32; 5 * 4];
        for t in 0..4 {
            values[t * 5 + 3] = UNREACHABLE; // wall at cell 3
        }
        let region_problem = |population: u32| {
            let plane = 5usize;
            let mut start = vec![UNREACHABLE; plane];
            start[0] = 0;
            start[4] = 0;
            PopulationSweepProblem {
                problem: ReachabilityProblem {
                    cost: ScalarField {
                        width: 5,
                        height: 1,
                        ticks: 4,
                        values: values.clone(),
                    },
                    stencil: MovementStencil {
                        offsets: stencil_4way(),
                    },
                    start,
                },
                goal: vec![2],
                budget: 100,
                population,
                window: 8,
                seed: 0x5EED,
                delays: vec![0],
            }
        };

        let small = solve_population_sweep(region_problem(128));
        let large = solve_population_sweep(region_problem(256));
        let frac_small = f64::from(small.samples[0].finished) / 128.0;
        let frac_large = f64::from(large.samples[0].finished) / 256.0;
        assert!(
            (0.3..0.7).contains(&frac_small),
            "half the region reaches the goal: frac_small = {frac_small}"
        );
        assert!(
            (frac_small - frac_large).abs() < 0.15,
            "doubling the population keeps the fraction stable: {frac_small} vs {frac_large}"
        );
    }

    #[test]
    fn empty_start_region_finishes_nobody() {
        // No finite start seed -> no spawn cells -> every agent fails, and
        // the curve still reports one sample per swept delay.
        let plane = 4usize;
        let problem = PopulationSweepProblem {
            problem: ReachabilityProblem {
                cost: ScalarField {
                    width: 2,
                    height: 2,
                    ticks: 3,
                    values: vec![1u32; plane * 3],
                },
                stencil: MovementStencil {
                    offsets: stencil_4way(),
                },
                start: vec![UNREACHABLE; plane],
            },
            goal: vec![3],
            budget: 100,
            population: 8,
            window: 4,
            seed: 1,
            delays: vec![0, 2],
        };
        let curve = solve_population_sweep(problem);
        assert_eq!(curve.samples.len(), 2);
        for sample in &curve.samples {
            assert_eq!(sample.finished, 0);
        }
    }
}
