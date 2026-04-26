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
    fn extreme_input_does_not_overflow() {
        // Coordinates near the ±256 boundary — verify i128 headroom.
        let plane = Plane3::from_points(p(256.0, 0.0, 0.0), p(0.0, 256.0, 0.0), p(0.0, 0.0, 256.0));
        // Point inside this triangle's span
        let inside = p(100.0, 100.0, 100.0);
        let _ = plane.side(inside); // must not panic / overflow
    }
}
