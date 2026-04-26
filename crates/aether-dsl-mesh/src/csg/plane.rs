//! Integer-coefficient plane representation: `n · P = d`.
//!
//! Computed from three integer points via the cross product. Side tests
//! and edge intersections use exact i128 arithmetic so classification
//! and topology never depend on float comparisons.
//!
//! Magnitude budget (input coords ≤ ±2^24 fixed units):
//! - edge components fit in i32 (≤ ±2^25 with margin)
//! - normal components are products of two edge components — up to
//!   `2^50` per term, fit in i64
//! - plane offset `d = n · a` — up to `2^51 · 2^24 = 2^75` per term,
//!   fits in i128
//! - side `n · P - d` — up to `2^51 · 2^24 = 2^75`, fits in i128

use super::point::Point3;

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

fn gcd_4(a: u128, b: u128, c: u128, d: u128) -> u128 {
    gcd_u128(gcd_u128(gcd_u128(a, b), c), d)
}

#[derive(Debug, Clone, Copy)]
pub struct Plane3 {
    pub n_x: i64,
    pub n_y: i64,
    pub n_z: i64,
    pub d: i128,
}

impl Plane3 {
    /// Construct the plane through three points (winding-CCW gives a
    /// right-handed normal). Returns a degenerate plane (zero normal)
    /// for collinear input — callers should filter via [`Self::is_degenerate`].
    pub fn from_points(a: Point3, b: Point3, c: Point3) -> Self {
        let e1x = b.x as i64 - a.x as i64;
        let e1y = b.y as i64 - a.y as i64;
        let e1z = b.z as i64 - a.z as i64;
        let e2x = c.x as i64 - a.x as i64;
        let e2y = c.y as i64 - a.y as i64;
        let e2z = c.z as i64 - a.z as i64;
        let n_x = e1y * e2z - e1z * e2y;
        let n_y = e1z * e2x - e1x * e2z;
        let n_z = e1x * e2y - e1y * e2x;
        let d = (n_x as i128) * (a.x as i128)
            + (n_y as i128) * (a.y as i128)
            + (n_z as i128) * (a.z as i128);
        Plane3 { n_x, n_y, n_z, d }
    }

    /// Zero-normal plane comes from collinear input (degenerate triangle).
    pub fn is_degenerate(&self) -> bool {
        self.n_x == 0 && self.n_y == 0 && self.n_z == 0
    }

    /// Signed integer side test. `> 0` in front of the plane (along the
    /// normal direction), `< 0` behind, `0` on the plane.
    pub fn side(&self, p: Point3) -> i128 {
        (self.n_x as i128) * (p.x as i128)
            + (self.n_y as i128) * (p.y as i128)
            + (self.n_z as i128) * (p.z as i128)
            - self.d
    }

    /// Snap-tolerance threshold for classifying a vertex as coplanar.
    ///
    /// `compute_intersection` snaps each new vertex to the integer grid
    /// via rounded division — the snap is up to 0.5 grid units off the
    /// partitioner per axis, contributing at most `0.5 * (|n_x| + |n_y| + |n_z|)`
    /// to `side()`. We use the full sum (a 2× margin) as the threshold
    /// for "definitely on this plane"; vertices with `|side| <= threshold`
    /// are classified as COPLANAR even though their integer side test is
    /// non-zero.
    ///
    /// This is the integer-arithmetic equivalent of csg.js's `EPSILON`
    /// constant — but unlike csg.js, the threshold is derived from the
    /// plane's own normal magnitude rather than a global guess, so it
    /// scales correctly across very small and very large meshes.
    /// Without it, snap drift in fragments of non-axis-aligned facets
    /// (cylinders, swept profiles, rotated boxes) makes the BSP
    /// classify split fragments as SPANNING against their own parent
    /// plane on subsequent passes, causing unbounded recursion.
    pub fn coplanar_threshold(&self) -> i128 {
        (self.n_x.unsigned_abs() as i128)
            + (self.n_y.unsigned_abs() as i128)
            + (self.n_z.unsigned_abs() as i128)
    }

    pub fn invert(self) -> Self {
        Plane3 {
            n_x: -self.n_x,
            n_y: -self.n_y,
            n_z: -self.n_z,
            d: -self.d,
        }
    }

