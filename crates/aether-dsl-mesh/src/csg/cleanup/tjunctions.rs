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

use super::merge::normalize_loop;
use super::mesh::{IndexedMesh, IndexedPolygon, VertexId};
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

            // Insert subdivisions unconditionally; if an insertion would
            // produce a non-simple loop (same pool vertex was already in
            // this polygon, or is the subdivision for two adjacent edges
            // of one polygon — the spike pattern from PR 371), feed the
            // result through `normalize_loop` to split at the pinch into
            // simple loops. This preserves both contracts: every
            // subdivision is inserted (no unrepaired T-junctions) and
            // every emitted polygon is simple. A polygon that fully
            // collapses (every branch is degenerate) is dropped.
            let mut next_polygons: Vec<IndexedPolygon> = Vec::with_capacity(polygons.len());
            for poly in &polygons {
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
                for verts in normalize_loop(&new_verts) {
                    next_polygons.push(IndexedPolygon {
                        vertices: verts,
                        plane: poly.plane,
                        color: poly.color,
                    });
                }
            }
            polygons = next_polygons;
        }

        IndexedMesh { vertices, polygons }
    }
}

/// Snap-drift bound for the collinearity check: a vertex within this
/// many fixed units of perpendicular distance from the edge is treated
/// as collinear.
///
/// Derivation matches [`super::weld::WELD_TOLERANCE_FIXED_UNITS`] —
/// each `compute_intersection` snaps by up to 0.5 fixed units per
/// axis, and under the polygon-throughout pipeline (PR 292) cleanup
/// runs once at the end of an entire CSG composition, so a single
/// vertex can accumulate snap drift from every BSP partitioner the
/// fragment survived. Empirically (off-axis CSG corpus, PR 298): a
/// 16-facet tilted cylinder cut through a cube produces T-junction
/// vertices up to ~3.5 fixed units perpendicular to the host edge.
/// Tolerance `4` catches every observed case with margin and stays
/// well below the next-nearest distinct-point spacing in practical
/// CSG inputs (sphere/cylinder facet spacing ≥ 3000+ fixed units).
///
/// Raised from `1` to `4` (2026-04-26) after the geometric-validator
/// corpus showed off-axis CSG produces T-junctions at perpendicular
/// distances of 1.0 to 3.5 fixed units that the prior tolerance
/// silently dropped, leaving render-visible cracks. See issue #299.
const COLLINEAR_TOLERANCE_FIXED_UNITS: i128 = 4;

