//! Closed-form escapability bound for snapshot-placed field contributions.
//!
//! A field contribution whose placement is snapshotted to a moving point's
//! position at a trigger tick is *escapable* — regardless of where the
//! snapshot lands — if the movement stencil carries the point out of the
//! contribution's covered region before the contribution takes effect.
//!
//! # Metric consistency
//!
//! `stencil_speed` and the covered-extent fields (`covered_extent_initial`,
//! `covered_growth_per_tick`) must be expressed in the **same spatial
//! metric** (e.g. both in grid-cell radii). The bound holds for any metric
//! under which the covered region is disc-like; a non-disc shape is
//! captured by its bounding extent in the chosen metric.
//!
//! # Conservative cap direction
//!
//! `max_concurrent` is a *conservative upper bound*: every configuration
//! with `N <= max_concurrent` simultaneously active contributions is
//! certified escapable; a configuration with `N > max_concurrent` is **not
//! certified** (a covering arrangement of that many contributions may
//! exist). The cap never falsely certifies an inescapable configuration.

/// Abstract model of a field contribution for escapability analysis.
///
/// The covered region grows monotonically as `r0 + g*t` in cell-radius units
/// after the trigger instant (`r0 = covered_extent_initial`,
/// `g = covered_growth_per_tick`). The contribution takes effect at
/// `t = lead_ticks`. The movement stencil permits at most `stencil_speed`
/// displacement per tick.
///
/// All values are in the same spatial metric; see the module-level note.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EscapeParams {
    /// Covered extent at the trigger instant (`r0`). Zero for a contribution
    /// centered exactly on the point's snapshot position.
    pub covered_extent_initial: f32,
    /// Growth in covered extent per tick (`g`). Zero for a fixed-size region.
    pub covered_growth_per_tick: f32,
    /// Maximum displacement per tick the stencil permits (`s`); the maximum
    /// magnitude over the stencil's offset list, including the zero "stay"
    /// offset.
    pub stencil_speed: f32,
    /// Ticks between trigger and effect (`L`). The point has this many ticks
    /// to escape before the contribution activates.
    pub lead_ticks: u32,
}

/// Escapability verdict for a contribution and concurrency cap.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EscapeVerdict {
    /// `true` iff the per-contribution bound certifies escapability.
    pub escapable: bool,
    /// `reach(L) - cover(L) = stencil_speed * L - covered_extent_initial -
    /// covered_growth_per_tick * L`. Positive when escapable (slack),
    /// zero at the boundary, negative when not escapable.
    pub margin: f32,
    /// Conservative concurrency cap: the largest `N` for which `N`
    /// simultaneously-active contributions of this shape are certified
    /// escapable. `0` when `escapable` is `false`. `u32::MAX` when the
    /// covered extent at the lead tick is zero (zero-extent contributions
    /// never block any cell).
    pub max_concurrent: u32,
}

/// Returns `(escapable, margin)` for a single contribution.
///
/// The contribution is escapable iff both clauses hold:
/// - `cover(L) < reach(L)`: the reachable radius outpaces the covered extent
///   at the moment of effect.
/// - `g < s`: the region grows strictly slower than the stencil speed so the
///   point keeps outrunning the edge after the lead tick. When `r0 = 0` this
///   clause is implied by the first; it is load-bearing when `r0 > 0`.
///
/// `margin = reach(L) - cover(L)`.
fn escapable_within_lead(p: &EscapeParams) -> (bool, f32) {
    // lead_ticks -> f32: precision loss is intentional; the bound is
    // conservative and tick counts are small in practice.
    #[allow(clippy::cast_precision_loss)]
    let l = p.lead_ticks as f32;
    let cover = p
        .covered_growth_per_tick
        .mul_add(l, p.covered_extent_initial);
    let reach = p.stencil_speed * l;
    let escapable = cover < reach && p.covered_growth_per_tick < p.stencil_speed;
    (escapable, reach - cover)
}

/// Returns the conservative concurrency cap for `p`.
///
/// `0` when the per-contribution bound is not met. `u32::MAX` when
/// `cover(L) == 0` (a zero-extent contribution never occupies any cell).
/// Otherwise the largest `N` satisfying `N * cover(L)^2 < reach(L)^2`,
/// i.e. `ceil((reach(L) / cover(L))^2) - 1`, saturating at `u32::MAX`.
fn max_concurrent(p: &EscapeParams) -> u32 {
    let (escapable, _) = escapable_within_lead(p);
    if !escapable {
        return 0;
    }
    #[allow(clippy::cast_precision_loss)]
    let l = p.lead_ticks as f32;
    let cover = p
        .covered_growth_per_tick
        .mul_add(l, p.covered_extent_initial);
    if cover == 0.0 {
        return u32::MAX;
    }
    let reach = p.stencil_speed * l;
    // ratio > 1 (reach > cover > 0); ratio_sq >= ~1.0+ε, ceil >= 2.
    // f32::INFINITY (when ratio is huge) saturates to u64::MAX below.
    let ratio_sq = (reach / cover).powi(2).ceil();
    // Sign is positive (ratio > 1), truncation is handled by saturation.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let n_plus_one = ratio_sq as u64;
    let capped = n_plus_one.saturating_sub(1).min(u64::from(u32::MAX));
    // capped <= u32::MAX, so the as-cast is exact.
    #[allow(clippy::cast_possible_truncation)]
    let result = capped as u32;
    result
}

