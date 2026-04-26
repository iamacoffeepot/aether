//! Convex polygon over integer-grid vertices, plus the split-against-plane
//! routine that drives BSP construction and clipping.
//!
//! Each polygon carries its own plane (cached at construction) and a
//! color tag inherited from the source mesh. After splitting, both
//! sub-fragments inherit the original polygon's plane and color — even
//! though the split may push new vertices fractionally off the cached
//! plane after integer rounding, the fragments are still treated as
//! coplanar with the parent for downstream classification.

use super::plane::Plane3;
use super::point::Point3;

#[derive(Debug, Clone)]
pub struct Polygon {
    pub vertices: Vec<Point3>,
    pub plane: Plane3,
    pub color: u32,
}

const COPLANAR: i32 = 0;
const FRONT: i32 = 1;
const BACK: i32 = 2;
const SPANNING: i32 = 3;

impl Polygon {
    /// Construct a polygon from a triangle. Returns `None` if the input
    /// is degenerate (collinear vertices → zero-normal plane).
    pub fn from_triangle(v0: Point3, v1: Point3, v2: Point3, color: u32) -> Option<Self> {
        let plane = Plane3::from_points(v0, v1, v2);
        if plane.is_degenerate() {
            return None;
        }
        Some(Polygon {
            vertices: vec![v0, v1, v2],
            plane,
            color,
        })
    }

    /// Reverse winding and flip the cached plane.
    pub fn invert(&mut self) {
        self.vertices.reverse();
        self.plane = self.plane.invert();
    }

    /// Classify this polygon against `partitioner` and route it into
    /// one (or two) of the four output buckets.
    ///
    /// `coplanar_front` / `coplanar_back` receive polygons whose plane
    /// matches the partitioner; the orientation distinction is needed
    /// so shared boundaries are processed symmetrically during CSG.
    /// `front` / `back` receive halves of polygons that span the plane.
    pub fn split(
        &self,
        partitioner: &Plane3,
        coplanar_front: &mut Vec<Polygon>,
        coplanar_back: &mut Vec<Polygon>,
        front: &mut Vec<Polygon>,
        back: &mut Vec<Polygon>,
    ) {
        // Snap-drift tolerance: a vertex within `coplanar_threshold()`
        // of the plane is treated as on it. This is what stops the
        // unbounded-recursion cascade where a split fragment's snapped
        // intersection vertex would otherwise re-classify as FRONT/BACK
        // against its own parent plane on subsequent BSP passes (see
        // `Plane3::coplanar_threshold` for the derivation).
        let threshold = partitioner.coplanar_threshold();
        let mut polygon_type = COPLANAR;
        let mut types: Vec<i32> = Vec::with_capacity(self.vertices.len());
        for v in &self.vertices {
            let s = partitioner.side(*v);
            let t = if s > threshold {
                FRONT
            } else if s < -threshold {
                BACK
            } else {
                COPLANAR
            };
            polygon_type |= t;
            types.push(t);
        }

        match polygon_type {
            COPLANAR => {
                if partitioner.normal_dot_sign(&self.plane) > 0 {
                    coplanar_front.push(self.clone());
                } else {
                    coplanar_back.push(self.clone());
                }
            }
            FRONT => front.push(self.clone()),
            BACK => back.push(self.clone()),
            _ => {
                // SPANNING: at least one vertex on each side. Walk the
                // edges, producing a front fragment and a back fragment.
                let n = self.vertices.len();
                let mut f = Vec::with_capacity(n + 1);
                let mut b = Vec::with_capacity(n + 1);
                for i in 0..n {
                    let j = (i + 1) % n;
                    let ti = types[i];
                    let tj = types[j];
                    let vi = self.vertices[i];
                    let vj = self.vertices[j];
                    if ti != BACK {
                        f.push(vi);
                    }
                    if ti != FRONT {
                        b.push(vi);
                    }
                    if (ti | tj) == SPANNING {
                        let split_pt = compute_intersection(vi, vj, partitioner);
                        f.push(split_pt);
                        b.push(split_pt);
                    }
                }
                if f.len() >= 3 {
                    front.push(Polygon {
                        vertices: f,
                        plane: self.plane,
                        color: self.color,
                    });
                }
                if b.len() >= 3 {
                    back.push(Polygon {
                        vertices: b,
                        plane: self.plane,
                        color: self.color,
                    });
                }
            }
        }
    }
}

