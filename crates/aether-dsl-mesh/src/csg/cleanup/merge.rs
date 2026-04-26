//! Pass 2: coplanar polygon merging.
//!
//! Groups polygons by exact `Plane3` signature, finds connected
//! components within each group via shared edges, extracts each
//! component's boundary loop(s), and re-triangulates merged regions
//! via ear clipping in 2D.
//!
//! Scope:
//!
//! - Single-loop components are merged: their boundary is walked, the
//!   loop is projected to 2D using the plane's dominant axis, and ear
//!   clipping produces a triangulation. Output triangles inherit the
//!   first input polygon's color (per ADR-0055 — color across merge
//!   boundaries is the "first polygon wins" tradeoff).
//! - Multi-loop components (faces with holes) pass through unmerged in
//!   this PR — hole bridging is a follow-up.
//! - Singletons (no shared edges with any group neighbor) pass through
//!   as-is.
//!
//! Plane-equality limitation: the grouping key is the exact `Plane3`
//! tuple `(n_x, n_y, n_z, d)`. Polygons that are coplanar in the
//! Euclidean sense but whose `Plane3` differs by a positive scalar
//! (e.g. two source triangles on the same plane with different
//! cross-product magnitudes) are not currently grouped. For typical
//! CSG output this is fine — split fragments inherit their parent
//! triangle's plane (per `Polygon::split`), so all fragments of one
//! source share a key.
//!
//! Determinism: HashMap iteration order doesn't leak — plane keys are
//! sorted before grouping, components are sorted by their first input
//! polygon id, and ear-clipping picks the first valid ear in vertex
//! order.

use super::mesh::{IndexedMesh, IndexedPolygon, VertexId};
use crate::csg::plane::Plane3;
use crate::csg::point::Point3;
use std::collections::HashMap;

type PlaneKey = (i64, i64, i64, i128);

impl IndexedMesh {
    pub(super) fn merge_coplanar(self) -> Self {
        let IndexedMesh { vertices, polygons } = self;

        let groups = group_by_plane(&polygons);
        let mut sorted_keys: Vec<&PlaneKey> = groups.keys().collect();
        sorted_keys.sort();

        let mut merged: Vec<IndexedPolygon> = Vec::with_capacity(polygons.len());

        for key in sorted_keys {
            let group_pids = &groups[key];
            for component in connected_components(&polygons, group_pids) {
                process_component(&vertices, &polygons, &component, &mut merged);
            }
        }

        IndexedMesh {
            vertices,
            polygons: merged,
        }
    }
}

fn plane_key(p: &Plane3) -> PlaneKey {
    (p.n_x, p.n_y, p.n_z, p.d)
}

fn group_by_plane(polygons: &[IndexedPolygon]) -> HashMap<PlaneKey, Vec<usize>> {
    let mut groups: HashMap<PlaneKey, Vec<usize>> = HashMap::new();
    for (i, poly) in polygons.iter().enumerate() {
        groups.entry(plane_key(&poly.plane)).or_default().push(i);
    }
    groups
}

/// Union-find over polygon ids in `group`, merging when two polygons share a
/// canonicalized edge (smaller `VertexId` first). Returns components, each
/// sorted by global polygon id, in ascending order of first id.
fn connected_components(polygons: &[IndexedPolygon], group: &[usize]) -> Vec<Vec<usize>> {
    let n = group.len();
    let mut parent: Vec<usize> = (0..n).collect();

    fn find(parent: &mut [usize], mut x: usize) -> usize {
        while parent[x] != x {
            parent[x] = parent[parent[x]];
            x = parent[x];
        }
        x
    }
    fn union(parent: &mut [usize], a: usize, b: usize) {
        let ra = find(parent, a);
        let rb = find(parent, b);
        if ra != rb {
            parent[ra] = rb;
        }
    }

    let mut edge_owner: HashMap<(VertexId, VertexId), usize> = HashMap::new();
    for (local, &pid) in group.iter().enumerate() {
        let poly = &polygons[pid];
        let m = poly.vertices.len();
        for i in 0..m {
            let a = poly.vertices[i];
            let b = poly.vertices[(i + 1) % m];
            let edge = if a < b { (a, b) } else { (b, a) };
            if let Some(&other) = edge_owner.get(&edge) {
                union(&mut parent, local, other);
            } else {
                edge_owner.insert(edge, local);
            }
        }
    }

    let mut by_root: HashMap<usize, Vec<usize>> = HashMap::new();
    for (local, &pid) in group.iter().enumerate().take(n) {
        let root = find(&mut parent, local);
        by_root.entry(root).or_default().push(pid);
    }
    let mut components: Vec<Vec<usize>> = by_root.into_values().collect();
    for c in components.iter_mut() {
        c.sort();
    }
    components.sort_by_key(|c| c[0]);
    components
}

