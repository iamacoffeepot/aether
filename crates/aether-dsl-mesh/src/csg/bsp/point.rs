//! Exact-rational 3D point for the BSP CSG path (ADR-0061, phase 2).
//!
//! `BspPoint3` carries three `i128` numerators sharing a single `i128`
//! denominator. Shared denominator (rather than three independent
//! [`super::rat::BspRat`] fields) keeps the per-plane side test as a
//! single linear combination — `n·p + d` — without cross-axis fraction
//! addition. It also matches the natural shape of
//! [`super::polygon::compute_intersection_rat`] output, where all three
//! axes share `(s0·p1.den − s1·p0.den)` as a denominator by
//! construction.
//!
//! # Phase 2 invariants
//!
//! - **Normal form.** Every value has `den > 0` and
//!   `gcd(|num[0]|, |num[1]|, |num[2]|, den) == 1`. All constructors
//!   enforce this.
//! - **Equality and hashing.** `==` is bit-equal after normalization;
//!   `Hash` agrees with `==`. Equal rationals produce identical bytes,
//!   which is what makes the canonicalization pass's interning correct.
//! - **Lift round-trip.** `BspPoint3::lift(p).snap() == p` for any
//!   integer `Point3` `p`.
//! - **Snap parity.** `BspPoint3::snap` mirrors the legacy `round_div`
//!   semantics in [`crate::csg::polygon`] per axis: round-to-nearest,
//!   ties away from zero.
//!
//! The internal representation reserves an extension point for vertex
//! provenance (plane-A ∩ plane-B line, source edge, owning side) per
//! ADR-0061's Decision section. No provenance semantics are required
//! in phase 2; the field is not allocated to avoid pretending behavior
//! that doesn't exist.

#![allow(dead_code)] // phase 2 boundary: callers land in phase 3.

use crate::csg::CsgError;
use crate::csg::point::Point3;

/// Exact-rational 3D point in fully-reduced normal form. Three `i128`
/// numerators share one positive `i128` denominator; `gcd` of all four
/// fields is `1` for any value that exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct BspPoint3 {
    num: [i128; 3],
    den: i128,
}

impl BspPoint3 {
    /// Lift an integer [`Point3`] to a rational point. Already in
    /// normal form (`den == 1`), so no reduction needed.
    pub(super) fn lift(p: Point3) -> BspPoint3 {
        BspPoint3 {
            num: [p.x as i128, p.y as i128, p.z as i128],
            den: 1,
        }
    }

    /// Construct from raw `num / den`, normalizing.
    ///
    /// - `den == 0` returns `Err(NumericOverflow)`.
    /// - `den < 0` flips signs (checked for `i128::MIN`).
    /// - All four fields are gcd-reduced so equal rationals end up
    ///   bit-identical.
    pub(super) fn new(num: [i128; 3], den: i128) -> Result<BspPoint3, CsgError> {
        if den == 0 {
            return Err(CsgError::NumericOverflow {
                stage: "BspPoint3::new",
                context: "denominator zero",
            });
        }
        let (num, den) = if den < 0 {
            let neg = |n: i128, ctx: &'static str| -> Result<i128, CsgError> {
                n.checked_neg().ok_or(CsgError::NumericOverflow {
                    stage: "BspPoint3::new",
                    context: ctx,
                })
            };
            (
                [
                    neg(num[0], "num[0] neg overflow (i128::MIN)")?,
                    neg(num[1], "num[1] neg overflow (i128::MIN)")?,
                    neg(num[2], "num[2] neg overflow (i128::MIN)")?,
                ],
                neg(den, "den neg overflow (i128::MIN)")?,
            )
        } else {
            (num, den)
        };
        // den > 0 holds.
        let g = gcd_u128(
            num[0].unsigned_abs(),
            gcd_u128(
                num[1].unsigned_abs(),
                gcd_u128(num[2].unsigned_abs(), den as u128),
            ),
        );
        // g <= den (positive i128), so cast back to i128 is safe.
        let g_signed = g as i128;
        Ok(BspPoint3 {
            num: [num[0] / g_signed, num[1] / g_signed, num[2] / g_signed],
            den: den / g_signed,
        })
    }

    pub(super) fn num(&self) -> [i128; 3] {
        self.num
    }

    pub(super) fn den(&self) -> i128 {
        self.den
    }

    /// Snap each axis to the nearest `i32`, ties away from zero. Mirrors
    /// the legacy `round_div` semantics in [`crate::csg::polygon`] so a
    /// lifted-integer point round-trips: `lift(p).snap() == p`.
    pub(super) fn snap(&self) -> Result<Point3, CsgError> {
        Ok(Point3 {
            x: snap_axis(self.num[0], self.den)?,
            y: snap_axis(self.num[1], self.den)?,
            z: snap_axis(self.num[2], self.den)?,
        })
    }
}

