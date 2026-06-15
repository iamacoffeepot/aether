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

use aether_kinds::StencilOffset;

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

#[cfg(test)]
mod tests {
    use super::{UNREACHABLE, solve_cost_to_reach};
    use aether_kinds::StencilOffset;

    /// Stay + the four orthogonal one-cell moves.
    fn stencil_4way() -> Vec<StencilOffset> {
        vec![
            StencilOffset { dx: 0, dy: 0 },
            StencilOffset { dx: 1, dy: 0 },
            StencilOffset { dx: -1, dy: 0 },
            StencilOffset { dx: 0, dy: 1 },
            StencilOffset { dx: 0, dy: -1 },
        ]
    }

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
