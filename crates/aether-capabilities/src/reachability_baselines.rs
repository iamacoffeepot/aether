//! Baseline-replay validation harness for the reachability solver (issue
//! 1860). Runs [`solve_cost_to_reach`] over a fixed set of
//! seed-replayable and analytic scalar fields and asserts their outputs
//! match inline-recorded baselines. Because the fields are integer-exact
//! and seed-deterministic, the baselines are stable regression gates: a
//! one-bit change in the solver's numbers surfaces as a mismatch rather
//! than slipping through.
//!
//! This module gates *regression* on the frozen numbers. First-principles
//! *correctness* of the solver pass stays the job of the unit tests in
//! `reachability.rs` and `transforms.rs`. The cross-pass invariants that
//! need the corridor and windowed passes land once issues 1858 / 1859
//! merge.
//!
//! Not `mod heavy` — the solver is a pure synchronous function with no
//! dispatcher or timing dependence, so this belongs in the normal
//! parallel test set.

use crate::reachability::{UNREACHABLE, solve_cost_to_reach};
use aether_kinds::StencilOffset;

/// 4-way movement stencil (stay + four orthogonal moves), matching the
/// convention used by the solver's own unit tests.
fn stencil_4way() -> Vec<StencilOffset> {
    vec![
        StencilOffset { dx: 0, dy: 0 },
        StencilOffset { dx: 1, dy: 0 },
        StencilOffset { dx: -1, dy: 0 },
        StencilOffset { dx: 0, dy: 1 },
        StencilOffset { dx: 0, dy: -1 },
    ]
}

/// Hand-rolled splitmix64 PRNG (Sebastiano Vigna, 2015). Dependency-
/// free and integer-exact across platforms, so a given seed always
/// produces the same sequence — the invariant frozen baselines require.
fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// Build a cost field of `width × height × ticks` values drawn from the
/// splitmix64 stream seeded at `seed`. Costs land in `1..=127` to avoid
/// the `UNREACHABLE` sentinel and keep accumulated values comfortably
/// below `u32::MAX` for the field sizes used here.
fn seeded_field(seed: u64, width: usize, height: usize, ticks: usize) -> Vec<u32> {
    let mut state = seed;
    let n = width.saturating_mul(height).saturating_mul(ticks);
    (0..n)
        .map(|_| {
            u32::try_from(splitmix64(&mut state) % 127 + 1).expect("value is 1..=127, fits u32")
        })
        .collect()
}

/// Start slice seeding only cell 0 at accumulated cost 0 — the
/// canonical single-origin start used across the fixture set.
fn start_at_origin(width: usize, height: usize) -> Vec<u32> {
    let mut s = vec![UNREACHABLE; width * height];
    if !s.is_empty() {
        s[0] = 0;
    }
    s
}

