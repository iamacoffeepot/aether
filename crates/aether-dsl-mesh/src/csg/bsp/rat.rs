//! Exact rationals for BSP CSG vertex construction (ADR-0061, Phase 1).
//!
//! `BspRat` carries an exact rational `num/den` in fully reduced normal
//! form, used per-axis by `BspPoint3` (Phase 2) so that BSP split
//! vertices can be represented without snap-rounding until a single
//! global pass at the BSP-to-cleanup boundary.
//!
//! # Phase 1 invariants
//!
//! - **Normal form.** Every `BspRat` value has `den > 0`,
//!   `gcd(|num|, den) == 1`, and zero is `{ num: 0, den: 1 }`. All
//!   constructors enforce this; no API permits a non-normalized value.
//! - **Equality and hashing.** `==` is bit-equal after normalization;
//!   `Hash` agrees with `==`. Equal rationals produce identical bytes
//!   and identical hashes.
//! - **Checked arithmetic.** All `i128` arithmetic uses checked
//!   operations; overflow surfaces as `CsgError::NumericOverflow` rather
//!   than panicking or wrapping.
//! - **Snap rule.** [`BspRat::snap`] matches the legacy `round_div`
//!   semantics in [`crate::csg::polygon::compute_intersection`]:
//!   round-to-nearest, ties away from zero. This preserves behavior
//!   parity for the eventual Phase 3 cutover.
//!
//! Phase 1 ships the type and its boundary; nothing in
//! `csg::polygon`, `csg::ops`, or the rest of `csg::bsp` references it
//! yet. Integration begins in Phase 2.

// Phase 1 boundary invariant: BspRat has no callers outside this
// module + its own tests. The `dead_code` allow comes off in Phase 2
// when BspPolygon brings the first real caller online.
#![allow(dead_code)]

use crate::csg::CsgError;

/// Exact rational in fully-reduced normal form.
///
/// Construction routes through [`BspRat::new`] (or the integer
/// shortcuts), so the `(den > 0, gcd(|num|, den) == 1)` invariant holds
/// for any value that exists. Equal rationals are guaranteed
/// bit-identical, which is what makes [`Eq`] and [`Hash`] safe to
/// derive without a custom impl.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct BspRat {
    num: i128,
    den: i128,
}

impl BspRat {
    pub(super) const ZERO: BspRat = BspRat { num: 0, den: 1 };
    pub(super) const ONE: BspRat = BspRat { num: 1, den: 1 };

    /// Lift an `i32` to a rational. Already in normal form
    /// (`den == 1`), so no reduction or check needed.
    pub(super) fn from_i32(n: i32) -> BspRat {
        BspRat {
            num: n as i128,
            den: 1,
        }
    }

    /// Lift an `i128` to a rational. Already in normal form.
    pub(super) fn from_i128(n: i128) -> BspRat {
        BspRat { num: n, den: 1 }
    }

    /// Construct from raw `num / den`, normalizing.
    ///
    /// - `den == 0` returns
    ///   `Err(NumericOverflow { context: "denominator zero" })` —
    ///   degenerate; callers (Phase 2 `compute_intersection_rat`) should
    ///   not produce a zero denominator.
    /// - `den < 0` flips both signs to enforce `den > 0`. The negation
    ///   is checked because `i128::MIN.checked_neg()` is `None`.
    /// - Both fields are reduced by their gcd so equal rationals end up
    ///   bit-identical.
    pub(super) fn new(num: i128, den: i128) -> Result<BspRat, CsgError> {
        if den == 0 {
            return Err(CsgError::NumericOverflow {
                stage: "BspRat::new",
                context: "denominator zero",
            });
        }
        let (num, den) = if den < 0 {
            let neg_num = num.checked_neg().ok_or(CsgError::NumericOverflow {
                stage: "BspRat::new",
                context: "num neg overflow (i128::MIN)",
            })?;
            let neg_den = den.checked_neg().ok_or(CsgError::NumericOverflow {
                stage: "BspRat::new",
                context: "den neg overflow (i128::MIN)",
            })?;
            (neg_num, neg_den)
        } else {
            (num, den)
        };
        // den > 0 holds.
        let g = gcd_u128(num.unsigned_abs(), den as u128);
        // g <= den (positive i128), so cast back to i128 is safe.
        let g_signed = g as i128;
        Ok(BspRat {
            num: num / g_signed,
            den: den / g_signed,
        })
    }

    pub(super) fn num(&self) -> i128 {
        self.num
    }

    pub(super) fn den(&self) -> i128 {
        self.den
    }