/// Round-to-nearest, ties away from zero. `den > 0` required.
fn snap_axis(num: i128, den: i128) -> Result<i32, CsgError> {
    let half = den / 2;
    let rounded = if num >= 0 {
        num.checked_add(half).ok_or(CsgError::NumericOverflow {
            stage: "BspPoint3::snap",
            context: "round add overflow",
        })?
    } else {
        num.checked_sub(half).ok_or(CsgError::NumericOverflow {
            stage: "BspPoint3::snap",
            context: "round sub overflow",
        })?
    };
    let div = rounded / den;
    i32::try_from(div).map_err(|_| CsgError::NumericOverflow {
        stage: "BspPoint3::snap",
        context: "narrow to i32",
    })
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

    fn p(x: i32, y: i32, z: i32) -> Point3 {
        Point3 { x, y, z }
    }

    #[test]
    fn lift_round_trip_is_identity() {
        let original = p(7, -13, 42);
        let lifted = BspPoint3::lift(original);
        assert_eq!(lifted.den(), 1);
        assert_eq!(lifted.num(), [7, -13, 42]);
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
        let q = BspPoint3::new([4, 6, 8], 2).unwrap();
        assert_eq!(q.num(), [2, 3, 4]);
        assert_eq!(q.den(), 1);
    }

    #[test]
    fn negative_denominator_flips_into_canonical_sign() {
        let q = BspPoint3::new([3, 6, 9], -3).unwrap();
        // After flipping: num = [-3, -6, -9], den = 3, gcd = 3, reduce.
        assert_eq!(q.num(), [-1, -2, -3]);
        assert_eq!(q.den(), 1);
    }

    #[test]
    fn equal_rationals_are_bit_identical() {
        let a = BspPoint3::new([1, 2, 3], 2).unwrap();
        let b = BspPoint3::new([2, 4, 6], 4).unwrap();
        let c = BspPoint3::new([100, 200, 300], 200).unwrap();
        let d = BspPoint3::new([-1, -2, -3], -2).unwrap();
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

        let a = BspPoint3::new([1, 2, 3], 2).unwrap();
        let b = BspPoint3::new([2, 4, 6], 4).unwrap();
        assert_eq!(hash_of(&a), hash_of(&b));

        let lifted = BspPoint3::lift(p(0, 0, 0));
        let manual_zero = BspPoint3::new([0, 0, 0], 1).unwrap();
        assert_eq!(hash_of(&lifted), hash_of(&manual_zero));
    }

    #[test]
    fn snap_round_half_away_from_zero_per_axis() {
        // (7/2, -7/2, 1/2) → (4, -4, 1)
        let q = BspPoint3::new([7, -7, 1], 2).unwrap();
        assert_eq!(q.snap().unwrap(), p(4, -4, 1));

        // (5/2, -5/2, -1/2) → (3, -3, -1)
        let q = BspPoint3::new([5, -5, -1], 2).unwrap();
        assert_eq!(q.snap().unwrap(), p(3, -3, -1));

        // (1/3, 2/3, -1/3) → (0, 1, 0)
        let q = BspPoint3::new([1, 2, -1], 3).unwrap();
        assert_eq!(q.snap().unwrap(), p(0, 1, 0));
    }

    #[test]
    fn denominator_zero_errors() {
        let err = BspPoint3::new([1, 2, 3], 0).unwrap_err();
        assert!(matches!(err, CsgError::NumericOverflow { .. }));
    }

    #[test]
    fn snap_overflow_on_i32_narrow() {
        let too_big = BspPoint3::new([i32::MAX as i128 + 1, 0, 0], 1).unwrap();
        assert!(matches!(
            too_big.snap(),
            Err(CsgError::NumericOverflow { .. })
        ));
    }

    #[test]
    fn equal_rationals_snap_equal() {
        // Phase 2 echo of phase 1's load-bearing base case.
        let a = BspPoint3::new([7, -3, 14], 4).unwrap();
        let b = BspPoint3::new([14, -6, 28], 8).unwrap();
        assert_eq!(a, b);
        assert_eq!(a.snap().unwrap(), b.snap().unwrap());
    }

    #[test]
    fn gcd_helper_total_at_zero() {
        assert_eq!(gcd_u128(0, 0), 0);
        assert_eq!(gcd_u128(0, 7), 7);
        assert_eq!(gcd_u128(7, 0), 7);
        assert_eq!(gcd_u128(12, 18), 6);
    }
}