/// Evaluate the complete escapability verdict for `p`.
#[must_use]
pub fn evaluate(p: &EscapeParams) -> EscapeVerdict {
    let (escapable, margin) = escapable_within_lead(p);
    EscapeVerdict {
        escapable,
        margin,
        max_concurrent: max_concurrent(p),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Helper to compute cover(L) for clarity in test assertions.
    fn cover_at_lead(p: &EscapeParams) -> f32 {
        #[allow(clippy::cast_precision_loss)]
        let l = p.lead_ticks as f32;
        p.covered_growth_per_tick
            .mul_add(l, p.covered_extent_initial)
    }

    // Helper to compute reach(L) for clarity in test assertions.
    fn reach_at_lead(p: &EscapeParams) -> f32 {
        #[allow(clippy::cast_precision_loss)]
        let l = p.lead_ticks as f32;
        p.stencil_speed * l
    }

    // Per-contribution bound tests.

    #[test]
    fn at_boundary_not_escapable() {
        // cover(L) == reach(L): strict < fails, margin is 0.
        // r0=0, g=2, s=2, L=5 → cover=10, reach=10.
        let p = EscapeParams {
            covered_extent_initial: 0.0,
            covered_growth_per_tick: 2.0,
            stencil_speed: 2.0,
            lead_ticks: 5,
        };
        assert_eq!(cover_at_lead(&p), reach_at_lead(&p));
        let (esc, margin) = escapable_within_lead(&p);
        assert!(!esc);
        assert_eq!(margin, 0.0);
    }

    #[test]
    fn just_inside_escapable() {
        // cover(L) < reach(L) and g < s.
        // r0=0, g=1, s=3, L=4 → cover=4, reach=12, margin=8.
        let p = EscapeParams {
            covered_extent_initial: 0.0,
            covered_growth_per_tick: 1.0,
            stencil_speed: 3.0,
            lead_ticks: 4,
        };
        let (esc, margin) = escapable_within_lead(&p);
        assert!(esc);
        assert!(margin > 0.0);
        assert_eq!(margin, 8.0);
    }

    #[test]
    fn just_outside_not_escapable() {
        // cover(L) > reach(L): margin negative.
        // r0=0, g=4, s=2, L=3 → cover=12, reach=6, margin=-6.
        let p = EscapeParams {
            covered_extent_initial: 0.0,
            covered_growth_per_tick: 4.0,
            stencil_speed: 2.0,
            lead_ticks: 3,
        };
        let (esc, margin) = escapable_within_lead(&p);
        assert!(!esc);
        assert!(margin < 0.0);
        assert_eq!(margin, -6.0);
    }

    #[test]
    fn persistence_clause_g_equals_s_not_escapable() {
        // g == s: second clause (g < s) fails; with r0 > 0 the first clause
        // also fails (cover > reach). Confirms the strict-inequality
        // requirement on both clauses.
        // r0=1, g=2, s=2, L=5 → cover=11, reach=10.
        let p = EscapeParams {
            covered_extent_initial: 1.0,
            covered_growth_per_tick: 2.0,
            stencil_speed: 2.0,
            lead_ticks: 5,
        };
        let (esc, _) = escapable_within_lead(&p);
        assert!(!esc);
    }

    #[test]
    fn persistence_clause_large_initial_extent_not_escapable() {
        // r0 large enough that cover(L) >= reach(L) even though g < s.
        // r0=10, g=1, s=2, L=4 → cover=14, reach=8. First clause fails.
        let p = EscapeParams {
            covered_extent_initial: 10.0,
            covered_growth_per_tick: 1.0,
            stencil_speed: 2.0,
            lead_ticks: 4,
        };
        let (esc, margin) = escapable_within_lead(&p);
        assert!(!esc);
        assert_eq!(margin, -6.0);
    }

    #[test]
    fn zero_lead_not_escapable() {
        // L=0: reach == 0; cover < reach is never satisfied.
        let p = EscapeParams {
            covered_extent_initial: 0.0,
            covered_growth_per_tick: 1.0,
            stencil_speed: 5.0,
            lead_ticks: 0,
        };
        assert_eq!(reach_at_lead(&p), 0.0);
        let (esc, _) = escapable_within_lead(&p);
        assert!(!esc);
    }

    #[test]
    fn zero_lead_zero_cover_not_escapable() {
        // L=0, r0=0, g=0: cover==0 and reach==0; 0 < 0 is false.
        let p = EscapeParams {
            covered_extent_initial: 0.0,
            covered_growth_per_tick: 0.0,
            stencil_speed: 5.0,
            lead_ticks: 0,
        };
        let (esc, margin) = escapable_within_lead(&p);
        assert!(!esc);
        assert_eq!(margin, 0.0);
    }

    // Concurrency cap tests.

    #[test]
    fn cap_at_threshold() {
        // r0=0, g=1, s=3, L=2 → cover=2, reach=6.
        // ratio=3, ratio^2=9, ceil(9)=9, N_max=8.
        // N=8: 8*4=32 < 36=reach^2. Certified.
        // N=9: 9*4=36 >= 36. Not certified.
        let p = EscapeParams {
            covered_extent_initial: 0.0,
            covered_growth_per_tick: 1.0,
            stencil_speed: 3.0,
            lead_ticks: 2,
        };
        assert_eq!(max_concurrent(&p), 8);
    }

    #[test]
    fn cap_is_zero_when_bound_fails() {
        // Not escapable → cap is 0.
        let p = EscapeParams {
            covered_extent_initial: 0.0,
            covered_growth_per_tick: 3.0,
            stencil_speed: 2.0,
            lead_ticks: 4,
        };
        let (esc, _) = escapable_within_lead(&p);
        assert!(!esc);
        assert_eq!(max_concurrent(&p), 0);
    }

    #[test]
    fn cap_grows_with_margin() {
        // Larger margin (faster stencil relative to growth) → larger cap.
        // Case A: r0=0, g=1, s=2, L=4 → cover=4, reach=8; ratio=2, N_max=3.
        // Case B: r0=0, g=1, s=4, L=4 → cover=4, reach=16; ratio=4, N_max=15.
        let p_a = EscapeParams {
            covered_extent_initial: 0.0,
            covered_growth_per_tick: 1.0,
            stencil_speed: 2.0,
            lead_ticks: 4,
        };
        let p_b = EscapeParams {
            covered_extent_initial: 0.0,
            covered_growth_per_tick: 1.0,
            stencil_speed: 4.0,
            lead_ticks: 4,
        };
        let cap_a = max_concurrent(&p_a);
        let cap_b = max_concurrent(&p_b);
        assert_eq!(cap_a, 3);
        assert_eq!(cap_b, 15);
        assert!(cap_b > cap_a);
    }

    #[test]
    fn cap_saturates_when_cover_zero() {
        // cover(L)==0: zero-extent contributions never block any cell.
        // r0=0, g=0, s=1, L=3 → cover=0, reach=3.
        let p = EscapeParams {
            covered_extent_initial: 0.0,
            covered_growth_per_tick: 0.0,
            stencil_speed: 1.0,
            lead_ticks: 3,
        };
        assert_eq!(cover_at_lead(&p), 0.0);
        assert_eq!(max_concurrent(&p), u32::MAX);
    }

    // Assembled verdict tests.

    #[test]
    fn evaluate_worked_example() {
        // r0=0, g=1, s=2, L=6 → cover=6, reach=12.
        // escapable: 6 < 12 ✓, 1 < 2 ✓ → true.
        // margin: 12 - 6 = 6.
        // ratio=2, ratio^2=4, ceil(4)=4, N_max=3.
        // Check: 3*36=108 < 144=reach^2 ✓; 4*36=144 >= 144 ✗.
        let p = EscapeParams {
            covered_extent_initial: 0.0,
            covered_growth_per_tick: 1.0,
            stencil_speed: 2.0,
            lead_ticks: 6,
        };
        let v = evaluate(&p);
        assert!(v.escapable);
        assert_eq!(v.margin, 6.0);
        assert_eq!(v.max_concurrent, 3);
    }

    #[test]
    fn evaluate_inescapable_verdict() {
        // cover > reach → not escapable, cap = 0.
        let p = EscapeParams {
            covered_extent_initial: 0.0,
            covered_growth_per_tick: 5.0,
            stencil_speed: 2.0,
            lead_ticks: 3,
        };
        let v = evaluate(&p);
        assert!(!v.escapable);
        assert!(v.margin < 0.0);
        assert_eq!(v.max_concurrent, 0);
    }
}