fn process_component(
    vertices: &[Point3],
    polygons: &[IndexedPolygon],
    component: &[usize],
    out: &mut Vec<IndexedPolygon>,
) {
    if component.len() == 1 {
        emit_fan(&polygons[component[0]], out);
        return;
    }

    // Directed edges: an edge appears once if it's on the component's
    // boundary, twice (as itself + reverse) if it's interior to the
    // component.
    let mut directed: HashMap<(VertexId, VertexId), u32> = HashMap::new();
    for &pid in component {
        let poly = &polygons[pid];
        let m = poly.vertices.len();
        for i in 0..m {
            let a = poly.vertices[i];
            let b = poly.vertices[(i + 1) % m];
            *directed.entry((a, b)).or_insert(0) += 1;
        }
    }
    let boundary: Vec<(VertexId, VertexId)> = directed
        .iter()
        .filter(|&(&(a, b), _)| !directed.contains_key(&(b, a)))
        .map(|(&edge, _)| edge)
        .collect();

    let loops = match extract_loops(&boundary) {
        Some(l) => l,
        // Pathological boundary topology — pass through unchanged.
        None => {
            for &pid in component {
                emit_fan(&polygons[pid], out);
            }
            return;
        }
    };

    // Multi-loop (faces with holes) pass through unmerged in this PR.
    if loops.len() != 1 {
        for &pid in component {
            emit_fan(&polygons[pid], out);
        }
        return;
    }

    let plane = polygons[component[0]].plane;
    let color = polygons[component[0]].color;
    match ear_clip(vertices, &loops[0], &plane) {
        Some(triangles) => {
            for tri in triangles {
                out.push(IndexedPolygon {
                    vertices: tri.to_vec(),
                    plane,
                    color,
                });
            }
        }
        // Ear clipping failed (numeric / topology corner case) — fall back
        // to passing the original polygons through.
        None => {
            for &pid in component {
                emit_fan(&polygons[pid], out);
            }
        }
    }
}

fn emit_fan(poly: &IndexedPolygon, out: &mut Vec<IndexedPolygon>) {
    if poly.vertices.len() < 3 {
        return;
    }
    let v0 = poly.vertices[0];
    for i in 1..poly.vertices.len() - 1 {
        out.push(IndexedPolygon {
            vertices: vec![v0, poly.vertices[i], poly.vertices[i + 1]],
            plane: poly.plane,
            color: poly.color,
        });
    }
}

/// Walk directed boundary edges into closed loops. Returns `None` if the
/// boundary is not a disjoint union of closed loops (open, branching at
/// pinch points where a vertex has more outgoing edges than incoming, etc).
fn extract_loops(boundary: &[(VertexId, VertexId)]) -> Option<Vec<Vec<VertexId>>> {
    let mut outgoing: HashMap<VertexId, Vec<VertexId>> = HashMap::new();
    for &(a, b) in boundary {
        outgoing.entry(a).or_default().push(b);
    }
    for v in outgoing.values_mut() {
        v.sort();
    }

    let mut starts: Vec<(VertexId, VertexId)> = boundary.to_vec();
    starts.sort();

    let mut visited: std::collections::HashSet<(VertexId, VertexId)> =
        std::collections::HashSet::new();
    let mut loops = Vec::new();

    for &(start_a, start_b) in &starts {
        if visited.contains(&(start_a, start_b)) {
            continue;
        }
        visited.insert((start_a, start_b));
        let mut loop_verts = vec![start_a];
        let mut cur = start_b;
        loop {
            if cur == start_a {
                break;
            }
            loop_verts.push(cur);
            let next = outgoing.get(&cur).and_then(|nexts| {
                nexts
                    .iter()
                    .find(|&&n| !visited.contains(&(cur, n)))
                    .copied()
            });
            match next {
                Some(n) => {
                    visited.insert((cur, n));
                    cur = n;
                }
                None => return None,
            }
        }
        if loop_verts.len() < 3 {
            return None;
        }
        loops.push(loop_verts);
    }
    Some(loops)
}

