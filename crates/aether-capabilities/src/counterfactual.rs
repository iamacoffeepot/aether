//! Counterfactual reachability-from-state query over a recorded path
//! (issue 1864). The pure core the `solve_counterfactual` transform
//! (ADR-0048) wraps, kept beside the reachability solver and the corridor
//! graph so it reuses [`solve_cost_to_reach`] directly rather than going
//! through the transform encode/decode boundary.
//!
//! Given a recorded path through a time-varying cost field (a
//! [`TrajectoryLog`], issue 1862) and a tick where its accumulator crossed
//! a budget `B`, the query re-runs the reachability solver from the path's
//! actual `(position, accumulated-cost)` state `W` ticks before the
//! crossing, restricted to the field visible in that window, and reports
//! whether a within-budget continuation existed. Each budget-crossing is
//! classified avoidable (a continuation existed) or unavoidable (none did).
//!
//! Per crossing the query: (1) detects the crossing — the first sample
//! whose accumulator reaches `B` (`prev.value < B` → `cur.value >= B`, plus
//! a first-sample-already-over crossing at the path start); (2) extracts
//! the path's actual state at the decision tick `t0 = crossing_tick − W`
//! (clamped to the first recorded tick) — the latest sample at or before
//! `t0`; (3) slices the cost field over the visible window `[t0, tc]`
//! inclusive, seeds the single cell `(x0, y0)` at the cost-valued seed `v0`,
//! and runs one full-lookahead [`solve_cost_to_reach`] over the slice; and
//! (4) reads the budget verdict — `best = min over cells of Vloc(·, tc)`,
//! `avoidable = best < B`, `margin = budget − best`. Iterative throughout
//! (a per-crossing loop, a per-window dense solve; no recursion, per the
//! load-bearing-code rule), so a given field + path replays byte-identically.

use aether_kinds::{
    CrossingClassification, CrossingVerdict, ScalarField, StencilOffset, TrajectoryLog,
};

use crate::reachability::{UNREACHABLE, solve_cost_to_reach};

/// The path's actual state at a decision tick — the seed of one windowed
/// re-solve. `(x, y)` is the grid cell the path occupied; `cost` is the
/// accumulated field cost there, in the same `u32` currency as `V` and the
/// budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SeedState {
    decision_tick: u32,
    x: u32,
    y: u32,
    cost: u32,
}

/// One detected budget-crossing: the tick at which the path's accumulator
/// first reached the budget, paired with the seed state at its decision
/// tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Crossing {
    crossing_tick: u32,
    seed: SeedState,
}

/// Detect every budget-crossing in a recorded path and, for each, extract
/// the path's actual state at its decision tick `t0 = crossing_tick −
/// window` (clamped to the first recorded tick).
///
/// A crossing is the first sample whose accumulator reaches the budget: a
/// `prev.value < budget` → `cur.value >= budget` transition, plus a
/// first-sample-already-`>= budget` crossing at the path start. The samples
/// are consumed in recorded order; a well-behaved producer records them in
/// ascending-tick order, and the state lookup tolerates gaps and a path
/// that stops short (truncated / aborted sessions, issue 1862).
fn detect_crossings(path: &TrajectoryLog, window: u32, budget: u32) -> Vec<Crossing> {
    let samples = &path.samples;
    let mut crossings = Vec::new();
    if samples.is_empty() {
        return crossings;
    }

    let first_tick = samples[0].tick;
    for (i, cur) in samples.iter().enumerate() {
        let crosses = if i == 0 {
            // A first sample already at or over the budget is a crossing at
            // the path start.
            cur.value >= budget
        } else {
            samples[i - 1].value < budget && cur.value >= budget
        };
        if !crosses {
            continue;
        }
        let crossing_tick = cur.tick;
        // Decision tick = crossing tick − window, clamped to the first
        // recorded tick so a short path (or `tc < window`) seeds from the
        // path start rather than a tick the path never visited.
        let decision_tick = crossing_tick.saturating_sub(window).max(first_tick);
        let seed = seed_state_at(samples, decision_tick);
        crossings.push(Crossing {
            crossing_tick,
            seed,
        });
    }
    crossings
}

