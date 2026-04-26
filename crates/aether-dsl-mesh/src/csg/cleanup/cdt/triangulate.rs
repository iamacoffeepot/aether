//! Public CDT entry point: project loops to 2D, build Delaunay, enforce
//! boundary edges as constraints, mark inside vs outside, return the
//! triangles inside the polygon.
//!
//! This is the function `merge::process_component` calls instead of the
//! ear-clipping path. Inputs are the welded vertex pool, the boundary
//! loops (outer + holes) as `VertexId` sequences, and the plane (for the
//! 2D projection). Output is `Vec<[VertexId; 3]>` mapped back to the
//! original `VertexId`s.
//!
//! On failure (constraint enforcement gives up, super-triangle setup
//! breaks down, etc.) returns `None`. The caller falls back to emitting
//! the input polygons as fans rather than producing broken geometry.

use super::super::mesh::VertexId;
use super::bowyer_watson::Mesh;
use super::predicates::Point2;
#[cfg(test)]
use super::predicates::orient2d;
use crate::csg::plane::Plane3;
use crate::csg::point::Point3;

#[derive(Debug, Clone, Copy)]
enum Axis {
    X,
    Y,
    Z,
}

fn projection_axes(plane: &Plane3) -> (Axis, Axis) {
    let ax = plane.n_x.unsigned_abs();
    let ay = plane.n_y.unsigned_abs();
    let az = plane.n_z.unsigned_abs();
    if ax >= ay && ax >= az {
        if plane.n_x >= 0 {
            (Axis::Y, Axis::Z)
        } else {
            (Axis::Z, Axis::Y)
        }
    } else if ay >= az {
        if plane.n_y >= 0 {
            (Axis::Z, Axis::X)
        } else {
            (Axis::X, Axis::Z)
        }
    } else if plane.n_z >= 0 {
        (Axis::X, Axis::Y)
    } else {
        (Axis::Y, Axis::X)
    }
}

fn project_point(p: Point3, axis_a: Axis, axis_b: Axis) -> Point2 {
    let pick = |a: Axis| -> i64 {
        match a {
            Axis::X => p.x as i64,
            Axis::Y => p.y as i64,
            Axis::Z => p.z as i64,
        }
    };
    (pick(axis_a), pick(axis_b))
}