/// Edge-vs-plane intersection in exact i128, snapped to the integer grid.
///
/// `I_k = (s0 · P1_k − s1 · P0_k) / (s0 − s1)` where `s0`, `s1` are the
/// signed sides of the endpoints. Snapped to nearest with ties resolved
/// away from zero (mirrors the behavior of `f64::round` closely enough
/// for our purposes — the snapped point is allowed to drift up to one
/// grid unit off the partitioner; classifications stay consistent
/// because the integer side test gives a definitive sign for the
/// snapped point).
fn compute_intersection(p0: Point3, p1: Point3, plane: &Plane3) -> Point3 {
    let s0 = plane.side(p0);
    let s1 = plane.side(p1);
    let denom = s0 - s1;
    debug_assert!(denom != 0, "split called on edge that does not cross plane");

    let intersect_axis = |a0: i32, a1: i32| -> i32 {
        let numer = s0 * (a1 as i128) - s1 * (a0 as i128);
        round_div(numer, denom) as i32
    };

    Point3 {
        x: intersect_axis(p0.x, p1.x),
        y: intersect_axis(p0.y, p1.y),
        z: intersect_axis(p0.z, p1.z),
    }
}

/// Integer division rounded to nearest, ties away from zero.
fn round_div(numer: i128, denom: i128) -> i128 {
    debug_assert!(denom != 0);
    let half = denom.abs() / 2;
    if (numer >= 0) == (denom > 0) {
        (numer + half * denom.signum()) / denom
    } else {
        (numer - half * denom.signum()) / denom
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::csg::fixed::f32_to_fixed;

    fn pt(x: f32, y: f32, z: f32) -> Point3 {
        Point3 {
            x: f32_to_fixed(x).unwrap(),
            y: f32_to_fixed(y).unwrap(),
            z: f32_to_fixed(z).unwrap(),
        }
    }

    #[test]
    fn degenerate_triangle_returns_none() {
        // Collinear points along x-axis.
        assert!(
            Polygon::from_triangle(pt(0.0, 0.0, 0.0), pt(1.0, 0.0, 0.0), pt(2.0, 0.0, 0.0), 0)
                .is_none()
        );
    }

    #[test]
    fn invert_reverses_winding_and_plane() {
        let mut poly =
            Polygon::from_triangle(pt(0.0, 0.0, 0.0), pt(1.0, 0.0, 0.0), pt(0.0, 1.0, 0.0), 0)
                .unwrap();
        let original_plane_z = poly.plane.n_z;
        poly.invert();
        assert_eq!(poly.vertices[0], pt(0.0, 1.0, 0.0));
        assert_eq!(poly.plane.n_z, -original_plane_z);
    }

    #[test]
    fn polygon_entirely_in_front_routes_to_front() {
        // Triangle at z=1, partitioner at z=0 (xy plane).
        let poly =
            Polygon::from_triangle(pt(0.0, 0.0, 1.0), pt(1.0, 0.0, 1.0), pt(0.0, 1.0, 1.0), 0)
                .unwrap();
        let partitioner =
            Plane3::from_points(pt(0.0, 0.0, 0.0), pt(1.0, 0.0, 0.0), pt(0.0, 1.0, 0.0));
        let mut cof = vec![];
        let mut cob = vec![];
        let mut f = vec![];
        let mut b = vec![];
        poly.split(&partitioner, &mut cof, &mut cob, &mut f, &mut b);
        assert_eq!(f.len(), 1);
        assert!(cof.is_empty() && cob.is_empty() && b.is_empty());
    }

    #[test]
    fn polygon_entirely_behind_routes_to_back() {
        let poly = Polygon::from_triangle(
            pt(0.0, 0.0, -1.0),
            pt(1.0, 0.0, -1.0),
            pt(0.0, 1.0, -1.0),
            0,
        )
        .unwrap();
        let partitioner =
            Plane3::from_points(pt(0.0, 0.0, 0.0), pt(1.0, 0.0, 0.0), pt(0.0, 1.0, 0.0));
        let mut cof = vec![];
        let mut cob = vec![];
        let mut f = vec![];
        let mut b = vec![];
        poly.split(&partitioner, &mut cof, &mut cob, &mut f, &mut b);
        assert_eq!(b.len(), 1);
        assert!(cof.is_empty() && cob.is_empty() && f.is_empty());
    }

    #[test]
    fn coplanar_aligned_normal_routes_to_coplanar_front() {
        let poly =
            Polygon::from_triangle(pt(0.0, 0.0, 0.0), pt(1.0, 0.0, 0.0), pt(0.0, 1.0, 0.0), 0)
                .unwrap();
        let partitioner = poly.plane;
        let mut cof = vec![];
        let mut cob = vec![];
        let mut f = vec![];
        let mut b = vec![];
        poly.split(&partitioner, &mut cof, &mut cob, &mut f, &mut b);
        assert_eq!(cof.len(), 1);
        assert!(cob.is_empty() && f.is_empty() && b.is_empty());
    }

    #[test]
    fn coplanar_opposed_normal_routes_to_coplanar_back() {
        let poly =
            Polygon::from_triangle(pt(0.0, 0.0, 0.0), pt(1.0, 0.0, 0.0), pt(0.0, 1.0, 0.0), 0)
                .unwrap();
        let partitioner = poly.plane.invert();
        let mut cof = vec![];
        let mut cob = vec![];
        let mut f = vec![];
        let mut b = vec![];
        poly.split(&partitioner, &mut cof, &mut cob, &mut f, &mut b);
        assert_eq!(cob.len(), 1);
        assert!(cof.is_empty() && f.is_empty() && b.is_empty());
    }

    #[test]
    fn spanning_triangle_splits_into_front_and_back() {
        // Triangle straddling z = 0.
        let poly = Polygon::from_triangle(
            pt(-1.0, 0.0, -1.0),
            pt(1.0, 0.0, -1.0),
            pt(0.0, 0.0, 1.0),
            0,
        )
        .unwrap();
        let partitioner =
            Plane3::from_points(pt(0.0, 0.0, 0.0), pt(1.0, 0.0, 0.0), pt(0.0, 1.0, 0.0));
        let mut cof = vec![];
        let mut cob = vec![];
        let mut f = vec![];
        let mut b = vec![];
        poly.split(&partitioner, &mut cof, &mut cob, &mut f, &mut b);
        assert_eq!(f.len(), 1);
        assert_eq!(b.len(), 1);
        assert!(cof.is_empty() && cob.is_empty());
        // The front fragment is a triangle (apex + 2 split points). Back
        // fragment is a quad (2 base verts + 2 split points).
        assert_eq!(f[0].vertices.len(), 3);
        assert_eq!(b[0].vertices.len(), 4);
    }

    #[test]
    fn split_preserves_color() {
        let poly = Polygon::from_triangle(
            pt(-1.0, 0.0, -1.0),
            pt(1.0, 0.0, -1.0),
            pt(0.0, 0.0, 1.0),
            42,
        )
        .unwrap();
        let partitioner =
            Plane3::from_points(pt(0.0, 0.0, 0.0), pt(1.0, 0.0, 0.0), pt(0.0, 1.0, 0.0));
        let mut cof = vec![];
        let mut cob = vec![];
        let mut f = vec![];
        let mut b = vec![];
        poly.split(&partitioner, &mut cof, &mut cob, &mut f, &mut b);
        assert_eq!(f[0].color, 42);
        assert_eq!(b[0].color, 42);
    }

    #[test]
    fn round_div_rounds_to_nearest() {
        assert_eq!(round_div(10, 4), 3); // 2.5 → 3 (away from zero)
        assert_eq!(round_div(-10, 4), -3);
        assert_eq!(round_div(10, -4), -3);
        assert_eq!(round_div(-10, -4), 3);
        assert_eq!(round_div(7, 4), 2); // 1.75 → 2
        assert_eq!(round_div(5, 4), 1); // 1.25 → 1
    }
}