    /// GCD-normalized canonical key for plane equality.
    ///
    /// Two coplanar polygons with parallel normals can have `Plane3`
    /// fields differing by a positive scalar (e.g. two triangles on the
    /// same face whose cross products differ in magnitude because the
    /// triangles themselves have different shapes). The canonical key
    /// divides `(n_x, n_y, n_z, d)` by their absolute GCD, collapsing
    /// proportional planes to the same key while preserving sign
    /// (so opposite-facing coplanar planes stay distinct).
    ///
    /// Used by the cleanup pipeline's coplanar grouping per ADR-0057
    /// — without it, CDT-triangulated faces re-emerge with one plane
    /// key per output triangle and never re-merge into n-gon faces.
    pub fn canonical_key(&self) -> (i64, i64, i64, i128) {
        let g = gcd_4(
            self.n_x.unsigned_abs() as u128,
            self.n_y.unsigned_abs() as u128,
            self.n_z.unsigned_abs() as u128,
            self.d.unsigned_abs(),
        );
        if g == 0 {
            // Fully zero plane (degenerate); leave as-is.
            return (self.n_x, self.n_y, self.n_z, self.d);
        }
        let g = g as i128;
        (
            (self.n_x as i128 / g) as i64,
            (self.n_y as i128 / g) as i64,
            (self.n_z as i128 / g) as i64,
            self.d / g,
        )
    }