/// Triangulate a polygon-with-holes using constrained Delaunay. The
/// caller passes the welded `vertices` pool, the boundary `loops`
/// (outer first or in any order — orientation determines inside via
/// signed area on the projected loops), and the `plane` for projection.
/// Returns the triangulation as `[VertexId; 3]` triples in the
/// original vertex pool's indexing, or `None` if the algorithm could
/// not produce a valid result.
pub(in crate::csg::cleanup) fn triangulate(
    vertices: &[Point3],
    loops: &[Vec<VertexId>],
    plane: &Plane3,
) -> Option<Vec<[VertexId; 3]>> {
    if loops.is_empty() {
        return Some(Vec::new());
    }

    // 1. Project loops to 2D and collect unique vertex ids in order of
    // first appearance. Build the index translation table.
    let (axis_a, axis_b) = projection_axes(plane);
    let mut input_ids: Vec<VertexId> = Vec::new();
    let mut id_to_local: std::collections::HashMap<VertexId, usize> =
        std::collections::HashMap::new();
    for loop_ in loops {
        for &vid in loop_ {
            if let std::collections::hash_map::Entry::Vacant(e) = id_to_local.entry(vid) {
                e.insert(input_ids.len());
                input_ids.push(vid);
            }
        }
    }
    if input_ids.len() < 3 {
        return None;
    }
    let projected: Vec<Point2> = input_ids
        .iter()
        .map(|&v| project_point(vertices[v], axis_a, axis_b))
        .collect();

    // 2. Build the unconstrained Delaunay triangulation. Bowyer-Watson
    // adds a super-triangle, so vertex indices in the mesh are offset by
    // `mesh.super_count` from our local indices.
    let mut mesh = Mesh::build(projected);
    let super_count = mesh.super_count;

    // 3. Convert each loop edge to a constraint and enforce it.
    let mut constraints: std::collections::HashSet<(usize, usize)> =
        std::collections::HashSet::new();
    for loop_ in loops {
        let n = loop_.len();
        for i in 0..n {
            let u = id_to_local[&loop_[i]] + super_count;
            let v = id_to_local[&loop_[(i + 1) % n]] + super_count;
            if u == v {
                continue;
            }
            // Canonical (smaller first) for the dedup set.
            let canon = if u < v { (u, v) } else { (v, u) };
            constraints.insert(canon);
            // Sequential enforcement: each call may flip many edges.
            mesh.enforce_constraint(u, v).ok()?;
        }
    }

    // 3b. Lawson re-Delaunization. Constraint enforcement flips diagonals
    // to thread the boundary through the mesh, but the new diagonals
    // adjacent to the constraints are not necessarily locally Delaunay —
    // which is what produces visible slivers in the merged regions even
    // though the topology is correct. For each non-constraint edge, run
    // the standard in-circle test against the opposite vertex; flip if
    // it fails. Iterate to a fixed point.
    mesh.lawson_redelaunize(&constraints);

    // 4. Inside / outside via *geometric* test. The naive topological
    // flood fill from super-vertex seeds is fragile: constraint
    // enforcement flips can pull super-vertices into triangles that are
    // geometrically inside the polygon, which then get marked outside
    // and propagate the wrong state. Instead, for each candidate
    // triangle (must have all constraint vertices, no super), compute
    // its 2D centroid and apply the even-odd point-in-polygon rule
    // against every input loop. A point is inside the polygon iff an
    // odd number of loops contain it (outer = 1; outer + hole = 2;
    // none = 0).
    let projected_loops: Vec<Vec<Point2>> = loops
        .iter()
        .map(|loop_| {
            loop_
                .iter()
                .map(|&v| project_point(vertices[v], axis_a, axis_b))
                .collect()
        })
        .collect();

    // 5. Collect alive triangles whose centroid is inside the polygon.
    let mut output = Vec::new();
    for (_tid, tri) in mesh.alive_triangles() {
        if tri.verts.iter().any(|&v| v < super_count) {
            continue;
        }
        let p0 = projected_loops_lookup(
            vertices,
            input_ids[tri.verts[0] - super_count],
            axis_a,
            axis_b,
        );
        let p1 = projected_loops_lookup(
            vertices,
            input_ids[tri.verts[1] - super_count],
            axis_a,
            axis_b,
        );
        let p2 = projected_loops_lookup(
            vertices,
            input_ids[tri.verts[2] - super_count],
            axis_a,
            axis_b,
        );
        // Centroid = (p0 + p1 + p2) / 3. Avoid division by working with 3*centroid:
        // shift the polygon-loop edges by 3x as well.
        let cx3 = p0.0 as i128 + p1.0 as i128 + p2.0 as i128;
        let cy3 = p0.1 as i128 + p1.1 as i128 + p2.1 as i128;

        let mut inside_count = 0u32;
        for ploop in &projected_loops {
            if point_in_polygon_3x(cx3, cy3, ploop) {
                inside_count += 1;
            }
        }
        if inside_count % 2 != 1 {
            continue;
        }

        let v0 = input_ids[tri.verts[0] - super_count];
        let v1 = input_ids[tri.verts[1] - super_count];
        let v2 = input_ids[tri.verts[2] - super_count];
        output.push([v0, v1, v2]);
    }
    Some(output)
}

fn projected_loops_lookup(
    vertices: &[Point3],
    vid: VertexId,
    axis_a: Axis,
    axis_b: Axis,
) -> Point2 {
    project_point(vertices[vid], axis_a, axis_b)
}

