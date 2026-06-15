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

use crate::{
    ClosureDistribution, MovementStencil, PopulationSweepProblem, ReachabilityProblem,
    RealizationProblem, RunOutcome, ScalarField, StencilOffset, SurvivalCurve, SurvivalSample,
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

/// The shared tail of the windowed step planners (`plan_next_cell`,
/// `plan_step_realized`): seed the solver at `current`, solve the
/// cost-to-reach field over the already-built `perceived` window, find the
/// cheapest perceived goal arrival across layers `1..layers` (PRNG tie-break
/// over equal-cost arrivals), then backtrack the optimal predecessor chain to
/// layer 1 and return that next cell. Returns `current` (stay put) when no
/// goal is visible inside the window or the chain is unreconstructable. The
/// two callers differ only in how they build `perceived` before this tail.
#[allow(clippy::too_many_arguments)]
fn step_along_optimal_chain(
    width: usize,
    height: usize,
    layers: usize,
    plane: usize,
    perceived: &[u32],
    stencil: &[StencilOffset],
    goal: &[usize],
    current: usize,
    rng: &mut Xorshift64,
) -> usize {
    let mut seed = vec![UNREACHABLE; plane];
    seed[current] = 0;
    let v = solve_cost_to_reach(width, height, layers, perceived, stencil, &seed);

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

    step_along_optimal_chain(
        width, height, layers, plane, &perceived, stencil, goal, current, rng,
    )
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

/// A snapshot-placed field contribution committed at a trigger tick (issue
/// 1867, #1866's contribution model). `center` is the flat row-major cell
/// the agent occupied when it was snapshotted; `trigger_tick` is when it was
/// committed. The contribution is *active* (covers cells) only once
/// `t - trigger_tick >= lead`, and at age `a = t - trigger_tick` it covers
/// every cell within radius `cover(a) = r0 + g*a` of `center`.
#[derive(Clone, Copy)]
struct Contribution {
    center: usize,
    trigger_tick: usize,
}

/// The fixed contribution shape shared by every contribution in one run —
/// #1866's `(L, r0, g)` plus the per-cell cost and concurrency ceiling.
#[derive(Clone, Copy)]
struct ContributionModel {
    lead: usize,
    extent_initial: f32,
    growth_per_tick: f32,
    cost: u32,
    max_concurrent: usize,
}

/// Realize the cost field at tick `now`: the base field's `now` layer (the
/// last available layer for a `now` past the field's horizon) plus, summed
/// over every *active* contribution, `model.cost` on each covered cell. A
/// contribution committed at `c.trigger_tick` is active iff
/// `now - c.trigger_tick >= model.lead`; at age `a = now - c.trigger_tick` it
/// covers every cell whose Euclidean grid distance from `c.center` is
/// `<= cover(a) = r0 + g*a`. Costs saturate at the [`UNREACHABLE`] sentinel
/// (a covered blocked cell stays blocked). The scan is bounded (the covered
/// radius is clamped to the grid) and iterative — no recursion.
fn realize_field(
    width: usize,
    height: usize,
    base: &[u32],
    base_ticks: usize,
    now: usize,
    contributions: &[Contribution],
    model: &ContributionModel,
) -> Vec<u32> {
    let plane = width.saturating_mul(height);
    // Past the base horizon the field holds at its final layer (the base is
    // the static backdrop; the realized motion comes from contributions).
    let base_tick = now.min(base_ticks.saturating_sub(1));
    let base_off = base_tick.saturating_mul(plane);
    let mut field = vec![UNREACHABLE; plane];
    for (idx, cell) in field.iter_mut().enumerate() {
        *cell = base.get(base_off + idx).copied().unwrap_or(UNREACHABLE);
    }

    for c in contributions {
        let age = now.saturating_sub(c.trigger_tick);
        if age < model.lead {
            continue;
        }
        // cover(age) = r0 + g*age, in grid-cell radii. Clamp to a sane
        // non-negative radius and to the grid extent so the box scan stays
        // bounded. age -> f32 / grid-extent -> f32 precision loss is
        // intentional: covered extents and tick counts are small in practice
        // (the same convention `escapability.rs`'s bound uses).
        #[allow(clippy::cast_precision_loss)]
        let age_f = age as f32;
        let radius = model
            .growth_per_tick
            .mul_add(age_f, model.extent_initial)
            .max(0.0);
        if !radius.is_finite() {
            continue;
        }
        let radius_sq = radius * radius;
        // Integer bounding box of the disc around the center.
        #[allow(clippy::cast_precision_loss)]
        let grid_extent = width.max(height) as f32;
        let r_cells = {
            // ceil >= 0 and finite; the cap keeps the cast in range.
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let v = radius.ceil().min(grid_extent) as usize;
            v
        };
        let cx = c.center % width;
        let cy = c.center / width;
        let x_lo = cx.saturating_sub(r_cells);
        let x_hi = (cx + r_cells).min(width.saturating_sub(1));
        let y_lo = cy.saturating_sub(r_cells);
        let y_hi = (cy + r_cells).min(height.saturating_sub(1));
        for y in y_lo..=y_hi {
            for x in x_lo..=x_hi {
                // Cell-offset -> f32 precision loss is intentional (grid
                // coordinates are small); `dx*dx + dy*dy` via `mul_add` keeps
                // the distance check accurate.
                #[allow(clippy::cast_precision_loss)]
                let dx = x as f32 - cx as f32;
                #[allow(clippy::cast_precision_loss)]
                let dy = y as f32 - cy as f32;
                if dx.mul_add(dx, dy * dy) <= radius_sq {
                    let idx = y * width + x;
                    field[idx] = field[idx].saturating_add(model.cost);
                }
            }
        }
    }

    field
}

/// Plan one step over the *realized* field. The realized field is only
/// known at the current tick (future contributions depend on future steps),
/// so the planner projects the tick-`now` realization constant across its
/// `window` look-ahead and runs #1857's exact [`solve_cost_to_reach`] over
/// that — the same machinery #1863's `plan_next_cell` uses, here over a
/// realized rather than a fixed field. Returns the first cell of the optimal
/// path to the cheapest perceived goal, or `current` (stay put) when no goal
/// is visible. The closed feedback loop is what makes the field
/// path-dependent; the planner stays the exact solver so closure is
/// structural rather than a heuristic artifact.
#[allow(clippy::too_many_arguments)]
fn plan_step_realized(
    width: usize,
    height: usize,
    realized: &[u32],
    stencil: &[StencilOffset],
    goal: &[usize],
    current: usize,
    window: usize,
    rng: &mut Xorshift64,
) -> usize {
    let plane = width.saturating_mul(height);
    if plane == 0 {
        return current;
    }
    // The look-ahead is the realized field repeated across `layers` layers
    // (window depth, at least 2 so there is a step to take).
    let layers = window.max(1) + 1;
    let mut perceived = vec![UNREACHABLE; plane * layers];
    for k in 0..layers {
        let dst = k * plane;
        perceived[dst..dst + plane].copy_from_slice(&realized[..plane]);
    }

    step_along_optimal_chain(
        width, height, layers, plane, &perceived, stencil, goal, current, rng,
    )
}

/// Whether the realized field has **closed around** the agent at `current`:
/// no stencil neighbor (the moves out of `current`, the zero "stay" offset
/// excluded) is both un-blocked and keeps the accumulated cost `< budget`.
/// This is the window-independent closure signal — the agent's own trail has
/// covered every feasible next cell. `acc` is the cost already paid; a
/// neighbor is feasible iff its realized cost is finite and
/// `acc + cost < budget`.
fn field_closed(
    width: usize,
    height: usize,
    realized: &[u32],
    stencil: &[StencilOffset],
    current: usize,
    acc: u32,
    budget: u32,
) -> bool {
    let plane = width.saturating_mul(height);
    let x = current % width;
    let y = current / width;
    for offset in stencil {
        if offset.dx == 0 && offset.dy == 0 {
            continue;
        }
        // The stencil reads predecessors as `c - offset`; a *successor* is
        // `c + offset`, so step the coordinate forward by the offset.
        let Some(nx) = forward_coord(x, offset.dx, width) else {
            continue;
        };
        let Some(ny) = forward_coord(y, offset.dy, height) else {
            continue;
        };
        let neighbor = ny * width + nx;
        if neighbor >= plane {
            continue;
        }
        let cost = realized.get(neighbor).copied().unwrap_or(UNREACHABLE);
        if cost == UNREACHABLE {
            continue;
        }
        if acc.saturating_add(cost) < budget {
            return false;
        }
    }
    true
}

/// Shift `coord` by `+delta` (the successor direction) and keep it in
/// `0..bound`; the forward complement of [`predecessor_coord`].
fn forward_coord(coord: usize, delta: i32, bound: usize) -> Option<usize> {
    let magnitude = delta.unsigned_abs() as usize;
    let next = if delta >= 0 {
        coord.checked_add(magnitude)?
    } else {
        coord.checked_sub(magnitude)?
    };
    (next < bound).then_some(next)
}

/// The mutable state one self-realizing run carries across ticks: the agent's
/// current cell, the cost accumulated so far, and the active snapshot
/// contributions. Shared by both run drivers — [`simulate_run`] (the closure /
/// finish verdict) and [`realize_single`] (the per-tick stacked field) — so the
/// one closed-loop step lives in exactly one place ([`step_realized_run`]).
struct RunState {
    current: usize,
    acc: u32,
    contributions: Vec<Contribution>,
}

/// The outcome of one [`step_realized_run`] call.
enum StepOutcome {
    /// The realized field closed around the agent before it could step — no
    /// stencil neighbor is both un-blocked and keeps `acc < budget`.
    Closed,
    /// The agent reached a goal cell with `acc < budget`.
    Reached,
    /// The agent stepped and the run continues.
    Continued,
}

/// One tick of the closed self-realizing loop, mutating `state` in place: realize
/// the field at `now`, check closure, spawn a snapshot contribution on the
/// placement cadence, plan one step via [`plan_step_realized`], then step and
/// accumulate the true realized cost (the `now + 1` realization at the landed
/// cell). Returns the realized field *at `now`* (the field the agent planned
/// against this tick, which the stacked-field driver records) and the
/// [`StepOutcome`]. This is the single per-step body both run drivers share, so
/// the closed loop is defined once. Iterative — the per-tick window solve and
/// the contribution scan are bounded; no recursion.
#[allow(clippy::too_many_arguments)]
fn step_realized_run(
    width: usize,
    height: usize,
    base: &[u32],
    base_ticks: usize,
    stencil: &[StencilOffset],
    goal: &[usize],
    budget: u32,
    placement_period: usize,
    model: &ContributionModel,
    window: usize,
    now: usize,
    rng: &mut Xorshift64,
    state: &mut RunState,
) -> (Vec<u32>, StepOutcome) {
    let is_goal = |cell: usize| goal.contains(&cell);
    let realized = realize_field(
        width,
        height,
        base,
        base_ticks,
        now,
        &state.contributions,
        model,
    );

    // Closure check before stepping: has the trail covered every feasible
    // next cell from the agent's current position this tick?
    if field_closed(
        width,
        height,
        &realized,
        stencil,
        state.current,
        state.acc,
        budget,
    ) {
        return (realized, StepOutcome::Closed);
    }

    // Spawn a snapshot contribution on the current cell at the placement
    // cadence (snapshot placement + lead, #1866's model), holding at most
    // `max_concurrent` active — the oldest expires.
    if placement_period > 0 && now.is_multiple_of(placement_period) {
        state.contributions.push(Contribution {
            center: state.current,
            trigger_tick: now,
        });
        if model.max_concurrent > 0 && state.contributions.len() > model.max_concurrent {
            state.contributions.remove(0);
        }
    }

    let next = plan_step_realized(
        width,
        height,
        &realized,
        stencil,
        goal,
        state.current,
        window,
        rng,
    );
    // The realized cost actually paid is the next-tick realization at the
    // landed cell (the contribution set the agent stepped into).
    let landed_realized = realize_field(
        width,
        height,
        base,
        base_ticks,
        now + 1,
        &state.contributions,
        model,
    );
    let true_cost = landed_realized.get(next).copied().unwrap_or(UNREACHABLE);
    state.acc = state.acc.saturating_add(true_cost);
    state.current = next;
    let outcome = if state.acc < budget && is_goal(state.current) {
        StepOutcome::Reached
    } else {
        StepOutcome::Continued
    };
    (realized, outcome)
}

/// Simulate one self-realizing run from `start` (issue 1867). Per tick the
/// closed loop is: realize the field (base plus active contributions) →
/// plan one step via [`plan_step_realized`] → step and accumulate the true
/// realized cost → spawn a snapshot contribution every `placement_period`
/// ticks → advance. Returns `(finished, closure_tick)`: `finished` iff the
/// agent reaches a goal cell under budget before the final tick;
/// `closure_tick` is the first tick the realized field closed around it
/// (`u32::MAX` if it never closed). Iterative throughout — the tick loop,
/// the per-tick window solve, and the contribution scan are all bounded.
#[allow(clippy::too_many_arguments)]
fn simulate_run(
    width: usize,
    height: usize,
    ticks: usize,
    base: &[u32],
    base_ticks: usize,
    stencil: &[StencilOffset],
    goal: &[usize],
    start: usize,
    start_cost: u32,
    budget: u32,
    placement_period: usize,
    model: &ContributionModel,
    window: usize,
    rng: &mut Xorshift64,
) -> (bool, u32) {
    let mut state = RunState {
        current: start,
        acc: start_cost,
        contributions: Vec::new(),
    };

    if state.acc < budget && goal.contains(&state.current) {
        return (true, u32::MAX);
    }

    for now in 0..ticks.saturating_sub(1) {
        let (_realized, outcome) = step_realized_run(
            width,
            height,
            base,
            base_ticks,
            stencil,
            goal,
            budget,
            placement_period,
            model,
            window,
            now,
            rng,
            &mut state,
        );
        match outcome {
            // `now < ticks`, and `ticks` came from a `u32` field, so the cast
            // is exact; saturate defensively.
            StepOutcome::Closed => return (false, u32::try_from(now).unwrap_or(u32::MAX)),
            StepOutcome::Reached => return (true, u32::MAX),
            StepOutcome::Continued => {}
        }
    }
    (false, u32::MAX)
}

/// Run a seeded distribution of self-realizing field simulations and emit
/// the closure-outcome distribution (issue 1867). Each run is the closed
/// feedback loop where the agent's own motion spawns the contributions it
/// then plans against, so the realized field is path-dependent and distinct
/// per seed. The whole sweep is a pure function of `input` (seed + field in,
/// distribution out): the PRNG samples each run's start cell from the start
/// region and breaks the planner's equal-cost ties, so a given input replays
/// byte-identically (ADR-0048 §4/§130). Iterative throughout (the per-run
/// tick loop, the per-tick window solve, and the contribution-realization
/// scan are all bounded; no recursion, per the load-bearing-code rule).
pub fn simulate_realization(input: RealizationProblem) -> ClosureDistribution {
    let (
        width,
        height,
        base_ticks,
        base,
        stencil,
        start_seed,
        goal,
        budget,
        placement_period,
        model,
        window,
        runs,
        seed,
    ) = unpack_realization(input);
    let plane = width.saturating_mul(height);

    let start_cells: Vec<usize> = (0..plane)
        .filter(|&c| start_seed.get(c).copied().unwrap_or(UNREACHABLE) != UNREACHABLE)
        .collect();

    // The simulated horizon: run for the base field's full tick span (or at
    // least a couple of ticks so the loop has room when the base is a single
    // static layer).
    let ticks = base_ticks.max(2);

    let mut samples: Vec<RunOutcome> = Vec::with_capacity(runs as usize);
    let mut closed: u32 = 0;
    for run in 0..runs {
        // Per-run start cell, drawn from a per-run mixed seed so each run is
        // an independent sample of the start region (a no-op start region
        // yields no run).
        let mut run_rng = Xorshift64::seeded(agent_seed(seed, u64::from(run) + 1));
        let start = if start_cells.is_empty() {
            0
        } else {
            start_cells[run_rng.next_bounded(start_cells.len())]
        };
        let start_cost = start_seed.get(start).copied().unwrap_or(0);

        let (finished, closure_tick) = if start_cells.is_empty() {
            (false, u32::MAX)
        } else {
            simulate_run(
                width,
                height,
                ticks,
                &base,
                base_ticks,
                &stencil,
                &goal,
                start,
                start_cost,
                budget,
                placement_period,
                &model,
                window,
                &mut run_rng,
            )
        };
        if closure_tick != u32::MAX {
            closed += 1;
        }
        samples.push(RunOutcome {
            run,
            finished,
            closure_tick,
        });
    }

    ClosureDistribution {
        runs,
        closed,
        samples,
    }
}

/// Replay a single run of the self-realizing simulation (the `input`'s seed,
/// run index `0`) and emit its **realized field** as a [`ScalarField`] — the
/// inspection path for the headline counts-only [`ClosureDistribution`]
/// (issue 1867). Leaning on the determinism contract (same seed → same
/// realized field), the realized field of any run is recoverable by
/// replaying that seed here, so the distribution output stays tiny. The
/// emitted field stacks every tick's realization into one `(tick, y, x)`
/// `ScalarField`: layer `t` is the realized cost field the run planned
/// against at tick `t`, so a consumer reads the path-dependent trail
/// accumulating over time. Iterative throughout.
pub fn realize_single(input: RealizationProblem) -> ScalarField {
    let (
        width,
        height,
        base_ticks,
        base,
        stencil,
        start_seed,
        goal,
        budget,
        placement_period,
        model,
        window,
        _runs,
        seed,
    ) = unpack_realization(input);
    let plane = width.saturating_mul(height);
    let ticks = base_ticks.max(2);

    let start_cells: Vec<usize> = (0..plane)
        .filter(|&c| start_seed.get(c).copied().unwrap_or(UNREACHABLE) != UNREACHABLE)
        .collect();

    // Replay run 0 exactly as `simulate_realization` does, recording the
    // realized field at every tick into the stacked output.
    let mut run_rng = Xorshift64::seeded(agent_seed(seed, 1));
    let start = if start_cells.is_empty() {
        0
    } else {
        start_cells[run_rng.next_bounded(start_cells.len())]
    };
    let mut state = RunState {
        current: start,
        acc: start_seed.get(start).copied().unwrap_or(0),
        contributions: Vec::new(),
    };

    let out_ticks = ticks.max(1);
    let mut values = vec![UNREACHABLE; plane.saturating_mul(out_ticks)];

    // Once the run stops stepping (goal reached, field closed, or no start
    // region) the contributions freeze, but the stacked field keeps recording
    // the still-aging realized field — so the loop continues to the horizon,
    // calling the shared step body only while the run is still live.
    let mut done = (state.acc < budget && goal.contains(&state.current)) || start_cells.is_empty();

    for now in 0..out_ticks {
        let active = !done && now + 1 < out_ticks;
        let realized = if active {
            let (realized, outcome) = step_realized_run(
                width,
                height,
                &base,
                base_ticks,
                &stencil,
                &goal,
                budget,
                placement_period,
                &model,
                window,
                now,
                &mut run_rng,
                &mut state,
            );
            if matches!(outcome, StepOutcome::Closed | StepOutcome::Reached) {
                done = true;
            }
            realized
        } else {
            // Frozen / final tick: just record the realization at `now`; the
            // contribution set no longer changes.
            realize_field(
                width,
                height,
                &base,
                base_ticks,
                now,
                &state.contributions,
                &model,
            )
        };
        let dst = now.saturating_mul(plane);
        if let Some(slot) = values.get_mut(dst..dst + plane) {
            slot.copy_from_slice(&realized[..plane]);
        }
    }

    // `width` / `height` round-trip from the input `u32` field dimensions;
    // `out_ticks` is the simulated horizon. All fit `u32`; saturate
    // defensively rather than truncate.
    ScalarField {
        width: u32::try_from(width).unwrap_or(u32::MAX),
        height: u32::try_from(height).unwrap_or(u32::MAX),
        ticks: u32::try_from(out_ticks).unwrap_or(u32::MAX),
        values,
    }
}

/// Unpack a [`RealizationProblem`] into the `usize`-typed simulation
/// parameters both [`simulate_realization`] and [`realize_single`] consume,
/// so the two replay-equivalent loops decode the input identically.
#[allow(clippy::type_complexity)]
fn unpack_realization(
    input: RealizationProblem,
) -> (
    usize,
    usize,
    usize,
    Vec<u32>,
    Vec<StencilOffset>,
    Vec<u32>,
    Vec<usize>,
    u32,
    usize,
    ContributionModel,
    usize,
    u32,
    u64,
) {
    let RealizationProblem {
        problem:
            ReachabilityProblem {
                cost:
                    ScalarField {
                        width,
                        height,
                        ticks,
                        values: base,
                    },
                stencil: MovementStencil { offsets: stencil },
                start: start_seed,
            },
        goal: goal_indices,
        budget,
        placement_period,
        lead_ticks,
        covered_extent_initial,
        covered_growth_per_tick,
        contribution_cost,
        max_concurrent,
        window,
        runs,
        seed,
    } = input;
    let width = width as usize;
    let height = height as usize;
    let base_ticks = ticks as usize;
    let plane = width.saturating_mul(height);
    let goal: Vec<usize> = goal_indices
        .iter()
        .map(|&g| g as usize)
        .filter(|&g| g < plane)
        .collect();
    let model = ContributionModel {
        lead: lead_ticks as usize,
        extent_initial: covered_extent_initial,
        growth_per_tick: covered_growth_per_tick,
        cost: contribution_cost,
        max_concurrent: max_concurrent as usize,
    };
    (
        width,
        height,
        base_ticks,
        base,
        stencil,
        start_seed,
        goal,
        budget,
        placement_period as usize,
        model,
        window as usize,
        runs,
        seed,
    )
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
    use crate::{MovementStencil, StencilOffset};

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
    use crate::StencilOffset;

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
    use crate::{MovementStencil, PopulationSweepProblem, ReachabilityProblem, ScalarField};

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

#[cfg(test)]
mod realization_tests {
    use super::test_fields::stencil_4way;
    use super::{UNREACHABLE, realize_single, simulate_realization};
    use crate::{MovementStencil, ReachabilityProblem, RealizationProblem, ScalarField};

    /// A `RealizationProblem` over a `width × height` base field with the
    /// 4-way stencil, the start region the finite cells of `start`, and the
    /// contribution model / sweep parameters passed in. The base field is a
    /// single static layer repeated implicitly past its horizon, so `ticks`
    /// here is the simulated tick span.
    #[allow(clippy::too_many_arguments)]
    fn problem(
        width: u32,
        height: u32,
        ticks: u32,
        values: Vec<u32>,
        start: Vec<u32>,
        goal: Vec<u32>,
        budget: u32,
        placement_period: u32,
        lead_ticks: u32,
        covered_extent_initial: f32,
        covered_growth_per_tick: f32,
        contribution_cost: u32,
        max_concurrent: u32,
        window: u32,
        runs: u32,
        seed: u64,
    ) -> RealizationProblem {
        RealizationProblem {
            problem: ReachabilityProblem {
                cost: ScalarField {
                    width,
                    height,
                    ticks,
                    values,
                },
                stencil: stencil_4way(),
                start,
            },
            goal,
            budget,
            placement_period,
            lead_ticks,
            covered_extent_initial,
            covered_growth_per_tick,
            contribution_cost,
            max_concurrent,
            window,
            runs,
            seed,
        }
    }

    #[test]
    fn no_contributions_reduces_to_plain_receding_horizon() {
        // With placement disabled (period 0) the realized field is just the
        // base field, so the loop degenerates to #1863's fixed-field
        // receding-horizon agent: a 5×1 uniform-cost corridor, start cell 0,
        // goal cell 4, ample budget — every run finishes, none closes.
        let p = problem(
            5,
            1,
            6,
            vec![1u32; 5 * 6],
            vec![0, UNREACHABLE, UNREACHABLE, UNREACHABLE, UNREACHABLE],
            vec![4],
            100,
            0, // placement_period 0 → no contributions
            0,
            0.0,
            0.0,
            0,
            0,
            8,
            8,
            0xC0FF_EE00,
        );
        let dist = simulate_realization(p);
        assert_eq!(dist.runs, 8);
        assert_eq!(dist.closed, 0);
        assert_eq!(dist.samples.len(), 8);
        assert!(dist.samples.iter().all(|s| s.finished));
        assert!(dist.samples.iter().all(|s| s.closure_tick == u32::MAX));
    }

    #[test]
    fn trail_closes_around_the_agent() {
        // A 3×1 corridor whose forward cell (2) is base-blocked: the agent
        // can only step 0 → 1, and its own snapshot contribution on cell 0
        // (cost far over budget, active immediately) covers the one feasible
        // way back — the realized field closes around it.
        //
        // Trace: tick 0 at cell 0, neighbor cell 1 is free → step + spawn a
        // contribution on cell 0. Tick 1 at cell 1: cell 2 is blocked, and
        // realized cell 0 = base 1 + 100 = 101, so acc(1) + 101 >= budget 10
        // — every feasible neighbor is over budget → closed at tick 1.
        let p = problem(
            3,
            1,
            4,
            vec![
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
            vec![0, UNREACHABLE, UNREACHABLE],
            vec![2],
            10,
            1, // spawn every tick
            0, // active immediately
            0.0,
            0.0,
            100, // contribution cost far over budget
            4,
            8,
            5,
            0x1111_2222,
        );
        let dist = simulate_realization(p);
        assert_eq!(dist.runs, 5);
        // Single start cell → every run is identical and closes.
        assert_eq!(dist.closed, 5);
        assert!(dist.samples.iter().all(|s| !s.finished));
        assert!(dist.samples.iter().all(|s| s.closure_tick == 1));
    }

    #[test]
    fn agent_always_escapes_when_lead_outpaces_cover() {
        // A 5×1 corridor with a long lead and zero-growth, zero-extent
        // contributions: each contribution only ever covers its own center
        // cell and not until well after the agent has left it, so the trail
        // never closes around a moving agent — no run closes, every run
        // reaches the goal under budget.
        let p = problem(
            5,
            1,
            8,
            vec![1u32; 5 * 8],
            vec![0, UNREACHABLE, UNREACHABLE, UNREACHABLE, UNREACHABLE],
            vec![4],
            100,
            1,
            6,   // lead far longer than the walk
            0.0, // zero extent
            0.0, // zero growth — covers only the center cell
            50,
            8,
            8,
            6,
            0x9999_8888,
        );
        let dist = simulate_realization(p);
        assert_eq!(dist.closed, 0);
        assert!(dist.samples.iter().all(|s| s.finished));
    }

    #[test]
    fn closure_fraction_is_stable_under_doubled_runs() {
        // Doubling the run count over a multi-cell start region leaves the
        // closure fraction stable (here exactly 0 — the field is escapable):
        // the per-run sampling is independent and the headline rate is a
        // property of the field, not the run count.
        let start = {
            let mut s = vec![UNREACHABLE; 5];
            s[0] = 0;
            s[1] = 0;
            s
        };
        let base = problem(
            5,
            1,
            8,
            vec![1u32; 5 * 8],
            start,
            vec![4],
            100,
            2,
            6,
            0.0,
            0.0,
            50,
            8,
            8,
            16,
            0x4242_4242,
        );
        let small = simulate_realization(base.clone());
        let mut doubled = base;
        doubled.runs = 32;
        let large = simulate_realization(doubled);
        assert_eq!(small.closed, 0);
        assert_eq!(large.closed, 0);
        assert_eq!(small.runs, 16);
        assert_eq!(large.runs, 32);
    }

    #[test]
    fn realize_single_emits_a_stacked_realized_field() {
        // The companion readout stacks each tick's realized field into a
        // `(tick, y, x)` `ScalarField` over the simulated horizon, so a
        // consumer reads the path-dependent trail accumulating over time.
        let p = problem(
            3,
            1,
            4,
            vec![1u32; 3 * 4],
            vec![0, UNREACHABLE, UNREACHABLE],
            vec![2],
            100,
            1,
            0,
            0.0,
            0.0,
            7,
            4,
            8,
            1,
            0x7777_0000,
        );
        let field = realize_single(p);
        assert_eq!(field.width, 3);
        assert_eq!(field.height, 1);
        assert_eq!(field.ticks, 4);
        assert_eq!(field.values.len(), 3 * 4);
        // Tick 0 is the pristine base field (no contribution active yet).
        assert_eq!(&field.values[0..3], &[1, 1, 1]);
        // By a later tick the agent's own trail has raised at least one cell
        // above the base cost — the path-dependent realization is visible.
        assert!(field.values.iter().any(|&v| v > 1 && v != UNREACHABLE));
    }

    #[test]
    fn empty_start_region_yields_no_closures() {
        // No finite start cell → no run can spawn; every run is a non-finish,
        // non-closure outcome (a degenerate but well-defined distribution).
        let p = problem(
            3,
            1,
            4,
            vec![1u32; 3 * 4],
            vec![UNREACHABLE, UNREACHABLE, UNREACHABLE],
            vec![2],
            10,
            1,
            0,
            0.0,
            0.0,
            100,
            4,
            8,
            4,
            0xDEAD_BEEF,
        );
        let dist = simulate_realization(p);
        assert_eq!(dist.closed, 0);
        assert!(dist.samples.iter().all(|s| !s.finished));
        assert!(dist.samples.iter().all(|s| s.closure_tick == u32::MAX));
    }

    #[test]
    fn stencil_unused_warning_guard() {
        // Touch the wrapped-stencil constructor so the import path matches the
        // sibling test modules (the realized-field tests bundle the 4-way set
        // through `problem`).
        let s: MovementStencil = stencil_4way();
        assert_eq!(s.offsets.len(), 5);
    }
}
