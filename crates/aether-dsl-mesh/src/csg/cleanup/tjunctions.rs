//! Pass 3: T-junction removal.
//!
//! After welding + coplanar merging, an edge of one merged region may
//! pass through an interior vertex of an adjacent region. Without
//! lighting these are mostly invisible in the wireframe (they look
//! like extra edges, indistinguishable from merge artifacts) but they
//! produce hairline rendering cracks the moment shading is introduced.
//! Repairing them now means lighting can land without a follow-up
//! pass.
//!
//! Algorithm: for each canonical edge in the mesh, find any vertex in
//! the pool that lies strictly between the endpoints (exact integer
//! collinearity + between-ness test). If found, insert that vertex
//! into every polygon containing the edge — in either direction. Loop
//! to fixed point: each iteration strictly reduces the count of
//! `(edge, intermediate vertex)` violations, so termination is
//! guaranteed.
//!
//! v1 ships the naive O(E·V) detection per iteration; spatial
//! bucketing is the Phase 2 optimization mentioned in ADR-0055 if
//! profiling shows cleanup time dominates mesh authoring.

use super::mesh::{IndexedMesh, VertexId};
use crate::csg::point::Point3;
use std::collections::{HashMap, HashSet};

/// Defensive upper bound on iteration count. The fixed-point argument
/// guarantees termination, but a runaway loop would still be worse than
/// a bounded one — exiting early leaves any remaining T-junctions in
/// place rather than hanging mesh authoring.
const MAX_TJUNCTION_ITERATIONS: usize = 256;

impl IndexedMesh {
    pub(super) fn repair_tjunctions(self) -> Self {
        let IndexedMesh {
            vertices,
            mut polygons,
        } = self;

        for _ in 0..MAX_TJUNCTION_ITERATIONS {
            let mut edges: HashSet<(VertexId, VertexId)> = HashSet::new();
            for poly in &polygons {
                let n = poly.vertices.len();
                for i in 0..n {
                    let a = poly.vertices[i];
                    let b = poly.vertices[(i + 1) % n];
                    if a == b {
                        continue;
                    }
                    edges.insert(if a < b { (a, b) } else { (b, a) });
                }
            }

            let mut sorted_edges: Vec<(VertexId, VertexId)> = edges.into_iter().collect();
            sorted_edges.sort();

            let mut subdivisions: HashMap<(VertexId, VertexId), VertexId> = HashMap::new();
            for &(a, b) in &sorted_edges {
                let pa = vertices[a];
                let pb = vertices[b];
                let mut found: Option<VertexId> = None;
                for (v, &p) in vertices.iter().enumerate() {
                    if v == a || v == b {
                        continue;
                    }
                    if is_strictly_between(p, pa, pb) {
                        match found {
                            None => found = Some(v),
                            Some(prev) if v < prev => found = Some(v),
                            _ => {}
                        }
                    }
                }
                if let Some(v) = found {
                    subdivisions.insert((a, b), v);
                }
            }

            if subdivisions.is_empty() {
                break;
            }

            for poly in &mut polygons {
                let n = poly.vertices.len();
                let mut new_verts: Vec<VertexId> = Vec::with_capacity(n + subdivisions.len());
                for i in 0..n {
                    let a = poly.vertices[i];
                    let b = poly.vertices[(i + 1) % n];
                    new_verts.push(a);
                    let canon = if a < b { (a, b) } else { (b, a) };
                    if let Some(&v) = subdivisions.get(&canon)
                        && v != a
                        && v != b
                    {
                        new_verts.push(v);
                    }
                }
                poly.vertices = new_verts;
            }
        }

        IndexedMesh { vertices, polygons }
    }
}