    /// Sign of `dot(self.normal, other.normal)`. Used to distinguish
    /// coplanar-front from coplanar-back when classifying a polygon
    /// against a partitioner whose plane it shares.
    pub fn normal_dot_sign(&self, other: &Plane3) -> i32 {
        let dot = (self.n_x as i128) * (other.n_x as i128)
            + (self.n_y as i128) * (other.n_y as i128)
            + (self.n_z as i128) * (other.n_z as i128);
        dot.signum() as i32
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::csg::fixed::f32_to_fixed;

    fn p(x: f32, y: f32, z: f32) -> Point3 {
        Point3 {
            x: f32_to_fixed(x).unwrap(),
            y: f32_to_fixed(y).unwrap(),
            z: f32_to_fixed(z).unwrap(),
        }
    }

    #[test]
    fn xy_plane_at_origin() {
        // CCW from above gives +Z normal.
        let plane = Plane3::from_points(p(0.0, 0.0, 0.0), p(1.0, 0.0, 0.0), p(0.0, 1.0, 0.0));
        assert!(plane.n_x == 0 && plane.n_y == 0 && plane.n_z > 0);
        assert_eq!(plane.d, 0);
        assert_eq!(plane.side(p(0.0, 0.0, 1.0)).signum(), 1);
        assert_eq!(plane.side(p(0.0, 0.0, -1.0)).signum(), -1);
        assert_eq!(plane.side(p(0.5, 0.5, 0.0)).signum(), 0);
    }

    #[test]
    fn shifted_plane_offset_is_correct() {
        // Plane z = 2.
        let plane = Plane3::from_points(p(0.0, 0.0, 2.0), p(1.0, 0.0, 2.0), p(0.0, 1.0, 2.0));
        assert_eq!(plane.side(p(0.0, 0.0, 2.0)).signum(), 0);
        assert_eq!(plane.side(p(0.0, 0.0, 3.0)).signum(), 1);
        assert_eq!(plane.side(p(0.0, 0.0, 1.0)).signum(), -1);
    }

    #[test]
    fn invert_flips_side() {
        let plane = Plane3::from_points(p(0.0, 0.0, 0.0), p(1.0, 0.0, 0.0), p(0.0, 1.0, 0.0));
        let above = p(0.0, 0.0, 1.0);
        assert_eq!(plane.side(above).signum(), 1);
        assert_eq!(plane.invert().side(above).signum(), -1);
    }

    #[test]
    fn collinear_points_produce_degenerate_plane() {
        let plane = Plane3::from_points(p(0.0, 0.0, 0.0), p(1.0, 0.0, 0.0), p(2.0, 0.0, 0.0));
        assert!(plane.is_degenerate());
    }

    #[test]
    fn normal_dot_sign_distinguishes_orientation() {
        let up = Plane3::from_points(p(0.0, 0.0, 0.0), p(1.0, 0.0, 0.0), p(0.0, 1.0, 0.0));
        let down = up.invert();
        assert!(up.normal_dot_sign(&up) > 0);
        assert!(up.normal_dot_sign(&down) < 0);
    }

    #[test]
    fn canonical_key_collapses_proportional_planes() {
        // Two triangles on the same physical plane (z = 0 in fixed
        // point), with different cross-product magnitudes — different
        // raw plane fields but the same canonical key.
        let small = Plane3::from_points(p(0.0, 0.0, 0.0), p(1.0, 0.0, 0.0), p(0.0, 1.0, 0.0));
        let large = Plane3::from_points(p(0.0, 0.0, 0.0), p(2.0, 0.0, 0.0), p(0.0, 2.0, 0.0));
        // Raw normals differ by factor 4 (cross product scales with edge length).
        assert_ne!(small.n_z, large.n_z);
        // But canonical keys match.
        assert_eq!(small.canonical_key(), large.canonical_key());
    }

    #[test]
    fn canonical_key_distinguishes_opposite_normals() {
        let up = Plane3::from_points(p(0.0, 0.0, 0.0), p(1.0, 0.0, 0.0), p(0.0, 1.0, 0.0));
        let down = up.invert();
        assert_ne!(up.canonical_key(), down.canonical_key());
    }

    #[test]
    fn extreme_input_does_not_overflow() {
        // Coordinates near the ±256 boundary — verify i128 headroom.
        let plane = Plane3::from_points(p(256.0, 0.0, 0.0), p(0.0, 256.0, 0.0), p(0.0, 0.0, 256.0));
        // Point inside this triangle's span
        let inside = p(100.0, 100.0, 100.0);
        let _ = plane.side(inside); // must not panic / overflow
    }

    /// Construct a Point3 from raw fixed-point integer fields. Use this
    /// for ULP-precision tests where the f32 → fixed snap would round
    /// the test input away from the value we're trying to assert about.
    fn pi(x: i32, y: i32, z: i32) -> Point3 {
        Point3 { x, y, z }
    }

    // ── coplanar_threshold characterization ──────────────────────────
    //
    // The bug hypothesis (per regression.rs ignored tests) is that this
    // threshold is too generous for non-axis-aligned planes — sphere
    // and cylinder facets get classified COPLANAR when they shouldn't.
    // These tests pin the formula and quantify the asymmetry between
    // axis-aligned and diagonal planes.

    #[test]
    fn coplanar_threshold_is_l1_norm_of_normal() {
        // Pin the formula: threshold = |n_x| + |n_y| + |n_z|.
        // If a future change switches to L2 norm or some other metric,
        // this test fails loudly so we can audit the BSP classification
        // boundary that follows.
        let xy_plane = Plane3::from_points(p(0.0, 0.0, 0.0), p(1.0, 0.0, 0.0), p(0.0, 1.0, 0.0));
        assert_eq!(
            xy_plane.coplanar_threshold(),
            xy_plane.n_x.unsigned_abs() as i128
                + xy_plane.n_y.unsigned_abs() as i128
                + xy_plane.n_z.unsigned_abs() as i128
        );
        let diag = Plane3::from_points(p(1.0, 0.0, 0.0), p(0.0, 1.0, 0.0), p(0.0, 0.0, 1.0));
        assert_eq!(
            diag.coplanar_threshold(),
            diag.n_x.unsigned_abs() as i128
                + diag.n_y.unsigned_abs() as i128
                + diag.n_z.unsigned_abs() as i128
        );
    }

    #[test]
    fn coplanar_threshold_axis_aligned_one_ulp_off_is_at_threshold() {
        // For an axis-aligned plane, a vertex one fixed-point ULP off
        // the plane in the normal direction has |side| exactly equal to
        // the threshold. With `<=` comparison in the BSP, that vertex is
        // classified COPLANAR. This is the expected behavior — a 1-ULP
        // snap drift after intersection should not re-trigger SPANNING
        // classification on the next BSP pass.
        let xy_plane = Plane3::from_points(p(0.0, 0.0, 0.0), p(1.0, 0.0, 0.0), p(0.0, 1.0, 0.0));
        // Point one fixed ULP above the plane in z.
        let one_ulp_above = pi(0, 0, 1);
        assert_eq!(
            xy_plane.side(one_ulp_above).unsigned_abs(),
            xy_plane.coplanar_threshold() as u128,
            "axis-aligned: 1-ULP-off vertex must sit at threshold boundary"
        );
    }

    #[test]
    fn coplanar_threshold_diagonal_overestimates_perpendicular_distance() {
        // **Bug-pinning test.** For a diagonal plane (normal = (n,n,n))
        // the threshold is `3n` but the perpendicular distance per unit
        // |side| is `1 / (n * sqrt(3))`. So a vertex with |side| = n
        // (one-third of the threshold) is at perpendicular distance
        // `1 / sqrt(3)` ≈ 0.577 fixed units from the plane — and the
        // threshold catches vertices up to perpendicular distance
        // `sqrt(3)` ≈ 1.732 fixed units away.
        //
        // Compare to axis-aligned where the threshold catches up to
        // 1.0 fixed unit perpendicular. The diagonal plane's threshold
        // is `sqrt(3)` ≈ 1.73× too generous in perpendicular terms.
        // This is the strongest candidate for the BSP misclassification
        // observed in box - sphere and box - cylinder regressions.
        let diag = Plane3::from_points(p(1.0, 0.0, 0.0), p(0.0, 1.0, 0.0), p(0.0, 0.0, 1.0));
        // a = (65536, 0, 0) fixed; one ULP off in +x direction.
        let one_ulp_off_in_x = pi(65537, 0, 0);
        let side_mag = diag.side(one_ulp_off_in_x).unsigned_abs();
        let threshold = diag.coplanar_threshold() as u128;
        // Per the geometry above: |side| should be exactly threshold / 3.
        assert_eq!(
            side_mag * 3,
            threshold,
            "diagonal: |side| of 1-ULP-off-along-axis should be 1/3 of threshold"
        );
        // And it's well within the threshold — confirming the asymmetry.
        assert!(side_mag < threshold);
    }

    #[test]
    fn coplanar_threshold_unchanged_by_invert() {
        // Symmetric polarity — inverting the plane must leave the
        // threshold magnitude untouched. If a future formula picks up
        // a sign asymmetry, BSP classification stability across CSG
        // operations (which heavily rely on plane inversion) breaks.
        let plane = Plane3::from_points(p(1.0, 0.0, 0.0), p(0.0, 1.0, 0.0), p(0.0, 0.0, 1.0));
        assert_eq!(
            plane.coplanar_threshold(),
            plane.invert().coplanar_threshold()
        );
    }

    // ── from_points coverage ──────────────────────────────────────────

    #[test]
    fn from_points_diagonal_normal() {
        // Triangle through (1,0,0), (0,1,0), (0,0,1). Normal points away
        // from origin. Pin the structure of the diagonal plane case so a
        // future change to the cross-product order is caught.
        let plane = Plane3::from_points(p(1.0, 0.0, 0.0), p(0.0, 1.0, 0.0), p(0.0, 0.0, 1.0));
        assert_eq!(plane.n_x, plane.n_y);
        assert_eq!(plane.n_y, plane.n_z);
        assert!(
            plane.n_x > 0,
            "winding-CCW from outside-origin gives outward normal"
        );
        // Origin is on the *negative* side of this plane.
        assert!(plane.side(p(0.0, 0.0, 0.0)) < 0);
        // Far-from-origin point along (+,+,+) is on the positive side.
        assert!(plane.side(p(2.0, 2.0, 2.0)) > 0);
    }

    #[test]
    fn winding_reversal_flips_normal() {
        // Reversing two of the three points reverses the winding and
        // flips the normal. BSP relies on this for orientation — if it
        // ever breaks, every back-face polygon would re-classify as
        // front-face on the next BSP pass.
        let ccw = Plane3::from_points(p(0.0, 0.0, 0.0), p(1.0, 0.0, 0.0), p(0.0, 1.0, 0.0));
        let cw = Plane3::from_points(p(0.0, 0.0, 0.0), p(0.0, 1.0, 0.0), p(1.0, 0.0, 0.0));
        assert_eq!(ccw.n_x, -cw.n_x);
        assert_eq!(ccw.n_y, -cw.n_y);
        assert_eq!(ccw.n_z, -cw.n_z);
        assert_eq!(ccw.d, -cw.d);
    }

    #[test]
    fn cyclic_permutation_preserves_plane() {
        // (a, b, c), (b, c, a), (c, a, b) all give the same plane —
        // CCW winding is preserved under cyclic shift. Without this,
        // BSP would classify the same triangle differently depending on
        // which vertex was "first," which would produce non-deterministic
        // tree shapes across runs.
        let abc = Plane3::from_points(p(0.0, 0.0, 0.0), p(1.0, 0.0, 0.0), p(0.0, 1.0, 0.0));
        let bca = Plane3::from_points(p(1.0, 0.0, 0.0), p(0.0, 1.0, 0.0), p(0.0, 0.0, 0.0));
        let cab = Plane3::from_points(p(0.0, 1.0, 0.0), p(0.0, 0.0, 0.0), p(1.0, 0.0, 0.0));
        assert_eq!((abc.n_x, abc.n_y, abc.n_z), (bca.n_x, bca.n_y, bca.n_z));
        assert_eq!((abc.n_x, abc.n_y, abc.n_z), (cab.n_x, cab.n_y, cab.n_z));
        assert_eq!(abc.d, bca.d);
        assert_eq!(abc.d, cab.d);
    }

    // ── is_degenerate coverage ────────────────────────────────────────

    #[test]
    fn three_identical_points_are_degenerate() {
        let q = p(1.0, 2.0, 3.0);
        assert!(Plane3::from_points(q, q, q).is_degenerate());
    }

    #[test]
    fn two_identical_points_are_degenerate() {
        // a == b: edge1 is zero, cross product is zero.
        let a = p(1.0, 2.0, 3.0);
        let b = p(1.0, 2.0, 3.0);
        let c = p(0.0, 0.0, 0.0);
        assert!(Plane3::from_points(a, b, c).is_degenerate());
        // Also a == c.
        let c2 = p(1.0, 2.0, 3.0);
        let b2 = p(0.0, 0.0, 0.0);
        assert!(Plane3::from_points(a, b2, c2).is_degenerate());
    }

    // ── side() linearity and magnitude ────────────────────────────────

    #[test]
    fn side_magnitude_scales_linearly_with_offset() {
        // For plane z = 0 with normal (0, 0, n_z), side(point at z = k)
        // must equal k * n_z. Catches any asymmetric implementation
        // (e.g. a stray quadratic term picked up during a refactor).
        let plane = Plane3::from_points(p(0.0, 0.0, 0.0), p(1.0, 0.0, 0.0), p(0.0, 1.0, 0.0));
        let n_z = plane.n_z as i128;
        for k in [-3, -1, 1, 5, 17] {
            let point = pi(0, 0, k);
            assert_eq!(plane.side(point), n_z * k as i128, "non-linear at k={k}");
        }
    }

    #[test]
    fn side_is_affine_in_position() {
        // The defining property of a plane's side function:
        // side(p + Δ) - side(p) == n · Δ for any displacement Δ.
        // Catches any future change that introduces non-affine terms.
        let plane = Plane3::from_points(p(1.0, 2.0, 0.0), p(2.0, 0.0, 1.0), p(0.0, 1.0, 2.0));
        let p0 = pi(100, 200, 300);
        let delta = pi(7, -11, 13);
        let p1 = pi(p0.x + delta.x, p0.y + delta.y, p0.z + delta.z);
        let observed = plane.side(p1) - plane.side(p0);
        let expected = (plane.n_x as i128) * (delta.x as i128)
            + (plane.n_y as i128) * (delta.y as i128)
            + (plane.n_z as i128) * (delta.z as i128);
        assert_eq!(observed, expected);
    }

    // ── invert() ──────────────────────────────────────────────────────

    #[test]
    fn invert_is_involution() {
        // Double-invert restores all four fields exactly. Pinned via
        // field-by-field comparison since Plane3 doesn't derive Eq.
        let plane = Plane3::from_points(p(1.0, 2.0, 3.0), p(4.0, -1.0, 0.5), p(-2.0, 0.0, 1.5));
        let twice = plane.invert().invert();
        assert_eq!(plane.n_x, twice.n_x);
        assert_eq!(plane.n_y, twice.n_y);
        assert_eq!(plane.n_z, twice.n_z);
        assert_eq!(plane.d, twice.d);
    }

    // ── canonical_key() ───────────────────────────────────────────────

    #[test]
    fn canonical_key_idempotent() {
        // A canonical key fed back through `from_points`-equivalent
        // construction should produce a plane that already has the same
        // key. We approximate this by reading the key, building a plane
        // with those fields directly, and re-keying.
        let plane = Plane3::from_points(p(0.0, 0.0, 0.0), p(2.0, 0.0, 0.0), p(0.0, 3.0, 0.0));
        let key = plane.canonical_key();
        let normalized = Plane3 {
            n_x: key.0,
            n_y: key.1,
            n_z: key.2,
            d: key.3,
        };
        assert_eq!(normalized.canonical_key(), key);
    }

    #[test]
    fn canonical_key_collapses_large_scale_difference() {
        // Two triangles on the same plane whose cross products differ
        // by a large factor (100×) — both must produce the same key.
        // Mirrors the case of CSG output where one face emits a tiny
        // triangle alongside a large one.
        let small = Plane3::from_points(p(0.0, 0.0, 0.0), p(0.5, 0.0, 0.0), p(0.0, 0.5, 0.0));
        let large = Plane3::from_points(p(0.0, 0.0, 0.0), p(50.0, 0.0, 0.0), p(0.0, 50.0, 0.0));
        // Raw normals differ by factor 100^2 = 10_000.
        assert_eq!(small.canonical_key(), large.canonical_key());
    }

    // ── normal_dot_sign() ─────────────────────────────────────────────

    #[test]
    fn normal_dot_sign_perpendicular_planes_returns_zero() {
        // xy-plane and yz-plane have perpendicular normals.
        let xy = Plane3::from_points(p(0.0, 0.0, 0.0), p(1.0, 0.0, 0.0), p(0.0, 1.0, 0.0));
        let yz = Plane3::from_points(p(0.0, 0.0, 0.0), p(0.0, 1.0, 0.0), p(0.0, 0.0, 1.0));
        assert_eq!(xy.normal_dot_sign(&yz), 0);
        assert_eq!(yz.normal_dot_sign(&xy), 0);
    }

    #[test]
    fn normal_dot_sign_acute_angle_is_positive() {
        // xy-plane (+z normal) vs a plane tilted slightly toward +z.
        // Their normals form an acute angle so dot is positive.
        let xy = Plane3::from_points(p(0.0, 0.0, 0.0), p(1.0, 0.0, 0.0), p(0.0, 1.0, 0.0));
        let tilt = Plane3::from_points(p(0.0, 0.0, 0.0), p(1.0, 0.0, 0.5), p(0.0, 1.0, 0.5));
        // tilt has +z component; dot with (+z) normal of xy must be > 0.
        assert!(xy.normal_dot_sign(&tilt) > 0);
    }

    // ── private gcd helpers ───────────────────────────────────────────

    #[test]
    fn gcd_u128_zero_identity() {
        // gcd(0, x) == x and gcd(x, 0) == x — the additive identity
        // of the gcd monoid. Important for canonical_key when one
        // normal component is zero (axis-aligned planes).
        assert_eq!(gcd_u128(0, 7), 7);
        assert_eq!(gcd_u128(7, 0), 7);
        assert_eq!(gcd_u128(0, 0), 0);
    }

    #[test]
    fn gcd_u128_coprime_yields_one() {
        assert_eq!(gcd_u128(3, 5), 1);
        assert_eq!(gcd_u128(13, 17), 1);
        assert_eq!(gcd_u128(9, 16), 1);
    }

    #[test]
    fn gcd_u128_known_values() {
        assert_eq!(gcd_u128(12, 18), 6);
        assert_eq!(gcd_u128(100, 75), 25);
        assert_eq!(gcd_u128(1024, 768), 256);
    }

    #[test]
    fn gcd_4_with_zeros() {
        // canonical_key uses gcd_4 across (n_x, n_y, n_z, d). For an
        // axis-aligned plane through origin two of those are zero —
        // gcd_4 must reduce to gcd of the non-zero terms.
        assert_eq!(gcd_4(0, 0, 12, 18), 6);
        assert_eq!(gcd_4(0, 18, 0, 12), 6);
        assert_eq!(gcd_4(12, 0, 0, 18), 6);
        assert_eq!(gcd_4(0, 0, 0, 7), 7);
        assert_eq!(gcd_4(0, 0, 0, 0), 0);
    }

    // ── magnitude budget adversarial test ─────────────────────────────

    #[test]
    fn side_at_extreme_inputs_does_not_overflow() {
        // Plane through three corners of the ±256 cube — the maximum-
        // coefficient construction. Query side at the opposite corner.
        // The doc claims i128 headroom; this test would panic on
        // overflow under debug.
        let plane = Plane3::from_points(
            p(256.0, -256.0, -256.0),
            p(-256.0, 256.0, -256.0),
            p(-256.0, -256.0, 256.0),
        );
        let extreme_query = p(256.0, 256.0, 256.0);
        let s = plane.side(extreme_query);
        // The opposite corner from the plane's centroid must be on
        // the positive side (the normal points outward).
        assert!(
            s != 0,
            "extreme query should not coincidentally lie on plane"
        );
    }
}