/// The path's state at `decision_tick`: the latest sample at or before it.
/// Walks the recorded order and keeps the last sample whose tick does not
/// exceed `decision_tick`, so it tolerates non-contiguous ticks and a path
/// that ends before some computed decision tick. Falls back to the first
/// sample when none precedes the decision tick (the caller clamps
/// `decision_tick` to the first recorded tick, so this is a safety net for
/// out-of-order producers).
fn seed_state_at(samples: &[aether_kinds::TrajectorySampleEntry], decision_tick: u32) -> SeedState {
    let mut chosen = &samples[0];
    for sample in samples {
        if sample.tick <= decision_tick {
            chosen = sample;
        }
    }
    SeedState {
        decision_tick,
        x: chosen.x,
        y: chosen.y,
        cost: chosen.value,
    }
}

/// Run one windowed re-solve from a crossing's seed state and read the
/// budget verdict. Slices `cost` over the visible window `[t0, tc]`
/// inclusive (the `C[·, t .. t+W]` convention with `t = t0`, `t+W = tc`),
/// seeds the single cell `(x0, y0)` at the cost-valued seed `v0`, runs a
/// full-lookahead [`solve_cost_to_reach`] over the slice, and takes `best =
/// min over cells of Vloc(·, tc)` (the slice's last layer).
///
/// `avoidable = best < budget`; `margin = budget − best`. An out-of-range
/// seed cell, a window past the field's tick range, or an empty slice all
/// read as unreachable (`best = u32::MAX`), so the verdict is well-defined
/// for a truncated path or a clamped window.
fn classify_crossing(
    cost: &ScalarField,
    stencil: &[StencilOffset],
    crossing: Crossing,
    budget: u32,
) -> CrossingVerdict {
    let width = cost.width as usize;
    let height = cost.height as usize;
    let plane = width.saturating_mul(height);
    let field_ticks = cost.ticks as usize;
    let t0 = crossing.seed.decision_tick as usize;
    let tc = crossing.crossing_tick as usize;

    // Window length in tick layers: `[t0, tc]` inclusive. A seed beyond the
    // crossing (only possible if the producer recorded out of order) or a
    // window that starts past the field collapses to an unreachable verdict.
    let best = if plane == 0 || tc < t0 || t0 >= field_ticks {
        UNREACHABLE
    } else {
        let window_ticks = tc - t0 + 1;
        // Slice the cost field's `[t0, tc]` layers. A slice that runs past
        // the field's recorded ticks reads as UNREACHABLE past its end (the
        // solver tolerates a short `costs`), so clamp the copy and let the
        // tail default.
        let slice_start = t0.saturating_mul(plane);
        let available_ticks = field_ticks - t0;
        let copy_ticks = window_ticks.min(available_ticks);
        let costs_slice = cost
            .values
            .get(slice_start..slice_start.saturating_add(copy_ticks.saturating_mul(plane)))
            .unwrap_or(&[]);

        // Cost-valued seed slice for the window's t = 0 layer: the single
        // cell `(x0, y0)` at the accumulated cost `v0`, sentinel elsewhere.
        let mut seed = vec![UNREACHABLE; plane];
        let sx = crossing.seed.x as usize;
        let sy = crossing.seed.y as usize;
        if sx < width && sy < height {
            seed[sy * width + sx] = crossing.seed.cost;
        }

        let vloc = solve_cost_to_reach(width, height, window_ticks, costs_slice, stencil, &seed);
        // `best = min over cells of Vloc(·, tc)` — the window's last layer.
        let last_base = (window_ticks - 1).saturating_mul(plane);
        vloc.get(last_base..last_base.saturating_add(plane))
            .and_then(|layer| layer.iter().copied().min())
            .unwrap_or(UNREACHABLE)
    };

    let avoidable = best < budget;
    let margin = i64::from(budget) - i64::from(best);
    CrossingVerdict {
        crossing_tick: crossing.crossing_tick,
        decision_tick: crossing.seed.decision_tick,
        seed_x: crossing.seed.x,
        seed_y: crossing.seed.y,
        seed_cost: crossing.seed.cost,
        avoidable,
        best_continuation_cost: best,
        margin,
    }
}