/// Even-odd point-in-polygon test, evaluated at point `(cx3 / 3, cy3 / 3)`
/// without performing the division. The polygon edges are scaled by 3 so
/// the rational inequality stays in integers.
///
/// Standard ray-casting: a horizontal ray from the test point to +x
/// crosses an edge iff the edge straddles the test y AND the
/// intersection x is to the right of the test x. We avoid division by
/// cross-multiplying by `denom = pi.y - pj.y` and tracking its sign.
fn point_in_polygon_3x(cx3: i128, cy3: i128, poly: &[Point2]) -> bool {
    let mut inside = false;
    let n = poly.len();
    for i in 0..n {
        let j = (i + n - 1) % n;
        // Scale polygon coords by 3 so we can compare against (cx3, cy3).
        let pix3 = (poly[i].0 as i128) * 3;
        let piy3 = (poly[i].1 as i128) * 3;
        let pjx3 = (poly[j].0 as i128) * 3;
        let pjy3 = (poly[j].1 as i128) * 3;
        // Does the edge from pj to pi straddle horizontal line y = cy3?
        // Use strict on one side, non-strict on the other to avoid
        // double-counting vertex grazes.
        if (piy3 > cy3) == (pjy3 > cy3) {
            continue;
        }
        // Intersection x with horizontal line y = cy3:
        //   x_at = pjx3 + (pix3 - pjx3) * (cy3 - pjy3) / (piy3 - pjy3)
        // We want: cx3 < x_at.
        //   cx3 - pjx3 < (pix3 - pjx3) * (cy3 - pjy3) / (piy3 - pjy3)
        // Multiply by (piy3 - pjy3); sign matters.
        let denom = piy3 - pjy3;
        let lhs = (cx3 - pjx3) * denom;
        let rhs = (pix3 - pjx3) * (cy3 - pjy3);
        let crosses = if denom > 0 { lhs < rhs } else { lhs > rhs };
        if crosses {
            inside = !inside;
        }
    }
    inside
}