/// Cheap FNV-1a digest of a `Vec<u32>` result field (interpreted as
/// little-endian bytes). Returns a `u64` that serves as the inline
/// content anchor for seed-deterministic runs.
fn fnv1a_digest(v: &[u32]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &val in v {
        for byte in val.to_le_bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    hash
}

/// Minimum cost-to-reach over the final tick of `v`.
fn final_tick_min(v: &[u32], width: usize, height: usize, ticks: usize) -> u32 {
    let plane = width * height;
    let base = ticks.saturating_sub(1) * plane;
    v.get(base..base.saturating_add(plane))
        .and_then(|layer| layer.iter().copied().min())
        .unwrap_or(UNREACHABLE)
}

/// Reachability verdict and signed margin for the given budget.
fn verdict_margin(min_cost: u32, budget: u32) -> (bool, i64) {
    let reachable = min_cost < budget;
    let margin = i64::from(budget) - i64::from(min_cost);
    (reachable, margin)
}

/// Analytic ramp fixture: 3×1 grid, uniform cost 1, 3 ticks, start at
/// cell 0. The V field is hand-checkable:
///
/// ```text
/// t0:  [0, MAX, MAX]
/// t1:  [1,   1, MAX]
/// t2:  [2,   2,   2]
/// ```
///
/// Minimum at the final tick is 2.
fn field_ramp() -> (Vec<u32>, usize, usize, usize) {
    let (width, height, ticks) = (3, 1, 3);
    let costs = vec![1u32; width * height * ticks];
    (costs, width, height, ticks)
}

/// Analytic two-basin fixture: 3×1, uniform cost 1 except cell 1 which
/// is blocked (`UNREACHABLE`) every tick, starting from cells 0 and 2.
/// The blocked center keeps the two basins isolated:
///
/// ```text
/// t0:  [0, MAX,   0]
/// t1:  [1, MAX,   1]
/// t2:  [2, MAX,   2]
/// ```
fn field_two_basin() -> (Vec<u32>, Vec<u32>, usize, usize, usize) {
    let (width, height, ticks) = (3, 1, 3);
    let plane = width * height;
    let mut costs = vec![1u32; plane * ticks];
    for t in 0..ticks {
        costs[t * plane + 1] = UNREACHABLE;
    }
    let mut start = vec![UNREACHABLE; plane];
    start[0] = 0;
    start[2] = 0;
    (costs, start, width, height, ticks)
}

/// Analytic false-corridor fixture: 3×1, costs 1 except cell 2 which
/// is permanently blocked, start at cell 0. The corridor terminates
/// before the right end:
///
/// ```text
/// t0:  [0, MAX, MAX]
/// t1:  [1,   1, MAX]
/// t2:  [2,   2, MAX]
/// ```
fn field_false_corridor() -> (Vec<u32>, usize, usize, usize) {
    let (width, height, ticks) = (3, 1, 3);
    let plane = width * height;
    let mut costs = vec![1u32; plane * ticks];
    for t in 0..ticks {
        costs[t * plane + 2] = UNREACHABLE;
    }
    (costs, width, height, ticks)
}

/// Analytic above-budget fixture: 3×1, uniform cost 5, 3 ticks, start
/// at cell 0. Final-tick minimum is 10, exceeding a budget of 8:
///
/// ```text
/// t0:  [0,   MAX, MAX]
/// t1:  [5,     5, MAX]
/// t2: [10,    10,  10]
/// ```
fn field_above_budget() -> (Vec<u32>, usize, usize, usize) {
    let (width, height, ticks) = (3, 1, 3);
    let costs = vec![5u32; width * height * ticks];
    (costs, width, height, ticks)
}

/// Test 1a — generator determinism: same seed and dims produce an
/// identical `Vec<u32>` across two independent calls.
#[test]
fn seeded_field_is_deterministic() {
    let a = seeded_field(0xdead_beef_cafe_f00d, 8, 8, 4);
    let b = seeded_field(0xdead_beef_cafe_f00d, 8, 8, 4);
    assert_eq!(a, b, "same seed/dims must produce identical field");
}

/// Test 1b — different seeds produce different fields (collision would
/// be an extraordinarily weak PRNG).
#[test]
fn different_seeds_produce_different_fields() {
    let a = seeded_field(1, 4, 4, 3);
    let b = seeded_field(2, 4, 4, 3);
    assert_ne!(a, b, "different seeds must produce different fields");
}

/// Test 1c — field length matches `width × height × ticks`.
#[test]
fn seeded_field_length_matches_dims() {
    for (seed, w, h, t) in [(1u64, 4, 4, 3), (2, 8, 8, 10), (3, 1, 1, 1)] {
        let f = seeded_field(seed, w, h, t);
        assert_eq!(f.len(), w * h * t, "seed={seed} {w}×{h}×{t}");
    }
}

/// Test 1d — seeded-field costs are never the `UNREACHABLE` sentinel, so
/// all cells stay traversable and the frozen baselines are tight.
#[test]
fn seeded_field_costs_are_never_unreachable_sentinel() {
    let f = seeded_field(0x1234_5678_9abc_def0, 8, 8, 5);
    assert!(
        f.iter().all(|&c| c < UNREACHABLE),
        "all costs must be < UNREACHABLE"
    );
}

/// Test 2a — ramp field: exact V values and baseline verdict/margin.
#[test]
fn ramp_exact_v_and_baseline_margin() {
    let (costs, width, height, ticks) = field_ramp();
    let start = start_at_origin(width, height);
    let v = solve_cost_to_reach(width, height, ticks, &costs, &stencil_4way(), &start);

    assert_eq!(
        v,
        vec![0, UNREACHABLE, UNREACHABLE, 1, 1, UNREACHABLE, 2, 2, 2],
        "ramp V field"
    );

    let min_cost = final_tick_min(&v, width, height, ticks);
    assert_eq!(min_cost, 2, "ramp final-tick `min_cost`");

    let (reachable, margin) = verdict_margin(min_cost, 5);
    assert!(reachable, "ramp: budget 5 should be reachable (min=2)");
    assert_eq!(margin, 3, "ramp: margin = 5 - 2 = 3");

    let (reachable_tight, margin_tight) = verdict_margin(min_cost, 1);
    assert!(
        !reachable_tight,
        "ramp: budget 1 < `min_cost` 2 → not reachable"
    );
    assert_eq!(margin_tight, -1, "ramp: margin = 1 - 2 = -1");
}

/// Test 2b — double-solve: running the solver twice with identical
/// inputs must produce byte-identical results (determinism property).
#[test]
fn ramp_double_solve_is_byte_identical() {
    let (costs, width, height, ticks) = field_ramp();
    let start = start_at_origin(width, height);
    let v1 = solve_cost_to_reach(width, height, ticks, &costs, &stencil_4way(), &start);
    let v2 = solve_cost_to_reach(width, height, ticks, &costs, &stencil_4way(), &start);
    assert_eq!(v1, v2, "ramp: double-solve must be byte-identical");
}

/// Test 2c — two-basin field: blocked center keeps basins isolated;
/// both halves carry the same accumulated cost.
#[test]
fn two_basin_exact_v_and_baseline_margin() {
    let (costs, start, width, height, ticks) = field_two_basin();
    let v = solve_cost_to_reach(width, height, ticks, &costs, &stencil_4way(), &start);

    assert_eq!(
        v,
        vec![0, UNREACHABLE, 0, 1, UNREACHABLE, 1, 2, UNREACHABLE, 2],
        "two-basin V field"
    );

    let min_cost = final_tick_min(&v, width, height, ticks);
    assert_eq!(min_cost, 2, "two-basin final-tick `min_cost`");

    let (reachable, margin) = verdict_margin(min_cost, 5);
    assert!(reachable, "two-basin: budget 5 reachable");
    assert_eq!(margin, 3, "two-basin: margin = 5 - 2 = 3");
}

/// Test 2d — false-corridor field: cell 2 permanently blocked; the
/// right end of the corridor is never reachable.
#[test]
fn false_corridor_exact_v_and_baseline_margin() {
    let (costs, width, height, ticks) = field_false_corridor();
    let start = start_at_origin(width, height);
    let v = solve_cost_to_reach(width, height, ticks, &costs, &stencil_4way(), &start);

    assert_eq!(
        v,
        vec![
            0,
            UNREACHABLE,
            UNREACHABLE,
            1,
            1,
            UNREACHABLE,
            2,
            2,
            UNREACHABLE
        ],
        "false-corridor V field"
    );

    let min_cost = final_tick_min(&v, width, height, ticks);
    assert_eq!(min_cost, 2, "false-corridor final-tick `min_cost`");

    let (reachable, margin) = verdict_margin(min_cost, 5);
    assert!(reachable, "false-corridor: budget 5 reachable (min=2)");
    assert_eq!(margin, 3, "false-corridor: margin = 5 - 2 = 3");
}

/// Test 2e — above-budget field: uniform cost 5, 3 ticks → `min_cost`=10,
/// which exceeds a budget of 8.
#[test]
fn above_budget_is_not_reachable() {
    let (costs, width, height, ticks) = field_above_budget();
    let start = start_at_origin(width, height);
    let v = solve_cost_to_reach(width, height, ticks, &costs, &stencil_4way(), &start);

    assert_eq!(
        v,
        vec![0, UNREACHABLE, UNREACHABLE, 5, 5, UNREACHABLE, 10, 10, 10],
        "above-budget V field"
    );

    let min_cost = final_tick_min(&v, width, height, ticks);
    assert_eq!(min_cost, 10, "above-budget final-tick `min_cost`");

    let (reachable, margin) = verdict_margin(min_cost, 8);
    assert!(
        !reachable,
        "above-budget: budget 8 < `min_cost` 10 → not reachable"
    );
    assert_eq!(margin, -2, "above-budget: margin = 8 - 10 = -2");
}

/// Test 2f — seed-deterministic field: double-solve byte-equality and
/// frozen digest. The digest anchors the full V field without committing
/// it; a one-bit solver change flips the digest.
///
/// Frozen baselines (`min_cost`, digest) recorded from the first passing
/// run. Seeds and dims are part of the fixture; change them and the
/// baselines must be re-recorded.
#[test]
fn seed_a_double_solve_and_digest() {
    const W: usize = 8;
    const H: usize = 8;
    const T: usize = 10;
    const SEED: u64 = 0xaabb_ccdd_eeff_0011;
    const BUDGET: u32 = 400;
    const EXPECTED_MIN_COST: u32 = 132;
    const EXPECTED_REACHABLE: bool = true;
    const EXPECTED_DIGEST: u64 = 0x74ae_5581_3463_27da;

    let costs = seeded_field(SEED, W, H, T);
    let start = start_at_origin(W, H);
    let stencil = stencil_4way();

    let v1 = solve_cost_to_reach(W, H, T, &costs, &stencil, &start);
    let v2 = solve_cost_to_reach(W, H, T, &costs, &stencil, &start);
    assert_eq!(v1, v2, "seed A: double-solve must be byte-identical");

    let min_cost = final_tick_min(&v1, W, H, T);
    let (reachable, _) = verdict_margin(min_cost, BUDGET);
    assert_eq!(
        min_cost, EXPECTED_MIN_COST,
        "seed A: frozen `min_cost` baseline"
    );
    assert_eq!(
        reachable, EXPECTED_REACHABLE,
        "seed A: frozen reachable baseline"
    );
    assert_eq!(
        fnv1a_digest(&v1),
        EXPECTED_DIGEST,
        "seed A: frozen V-field digest"
    );
}

/// Test 2g — second seed-deterministic field with a tighter budget that
/// falls below the `min_cost`, confirming the not-reachable baseline.
#[test]
fn seed_b_double_solve_and_digest() {
    const W: usize = 6;
    const H: usize = 6;
    const T: usize = 8;
    const SEED: u64 = 0x1234_5678_9abc_def0;
    const BUDGET: u32 = 40;
    const EXPECTED_MIN_COST: u32 = 96;
    const EXPECTED_REACHABLE: bool = false;
    const EXPECTED_DIGEST: u64 = 0xd489_80b6_6a79_42bd;

    let costs = seeded_field(SEED, W, H, T);
    let start = start_at_origin(W, H);
    let stencil = stencil_4way();

    let v1 = solve_cost_to_reach(W, H, T, &costs, &stencil, &start);
    let v2 = solve_cost_to_reach(W, H, T, &costs, &stencil, &start);
    assert_eq!(v1, v2, "seed B: double-solve must be byte-identical");

    let min_cost = final_tick_min(&v1, W, H, T);
    let (reachable, _) = verdict_margin(min_cost, BUDGET);
    assert_eq!(
        min_cost, EXPECTED_MIN_COST,
        "seed B: frozen `min_cost` baseline"
    );
    assert_eq!(
        reachable, EXPECTED_REACHABLE,
        "seed B: frozen reachable baseline"
    );
    assert_eq!(
        fnv1a_digest(&v1),
        EXPECTED_DIGEST,
        "seed B: frozen V-field digest"
    );
}

/// Test 5 (partial) — monotonicity: scaling a field's cost up weakly
/// shrinks the margin. Tests with the ramp shape (well-defined analytic
/// behaviour: doubling cost doubles `min_cost`, halving margin).
#[test]
fn margin_shrinks_as_cost_rises() {
    const BUDGET: u32 = 10;
    let (width, height, ticks) = (3, 1, 3);
    let start = start_at_origin(width, height);
    let stencil = stencil_4way();

    let costs_1 = vec![1u32; width * height * ticks];
    let v_1 = solve_cost_to_reach(width, height, ticks, &costs_1, &stencil, &start);
    let (_, margin_1) = verdict_margin(final_tick_min(&v_1, width, height, ticks), BUDGET);

    let costs_2 = vec![2u32; width * height * ticks];
    let v_2 = solve_cost_to_reach(width, height, ticks, &costs_2, &stencil, &start);
    let (_, margin_2) = verdict_margin(final_tick_min(&v_2, width, height, ticks), BUDGET);

    let costs_3 = vec![3u32; width * height * ticks];
    let v_3 = solve_cost_to_reach(width, height, ticks, &costs_3, &stencil, &start);
    let (_, margin_3) = verdict_margin(final_tick_min(&v_3, width, height, ticks), BUDGET);

    assert!(
        margin_1 >= margin_2,
        "margin should not increase as cost rises (cost 1→2): {margin_1} >= {margin_2}"
    );
    assert!(
        margin_2 >= margin_3,
        "margin should not increase as cost rises (cost 2→3): {margin_2} >= {margin_3}"
    );
}

/// Test 5 (partial) — monotonicity: raising the budget weakly grows
/// reachability (or keeps it the same). Reachability is `min_cost <
/// budget` (strict), so it flips from false to true when budget rises
/// strictly above the minimum. For the ramp field (`min_cost` = 2):
///
/// | budget | reachable |
/// |--------|-----------|
/// | 1      | false     |
/// | 2      | false     | (strict <: 2 < 2 is false)
/// | 3      | true      |
/// | 5      | true      |
#[test]
fn reachability_non_decreasing_in_budget() {
    let (costs, width, height, ticks) = field_ramp();
    let start = start_at_origin(width, height);
    let stencil = stencil_4way();
    let v = solve_cost_to_reach(width, height, ticks, &costs, &stencil, &start);
    let min = final_tick_min(&v, width, height, ticks);
    assert_eq!(min, 2, "ramp final-tick min used by monotonicity test");

    let (reachable_b1, _) = verdict_margin(min, 1);
    let (reachable_b2, _) = verdict_margin(min, 2);
    let (reachable_b3, _) = verdict_margin(min, 3);
    let (reachable_b5, _) = verdict_margin(min, 5);

    assert!(!reachable_b1, "budget 1 < `min_cost` 2: not reachable");
    assert!(
        !reachable_b2,
        "budget 2 == `min_cost` 2: not reachable (strict <)"
    );
    assert!(reachable_b3, "budget 3 > `min_cost` 2: reachable");
    assert!(reachable_b5, "budget 5 > `min_cost` 2: reachable");

    assert!(
        !reachable_b1 || reachable_b2,
        "reachability non-decreasing: b1 → b2"
    );
    assert!(
        !reachable_b2 || reachable_b3,
        "reachability non-decreasing: b2 → b3"
    );
    assert!(
        !reachable_b3 || reachable_b5,
        "reachability non-decreasing: b3 → b5"
    );
}

/// Test 5 (partial) — size sanity: a canonical 64×64×1800 field
/// encodes under the 64 MiB transform cap
/// (`DEFAULT_TRANSFORM_MAX_OUTPUT_BYTES` from the DAG executor).
#[test]
fn canonical_large_field_output_fits_transform_cap() {
    const MAX_OUTPUT_BYTES: usize = 64 * 1024 * 1024;
    let v_len_bytes = 64_usize * 64 * 1800 * size_of::<u32>();
    assert!(
        v_len_bytes <= MAX_OUTPUT_BYTES,
        "64×64×1800 V field ({v_len_bytes} bytes) must fit the 64 MiB transform cap"
    );
}