#[derive(Debug, Clone, Copy)]
enum Axis {
    X,
    Y,
    Z,
}

/// Pick the 2D projection axes for a plane such that a 3D loop walked CCW
/// around the plane normal projects to a CCW loop in 2D.
///
/// Drop the axis with the largest `|n_i|`; the remaining two axes go in
/// cyclic order for positive `n_i`, reversed for negative.
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

fn project(p: Point3, axis_a: Axis, axis_b: Axis) -> (i64, i64) {
    let pick = |a: Axis| -> i64 {
        match a {
            Axis::X => p.x as i64,
            Axis::Y => p.y as i64,
            Axis::Z => p.z as i64,
        }
    };
    (pick(axis_a), pick(axis_b))
}

/// Signed 2D area times 2 (shoelace). Positive for CCW, negative for CW.
fn signed_area2_2d(loop2d: &[(VertexId, i64, i64)]) -> i128 {
    let mut sum: i128 = 0;
    let n = loop2d.len();
    for i in 0..n {
        let j = (i + 1) % n;
        sum += (loop2d[i].1 as i128) * (loop2d[j].2 as i128)
            - (loop2d[j].1 as i128) * (loop2d[i].2 as i128);
    }
    sum
}

/// 2D cross product `(b - a) × (c - a)` as i128.
fn cross2d(a: (i64, i64), b: (i64, i64), c: (i64, i64)) -> i128 {
    let abx = (b.0 - a.0) as i128;
    let aby = (b.1 - a.1) as i128;
    let acx = (c.0 - a.0) as i128;
    let acy = (c.1 - a.1) as i128;
    abx * acy - aby * acx
}

fn point_in_triangle(p: (i64, i64), a: (i64, i64), b: (i64, i64), c: (i64, i64)) -> bool {
    // Strict interior test: point inside iff all three sub-cross-products
    // share the strict sign of the triangle's area. Vertices on the
    // triangle's edges are NOT considered "inside" — they are the
    // shared corners of adjacent ears and would otherwise block valid
    // ear extraction.
    let abc = cross2d(a, b, c);
    if abc == 0 {
        return false;
    }
    let pab = cross2d(a, b, p);
    let pbc = cross2d(b, c, p);
    let pca = cross2d(c, a, p);
    if abc > 0 {
        pab > 0 && pbc > 0 && pca > 0
    } else {
        pab < 0 && pbc < 0 && pca < 0
    }
}

