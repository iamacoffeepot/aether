//! Pass 2: coplanar polygon merging — emits boundary loops as n-gons.
//!
//! Groups polygons by exact `Plane3` signature, finds connected
//! components within each group via shared edges, extracts each
//! component's boundary loop(s), and emits **one [`IndexedPolygon`]
//! per loop** (per ADR-0057). No triangulation here — CDT runs in
//! [`IndexedMesh::cdt_triangulate`] (pass 4) on the post-T-junction
//! loops so T-junction repair operates on the n-gon edges rather
//! than triangulator-chosen diagonals.
//!
//! Each component goes through:
//!
//! 1. Boundary edge collection (directed edges with no reverse twin).
//! 2. Loop extraction (walk directed boundary into closed loops).
//! 3. Emit one `IndexedPolygon` per loop, sharing the component's
//!    plane and the first input polygon's color (per ADR-0055 — color
//!    across merge boundaries is the "first polygon wins" tradeoff).
//! 4. Singletons pass through unchanged — they're already a single
//!    loop, no extraction needed.
//! 5. Loop extraction failure (pathological boundary topology) falls
//!    back to passing the original polygons through unchanged.
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
//! polygon id, and loop extraction walks edges in deterministic order.

use super::mesh::{IndexedMesh, IndexedPolygon, VertexId};
use crate::csg::plane::Plane3;
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
                process_component(&polygons, &component, &mut merged);
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
    polygons: &[IndexedPolygon],
    component: &[usize],
    out: &mut Vec<IndexedPolygon>,
) {
    // Singletons are already a single loop — no boundary extraction needed.
    if component.len() == 1 {
        out.push(polygons[component[0]].clone());
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
                out.push(polygons[pid].clone());
            }
            return;
        }
    };

    let plane = polygons[component[0]].plane;
    let color = polygons[component[0]].color;
    for loop_verts in loops {
        out.push(IndexedPolygon {
            vertices: loop_verts,
            plane,
            color,
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
    use crate::csg::point::Point3;
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
    fn two_triangles_forming_a_quad_merge_to_one_quad_polygon() {
        // Quad split into two triangles by the (0,0)-(1,1) diagonal. After
        // merge they collapse into one 4-vertex polygon (the boundary
        // loop) — the diagonal disappears.
        let t1 = Polygon::from_triangle(pt(0.0, 0.0, 0.0), pt(1.0, 0.0, 0.0), pt(1.0, 1.0, 0.0), 0)
            .unwrap();
        let t2 = Polygon::from_triangle(pt(0.0, 0.0, 0.0), pt(1.0, 1.0, 0.0), pt(0.0, 1.0, 0.0), 0)
            .unwrap();
        let out = weld_then_merge(vec![t1, t2]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].vertices.len(), 4);
        let covered: std::collections::BTreeSet<Point3> = out[0].vertices.iter().copied().collect();
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
    fn shattered_quad_collapses_to_one_quad_polygon() {
        // Quad [(0,0)-(2,0)-(2,2)-(0,2)] covered by 4 fan triangles meeting
        // at the centre (1,1). After merge: one 4-vertex outer-boundary
        // polygon, dropping the central pivot vertex entirely.
        let c = pt(1.0, 1.0, 0.0);
        let nw = pt(0.0, 2.0, 0.0);
        let ne = pt(2.0, 2.0, 0.0);
        let se = pt(2.0, 0.0, 0.0);
        let sw = pt(0.0, 0.0, 0.0);
        let polys = vec![
            Polygon::from_triangle(c, nw, ne, 0).unwrap(),
            Polygon::from_triangle(c, ne, se, 0).unwrap(),
            Polygon::from_triangle(c, se, sw, 0).unwrap(),
            Polygon::from_triangle(c, sw, nw, 0).unwrap(),
        ];
        let out = weld_then_merge(polys);
        assert_eq!(out.len(), 1, "expected 1 merged polygon, got {}", out.len());
        assert_eq!(out[0].vertices.len(), 4);
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
        // Plane keys differ across the fan because each triangle has its
        // own cross-product magnitude. The middle two share a plane key
        // (both have the same edge magnitudes from bl) and a shared edge
        // bl→mid, so they merge into one 4-vertex loop. The first and
        // last stay as singletons. 4 in → 3 out.
        let out = weld_then_merge(polys);
        assert_eq!(out.len(), 3);
        let lens: std::collections::BTreeSet<usize> =
            out.iter().map(|p| p.vertices.len()).collect();
        assert_eq!(
            lens,
            [3, 4].into_iter().collect(),
            "expected two 3-vert singletons and one 4-vert merged loop"
        );
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

    /// Shoelace (signed) doubled area for an XY-projected polygon.
    /// Positive = CCW around +Z, negative = CW.
    fn shoelace_2d(vertices: &[Point3], indices: &[VertexId]) -> i128 {
        let mut sum: i128 = 0;
        let n = indices.len();
        for i in 0..n {
            let j = (i + 1) % n;
            let a = vertices[indices[i]];
            let b = vertices[indices[j]];
            sum += (a.x as i128) * (b.y as i128) - (b.x as i128) * (a.y as i128);
        }
        sum
    }

    #[test]
    fn square_with_square_hole_emits_outer_and_hole_loops() {
        let vertices = annular_indexed_mesh().vertices.clone();
        let merged = annular_indexed_mesh().merge_coplanar();
        // After ADR-0057: one polygon per boundary loop. Annular face has
        // an outer loop (4 verts, CCW) and a hole loop (4 verts, CW) →
        // 2 polygons.
        assert_eq!(
            merged.polygons.len(),
            2,
            "expected 2 boundary loops (outer + hole), got {}",
            merged.polygons.len()
        );
        // Every output loop has exactly 4 vertices.
        for poly in &merged.polygons {
            assert_eq!(poly.vertices.len(), 4);
        }
        // One loop is CCW (outer, positive area), the other CW (hole, negative).
        let signed_areas: Vec<i128> = merged
            .polygons
            .iter()
            .map(|p| shoelace_2d(&vertices, &p.vertices))
            .collect();
        let positive = signed_areas.iter().filter(|&&a| a > 0).count();
        let negative = signed_areas.iter().filter(|&&a| a < 0).count();
        assert_eq!(positive, 1, "expected one CCW outer loop");
        assert_eq!(negative, 1, "expected one CW hole loop");
        // Sum of signed areas = annular area (outer 2*2=4 minus hole 1*1=1, so 3).
        let total: i128 = signed_areas.iter().sum();
        let unit = 1_i128 << 16;
        assert_eq!(
            total,
            3 * 2 * unit * unit,
            "annular area mismatch — outer + hole signed sum should equal the annular region"
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