    /// Snap to nearest `i32`, ties away from zero. Mirrors the legacy
    /// `round_div` semantics in [`crate::csg::polygon`] so the Phase 3
    /// cutover preserves bit-identical behavior on integer-equal
    /// inputs.
    ///
    /// Returns `Err(NumericOverflow)` if the rounded quotient does not
    /// fit in `i32` (CSG coordinates are bounded by `±256` in fixed
    /// units per ADR-0054 / `fixed::f32_to_fixed`, so this should never
    /// trigger in practice — but checked is the right shape).
    pub(super) fn snap(&self) -> Result<i32, CsgError> {
        // den > 0 by invariant.
        let half = self.den / 2;
        let rounded = if self.num >= 0 {
            self.num
                .checked_add(half)
                .ok_or(CsgError::NumericOverflow {
                    stage: "BspRat::snap",
                    context: "round add overflow",
                })?
        } else {
            self.num
                .checked_sub(half)
                .ok_or(CsgError::NumericOverflow {
                    stage: "BspRat::snap",
                    context: "round sub overflow",
                })?
        };
        let div = rounded / self.den;
        i32::try_from(div).map_err(|_| CsgError::NumericOverflow {
            stage: "BspRat::snap",
            context: "narrow to i32",
        })
    }
}

/// Euclidean gcd on `u128`. Total: `gcd(0, n) == n`, `gcd(n, 0) == n`.
fn gcd_u128(a: u128, b: u128) -> u128 {
    let mut a = a;
    let mut b = b;
    while b != 0 {
        let t = b;
        b = a % b;
        a = t;
    }
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(num: i128, den: i128) -> BspRat {
        BspRat::new(num, den).expect("test fixture should not overflow")
    }

    #[test]
    fn zero_normalizes_to_canonical_form() {
        let z = r(0, 5);
        assert_eq!(z.num(), 0);
        assert_eq!(z.den(), 1);
        assert_eq!(z, BspRat::ZERO);
        assert_eq!(z, r(0, 999));
        assert_eq!(z, r(0, -7));
    }

    #[test]
    fn one_normalizes_correctly() {
        assert_eq!(r(1, 1), BspRat::ONE);
        assert_eq!(r(7, 7), BspRat::ONE);
        assert_eq!(r(-7, -7), BspRat::ONE);
    }

    #[test]
    fn positive_rational_reduces_by_gcd() {
        let x = r(2, 4);
        assert_eq!(x.num(), 1);
        assert_eq!(x.den(), 2);
        assert_eq!(x, r(50, 100));
        assert_eq!(x, r(1, 2));
    }

    #[test]
    fn negative_numerator_is_carried_in_num() {
        let x = r(-3, 6);
        assert_eq!(x.num(), -1);
        assert_eq!(x.den(), 2);
    }

    #[test]
    fn negative_denominator_flips_into_canonical_sign() {
        let x = r(3, -6);
        assert_eq!(x.num(), -1);
        assert_eq!(x.den(), 2);
        let y = r(-3, -6);
        assert_eq!(y.num(), 1);
        assert_eq!(y.den(), 2);
    }

    #[test]
    fn equal_rationals_bit_identical() {
        // The load-bearing property: equal-but-differently-spelled
        // rationals normalize to the same bytes.
        let a = r(2, 4);
        let b = r(1, 2);
        let c = r(50, 100);
        let d = r(-2, -4);
        assert_eq!(a, b);
        assert_eq!(a, c);
        assert_eq!(a, d);
    }

    #[test]
    fn hash_agrees_with_eq() {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        fn hash_of(x: &BspRat) -> u64 {
            let mut h = DefaultHasher::new();
            x.hash(&mut h);
            h.finish()
        }

        // Equal values must hash identically.
        assert_eq!(hash_of(&r(2, 4)), hash_of(&r(1, 2)));
        assert_eq!(hash_of(&r(0, 5)), hash_of(&BspRat::ZERO));
        assert_eq!(hash_of(&r(7, 1)), hash_of(&BspRat::from_i32(7)));
    }

    #[test]
    fn from_i32_is_already_normalized() {
        let x = BspRat::from_i32(5);
        assert_eq!(x.num(), 5);
        assert_eq!(x.den(), 1);
        assert_eq!(BspRat::from_i32(-5), r(-5, 1));
        assert_eq!(BspRat::from_i32(0), BspRat::ZERO);
    }

    #[test]
    fn from_i128_is_already_normalized() {
        let big = BspRat::from_i128(i128::MAX);
        assert_eq!(big.num(), i128::MAX);
        assert_eq!(big.den(), 1);
    }

    #[test]
    fn denominator_zero_errors() {
        let err = BspRat::new(5, 0).unwrap_err();
        match err {
            CsgError::NumericOverflow { stage, context } => {
                assert_eq!(stage, "BspRat::new");
                assert_eq!(context, "denominator zero");
            }
            other => panic!("expected NumericOverflow, got {other:?}"),
        }
    }

    #[test]
    fn snap_round_half_away_from_zero_positive() {
        // 7/2 = 3.5 → 4 (away from zero)
        assert_eq!(r(7, 2).snap().unwrap(), 4);
        // 5/2 = 2.5 → 3
        assert_eq!(r(5, 2).snap().unwrap(), 3);
        // 3/2 = 1.5 → 2
        assert_eq!(r(3, 2).snap().unwrap(), 2);
        // 1/2 = 0.5 → 1
        assert_eq!(r(1, 2).snap().unwrap(), 1);
        // 1/3 ≈ 0.33 → 0
        assert_eq!(r(1, 3).snap().unwrap(), 0);
        // 2/3 ≈ 0.67 → 1
        assert_eq!(r(2, 3).snap().unwrap(), 1);
    }

    #[test]
    fn snap_round_half_away_from_zero_negative() {
        // -7/2 = -3.5 → -4 (away from zero)
        assert_eq!(r(-7, 2).snap().unwrap(), -4);
        // -5/2 = -2.5 → -3
        assert_eq!(r(-5, 2).snap().unwrap(), -3);
        // -1/2 = -0.5 → -1
        assert_eq!(r(-1, 2).snap().unwrap(), -1);
        // -1/3 ≈ -0.33 → 0
        assert_eq!(r(-1, 3).snap().unwrap(), 0);
        // -2/3 ≈ -0.67 → -1
        assert_eq!(r(-2, 3).snap().unwrap(), -1);
    }

    #[test]
    fn snap_integer_pass_through() {
        assert_eq!(BspRat::from_i32(42).snap().unwrap(), 42);
        assert_eq!(BspRat::from_i32(-42).snap().unwrap(), -42);
        assert_eq!(BspRat::ZERO.snap().unwrap(), 0);
    }

    #[test]
    fn equal_rationals_snap_equal() {
        // The Phase 1 base case for the load-bearing snap invariant:
        // a == b implies a.snap() == b.snap(). Trivial under bit-equal
        // normalization but worth pinning.
        let a = r(7, 4);
        let b = r(14, 8);
        let c = r(700, 400);
        assert_eq!(a, b);
        assert_eq!(a, c);
        assert_eq!(a.snap().unwrap(), b.snap().unwrap());
        assert_eq!(a.snap().unwrap(), c.snap().unwrap());
    }

    #[test]
    fn snap_matches_legacy_round_div() {
        // Parity gate against the existing round_div semantics in
        // csg::polygon::compute_intersection. Sample (numer, denom)
        // pairs covering the four sign combinations.
        let cases: &[(i128, i128, i32)] = &[
            (7, 2, 4),        // +/+ above .5
            (-7, 2, -4),      // -/+ above .5
            (1, 2, 1),        // +/+ exactly .5
            (-1, 2, -1),      // -/+ exactly .5
            (1, 3, 0),        // +/+ below .5
            (-1, 3, 0),       // -/+ below .5
            (10, 5, 2),       // exact integer, no rounding
            (0, 99, 0),       // zero
            (-9, -2, 5),      // -/- (sign-flips on construction) → 9/2 = 4.5 → 5
            (12345, 7, 1764), // arbitrary
        ];
        for &(n, d, expected) in cases {
            let got = r(n, d).snap().unwrap();
            assert_eq!(got, expected, "snap({n}/{d}) = {got}, expected {expected}");
        }
    }

    #[test]
    fn snap_overflow_on_i32_narrow() {
        // num / 1 = num; if num exceeds i32 range, snap errors.
        let too_big = BspRat::from_i128(i32::MAX as i128 + 1);
        let err = too_big.snap().unwrap_err();
        match err {
            CsgError::NumericOverflow { stage, context } => {
                assert_eq!(stage, "BspRat::snap");
                assert_eq!(context, "narrow to i32");
            }
            other => panic!("expected NumericOverflow, got {other:?}"),
        }
        let too_small = BspRat::from_i128(i32::MIN as i128 - 1);
        assert!(matches!(
            too_small.snap(),
            Err(CsgError::NumericOverflow { .. })
        ));
    }

    #[test]
    fn construction_with_i128_min_in_negative_denominator_errors() {
        // num=1, den=i128::MIN: flipping signs would overflow on den.
        let err = BspRat::new(1, i128::MIN).unwrap_err();
        assert!(matches!(err, CsgError::NumericOverflow { .. }));
    }

    #[test]
    fn construction_with_i128_min_numerator_in_negative_denominator_errors() {
        // num=i128::MIN, den=-1: flipping num would overflow.
        let err = BspRat::new(i128::MIN, -1).unwrap_err();
        assert!(matches!(err, CsgError::NumericOverflow { .. }));
    }

    #[test]
    fn construction_with_i128_min_in_positive_denominator_succeeds() {
        // num=i128::MIN, den=positive: no sign flip, just gcd reduction.
        // gcd(2^127, 2) = 2, so reduces to (-2^126, 1).
        let x = BspRat::new(i128::MIN, 2).unwrap();
        assert_eq!(x.num(), i128::MIN / 2);
        assert_eq!(x.den(), 1);
    }

    #[test]
    fn gcd_helper_total_at_zero() {
        assert_eq!(gcd_u128(0, 0), 0);
        assert_eq!(gcd_u128(0, 7), 7);
        assert_eq!(gcd_u128(7, 0), 7);
        assert_eq!(gcd_u128(12, 18), 6);
        assert_eq!(gcd_u128(17, 13), 1);
    }
}