/// Between-ness test in 3D fixed-point: returns `true` iff `p` lies
/// on the open segment from `a` to `b`, within
/// [`COLLINEAR_TOLERANCE_FIXED_UNITS`] of perpendicular distance.
/// All arithmetic in i128.
///
/// Magnitude budget: input coords ≤ ±2^24 (per ADR-0054 ±256-unit cap),
/// so each i128 difference fits in i32 with margin, and the dot/cross
/// products fit in 2^51 — well within i128. Cross-magnitude squared
/// fits in i128 (≤ 2^102), so the perpendicular distance comparison
/// `cross² ≤ tolerance² · len²` stays in integer arithmetic.
pub(super) fn is_strictly_between(p: Point3, a: Point3, b: Point3) -> bool {
    let abx = (b.x - a.x) as i128;
    let aby = (b.y - a.y) as i128;
    let abz = (b.z - a.z) as i128;
    let apx = (p.x - a.x) as i128;
    let apy = (p.y - a.y) as i128;
    let apz = (p.z - a.z) as i128;

    let len2 = abx * abx + aby * aby + abz * abz;
    if len2 == 0 {
        return false;
    }

    // Collinearity within tolerance: |cross|² ≤ tolerance² · len²
    // (the integer-safe form of "perpendicular distance ≤ tolerance").
    let cx = apy * abz - apz * aby;
    let cy = apz * abx - apx * abz;
    let cz = apx * aby - apy * abx;
    let cross_mag2 = cx * cx + cy * cy + cz * cz;
    let tol2 = COLLINEAR_TOLERANCE_FIXED_UNITS * COLLINEAR_TOLERANCE_FIXED_UNITS;
    if cross_mag2 > tol2 * len2 {
        return false;
    }

    let dot = apx * abx + apy * aby + apz * abz;
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

    /// Regression for the post-merge twin-edge invariant fired by the
    /// box × sphere matrix cells (issue 350 family). When the same pool
    /// vertex `D` is the subdivision for *two* adjacent edges of one
    /// polygon (`(A, B) → D` and `(B, C) → D`), the previous algorithm
    /// inserted `D` twice in one pass — producing the spike pattern
    /// `[A, D, B, D, C]`. Downstream merge cancellation reads the spike
    /// as an internal twin edge `(D,B) + (B,D)`. The fix tracks
    /// inserts within the per-polygon pass and skips repeats.
    #[test]
    fn same_subdivision_vertex_on_two_adjacent_edges_does_not_spike() {
        let plane = xy_plane();
        // Polygon `[A, B, C]` is a triangle with three collinear-ish
        // points so D sits "strictly between" both A→B and B→C under
        // the collinearity tolerance. The setup mirrors what off-axis
        // BSP cuts produce when a sphere rim vertex snap-drifts onto
        // the cube edge it meant to bisect, then onto the adjacent
        // segment of the same edge after the first subdivision.
        let vertices = vec![
            pt(0.0, 0.0, 0.0), // 0: A
            pt(2.0, 0.0, 0.0), // 1: B
            pt(4.0, 0.0, 0.0), // 2: C
            pt(2.0, 1.0, 0.0), // 3: anchor on the polygon (away from line)
            pt(1.0, 0.0, 0.0), // 4: D — strictly between A and B, AND between A and C (so the canonicalization picks D for both segments)
        ];
        let polygons = vec![
            // Source polygon for D so the pool entry is referenced.
            poly(vec![0, 4, 3], plane, 0),
            // The polygon under test: visits A, B, C without any
            // duplicate vertex. After repair, must NOT have D appearing
            // twice — the spike pattern is the bug.
            poly(vec![0, 1, 2, 3], plane, 0),
        ];
        let mesh = IndexedMesh { vertices, polygons };
        let repaired = mesh.repair_tjunctions();
        let target = &repaired.polygons[1].vertices;
        let mut counts = std::collections::HashMap::new();
        for &v in target {
            *counts.entry(v).or_insert(0) += 1;
        }
        for (&v, &c) in &counts {
            assert!(
                c == 1,
                "vertex {v} appears {c} times in repaired polygon {target:?} — spike emitted"
            );
        }
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
    fn strict_between_is_symmetric_in_endpoints() {
        // Pin endpoint symmetry: between(p, a, b) == between(p, b, a).
        // The repair pass canonicalizes edges as (min, max) before
        // looking up subdivisions, but the predicate itself is called
        // on raw endpoints — if it ever stops being symmetric, edge
        // canonicalization wouldn't save us.
        let a = pt(0.0, 0.0, 0.0);
        let b = pt(4.0, 0.0, 0.0);
        let m = pt(1.0, 0.0, 0.0); // strictly between
        let off = pt(1.0, 1.0, 0.0);
        let beyond = pt(5.0, 0.0, 0.0);
        assert_eq!(is_strictly_between(m, a, b), is_strictly_between(m, b, a));
        assert_eq!(
            is_strictly_between(off, a, b),
            is_strictly_between(off, b, a)
        );
        assert_eq!(
            is_strictly_between(beyond, a, b),
            is_strictly_between(beyond, b, a)
        );
    }

    #[test]
    fn strict_between_degenerate_segment_returns_false() {
        // a == b: zero-length segment, no point can be strictly between.
        // Pinned because the `len2 == 0` guard is the only thing
        // preventing a divide-by-zero-style logic error in the dot
        // product comparison.
        let a = pt(1.0, 2.0, 3.0);
        let b = pt(1.0, 2.0, 3.0);
        assert!(!is_strictly_between(a, a, b));
        assert!(!is_strictly_between(pt(0.0, 0.0, 0.0), a, b));
        assert!(!is_strictly_between(pt(2.0, 4.0, 6.0), a, b));
    }

    #[test]
    fn cascading_tjunctions_converge() {
        // Iter 1: edge A→B has two collinear pool vertices M (mid) and
        //         Q (quarter). Code selects the smallest VertexId — M
        //         (id 3) wins over Q (id 4). Polygon gets M inserted:
        //         [A, B, C] → [A, M, B, C].
        // Iter 2: new edge A→M now has Q collinear. Q gets inserted:
        //         [A, M, B, C] → [A, Q, M, B, C].
        // Iter 3: no more violations, loop terminates.
        //
        // Tests the fixed-point convergence — multiple existing tests
        // exercise single-iteration repair, but none cross the
        // iteration boundary.
        let plane = xy_plane();
        let vertices = vec![
            pt(0.0, 0.0, 0.0), // 0: A
            pt(4.0, 0.0, 0.0), // 1: B
            pt(2.0, 1.0, 0.0), // 2: C
            pt(2.0, 0.0, 0.0), // 3: M (midpoint AB)
            pt(1.0, 0.0, 0.0), // 4: Q (midpoint AM)
        ];
        let polygons = vec![poly(vec![0, 1, 2], plane, 0)];
        let mesh = IndexedMesh { vertices, polygons };
        let repaired = mesh.repair_tjunctions();
        assert_eq!(repaired.polygons[0].vertices, vec![0, 4, 3, 1, 2]);
    }

    #[test]
    fn multiple_subdivisions_on_different_edges_of_one_polygon() {
        // Triangle (A, B, C) with M on edge A→B and N on edge B→C.
        // Both subdivisions happen in a single iteration.
        let plane = xy_plane();
        let vertices = vec![
            pt(0.0, 0.0, 0.0), // 0: A
            pt(2.0, 0.0, 0.0), // 1: B
            pt(0.0, 2.0, 0.0), // 2: C
            pt(1.0, 0.0, 0.0), // 3: M on A→B
            pt(1.0, 1.0, 0.0), // 4: N on B→C (midpoint of (2,0)-(0,2))
        ];
        let polygons = vec![poly(vec![0, 1, 2], plane, 0)];
        let mesh = IndexedMesh { vertices, polygons };
        let repaired = mesh.repair_tjunctions();
        assert_eq!(repaired.polygons[0].vertices, vec![0, 3, 1, 4, 2]);
    }

    /// Off-axis CSG accumulates snap drift across cascaded BSP cuts;
    /// the failing T-junction from `box_minus_tilted_cylinder_is_geometric`
    /// (PR 298) sits 2.05 fixed units perpendicular to its host edge.
    /// Pinned with the actual fixed-point coords so a regression in
    /// `COLLINEAR_TOLERANCE_FIXED_UNITS` re-exposes the bug at the unit
    /// level rather than only via the integration corpus.
    #[test]
    fn off_axis_t_junction_within_cascaded_snap_drift_is_detected() {
        // Coords lifted from validate_no_t_junctions on the failing
        // case: vertex at (-0.0961, -0.75, -0.7010), edge endpoints
        // (-0.1824, -0.75, -0.7009) → (0, -0.75, -0.7010). Snapped
        // to 16:16 fixed:
        let a = Point3 {
            x: -11951,
            y: -49152,
            z: -45936,
        };
        let b = Point3 {
            x: 0,
            y: -49152,
            z: -45938,
        };
        let v = Point3 {
            x: -6299,
            y: -49152,
            z: -45939,
        };
        // Perpendicular distance: ~2.05 fixed units, within the
        // post-fix collinearity tolerance.
        assert!(
            is_strictly_between(v, a, b),
            "off-axis T-junction at ~2 fixed units perpendicular drift \
             must be classified as collinear (issue #299)"
        );
    }

    /// Loop-splitting regression. When a subdivision `w` for edge
    /// `(a, b)` is already present in the polygon's loop at a different
    /// position, inserting blindly would emit `[..., a, w, b, ..., w, ...]`
    /// — non-simple. Skipping (PR 371's containment) leaves an
    /// unrepaired T-junction and trips the post-tjunctions invariant.
    /// The fix splits at `w` into two simple loops sharing the vertex.
    /// Box × sphere was the canonical matrix repro.
    #[test]
    fn unsafe_subdivision_splits_polygon_into_two_simple_loops() {
        // Polygon under test is a 7-gon `[A, B, C, w, D, E, F]` where
        // `w` already sits at index 3 and is also collinear-strictly-
        // between the polygon's first edge `A → B`. Inserting `w` into
        // `(A, B)` produces `[A, w, B, C, w, D, E, F]` — w at indices
        // 1 and 4. `normalize_loop` splits at the pinch:
        //   outer = verts[..=1] + verts[5..] = [A, w, D, E, F]
        //   inner = verts[1..4]              = [w, B, C]
        let plane = xy_plane();
        let vertices = vec![
            pt(0.0, 0.0, 0.0),  // 0: A
            pt(4.0, 0.0, 0.0),  // 1: B
            pt(3.0, 1.0, 0.0),  // 2: C
            pt(2.0, 0.0, 0.0),  // 3: w (on edge A→B, also in poly loop)
            pt(1.0, 2.0, 0.0),  // 4: D
            pt(0.0, 2.0, 0.0),  // 5: E
            pt(-1.0, 1.0, 0.0), // 6: F
        ];
        let polygons = vec![
            // Source polygon to keep w referenced.
            poly(vec![0, 3, 4], plane, 0),
            // Polygon under test.
            poly(vec![0, 1, 2, 3, 4, 5, 6], plane, 0),
        ];
        let mesh = IndexedMesh { vertices, polygons };
        let repaired = mesh.repair_tjunctions();

        // Every emitted polygon must be simple (no repeated vertex).
        for p in &repaired.polygons {
            let mut seen = std::collections::HashSet::new();
            for &v in &p.vertices {
                assert!(
                    seen.insert(v),
                    "polygon {:?} contains repeated vertex {v}",
                    p.vertices
                );
            }
        }

        // The 7-gon split — the source polygon (3 verts) plus two
        // simple loops sharing w. Compare loops as multisets to
        // tolerate rotation / order-of-emission differences.
        assert_eq!(repaired.polygons.len(), 3);
        let actual_loops: std::collections::BTreeSet<std::collections::BTreeSet<VertexId>> =
            repaired
                .polygons
                .iter()
                .map(|p| p.vertices.iter().copied().collect())
                .collect();
        let expected_loops: std::collections::BTreeSet<std::collections::BTreeSet<VertexId>> =
            [vec![0, 3, 4], vec![0, 3, 4, 5, 6], vec![3, 1, 2]]
                .into_iter()
                .map(|v| v.into_iter().collect())
                .collect();
        assert_eq!(actual_loops, expected_loops);
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