/// Strict between-ness test in 3D fixed-point: returns `true` iff `p`
/// lies on the open segment from `a` to `b` (excluding endpoints). All
/// arithmetic is exact in `i128`.
///
/// Magnitude budget: input coords ≤ ±2^24 (per ADR-0054 ±256-unit cap),
/// so each i128 difference fits in i32 with margin, and the dot/cross
/// products fit in 2^51 — well within i128.
fn is_strictly_between(p: Point3, a: Point3, b: Point3) -> bool {
    let abx = (b.x - a.x) as i128;
    let aby = (b.y - a.y) as i128;
    let abz = (b.z - a.z) as i128;
    let apx = (p.x - a.x) as i128;
    let apy = (p.y - a.y) as i128;
    let apz = (p.z - a.z) as i128;

    let cx = apy * abz - apz * aby;
    let cy = apz * abx - apx * abz;
    let cz = apx * aby - apy * abx;
    if cx != 0 || cy != 0 || cz != 0 {
        return false;
    }

    let dot = apx * abx + apy * aby + apz * abz;
    let len2 = abx * abx + aby * aby + abz * abz;
    if len2 == 0 {
        return false;
    }
    dot > 0 && dot < len2
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::csg::fixed::f32_to_fixed;
    use crate::csg::plane::Plane3;

    use super::super::mesh::IndexedPolygon;

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

    fn poly(vertices: Vec<VertexId>, plane: Plane3, color: u32) -> IndexedPolygon {
        IndexedPolygon {
            vertices,
            plane,
            color,
        }
    }

    #[test]
    fn strict_between_accepts_midpoint_of_axis_aligned_edge() {
        let a = pt(0.0, 0.0, 0.0);
        let b = pt(2.0, 0.0, 0.0);
        let m = pt(1.0, 0.0, 0.0);
        assert!(is_strictly_between(m, a, b));
    }

    #[test]
    fn strict_between_rejects_endpoints() {
        let a = pt(0.0, 0.0, 0.0);
        let b = pt(2.0, 0.0, 0.0);
        assert!(!is_strictly_between(a, a, b));
        assert!(!is_strictly_between(b, a, b));
    }

    #[test]
    fn strict_between_rejects_off_line() {
        let a = pt(0.0, 0.0, 0.0);
        let b = pt(2.0, 0.0, 0.0);
        let off = pt(1.0, 0.5, 0.0);
        assert!(!is_strictly_between(off, a, b));
    }

    #[test]
    fn strict_between_rejects_collinear_outside_segment() {
        let a = pt(0.0, 0.0, 0.0);
        let b = pt(2.0, 0.0, 0.0);
        let beyond = pt(3.0, 0.0, 0.0);
        let behind = pt(-1.0, 0.0, 0.0);
        assert!(!is_strictly_between(beyond, a, b));
        assert!(!is_strictly_between(behind, a, b));
    }

    #[test]
    fn strict_between_handles_3d_diagonal_edge() {
        let a = pt(0.0, 0.0, 0.0);
        let b = pt(2.0, 2.0, 2.0);
        let m = pt(1.0, 1.0, 1.0);
        let off = pt(1.0, 1.0, 0.5);
        assert!(is_strictly_between(m, a, b));
        assert!(!is_strictly_between(off, a, b));
    }

    #[test]
    fn t_junction_inserts_vertex_into_long_edge() {
        // Polygon 1: triangle (A, B, C) with edge A→B at y=0.
        // Polygon 2: triangle (A, D, E) with vertex D = (1, 0) on A→B.
        let plane = xy_plane();
        let vertices = vec![
            pt(0.0, 0.0, 0.0),  // 0: A
            pt(2.0, 0.0, 0.0),  // 1: B
            pt(1.0, 1.0, 0.0),  // 2: C
            pt(1.0, 0.0, 0.0),  // 3: D — on AB
            pt(1.0, -1.0, 0.0), // 4: E
        ];
        let polygons = vec![poly(vec![0, 1, 2], plane, 0), poly(vec![0, 3, 4], plane, 0)];
        let mesh = IndexedMesh { vertices, polygons };
        let repaired = mesh.repair_tjunctions();
        assert_eq!(repaired.polygons[0].vertices, vec![0, 3, 1, 2]);
        assert_eq!(repaired.polygons[1].vertices, vec![0, 3, 4]);
    }

    #[test]
    fn t_junction_inserts_into_polygon_walking_edge_in_reverse() {
        // Polygon 1 has edge B→A (reverse of polygon 2's A→B).
        let plane = xy_plane();
        let vertices = vec![
            pt(0.0, 0.0, 0.0),
            pt(2.0, 0.0, 0.0),
            pt(1.0, 1.0, 0.0),
            pt(1.0, 0.0, 0.0),
            pt(1.0, -1.0, 0.0),
        ];
        let polygons = vec![
            poly(vec![1, 0, 2], plane, 0), // walks B → A → C
            poly(vec![0, 3, 4], plane, 0),
        ];
        let mesh = IndexedMesh { vertices, polygons };
        let repaired = mesh.repair_tjunctions();
        // Walking B → A, the subdivision D should slot between B and A.
        assert_eq!(repaired.polygons[0].vertices, vec![1, 3, 0, 2]);
    }

    #[test]
    fn no_tjunctions_is_a_no_op() {
        let plane = xy_plane();
        let vertices = vec![pt(0.0, 0.0, 0.0), pt(1.0, 0.0, 0.0), pt(0.0, 1.0, 0.0)];
        let polygons = vec![poly(vec![0, 1, 2], plane, 0)];
        let mesh = IndexedMesh {
            vertices: vertices.clone(),
            polygons: polygons.clone(),
        };
        let repaired = mesh.repair_tjunctions();
        assert_eq!(repaired.polygons.len(), 1);
        assert_eq!(repaired.polygons[0].vertices, vec![0, 1, 2]);
    }

    #[test]
    fn multiple_tjunctions_on_same_edge_resolve_in_order() {
        // Edge A→B has TWO interior vertices D₁ = (1, 0), D₂ = (3, 0).
        let plane = xy_plane();
        let vertices = vec![
            pt(0.0, 0.0, 0.0),  // 0: A
            pt(4.0, 0.0, 0.0),  // 1: B
            pt(2.0, 1.0, 0.0),  // 2: C
            pt(1.0, 0.0, 0.0),  // 3: D₁
            pt(1.0, -1.0, 0.0), // 4: E₁
            pt(3.0, 0.0, 0.0),  // 5: D₂
            pt(3.0, -1.0, 0.0), // 6: E₂
        ];
        let polygons = vec![
            poly(vec![0, 1, 2], plane, 0),
            poly(vec![0, 3, 4], plane, 0), // hosts D₁
            poly(vec![1, 5, 6], plane, 0), // hosts D₂
        ];
        let mesh = IndexedMesh { vertices, polygons };
        let repaired = mesh.repair_tjunctions();
        // Polygon 0 has D₁ and D₂ inserted in order along A→B.
        assert_eq!(repaired.polygons[0].vertices, vec![0, 3, 5, 1, 2]);
    }

    #[test]
    fn tjunction_repair_is_deterministic() {
        let plane = xy_plane();
        let vertices = vec![
            pt(0.0, 0.0, 0.0),
            pt(2.0, 0.0, 0.0),
            pt(1.0, 1.0, 0.0),
            pt(1.0, 0.0, 0.0),
            pt(1.0, -1.0, 0.0),
        ];
        let polygons = vec![poly(vec![0, 1, 2], plane, 0), poly(vec![0, 3, 4], plane, 0)];
        let m1 = IndexedMesh {
            vertices: vertices.clone(),
            polygons: polygons.clone(),
        };
        let m2 = IndexedMesh { vertices, polygons };
        let r1 = m1.repair_tjunctions();
        let r2 = m2.repair_tjunctions();
        for (p, q) in r1.polygons.iter().zip(r2.polygons.iter()) {
            assert_eq!(p.vertices, q.vertices);
        }
    }

    #[test]
    fn unreferenced_pool_vertex_collinear_on_an_edge_is_still_inserted() {
        // The vertex pool may legitimately contain vertices that aren't
        // referenced by any polygon (e.g., dropped degenerate slivers from
        // welding). They must NOT be inserted as T-junction subdivisions —
        // adding a phantom vertex into an otherwise clean edge would be
        // wrong. But here we *do* want them inserted IF the geometry says
        // they're on an edge: the welded vertex pool is the canonical
        // identity, so any pool vertex that is collinear on an edge is a
        // genuine T-junction by definition.
        //
        // (This test documents the current behavior — pool membership is
        // the trigger, not polygon-reference. If welding ever stops pruning
        // unreferenced vertices, this becomes worth revisiting.)
        let plane = xy_plane();
        let vertices = vec![
            pt(0.0, 0.0, 0.0), // 0
            pt(2.0, 0.0, 0.0), // 1
            pt(1.0, 1.0, 0.0), // 2
            pt(1.0, 0.0, 0.0), // 3 — referenced by no polygon yet present in pool
        ];
        let polygons = vec![poly(vec![0, 1, 2], plane, 0)];
        let mesh = IndexedMesh { vertices, polygons };
        let repaired = mesh.repair_tjunctions();
        assert_eq!(repaired.polygons[0].vertices, vec![0, 3, 1, 2]);
    }
}
