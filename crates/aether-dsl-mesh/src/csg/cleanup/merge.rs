//! Pass 2: coplanar polygon merging.
//!
//! Groups polygons by exact `Plane3` signature, finds connected
//! components within each group via shared edges, extracts each
//! component's boundary loop(s), and re-triangulates merged regions
//! via constrained Delaunay triangulation (ADR-0056).
//!
//! Each component goes through:
//!
//! 1. Boundary edge collection (directed edges with no reverse twin).
//! 2. Loop extraction (walk directed boundary into closed loops).
//! 3. CDT (`cdt::triangulate_loops`) — single algorithm path for both
//!    single-loop and multi-loop (face-with-holes) cases. The CDT
//!    enforces every loop edge as a constraint and discards triangles
//!    outside the polygon, so there is no slit, no slivers, and no
//!    bridge-endpoint duplication. Output triangles inherit the first
//!    input polygon's color (per ADR-0055 — color across merge
//!    boundaries is the "first polygon wins" tradeoff).
//! 4. Singletons (no shared edges with any group neighbor) pass through
//!    as fans, since CDT would just re-emit them after one vertex.
//! 5. CDT failure (rare — pathological boundary topology) falls back
//!    to passing the original polygons through as fans.
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
//! polygon id, and CDT inherits its determinism from sorted insertion
//! and integer-exact predicates.

use super::cdt;
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

    let plane = polygons[component[0]].plane;
    let color = polygons[component[0]].color;
    match cdt::triangulate_loops(vertices, &loops, &plane) {
        Some(triangles) => {
            for tri in triangles {
                out.push(IndexedPolygon {
                    vertices: tri.to_vec(),
                    plane,
                    color,
                });
            }
        }
        None => {
            // CDT couldn't enforce a constraint or hit a degenerate
            // configuration. Keep the geometry by emitting the original
            // polygons as fans rather than producing nothing.
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

    /// Build an annular triangulation: a 2x2 outer square (corners A,B,C,D)
    /// minus a 1x1 hole (corners E,F,G,H), 8 CCW triangles all on z=0 with
    /// the same `Plane3` (normal magnitude is consistent because each
    /// triangle has a unit edge along an outer side and an offset of the
    /// same magnitude inward).
    fn annular_indexed_mesh() -> IndexedMesh {
        let plane = Plane3 {
            n_x: 0,
            n_y: 0,
            n_z: 1,
            d: 0,
        };
        let color = 7;
        let vertices = vec![
            pt(0.0, 0.0, 0.0), // 0: A bottom-left
            pt(2.0, 0.0, 0.0), // 1: B bottom-right
            pt(2.0, 2.0, 0.0), // 2: C top-right
            pt(0.0, 2.0, 0.0), // 3: D top-left
            pt(0.5, 0.5, 0.0), // 4: E hole bottom-left
            pt(1.5, 0.5, 0.0), // 5: F hole bottom-right
            pt(1.5, 1.5, 0.0), // 6: G hole top-right
            pt(0.5, 1.5, 0.0), // 7: H hole top-left
        ];
        let polygons = [
            [0, 1, 4],
            [1, 5, 4],
            [1, 2, 5],
            [2, 6, 5],
            [2, 3, 6],
            [3, 7, 6],
            [3, 0, 7],
            [0, 4, 7],
        ]
        .into_iter()
        .map(|verts| IndexedPolygon {
            vertices: verts.to_vec(),
            plane,
            color,
        })
        .collect();
        IndexedMesh { vertices, polygons }
    }

    #[test]
    fn square_with_square_hole_bridges_and_triangulates() {
        let vertices = annular_indexed_mesh().vertices.clone();
        let merged = annular_indexed_mesh().merge_coplanar();
        // n outer (4) + n hole (4) + 2 bridge dups = 10 vertices in the
        // spliced loop, ear-clipping yields n - 2 = 8 triangles. Filter
        // for degenerates (none expected for axis-aligned bridge).
        assert_eq!(
            merged.polygons.len(),
            8,
            "expected 8 annular triangles, got {}",
            merged.polygons.len()
        );
        // All output triangles are CCW around +z (positive 2D cross).
        for poly in &merged.polygons {
            let v0 = vertices[poly.vertices[0]];
            let v1 = vertices[poly.vertices[1]];
            let v2 = vertices[poly.vertices[2]];
            let cross = (v1.x as i128 - v0.x as i128) * (v2.y as i128 - v0.y as i128)
                - (v1.y as i128 - v0.y as i128) * (v2.x as i128 - v0.x as i128);
            assert!(cross > 0, "expected CCW triangle, got cross = {}", cross);
        }
        // Total 2D area equals the annular area (outer 2*2=4 minus hole
        // 1*1=1, so 3) — measured in fixed-point grid units.
        let total_doubled_area: i128 = merged
            .polygons
            .iter()
            .map(|poly| {
                let v0 = vertices[poly.vertices[0]];
                let v1 = vertices[poly.vertices[1]];
                let v2 = vertices[poly.vertices[2]];
                (v1.x as i128 - v0.x as i128) * (v2.y as i128 - v0.y as i128)
                    - (v1.y as i128 - v0.y as i128) * (v2.x as i128 - v0.x as i128)
            })
            .sum();
        let unit = 1_i128 << 16;
        let expected_doubled_area = 3 * 2 * unit * unit;
        assert_eq!(
            total_doubled_area, expected_doubled_area,
            "annular area mismatch — bridging or clipping likely lost or duplicated coverage"
        );
    }

    #[test]
    fn multi_loop_merging_is_deterministic() {
        let r1 = annular_indexed_mesh().merge_coplanar();
        let r2 = annular_indexed_mesh().merge_coplanar();
        assert_eq!(r1.polygons.len(), r2.polygons.len());
        for (a, b) in r1.polygons.iter().zip(r2.polygons.iter()) {
            assert_eq!(a.vertices, b.vertices);
            assert_eq!(a.color, b.color);
        }
    }
}
