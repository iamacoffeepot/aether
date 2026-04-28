//! Exact-rational 3D point for the BSP CSG path (ADR-0061).
//!
//! `BspPoint3` carries three [`BigInt`] numerators sharing a single
//! [`BigInt`] denominator. Arbitrary-precision integers (rather than
//! `i128`) are necessary because deep `clip_to` recursion compounds
//! intermediate magnitudes faster than any fixed width tolerates —
//! the matrix's curved×sphere class hits ~2^150-bit numerators within
//! a few split levels. `i128`-checked arithmetic surfaced this as
//! `CsgError::NumericOverflow` under ADR-0054 coordinate bounds, so
//! per the ADR's "any overflow under those bounds is a bug to fix"
//! we widened the integer rather than capping with periodic snapping.
//!
//! Shared denominator (rather than three independent
//! [`super::rat::BspRat`] fields) keeps the per-plane side test as a
//! single linear combination — `n·p + d` — without cross-axis fraction
//! addition. It also matches the natural shape of
//! [`super::polygon::compute_intersection_rat`] output, where all three
//! axes share `(s0·p1.den − s1·p0.den)` as a denominator by
//! construction.
//!
//! # Phase 3 invariants
//!
//! - **Normal form.** Every value has `den > 0` and
//!   `gcd(|num[0]|, |num[1]|, |num[2]|, den) == 1`. All constructors
//!   enforce this.
//! - **Equality and hashing.** `==` is bit-equal after normalization;
//!   `Hash` agrees with `==`. Equal rationals produce identical
//!   [`BigInt`] limbs and identical hashes.
//! - **Lift round-trip.** `BspPoint3::lift(p).snap() == p` for any
//!   integer `Point3` `p`.
//! - **Snap parity.** [`BspPoint3::snap`] mirrors the legacy
//!   `round_div` semantics in [`crate::csg::polygon`] per axis:
//!   round-to-nearest, ties away from zero.
//!
//! The internal representation reserves an extension point for vertex
//! provenance (plane-A ∩ plane-B line, source edge, owning side) per
//! ADR-0061's Decision section. No provenance semantics are required
//! in phase 3; the field is not allocated to avoid pretending behavior
//! that doesn't exist.

use num_bigint::BigInt;
use num_integer::Integer;
use num_traits::{One, Signed, ToPrimitive, Zero};

use crate::csg::CsgError;
use crate::csg::point::Point3;

/// Exact-rational 3D point in fully-reduced normal form. Three
/// arbitrary-precision numerators share one positive denominator;
/// `gcd` of all four `BigInt`s is `1` for any value that exists.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct BspPoint3 {
    num: [BigInt; 3],
    den: BigInt,
}

impl BspPoint3 {
    /// Lift an integer [`Point3`] to a rational point. Already in
    /// normal form (`den == 1`), so no reduction needed.
    pub(super) fn lift(p: Point3) -> BspPoint3 {
        BspPoint3 {
            num: [BigInt::from(p.x), BigInt::from(p.y), BigInt::from(p.z)],
            den: BigInt::one(),
        }
    }

    /// Construct from raw `num / den`, normalizing to:
    /// - `den > 0` (sign carried by the numerators),
    /// - `gcd(|num[0]|, |num[1]|, |num[2]|, den) == 1`,
    /// - zero represented as `{[0, 0, 0], 1}`.
    ///
    /// Generic over `Into<BigInt>` so callers can pass either
    /// `[BigInt; 3]` (the natural shape of
    /// [`super::polygon::compute_intersection_rat`] output) or
    /// `[i128; 3]` (test fixtures, lifted-integer fast paths).
    ///
    /// Returns `Err(NumericOverflow { context: "denominator zero" })`
    /// for `den == 0` — degenerate; callers (Phase 3
    /// `compute_intersection_rat`) gate on SPANNING classification, so
    /// this should not fire under valid composition.
    pub(super) fn new<N, D>(num: [N; 3], den: D) -> Result<BspPoint3, CsgError>
    where
        N: Into<BigInt>,
        D: Into<BigInt>,
    {
        let num: [BigInt; 3] = num.map(Into::into);
        let den: BigInt = den.into();
        if den.is_zero() {
            return Err(CsgError::NumericOverflow {
                stage: "BspPoint3::new",
                context: "denominator zero",
            });
        }
        let (num, den) = if den.is_negative() {
            ([-&num[0], -&num[1], -&num[2]], -den)
        } else {
            (num, den)
        };
        // den > 0 holds.
        let g = num[0].gcd(&num[1]).gcd(&num[2]).gcd(&den);
        if g.is_one() {
            return Ok(BspPoint3 { num, den });
        }
        Ok(BspPoint3 {
            num: [&num[0] / &g, &num[1] / &g, &num[2] / &g],
            den: &den / &g,
        })
    }

    pub(super) fn num(&self) -> &[BigInt; 3] {
        &self.num
    }

    pub(super) fn den(&self) -> &BigInt {
        &self.den
    }

    /// Snap each axis to the nearest `i32`, ties away from zero.
    /// Mirrors the legacy `round_div` semantics in
    /// [`crate::csg::polygon`] so a lifted-integer point round-trips:
    /// `lift(p).snap() == p`.
    ///
    /// Returns `Err(NumericOverflow { context: "narrow to i32" })` if
    /// the rounded quotient does not fit in `i32`. CSG coordinates
    /// are bounded by `±256` in fixed units per ADR-0054 /
    /// `fixed::f32_to_fixed`, so this should never trigger in
    /// practice — but the typed error is the right shape.
    pub(super) fn snap(&self) -> Result<Point3, CsgError> {
        Ok(Point3 {
            x: snap_axis(&self.num[0], &self.den)?,
            y: snap_axis(&self.num[1], &self.den)?,
            z: snap_axis(&self.num[2], &self.den)?,
        })
    }
}

