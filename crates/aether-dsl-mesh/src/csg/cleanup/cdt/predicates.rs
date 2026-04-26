//! Exact integer geometric predicates for CDT (ADR-0056).
//!
//! Both predicates take 2D points as `(i64, i64)` and return an `i32`
//! signum (-1 / 0 / +1). All arithmetic is exact i128; no float, no
//! epsilon, no rounding.
//!
//! Magnitude budget at the ADR-0054 coord cap (input fixed-point coords
//! ≤ ±2^24, so projected 2D differences ≤ ±2^25):
//!
//! - **orient2d**: a single 2×2 determinant of i64 differences;
//!   bounded by 2 · 2^25 · 2^25 = 2^51. Trivial.
//! - **in_circle**: 3×3 determinant whose third column carries
//!   squared-sum entries (≤ 2^51). Expanding along row 0 keeps the
//!   small (linear-difference, ≤ 2^25) multiplier on the outside of
//!   the largest 2×2 sub-determinant. Each of the three terms is
//!   bounded by 2^102; the sum is ≤ 3·2^102 < 2^104, well within
//!   i128's signed range with 23 bits of headroom (see ADR-0056
//!   amendment for the corrected analysis).

pub(super) type Point2 = (i64, i64);

/// Sign of `(b - a) × (c - a)`. Positive means `c` is strictly left of
/// the line `a → b` (CCW orientation of triangle `a, b, c`); negative
/// means strictly right (CW); zero means collinear.
pub(super) fn orient2d(a: Point2, b: Point2, c: Point2) -> i32 {
    let abx = (b.0 - a.0) as i128;
    let aby = (b.1 - a.1) as i128;
    let acx = (c.0 - a.0) as i128;
    let acy = (c.1 - a.1) as i128;
    (abx * acy - aby * acx).signum() as i32
}