/// CCW signed area test for the projected outer loop, exposed because
/// some callers want to verify the loop orientation matches the plane
/// normal before passing it in. (Currently unused — kept as part of the
/// projection helper surface.)
#[allow(dead_code)]
pub(in crate::csg::cleanup) fn signed_area2_2d(loop2d: &[Point2]) -> i128 {
    let mut sum: i128 = 0;
    let n = loop2d.len();
    for i in 0..n {
        let j = (i + 1) % n;
        sum += (loop2d[i].0 as i128) * (loop2d[j].1 as i128)
            - (loop2d[j].0 as i128) * (loop2d[i].1 as i128);
    }
    sum
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

    fn xy_plane() -> Plane3 {
        Plane3 {
            n_x: 0,
            n_y: 0,
            n_z: 1,
            d: 0,
        }
    }

    fn assert_ccw_around_plane(triangles: &[[VertexId; 3]], vertices: &[Point3], plane: &Plane3) {
        for tri in triangles {
            let p0 = vertices[tri[0]];
            let p1 = vertices[tri[1]];
            let p2 = vertices[tri[2]];
            // 2D cross in the projection used by CDT.
            let (axis_a, axis_b) = projection_axes(plane);
            let q0 = project_point(p0, axis_a, axis_b);
            let q1 = project_point(p1, axis_a, axis_b);
            let q2 = project_point(p2, axis_a, axis_b);
            assert!(
                orient2d(q0, q1, q2) > 0,
                "triangle {:?} not CCW in projection",
                tri
            );
        }
    }

    fn total_doubled_area(triangles: &[[VertexId; 3]], vertices: &[Point3]) -> i128 {
        triangles
            .iter()
            .map(|tri| {
                let v0 = vertices[tri[0]];
                let v1 = vertices[tri[1]];
                let v2 = vertices[tri[2]];
                (v1.x as i128 - v0.x as i128) * (v2.y as i128 - v0.y as i128)
                    - (v1.y as i128 - v0.y as i128) * (v2.x as i128 - v0.x as i128)
            })
            .sum()
    }

    #[test]
    fn empty_input_yields_empty_triangulation() {
        let result = triangulate(&[], &[], &xy_plane());
        assert_eq!(result, Some(Vec::new()));
    }

    #[test]
    fn triangle_yields_one_triangle() {
        let vertices = vec![pt(0.0, 0.0, 0.0), pt(1.0, 0.0, 0.0), pt(0.0, 1.0, 0.0)];
        let loops = vec![vec![0, 1, 2]];
        let tris = triangulate(&vertices, &loops, &xy_plane()).unwrap();
        assert_eq!(tris.len(), 1);
        assert_ccw_around_plane(&tris, &vertices, &xy_plane());
    }

    #[test]
    fn quad_yields_two_triangles() {
        let vertices = vec![
            pt(0.0, 0.0, 0.0),
            pt(1.0, 0.0, 0.0),
            pt(1.0, 1.0, 0.0),
            pt(0.0, 1.0, 0.0),
        ];
        let loops = vec![vec![0, 1, 2, 3]];
        let tris = triangulate(&vertices, &loops, &xy_plane()).unwrap();
        assert_eq!(tris.len(), 2);
        assert_ccw_around_plane(&tris, &vertices, &xy_plane());
        // Total area = 1 (a unit square).
        let unit = 1_i128 << 16;
        assert_eq!(total_doubled_area(&tris, &vertices), 2 * unit * unit);
    }

    #[test]
    fn square_with_square_hole_triangulates_at_topological_minimum() {
        // Outer 2x2 square (CCW around +z), inner 1x1 hole (CW around +z).
        let vertices = vec![
            pt(0.0, 0.0, 0.0), // 0: outer BL
            pt(2.0, 0.0, 0.0), // 1: outer BR
            pt(2.0, 2.0, 0.0), // 2: outer TR
            pt(0.0, 2.0, 0.0), // 3: outer TL
            pt(0.5, 0.5, 0.0), // 4: hole BL
            pt(1.5, 0.5, 0.0), // 5: hole BR
            pt(1.5, 1.5, 0.0), // 6: hole TR
            pt(0.5, 1.5, 0.0), // 7: hole TL
        ];
        let loops = vec![
            vec![0, 1, 2, 3], // outer CCW
            vec![4, 7, 6, 5], // hole CW (reverse of CCW order)
        ];
        let tris = triangulate(&vertices, &loops, &xy_plane()).unwrap();
        // Topological minimum for a rectangle-with-rectangular-hole on
        // 8 boundary vertices: V + 2H - 2 = 8 triangles.
        assert_eq!(
            tris.len(),
            8,
            "expected 8 annular triangles, got {}",
            tris.len()
        );
        assert_ccw_around_plane(&tris, &vertices, &xy_plane());
        // Total area = outer (4) - hole (1) = 3.
        let unit = 1_i128 << 16;
        assert_eq!(
            total_doubled_area(&tris, &vertices),
            3 * 2 * unit * unit,
            "annular area mismatch"
        );
    }

    #[test]
    fn cdt_is_deterministic_across_runs() {
        let vertices = vec![
            pt(0.0, 0.0, 0.0),
            pt(2.0, 0.0, 0.0),
            pt(2.0, 2.0, 0.0),
            pt(0.0, 2.0, 0.0),
            pt(0.5, 0.5, 0.0),
            pt(1.5, 0.5, 0.0),
            pt(1.5, 1.5, 0.0),
            pt(0.5, 1.5, 0.0),
        ];
        let loops = vec![vec![0, 1, 2, 3], vec![4, 7, 6, 5]];
        let r1 = triangulate(&vertices, &loops, &xy_plane()).unwrap();
        let r2 = triangulate(&vertices, &loops, &xy_plane()).unwrap();
        assert_eq!(r1, r2);
    }

    #[test]
    fn l_shaped_outer_loop_triangulates() {
        // Non-convex L-shape boundary, CCW.
        //   (0,2)-(1,2)
        //   |     |
        //   |     (1,1)-(2,1)
        //   |             |
        //   (0,0)-------(2,0)
        let vertices = vec![
            pt(0.0, 0.0, 0.0), // 0
            pt(2.0, 0.0, 0.0), // 1
            pt(2.0, 1.0, 0.0), // 2
            pt(1.0, 1.0, 0.0), // 3
            pt(1.0, 2.0, 0.0), // 4
            pt(0.0, 2.0, 0.0), // 5
        ];
        let loops = vec![vec![0, 1, 2, 3, 4, 5]];
        let tris = triangulate(&vertices, &loops, &xy_plane()).unwrap();
        // 6-vertex non-convex polygon: V - 2 = 4 triangles.
        assert_eq!(tris.len(), 4);
        assert_ccw_around_plane(&tris, &vertices, &xy_plane());
        // L-shape area = 2 + 1 = 3.
        let unit = 1_i128 << 16;
        assert_eq!(total_doubled_area(&tris, &vertices), 3 * 2 * unit * unit);
    }
}