/// Round-to-nearest, ties away from zero. `den > 0` required.
fn snap_axis(num: &BigInt, den: &BigInt) -> Result<i32, CsgError> {
    let half = den / 2;
    let rounded = if !num.is_negative() {
        num + &half
    } else {
        num - &half
    };
    let div: BigInt = &rounded / den;
    div.to_i32().ok_or(CsgError::NumericOverflow {
        stage: "BspPoint3::snap",
        context: "narrow to i32",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(x: i32, y: i32, z: i32) -> Point3 {
        Point3 { x, y, z }
    }

    fn r(num: [i128; 3], den: i128) -> BspPoint3 {
        BspPoint3::new(
            [
                BigInt::from(num[0]),
                BigInt::from(num[1]),
                BigInt::from(num[2]),
            ],
            BigInt::from(den),
        )
        .expect("test fixture should not fail")
    }

    #[test]
    fn lift_round_trip_is_identity() {
        let original = p(7, -13, 42);
        let lifted = BspPoint3::lift(original);
        assert_eq!(lifted.den(), &BigInt::one());
        assert_eq!(
            lifted.num(),
            &[BigInt::from(7), BigInt::from(-13), BigInt::from(42)]
        );
        assert_eq!(lifted.snap().unwrap(), original);
    }

    #[test]
    fn lift_round_trip_for_zero() {
        let original = p(0, 0, 0);
        assert_eq!(BspPoint3::lift(original).snap().unwrap(), original);
    }

    #[test]
    fn shared_denom_normalizes_via_total_gcd() {
        // gcd(4, 6, 8, 2) = 2, so reduces to (2, 3, 4, 1).
        let q = r([4, 6, 8], 2);
        assert_eq!(
            q.num(),
            &[BigInt::from(2), BigInt::from(3), BigInt::from(4)]
        );
        assert_eq!(q.den(), &BigInt::one());
    }

    #[test]
    fn negative_denominator_flips_into_canonical_sign() {
        let q = r([3, 6, 9], -3);
        assert_eq!(
            q.num(),
            &[BigInt::from(-1), BigInt::from(-2), BigInt::from(-3)]
        );
        assert_eq!(q.den(), &BigInt::one());
    }

    #[test]
    fn equal_rationals_are_bit_identical() {
        let a = r([1, 2, 3], 2);
        let b = r([2, 4, 6], 4);
        let c = r([100, 200, 300], 200);
        let d = r([-1, -2, -3], -2);
        assert_eq!(a, b);
        assert_eq!(a, c);
        assert_eq!(a, d);
    }

    #[test]
    fn hash_agrees_with_eq() {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        fn hash_of(x: &BspPoint3) -> u64 {
            let mut h = DefaultHasher::new();
            x.hash(&mut h);
            h.finish()
        }

        let a = r([1, 2, 3], 2);
        let b = r([2, 4, 6], 4);
        assert_eq!(hash_of(&a), hash_of(&b));

        let lifted = BspPoint3::lift(p(0, 0, 0));
        let manual_zero = r([0, 0, 0], 1);
        assert_eq!(hash_of(&lifted), hash_of(&manual_zero));
    }

    #[test]
    fn snap_round_half_away_from_zero_per_axis() {
        // (7/2, -7/2, 1/2) → (4, -4, 1)
        assert_eq!(r([7, -7, 1], 2).snap().unwrap(), p(4, -4, 1));
        // (5/2, -5/2, -1/2) → (3, -3, -1)
        assert_eq!(r([5, -5, -1], 2).snap().unwrap(), p(3, -3, -1));
        // (1/3, 2/3, -1/3) → (0, 1, 0)
        assert_eq!(r([1, 2, -1], 3).snap().unwrap(), p(0, 1, 0));
    }

    #[test]
    fn denominator_zero_errors() {
        let err = BspPoint3::new(
            [BigInt::from(1), BigInt::from(2), BigInt::from(3)],
            BigInt::zero(),
        )
        .unwrap_err();
        assert!(matches!(err, CsgError::NumericOverflow { .. }));
    }

    #[test]
    fn snap_overflow_on_i32_narrow() {
        // num = i32::MAX + 1 / 1 → narrow fails.
        let too_big = BspPoint3::new(
            [
                BigInt::from(i32::MAX as i128 + 1),
                BigInt::zero(),
                BigInt::zero(),
            ],
            BigInt::one(),
        )
        .unwrap();
        assert!(matches!(
            too_big.snap(),
            Err(CsgError::NumericOverflow { .. })
        ));
    }

    #[test]
    fn equal_rationals_snap_equal() {
        // Phase 3 echo of phase 1's load-bearing base case.
        let a = r([7, -3, 14], 4);
        let b = r([14, -6, 28], 8);
        assert_eq!(a, b);
        assert_eq!(a.snap().unwrap(), b.snap().unwrap());
    }

    #[test]
    fn arbitrary_precision_does_not_overflow() {
        // The motivating case for BigInt. With i128 this overflows
        // during gcd reduction; with BigInt it's handled.
        let huge = BigInt::from(i128::MAX) * BigInt::from(i128::MAX);
        let p = BspPoint3::new([huge.clone(), huge.clone(), huge.clone()], huge).unwrap();
        // gcd of all four is the same value, reducing to (1, 1, 1, 1).
        assert_eq!(p.num(), &[BigInt::one(), BigInt::one(), BigInt::one()]);
        assert_eq!(p.den(), &BigInt::one());
    }
}