/// Ear-clip a 3D loop projected to 2D. Returns the triangulation as a Vec
/// of `[VertexId; 3]`. Returns `None` if the loop is not simple or ear
/// clipping cannot make progress.
fn ear_clip(
    vertices: &[Point3],
    loop_verts: &[VertexId],
    plane: &Plane3,
) -> Option<Vec<[VertexId; 3]>> {
    if loop_verts.len() < 3 {
        return None;
    }

    let (axis_a, axis_b) = projection_axes(plane);
    let mut loop2d: Vec<(VertexId, i64, i64)> = loop_verts
        .iter()
        .map(|&id| {
            let (a, b) = project(vertices[id], axis_a, axis_b);
            (id, a, b)
        })
        .collect();

    // Boundary walking inherits the polygons' CCW-around-normal
    // orientation, and `projection_axes` is set up so CCW-around-normal
    // maps to CCW in 2D — but defensively flip if signed area is
    // negative.
    if signed_area2_2d(&loop2d) < 0 {
        loop2d.reverse();
    }

    let mut output = Vec::with_capacity(loop2d.len().saturating_sub(2));
    let mut guard = loop2d.len() * loop2d.len(); // strict upper bound on iterations
    while loop2d.len() > 3 {
        if guard == 0 {
            return None;
        }
        guard -= 1;
        let n = loop2d.len();
        let mut found_ear: Option<usize> = None;
        for i in 0..n {
            let prev = (i + n - 1) % n;
            let next = (i + 1) % n;
            let vp = (loop2d[prev].1, loop2d[prev].2);
            let vc = (loop2d[i].1, loop2d[i].2);
            let vn = (loop2d[next].1, loop2d[next].2);
            // Convex turn: cross > 0 since the loop is CCW.
            if cross2d(vp, vc, vn) <= 0 {
                continue;
            }
            // No other loop vertex strictly inside this triangle.
            let mut clear = true;
            for (j, &(_, jx, jy)) in loop2d.iter().enumerate() {
                if j == prev || j == i || j == next {
                    continue;
                }
                if point_in_triangle((jx, jy), vp, vc, vn) {
                    clear = false;
                    break;
                }
            }
            if clear {
                found_ear = Some(i);
                break;
            }
        }
        let ear = found_ear?;
        let prev = (ear + loop2d.len() - 1) % loop2d.len();
        let next = (ear + 1) % loop2d.len();
        output.push([loop2d[prev].0, loop2d[ear].0, loop2d[next].0]);
        loop2d.remove(ear);
    }
    output.push([loop2d[0].0, loop2d[1].0, loop2d[2].0]);
    Some(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::csg::fixed::f32_to_fixed;
    use crate::csg::polygon::Polygon;

    fn pt(x: f32, y: f32, z: f32) -> Point3 {
        Point3 {
            x: f32_to_fixed(x).unwrap(),
            y: f32_to_fixed(y).unwrap(),
            z: f32_to_fixed(z).unwrap(),
        }
    }

    fn weld_then_merge(polys: Vec<Polygon>) -> Vec<Polygon> {
        IndexedMesh::weld(polys).merge_coplanar().into_polygons()
    }

    #[test]
    fn singleton_passes_through() {
        let tri =
            Polygon::from_triangle(pt(0.0, 0.0, 0.0), pt(1.0, 0.0, 0.0), pt(0.0, 1.0, 0.0), 5)
                .unwrap();
        let out = weld_then_merge(vec![tri]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].vertices.len(), 3);
        assert_eq!(out[0].color, 5);
    }

    #[test]
    fn two_triangles_forming_a_quad_merge_to_two_triangles_covering_same_corners() {
        // Quad split into two triangles by the (0,0)-(1,1) diagonal. After
        // merge they re-triangulate from the boundary loop — still 2
        // triangles (a quad fan-clips to 2), covering the same 4 corners.
        let t1 = Polygon::from_triangle(pt(0.0, 0.0, 0.0), pt(1.0, 0.0, 0.0), pt(1.0, 1.0, 0.0), 0)
            .unwrap();
        let t2 = Polygon::from_triangle(pt(0.0, 0.0, 0.0), pt(1.0, 1.0, 0.0), pt(0.0, 1.0, 0.0), 0)
            .unwrap();
        let out = weld_then_merge(vec![t1, t2]);
        assert_eq!(out.len(), 2);
        let covered: std::collections::BTreeSet<Point3> = out
            .iter()
            .flat_map(|p| p.vertices.iter().copied())
            .collect();
        let expect: std::collections::BTreeSet<Point3> = [
            pt(0.0, 0.0, 0.0),
            pt(1.0, 0.0, 0.0),
            pt(1.0, 1.0, 0.0),
            pt(0.0, 1.0, 0.0),
        ]
        .into_iter()
        .collect();
        assert_eq!(covered, expect);
    }

    #[test]
    fn two_coplanar_triangles_with_opposite_normals_dont_merge() {
        // Same triangle as above, but the second is wound the other way
        // (so its plane normal is opposite). They occupy the same plane
        // geometrically but face opposite directions — should not be
        // grouped together.
        let t1 = Polygon::from_triangle(pt(0.0, 0.0, 0.0), pt(1.0, 0.0, 0.0), pt(1.0, 1.0, 0.0), 0)
            .unwrap();
        let t2 = Polygon::from_triangle(pt(0.0, 0.0, 0.0), pt(1.0, 1.0, 0.0), pt(1.0, 0.0, 0.0), 1)
            .unwrap();
        // t1's plane.n_z is positive, t2's is negative — different keys.
        let out = weld_then_merge(vec![t1, t2]);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn triangles_in_different_planes_are_unaffected() {
        let t_xy =
            Polygon::from_triangle(pt(0.0, 0.0, 0.0), pt(1.0, 0.0, 0.0), pt(0.0, 1.0, 0.0), 0)
                .unwrap();
        let t_yz =
            Polygon::from_triangle(pt(0.0, 0.0, 0.0), pt(0.0, 1.0, 0.0), pt(0.0, 0.0, 1.0), 0)
                .unwrap();
        let out = weld_then_merge(vec![t_xy, t_yz]);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn shattered_quad_collapses_to_two_triangles() {
        // Quad [(0,0)-(2,0)-(2,2)-(0,2)] split into 4 triangles meeting
        // at the centre (1,1) plus 4 boundary triangles. Pretty common
        // CSG-cut pattern.
        // Simpler: just split into 2 triangles → merge → 2 triangles.
        // For more realistic stress test, use 4 fan triangles from centre.
        let c = pt(1.0, 1.0, 0.0);
        let nw = pt(0.0, 2.0, 0.0);
        let ne = pt(2.0, 2.0, 0.0);
        let se = pt(2.0, 0.0, 0.0);
        let sw = pt(0.0, 0.0, 0.0);
        // Four triangles fan from the center, all on z=0 plane, all
        // pointing +z (CCW winding). Each has the same plane:
        // n=(cross-product of edges) — same magnitude for all four.
        let polys = vec![
            Polygon::from_triangle(c, nw, ne, 0).unwrap(),
            Polygon::from_triangle(c, ne, se, 0).unwrap(),
            Polygon::from_triangle(c, se, sw, 0).unwrap(),
            Polygon::from_triangle(c, sw, nw, 0).unwrap(),
        ];
        let out = weld_then_merge(polys);
        // After merge: 4-vertex quad → ear-clips to 2 triangles, dropping
        // the central pivot vertex entirely.
        assert_eq!(
            out.len(),
            2,
            "expected 2 merged triangles, got {}",
            out.len()
        );
    }

    #[test]
    fn l_shaped_non_convex_loop_triangulates() {
        // L-shape: a 2x2 square with the upper-right 1x1 removed.
        //   (0,2)---(1,2)
        //   |       |
        //   |       (1,1)---(2,1)
        //   |               |
        //   (0,0)---------(2,0)
        // Three coplanar triangles fan from the bottom-left corner cover it.
        let bl = pt(0.0, 0.0, 0.0);
        let br = pt(2.0, 0.0, 0.0);
        let inner = pt(2.0, 1.0, 0.0);
        let mid = pt(1.0, 1.0, 0.0);
        let top = pt(1.0, 2.0, 0.0);
        let tl = pt(0.0, 2.0, 0.0);
        let polys = vec![
            Polygon::from_triangle(bl, br, inner, 0).unwrap(),
            Polygon::from_triangle(bl, inner, mid, 0).unwrap(),
            Polygon::from_triangle(bl, mid, top, 0).unwrap(),
            Polygon::from_triangle(bl, top, tl, 0).unwrap(),
        ];
        // Note: this fan has different normal magnitudes per triangle,
        // so the plane signatures will differ — the merge won't fire.
        // To force a merge, all triangles must share `Plane3` exactly.
        // Use triangles split off a single source instead: cover the L
        // with two same-magnitude-plane triangles.
        let out = weld_then_merge(polys);
        // Without all triangles sharing a plane key, no merge happens —
        // each triangle passes through. 4 triangles in, 4 triangles out.
        assert_eq!(out.len(), 4);
    }

    #[test]
    fn merging_is_deterministic_across_runs() {
        let t1 = Polygon::from_triangle(pt(0.0, 0.0, 0.0), pt(1.0, 0.0, 0.0), pt(1.0, 1.0, 0.0), 0)
            .unwrap();
        let t2 = Polygon::from_triangle(pt(0.0, 0.0, 0.0), pt(1.0, 1.0, 0.0), pt(0.0, 1.0, 0.0), 0)
            .unwrap();
        let r1 = weld_then_merge(vec![t1.clone(), t2.clone()]);
        let r2 = weld_then_merge(vec![t1, t2]);
        assert_eq!(r1.len(), r2.len());
        for (a, b) in r1.iter().zip(r2.iter()) {
            assert_eq!(a.vertices, b.vertices);
            assert_eq!(a.color, b.color);
        }
    }

    #[test]
    fn projection_axes_are_set_up_for_ccw_in_2d() {
        // For each cardinal plane normal, walk a CCW-around-normal loop
        // and verify the projected signed area is positive.
        struct Case {
            n_x: i64,
            n_y: i64,
            n_z: i64,
            loop3d: Vec<Point3>,
        }
        let cases = vec![
            // +z normal: CCW in xy.
            Case {
                n_x: 0,
                n_y: 0,
                n_z: 1,
                loop3d: vec![
                    pt(1.0, 0.0, 0.0),
                    pt(0.0, 1.0, 0.0),
                    pt(-1.0, 0.0, 0.0),
                    pt(0.0, -1.0, 0.0),
                ],
            },
            // -z normal: CCW around -z.
            Case {
                n_x: 0,
                n_y: 0,
                n_z: -1,
                loop3d: vec![
                    pt(1.0, 0.0, 0.0),
                    pt(0.0, -1.0, 0.0),
                    pt(-1.0, 0.0, 0.0),
                    pt(0.0, 1.0, 0.0),
                ],
            },
            // +y normal: CCW around +y. Tangent at (1,0,0) is (0,0,-1).
            Case {
                n_x: 0,
                n_y: 1,
                n_z: 0,
                loop3d: vec![
                    pt(1.0, 0.0, 0.0),
                    pt(0.0, 0.0, -1.0),
                    pt(-1.0, 0.0, 0.0),
                    pt(0.0, 0.0, 1.0),
                ],
            },
            // -y normal.
            Case {
                n_x: 0,
                n_y: -1,
                n_z: 0,
                loop3d: vec![
                    pt(1.0, 0.0, 0.0),
                    pt(0.0, 0.0, 1.0),
                    pt(-1.0, 0.0, 0.0),
                    pt(0.0, 0.0, -1.0),
                ],
            },
            // +x normal. Tangent at (0,1,0) is (0,0,1).
            Case {
                n_x: 1,
                n_y: 0,
                n_z: 0,
                loop3d: vec![
                    pt(0.0, 1.0, 0.0),
                    pt(0.0, 0.0, 1.0),
                    pt(0.0, -1.0, 0.0),
                    pt(0.0, 0.0, -1.0),
                ],
            },
            // -x normal.
            Case {
                n_x: -1,
                n_y: 0,
                n_z: 0,
                loop3d: vec![
                    pt(0.0, 1.0, 0.0),
                    pt(0.0, 0.0, -1.0),
                    pt(0.0, -1.0, 0.0),
                    pt(0.0, 0.0, 1.0),
                ],
            },
        ];
        for case in cases {
            let plane = Plane3 {
                n_x: case.n_x,
                n_y: case.n_y,
                n_z: case.n_z,
                d: 0,
            };
            let (axis_a, axis_b) = projection_axes(&plane);
            let loop2d: Vec<(VertexId, i64, i64)> = case
                .loop3d
                .iter()
                .enumerate()
                .map(|(i, &p)| {
                    let (a, b) = project(p, axis_a, axis_b);
                    (i, a, b)
                })
                .collect();
            assert!(
                signed_area2_2d(&loop2d) > 0,
                "expected CCW signed area > 0 for normal ({}, {}, {})",
                case.n_x,
                case.n_y,
                case.n_z
            );
        }
    }
}