/// Sign of the in-circle determinant for CCW triangle `a, b, c` and
/// query point `d`. Positive iff `d` is strictly inside the
/// circumcircle of `(a, b, c)`; negative iff strictly outside; zero iff
/// cocircular.
///
/// **The caller must ensure `(a, b, c)` is wound CCW** — the
/// predicate's sign convention assumes it. Passing CW triangles
/// inverts the result.
pub(super) fn in_circle(a: Point2, b: Point2, c: Point2, d: Point2) -> i32 {
    // Translate so D is at the origin: this lets the row-0 expansion
    // share the squared-sum entries cleanly.
    let ax = (a.0 - d.0) as i128;
    let ay = (a.1 - d.1) as i128;
    let bx = (b.0 - d.0) as i128;
    let by = (b.1 - d.1) as i128;
    let cx = (c.0 - d.0) as i128;
    let cy = (c.1 - d.1) as i128;
    let aa = ax * ax + ay * ay;
    let bb = bx * bx + by * by;
    let cc = cx * cx + cy * cy;

    // | ax  ay  aa |
    // | bx  by  bb |  expanded along row 0
    // | cx  cy  cc |
    //
    // = ax * (by*cc - bb*cy) - ay * (bx*cc - bb*cx) + aa * (bx*cy - by*cx)
    let t1 = ax * (by * cc - bb * cy);
    let t2 = ay * (bx * cc - bb * cx);
    let t3 = aa * (bx * cy - by * cx);
    (t1 - t2 + t3).signum() as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orient2d_ccw_triangle_returns_positive() {
        // Standard CCW triangle: counterclockwise around the centroid.
        assert_eq!(orient2d((0, 0), (10, 0), (5, 10)), 1);
    }

    #[test]
    fn orient2d_cw_triangle_returns_negative() {
        assert_eq!(orient2d((0, 0), (5, 10), (10, 0)), -1);
    }

    #[test]
    fn orient2d_collinear_returns_zero() {
        assert_eq!(orient2d((0, 0), (5, 0), (10, 0)), 0);
        assert_eq!(orient2d((0, 0), (5, 5), (10, 10)), 0);
        assert_eq!(orient2d((0, 0), (5, 5), (-5, -5)), 0);
    }

    #[test]
    fn orient2d_handles_extreme_coordinates() {
        // Coords near the projected-2D cap (±2^25). Verify no overflow.
        let big: i64 = 1 << 25;
        // Triangle (0, 0), (big, 0), (0, big) is CCW.
        assert_eq!(orient2d((0, 0), (big, 0), (0, big)), 1);
        // Reversing two corners flips winding.
        assert_eq!(orient2d((0, 0), (0, big), (big, 0)), -1);
    }

    /// CCW triangle inscribed in the unit circle (radius 2 to keep
    /// integer coords): (2, 0), (0, 2), (-2, 0). Circumcircle is centered
    /// at the origin with radius 2.
    fn unit_triangle() -> ((i64, i64), (i64, i64), (i64, i64)) {
        ((2, 0), (0, 2), (-2, 0))
    }

    #[test]
    fn in_circle_origin_is_inside_unit_triangle() {
        let (a, b, c) = unit_triangle();
        assert_eq!(in_circle(a, b, c, (0, 0)), 1);
    }

    #[test]
    fn in_circle_strict_interior_returns_positive() {
        let (a, b, c) = unit_triangle();
        assert_eq!(in_circle(a, b, c, (0, 1)), 1);
        assert_eq!(in_circle(a, b, c, (0, -1)), 1);
        assert_eq!(in_circle(a, b, c, (1, 1)), 1); // distance √2 < 2
    }

    #[test]
    fn in_circle_on_circle_returns_zero() {
        let (a, b, c) = unit_triangle();
        // (0, -2) lies on the circumcircle (distance 2 from origin).
        assert_eq!(in_circle(a, b, c, (0, -2)), 0);
        // The triangle's own vertices are obviously on the circle.
        assert_eq!(in_circle(a, b, c, a), 0);
        assert_eq!(in_circle(a, b, c, b), 0);
        assert_eq!(in_circle(a, b, c, c), 0);
    }

    #[test]
    fn in_circle_strict_exterior_returns_negative() {
        let (a, b, c) = unit_triangle();
        assert_eq!(in_circle(a, b, c, (0, -3)), -1);
        assert_eq!(in_circle(a, b, c, (3, 0)), -1);
        assert_eq!(in_circle(a, b, c, (-3, 0)), -1);
        assert_eq!(in_circle(a, b, c, (10, 10)), -1);
    }

    #[test]
    fn in_circle_passing_cw_triangle_inverts_sign() {
        // Document the CCW-required convention by demonstrating that CW
        // input flips the sign — an inside point reads as outside.
        let (a, b, c) = unit_triangle();
        assert_eq!(in_circle(a, b, c, (0, 0)), 1);
        assert_eq!(in_circle(c, b, a, (0, 0)), -1);
    }

    #[test]
    fn in_circle_handles_extreme_coordinates_without_overflow() {
        // Coords near the projected-2D cap (±2^25). The in-circle
        // determinant's worst-case intermediate value is ≤ 2^102; this
        // exercise should not panic or wrap.
        let big: i64 = 1 << 25;
        let a = (big, 0);
        let b = (0, big);
        let c = (-big, 0);
        // All three are equidistant from the origin (circumcircle through
        // the origin's "north / east / west" points at radius `big`).
        assert_eq!(in_circle(a, b, c, (0, 0)), 1);
        let beyond_radius = (0, -2 * big); // distance 2*big > big.
        assert_eq!(in_circle(a, b, c, beyond_radius), -1);
        // Point exactly on the circumcircle: (0, -big).
        assert_eq!(in_circle(a, b, c, (0, -big)), 0);
    }

    #[test]
    fn in_circle_off_origin_circumcenter() {
        // Move the circumcircle off the origin to verify the predicate
        // doesn't depend on a centered configuration. Triangle inscribed
        // in a circle centered at (10, 10), radius 5: pick any 3 points.
        let a = (15, 10); // east
        let b = (10, 15); // north
        let c = (5, 10); // west
        // CCW: orient2d should be positive.
        assert_eq!(orient2d(a, b, c), 1);
        // Circumcenter (10, 10) is inside.
        assert_eq!(in_circle(a, b, c, (10, 10)), 1);
        // Far-away point is outside.
        assert_eq!(in_circle(a, b, c, (100, 100)), -1);
        // South point of the circle: (10, 5) — on the circle.
        assert_eq!(in_circle(a, b, c, (10, 5)), 0);
    }

    #[test]
    fn predicates_are_deterministic() {
        // Same inputs must always produce the same output (no
        // dependency on hashmap order, float rounding, etc).
        let (a, b, c) = unit_triangle();
        for _ in 0..16 {
            assert_eq!(in_circle(a, b, c, (0, 0)), 1);
            assert_eq!(orient2d(a, b, c), 1);
        }
    }
}