/// Classify every budget-crossing in a recorded path against a cost field
/// (issue 1864) — the pure core the `solve_counterfactual` transform wraps.
/// Detects the crossings, extracts each crossing's seed state at its
/// decision tick, runs a windowed re-solve from that state, and emits one
/// [`CrossingVerdict`] per crossing in tick order. A path with no crossing
/// yields an empty classification.
pub fn solve_counterfactual_core(
    cost: &ScalarField,
    stencil: &[StencilOffset],
    path: &TrajectoryLog,
    window: u32,
    budget: u32,
) -> CrossingClassification {
    let crossings = detect_crossings(path, window, budget);
    let mut verdicts = Vec::with_capacity(crossings.len());
    for crossing in crossings {
        verdicts.push(classify_crossing(cost, stencil, crossing, budget));
    }
    CrossingClassification {
        crossings: verdicts,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Crossing, SeedState, classify_crossing, detect_crossings, solve_counterfactual_core,
    };
    use aether_kinds::{
        ScalarField, StencilOffset, TrajectoryEndReason, TrajectoryLog, TrajectorySampleEntry,
    };

    const UNREACHABLE: u32 = u32::MAX;

    fn stencil_4way() -> Vec<StencilOffset> {
        vec![
            StencilOffset { dx: 0, dy: 0 },
            StencilOffset { dx: 1, dy: 0 },
            StencilOffset { dx: -1, dy: 0 },
            StencilOffset { dx: 0, dy: 1 },
            StencilOffset { dx: 0, dy: -1 },
        ]
    }

    fn sample(tick: u32, x: u32, y: u32, value: u32) -> TrajectorySampleEntry {
        TrajectorySampleEntry { tick, x, y, value }
    }

    fn log(samples: Vec<TrajectorySampleEntry>) -> TrajectoryLog {
        TrajectoryLog {
            seed: 1,
            samples,
            end_reason: TrajectoryEndReason::Completed,
        }
    }

    #[test]
    fn single_clean_crossing_detected_with_clamped_decision_tick() {
        // Accumulator climbs 0,2,4,6,8 over ticks 0..5; budget 5 is first
        // reached at tick 3 (value 6). Window 2 → decision tick 1.
        let path = log(vec![
            sample(0, 0, 0, 0),
            sample(1, 1, 0, 2),
            sample(2, 2, 0, 4),
            sample(3, 3, 0, 6),
            sample(4, 4, 0, 8),
        ]);
        let crossings = detect_crossings(&path, 2, 5);
        assert_eq!(crossings.len(), 1);
        assert_eq!(crossings[0].crossing_tick, 3);
        assert_eq!(
            crossings[0].seed,
            SeedState {
                decision_tick: 1,
                x: 1,
                y: 0,
                cost: 2,
            }
        );
    }

    #[test]
    fn multiple_crossings_in_one_path() {
        // Budget 5. value dips back under 5 then crosses again: the
        // `< B → >= B` operator fires once per up-crossing.
        let path = log(vec![
            sample(0, 0, 0, 0),
            sample(1, 1, 0, 6), // first crossing
            sample(2, 2, 0, 3), // back under
            sample(3, 3, 0, 4),
            sample(4, 4, 0, 7), // second crossing
        ]);
        let crossings = detect_crossings(&path, 1, 5);
        assert_eq!(crossings.len(), 2);
        assert_eq!(crossings[0].crossing_tick, 1);
        assert_eq!(crossings[1].crossing_tick, 4);
    }

    #[test]
    fn no_crossing_yields_empty() {
        // Accumulator never reaches the budget.
        let path = log(vec![
            sample(0, 0, 0, 0),
            sample(1, 1, 0, 1),
            sample(2, 2, 0, 2),
        ]);
        assert!(detect_crossings(&path, 1, 5).is_empty());
    }

    #[test]
    fn first_sample_already_over_is_a_crossing_at_path_start() {
        // First sample is already >= budget: a crossing at the path start,
        // decision tick clamps to the first recorded tick.
        let path = log(vec![sample(2, 5, 5, 9), sample(3, 6, 5, 11)]);
        let crossings = detect_crossings(&path, 4, 5);
        assert_eq!(crossings.len(), 1);
        assert_eq!(crossings[0].crossing_tick, 2);
        // tc(2) − window(4) saturates to 0, then clamps to first tick 2.
        assert_eq!(crossings[0].seed.decision_tick, 2);
        assert_eq!(crossings[0].seed.cost, 9);
    }

    #[test]
    fn tc_less_than_window_clamps_decision_tick_to_path_start() {
        let path = log(vec![
            sample(0, 0, 0, 0),
            sample(1, 1, 0, 3),
            sample(2, 2, 0, 8), // crossing at tick 2
        ]);
        // window 10 ≫ tc 2 → decision tick saturates to 0, the path start.
        let crossings = detect_crossings(&path, 10, 5);
        assert_eq!(crossings.len(), 1);
        assert_eq!(crossings[0].seed.decision_tick, 0);
        assert_eq!(crossings[0].seed.cost, 0);
    }

    #[test]
    fn state_lookup_picks_latest_sample_at_or_before_decision_tick() {
        // Non-contiguous ticks: decision tick 5 has no exact sample, so the
        // lookup picks the latest at or before it (tick 4).
        let path = log(vec![
            sample(0, 0, 0, 0),
            sample(4, 4, 1, 3),
            sample(8, 8, 2, 7), // crossing at tick 8
        ]);
        let crossings = detect_crossings(&path, 3, 5);
        assert_eq!(crossings.len(), 1);
        assert_eq!(crossings[0].crossing_tick, 8);
        assert_eq!(
            crossings[0].seed,
            SeedState {
                decision_tick: 5,
                x: 4,
                y: 1,
                cost: 3,
            }
        );
    }

    /// A 3×1 uniform-cost-1 field over `ticks` layers.
    fn uniform_field(ticks: u32) -> ScalarField {
        ScalarField {
            width: 3,
            height: 1,
            ticks,
            values: vec![1u32; 3 * ticks as usize],
        }
    }

    #[test]
    fn avoidable_when_a_within_budget_continuation_exists() {
        // Seed cell 0 at accumulated cost 2, window [0, 2] (3 layers), cost
        // 1/tick. Vloc at tick 2 reaches cost 4 (2 + 1 + 1). Budget 5 → the
        // continuation is under budget, so the crossing is avoidable.
        let field = uniform_field(3);
        let crossing = Crossing {
            crossing_tick: 2,
            seed: SeedState {
                decision_tick: 0,
                x: 0,
                y: 0,
                cost: 2,
            },
        };
        let verdict = classify_crossing(&field, &stencil_4way(), crossing, 5);
        assert!(verdict.avoidable);
        assert_eq!(verdict.best_continuation_cost, 4);
        assert_eq!(verdict.margin, 1);
    }

    #[test]
    fn unavoidable_when_every_window_end_cell_is_over_budget() {
        // Same window but seed cost 4: Vloc at tick 2 is 6 everywhere, over
        // a budget of 5 — unavoidable.
        let field = uniform_field(3);
        let crossing = Crossing {
            crossing_tick: 2,
            seed: SeedState {
                decision_tick: 0,
                x: 0,
                y: 0,
                cost: 4,
            },
        };
        let verdict = classify_crossing(&field, &stencil_4way(), crossing, 5);
        assert!(!verdict.avoidable);
        assert_eq!(verdict.best_continuation_cost, 6);
        assert_eq!(verdict.margin, -1);
    }

    #[test]
    fn lookahead_boundary_flips_avoidable_with_window_depth() {
        // A field where the cheap escape lies 3 cells from the start, and a
        // tall barrier sits in between every tick. With a short window the
        // solve can't reach the cheap cell at the crossing tick; with a long
        // enough window it can, flipping the verdict.
        //
        // 5×1 grid, cost layout per tick: [1, 1, 9, 1, 1]. Seed cell 0.
        // Through-the-barrier (cell 2, cost 9) is the only orthogonal route
        // to cells 3/4, so reaching them is expensive; staying at/near the
        // start accumulates cheaply. We assert the verdict shifts as the
        // window (and thus the crossing tick) grows.
        let ticks = 6u32;
        let plane = 5usize;
        let mut values = Vec::with_capacity(plane * ticks as usize);
        for _ in 0..ticks {
            values.extend_from_slice(&[1, 1, 9, 1, 1]);
        }
        let field = ScalarField {
            width: 5,
            height: 1,
            ticks,
            values,
        };

        // Short window [0, 1]: seed cell 0 at cost 0; at tick 1 the cheapest
        // reachable cell is cost 1 (stay or step to cell 1). Budget 2 →
        // avoidable (cheap cells are within budget early).
        let short = Crossing {
            crossing_tick: 1,
            seed: SeedState {
                decision_tick: 0,
                x: 0,
                y: 0,
                cost: 0,
            },
        };
        let short_verdict = classify_crossing(&field, &stencil_4way(), short, 2);
        assert!(short_verdict.avoidable, "short window stays under budget");

        // Long window [0, 5]: seed cell 0 at cost 4 — by the crossing tick
        // the accumulator is high, and the only sub-budget escape needs to
        // pay the cost-9 barrier. Best continuation at tick 5 exceeds budget
        // 5 → unavoidable. The flip is the exact lookahead/budget boundary.
        let long = Crossing {
            crossing_tick: 5,
            seed: SeedState {
                decision_tick: 0,
                x: 0,
                y: 0,
                cost: 4,
            },
        };
        let long_verdict = classify_crossing(&field, &stencil_4way(), long, 5);
        assert!(
            !long_verdict.avoidable,
            "long window over budget is unavoidable",
        );
    }

    #[test]
    fn end_to_end_classifies_a_recorded_path() {
        // Accumulator 0,2,4,6,8 over ticks 0..5. Budget 5 is first reached
        // at tick 3 (value 6 — the `4 < 5 → 6 >= 5` transition), so the
        // crossing tick is 3, not 4. Window 3 → decision tick 0, seed cost 0
        // at cell 0. Vloc(·, 3) over 4 uniform-cost-1 layers from cost 0
        // reaches 3 → under budget 5, so this crossing is avoidable.
        let field = uniform_field(5);
        let path = log(vec![
            sample(0, 0, 0, 0),
            sample(1, 1, 0, 2),
            sample(2, 2, 0, 4),
            sample(3, 2, 0, 6),
            sample(4, 2, 0, 8),
        ]);
        let cls = solve_counterfactual_core(&field, &stencil_4way(), &path, 3, 5);
        assert_eq!(cls.crossings.len(), 1);
        let v = cls.crossings[0];
        assert_eq!(v.crossing_tick, 3);
        assert_eq!(v.decision_tick, 0);
        assert_eq!(v.seed_x, 0);
        assert_eq!(v.seed_cost, 0);
        // Vloc(·, 3) from seed-cost 0 over 4 uniform-cost-1 layers = 3.
        assert_eq!(v.best_continuation_cost, 3);
        assert_eq!(v.margin, 2);
        assert!(v.avoidable, "best continuation 3 < budget 5");
    }

    #[test]
    fn truncated_path_window_past_field_stays_unreachable() {
        // A crossing whose window runs past the field's recorded ticks: the
        // slice reads UNREACHABLE past its end, so the verdict is a
        // well-defined unavoidable rather than a panic.
        let field = uniform_field(2);
        let crossing = Crossing {
            crossing_tick: 5, // beyond the 2-tick field
            seed: SeedState {
                decision_tick: 1,
                x: 0,
                y: 0,
                cost: 1,
            },
        };
        let verdict = classify_crossing(&field, &stencil_4way(), crossing, 100);
        assert_eq!(verdict.best_continuation_cost, UNREACHABLE);
        assert!(!verdict.avoidable);
    }
}
