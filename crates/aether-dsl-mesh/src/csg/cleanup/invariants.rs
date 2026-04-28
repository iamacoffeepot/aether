//! Pipeline step invariants (issue 337).
//!
//! Diagnostic-only contracts between cleanup-pipeline passes. Each
//! `find_*` fn inspects an [`IndexedMesh`] at a stage boundary and
//! returns a list of violations — empty Vec means the invariant holds.
//! Wiring code in [`super::run_to_indexed`] emits a `tracing::warn!`
//! when violations surface, carrying enough detail to repro.
//!
//! Soak-then-promote cadence: ship as warn-only first; once warns have
//! gone quiet against the test corpus and live smoke tests, promote the
//! check to `debug_assert!` so the invariant is enforced in tests but
//! costs nothing in release builds.
//!
//! Invariants are deliberately **structural**, not geometric — they pin
//! the contracts the pipeline's docstring already describes prose-style
//! (no twin edges after merge, every polygon has ≥3 vertices, etc.).
//! Geometric correctness (manifold-ness, no self-intersection) belongs
//! in a separate validation pass; this module is about pass composition.

use super::mesh::{IndexedMesh, VertexId};
use super::tjunctions::is_strictly_between;
use crate::csg::plane::Plane3;
use crate::csg::point::Point3;

/// One twin-edge violation surfaced by [`find_twin_edges`].
#[derive(Debug, Clone)]
pub(in crate::csg) struct TwinEdgeViolation {
    pub plane: Plane3,
    pub color: u32,
    pub edge: (VertexId, VertexId),
}

/// One post-weld pool-integrity violation. Either a polygon references a
/// `VertexId` outside the pool, or two distinct ids share identical
/// fixed-point coordinates — both mean the welded mesh's vertex identity
/// guarantee broke.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::csg) enum PostWeldViolation {
    /// Polygon[`poly_idx`] references `vertex_id ≥ pool_size`.
    OrphanedId {
        poly_idx: usize,
        vertex_id: VertexId,
        pool_size: usize,
    },
    /// Two distinct pool ids share identical coordinates — the welding
    /// pass's tolerance lookup missed them.
    DuplicateCoords {
        keep_id: VertexId,
        drop_id: VertexId,
        point: Point3,
    },
}

/// One post-T-junction-repair violation: a vertex in the pool lies
/// strictly interior to some polygon's edge, meaning the repair pass
/// didn't reach a fixed point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::csg) struct UnrepairedTJunction {
    pub edge: (VertexId, VertexId),
    pub interior_vertex: VertexId,
}

/// One post-sliver-removal degeneracy violation. A polygon either
/// dropped below 3 vertices (should have been retained-out by the pass)
/// or carries adjacent duplicate `VertexId`s (the dedup didn't fire).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::csg) enum PostSliverViolation {
    /// Polygon has fewer than 3 vertices.
    TooFewVertices { poly_idx: usize, len: usize },
    /// `polygon.vertices[i] == polygon.vertices[(i+1) % n]`.
    AdjacentDuplicate {
        poly_idx: usize,
        index: usize,
        vertex_id: VertexId,
    },
}

type BucketKey = ((i64, i64, i64, i128), u32);

fn bucket_key(plane: &Plane3, color: u32) -> BucketKey {
    ((plane.n_x, plane.n_y, plane.n_z, plane.d), color)
}

/// Post-`merge_coplanar` invariant: no two coplanar same-color polygons
/// share a directed edge (twin pair (a,b) + (b,a) within the same
/// `(plane, color)` bucket). Survivors mean twin cancellation didn't
/// fire — the merge pass produced output the renderer will try to draw
/// twice, with whichever poly wins the depth test on top.
///
/// Returns one violation per surviving twin pair. Buckets are scanned
/// in undefined order; within a bucket, `edge` is the lexicographically
/// smaller direction so each twin pair surfaces exactly once.
pub(in crate::csg) fn find_twin_edges(mesh: &IndexedMesh) -> Vec<TwinEdgeViolation> {
    use std::collections::HashMap;
    let mut directed: HashMap<BucketKey, HashMap<(VertexId, VertexId), usize>> = HashMap::new();
    for poly in &mesh.polygons {
        let key = bucket_key(&poly.plane, poly.color);
        let entry = directed.entry(key).or_default();
        let m = poly.vertices.len();
        for i in 0..m {
            let a = poly.vertices[i];
            let b = poly.vertices[(i + 1) % m];
            *entry.entry((a, b)).or_insert(0) += 1;
        }
    }

    let mut violations: Vec<TwinEdgeViolation> = Vec::new();
    for (key, edges) in &directed {
        let plane = Plane3 {
            n_x: key.0.0,
            n_y: key.0.1,
            n_z: key.0.2,
            d: key.0.3,
        };
        let color = key.1;
        for &(a, b) in edges.keys() {
            // Only report each pair once: skip if (b,a) is the
            // canonical direction.
            if a > b {
                continue;
            }
            if edges.contains_key(&(b, a)) {
                violations.push(TwinEdgeViolation {
                    plane,
                    color,
                    edge: (a, b),
                });
            }
        }
    }
    // Deterministic order so warn output and tests are stable.
    violations.sort_by_key(|v| {
        (
            v.plane.n_x,
            v.plane.n_y,
            v.plane.n_z,
            v.plane.d,
            v.color,
            v.edge,
        )
    });
    violations
}

/// Post-`weld` invariant: every `VertexId` referenced by a polygon
/// exists in the pool, and no two distinct pool ids share identical
/// fixed-point coordinates. Catches a tolerance-lookup regression that
/// would silently break vertex identity for downstream passes.
///
/// O(P + V) — orphan check is per-polygon-vertex, duplicate check is
/// one linear sweep into a `HashMap<Point3, VertexId>`.
pub(in crate::csg) fn find_post_weld_violations(mesh: &IndexedMesh) -> Vec<PostWeldViolation> {
    use std::collections::HashMap;
    let mut violations: Vec<PostWeldViolation> = Vec::new();
    let pool_size = mesh.vertices.len();

    for (poly_idx, poly) in mesh.polygons.iter().enumerate() {
        for &vertex_id in &poly.vertices {
            if vertex_id >= pool_size {
                violations.push(PostWeldViolation::OrphanedId {
                    poly_idx,
                    vertex_id,
                    pool_size,
                });
            }
        }
    }

    let mut by_coord: HashMap<Point3, VertexId> = HashMap::with_capacity(pool_size);
    for (id, &point) in mesh.vertices.iter().enumerate() {
        match by_coord.get(&point) {
            Some(&prior) => violations.push(PostWeldViolation::DuplicateCoords {
                keep_id: prior,
                drop_id: id,
                point,
            }),
            None => {
                by_coord.insert(point, id);
            }
        }
    }

    violations.sort_by_key(|v| match v {
        PostWeldViolation::OrphanedId {
            poly_idx,
            vertex_id,
            ..
        } => (0u8, *poly_idx, *vertex_id, 0usize),
        PostWeldViolation::DuplicateCoords {
            keep_id, drop_id, ..
        } => (1u8, *keep_id, *drop_id, 0usize),
    });
    violations
}

/// Post-`repair_tjunctions` invariant: no vertex in the pool lies
/// strictly interior to another polygon's edge. The repair pass loops
/// to a fixed point, so anything surviving here means the pass exited
/// before convergence (iteration cap hit, tolerance miss, etc.).
///
/// O(E·V) — same complexity as one repair iteration, deliberately. The
/// check is warn-only diagnostic; if it dominates cleanup time the
/// soak-then-promote cadence in the module doc applies (cull the warn
/// or move it behind a `cfg(debug_assertions)`).
pub(in crate::csg) fn find_unrepaired_tjunctions(mesh: &IndexedMesh) -> Vec<UnrepairedTJunction> {
    use std::collections::HashSet;
    let mut edges: HashSet<(VertexId, VertexId)> = HashSet::new();
    for poly in &mesh.polygons {
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

    let mut violations: Vec<UnrepairedTJunction> = Vec::new();
    let mut sorted_edges: Vec<(VertexId, VertexId)> = edges.into_iter().collect();
    sorted_edges.sort();
    for &(a, b) in &sorted_edges {
        let pa = mesh.vertices[a];
        let pb = mesh.vertices[b];
        for (v, &p) in mesh.vertices.iter().enumerate() {
            if v == a || v == b {
                continue;
            }
            if is_strictly_between(p, pa, pb) {
                violations.push(UnrepairedTJunction {
                    edge: (a, b),
                    interior_vertex: v,
                });
            }
        }
    }
    violations
}

/// Post-`remove_slivers` invariant: every polygon has ≥3 vertices and
/// no two consecutive vertices coincide. The pass guarantees both
/// conditions tautologically (retain on `len >= 3`, dedup-consecutive
/// before retain), so violations here mean a regression in either step.
///
/// O(P · V_avg).
pub(in crate::csg) fn find_post_sliver_violations(mesh: &IndexedMesh) -> Vec<PostSliverViolation> {
    let mut violations: Vec<PostSliverViolation> = Vec::new();
    for (poly_idx, poly) in mesh.polygons.iter().enumerate() {
        let n = poly.vertices.len();
        if n < 3 {
            violations.push(PostSliverViolation::TooFewVertices { poly_idx, len: n });
            continue;
        }
        for i in 0..n {
            let a = poly.vertices[i];
            let b = poly.vertices[(i + 1) % n];
            if a == b {
                violations.push(PostSliverViolation::AdjacentDuplicate {
                    poly_idx,
                    index: i,
                    vertex_id: a,
                });
            }
        }
    }
    violations
}

#[cfg(test)]
mod tests {
    use super::super::mesh::IndexedPolygon;
    use super::*;
    use crate::csg::point::Point3;

    fn pt(x: i32, y: i32, z: i32) -> Point3 {
        Point3 { x, y, z }
    }

    fn xy_plane() -> Plane3 {
        Plane3 {
            n_x: 0,
            n_y: 0,
            n_z: 1,
            d: 0,
        }
    }

    #[test]
    fn empty_mesh_has_no_violations() {
        let mesh = IndexedMesh {
            vertices: vec![],
            polygons: vec![],
        };
        assert!(find_twin_edges(&mesh).is_empty());
    }

    #[test]
    fn single_triangle_has_no_twin_edges() {
        let mesh = IndexedMesh {
            vertices: vec![pt(0, 0, 0), pt(1, 0, 0), pt(0, 1, 0)],
            polygons: vec![IndexedPolygon {
                vertices: vec![0, 1, 2],
                plane: xy_plane(),
                color: 0,
            }],
        };
        assert!(find_twin_edges(&mesh).is_empty());
    }

    /// Two coplanar same-color triangles sharing edge (0,1) — one walks
    /// 0→1, the other walks 1→0, so the twin cancels. Merge would have
    /// eliminated this; finding it post-merge means the pass missed it.
    #[test]
    fn twin_edge_in_same_bucket_is_a_violation() {
        let mesh = IndexedMesh {
            vertices: vec![pt(0, 0, 0), pt(1, 0, 0), pt(0, 1, 0), pt(1, 1, 0)],
            polygons: vec![
                IndexedPolygon {
                    vertices: vec![0, 1, 2],
                    plane: xy_plane(),
                    color: 0,
                },
                IndexedPolygon {
                    vertices: vec![1, 0, 3],
                    plane: xy_plane(),
                    color: 0,
                },
            ],
        };
        let violations = find_twin_edges(&mesh);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].edge, (0, 1));
        assert_eq!(violations[0].color, 0);
    }

    /// Two same-edge polygons in **different color** buckets are NOT a
    /// violation — the merge pass deliberately leaves color seams alone
    /// (see merge.rs module doc § "Why the bucket key includes color").
    #[test]
    fn twin_edge_across_color_buckets_is_not_a_violation() {
        let mesh = IndexedMesh {
            vertices: vec![pt(0, 0, 0), pt(1, 0, 0), pt(0, 1, 0), pt(1, 1, 0)],
            polygons: vec![
                IndexedPolygon {
                    vertices: vec![0, 1, 2],
                    plane: xy_plane(),
                    color: 0,
                },
                IndexedPolygon {
                    vertices: vec![1, 0, 3],
                    plane: xy_plane(),
                    color: 1,
                },
            ],
        };
        assert!(find_twin_edges(&mesh).is_empty());
    }

    /// Same-edge polygons on opposite-facing planes (different `n_z`
    /// sign) are NOT a violation — they're back-to-back faces, which is
    /// the normal CSG result for a thin sliver. The bucket key is the
    /// exact `Plane3`, so opposite normals fall into different buckets.
    #[test]
    fn twin_edge_across_opposite_planes_is_not_a_violation() {
        let opp = Plane3 {
            n_x: 0,
            n_y: 0,
            n_z: -1,
            d: 0,
        };
        let mesh = IndexedMesh {
            vertices: vec![pt(0, 0, 0), pt(1, 0, 0), pt(0, 1, 0), pt(1, 1, 0)],
            polygons: vec![
                IndexedPolygon {
                    vertices: vec![0, 1, 2],
                    plane: xy_plane(),
                    color: 0,
                },
                IndexedPolygon {
                    vertices: vec![1, 0, 3],
                    plane: opp,
                    color: 0,
                },
            ],
        };
        assert!(find_twin_edges(&mesh).is_empty());
    }

    /// Two surviving twin pairs in the same bucket — make sure both
    /// surface and the output is deterministic.
    #[test]
    fn multiple_twin_edges_all_surface() {
        let mesh = IndexedMesh {
            vertices: vec![
                pt(0, 0, 0),
                pt(1, 0, 0),
                pt(1, 1, 0),
                pt(0, 1, 0),
                pt(2, 0, 0),
                pt(2, 1, 0),
            ],
            polygons: vec![
                // Quad A walks 0→1→2→3.
                IndexedPolygon {
                    vertices: vec![0, 1, 2, 3],
                    plane: xy_plane(),
                    color: 0,
                },
                // Quad B walks 1→4→5→2 — shares edge (1,2) reversed
                // (i.e. (2,1) inside quad A).
                IndexedPolygon {
                    vertices: vec![1, 4, 5, 2],
                    plane: xy_plane(),
                    color: 0,
                },
                // Triangle C walks 0→3→2 — shares edge (3,2) reversed
                // with quad A's (2,3).
                IndexedPolygon {
                    vertices: vec![0, 3, 2],
                    plane: xy_plane(),
                    color: 0,
                },
            ],
        };
        let violations = find_twin_edges(&mesh);
        // Twins: A.(1,2) ↔ B.(2,1), A.(2,3) ↔ C.(3,2), A.(3,0) ↔ C.(0,3)
        // → three pairs canonicalised to (1,2), (2,3), and (0,3).
        assert_eq!(violations.len(), 3);
        let edges: Vec<_> = violations.iter().map(|v| v.edge).collect();
        assert!(edges.contains(&(0, 3)));
        assert!(edges.contains(&(1, 2)));
        assert!(edges.contains(&(2, 3)));
    }

    #[test]
    fn post_weld_clean_mesh_has_no_violations() {
        let mesh = IndexedMesh {
            vertices: vec![pt(0, 0, 0), pt(1, 0, 0), pt(0, 1, 0)],
            polygons: vec![IndexedPolygon {
                vertices: vec![0, 1, 2],
                plane: xy_plane(),
                color: 0,
            }],
        };
        assert!(find_post_weld_violations(&mesh).is_empty());
    }

    #[test]
    fn post_weld_orphaned_vertex_id_surfaces() {
        // Polygon references id 5 but pool only has 3 entries.
        let mesh = IndexedMesh {
            vertices: vec![pt(0, 0, 0), pt(1, 0, 0), pt(0, 1, 0)],
            polygons: vec![IndexedPolygon {
                vertices: vec![0, 1, 5],
                plane: xy_plane(),
                color: 0,
            }],
        };
        let violations = find_post_weld_violations(&mesh);
        assert_eq!(
            violations,
            vec![PostWeldViolation::OrphanedId {
                poly_idx: 0,
                vertex_id: 5,
                pool_size: 3,
            }]
        );
    }

    /// Two distinct VertexIds resolving to identical fixed-point
    /// coords — exactly what the welding pass should have folded
    /// together. Surfacing this means the tolerance lookup missed.
    #[test]
    fn post_weld_duplicate_coords_surface() {
        let mesh = IndexedMesh {
            vertices: vec![pt(0, 0, 0), pt(1, 0, 0), pt(1, 0, 0)],
            polygons: vec![IndexedPolygon {
                vertices: vec![0, 1, 2],
                plane: xy_plane(),
                color: 0,
            }],
        };
        let violations = find_post_weld_violations(&mesh);
        assert_eq!(violations.len(), 1);
        assert!(matches!(
            violations[0],
            PostWeldViolation::DuplicateCoords {
                keep_id: 1,
                drop_id: 2,
                ..
            }
        ));
    }

    #[test]
    fn post_tjunctions_clean_mesh_has_no_violations() {
        // Two adjacent triangles sharing edge (0, 1). Coords are at
        // CSG-realistic spacing (~1 world unit = 65536 fixed) so the
        // 4-fixed-unit perpendicular tolerance in `is_strictly_between`
        // doesn't spuriously flag triangle apexes as collinear.
        let mesh = IndexedMesh {
            vertices: vec![
                pt(0, 0, 0),
                pt(20000, 0, 0),
                pt(0, 20000, 0),
                pt(20000, 20000, 0),
            ],
            polygons: vec![
                IndexedPolygon {
                    vertices: vec![0, 1, 2],
                    plane: xy_plane(),
                    color: 0,
                },
                IndexedPolygon {
                    vertices: vec![1, 3, 2],
                    plane: xy_plane(),
                    color: 0,
                },
            ],
        };
        assert!(find_unrepaired_tjunctions(&mesh).is_empty());
    }

    /// Pool vertex id 2 lies strictly between (0, 0, 0) and
    /// (40000, 0, 0). A surviving polygon edge (0, 1) means the repair
    /// pass exited before subdividing — exactly the condition this
    /// invariant is designed to flag.
    #[test]
    fn post_tjunctions_strictly_interior_vertex_surfaces() {
        let mesh = IndexedMesh {
            vertices: vec![
                pt(0, 0, 0),     // 0: edge start
                pt(40000, 0, 0), // 1: edge end
                pt(20000, 0, 0), // 2: midpoint — strictly between (0,1)
                pt(0, 20000, 0), // 3: triangle apex (well clear of tolerance)
            ],
            polygons: vec![IndexedPolygon {
                vertices: vec![0, 1, 3],
                plane: xy_plane(),
                color: 0,
            }],
        };
        let violations = find_unrepaired_tjunctions(&mesh);
        assert_eq!(
            violations,
            vec![UnrepairedTJunction {
                edge: (0, 1),
                interior_vertex: 2,
            }]
        );
    }

    /// Endpoint-only vertices (lying *at* a, b — not strictly between)
    /// are not violations. Pin so a tolerance bump in
    /// `is_strictly_between` doesn't accidentally start flagging them.
    #[test]
    fn post_tjunctions_endpoint_collinear_vertex_is_not_a_violation() {
        let mesh = IndexedMesh {
            vertices: vec![
                pt(0, 0, 0),     // 0
                pt(40000, 0, 0), // 1
                pt(0, 0, 0),     // 2: duplicate of 0 — endpoint, not interior
                pt(0, 20000, 0), // 3
            ],
            polygons: vec![IndexedPolygon {
                vertices: vec![0, 1, 3],
                plane: xy_plane(),
                color: 0,
            }],
        };
        // Note: pool has duplicate coords so post-weld would flag it,
        // but post-tjunctions only checks strict between-ness.
        assert!(find_unrepaired_tjunctions(&mesh).is_empty());
    }

    #[test]
    fn post_sliver_clean_triangle_has_no_violations() {
        let mesh = IndexedMesh {
            vertices: vec![pt(0, 0, 0), pt(1, 0, 0), pt(0, 1, 0)],
            polygons: vec![IndexedPolygon {
                vertices: vec![0, 1, 2],
                plane: xy_plane(),
                color: 0,
            }],
        };
        assert!(find_post_sliver_violations(&mesh).is_empty());
    }

    #[test]
    fn post_sliver_polygon_with_two_vertices_is_a_violation() {
        let mesh = IndexedMesh {
            vertices: vec![pt(0, 0, 0), pt(1, 0, 0), pt(0, 1, 0)],
            polygons: vec![IndexedPolygon {
                vertices: vec![0, 1],
                plane: xy_plane(),
                color: 0,
            }],
        };
        let violations = find_post_sliver_violations(&mesh);
        assert_eq!(
            violations,
            vec![PostSliverViolation::TooFewVertices {
                poly_idx: 0,
                len: 2,
            }]
        );
    }

    /// Adjacent duplicate within a polygon's vertex list — the slivers
    /// pass's `dedup_consecutive_and_self_close` should have collapsed
    /// it. Includes wrap-around case (last == first) since that's the
    /// closing-edge form the dedup also handles.
    #[test]
    fn post_sliver_adjacent_duplicate_surfaces() {
        let mesh = IndexedMesh {
            vertices: vec![pt(0, 0, 0), pt(1, 0, 0), pt(0, 1, 0)],
            polygons: vec![IndexedPolygon {
                vertices: vec![0, 1, 1, 2], // adjacent duplicate at index 1
                plane: xy_plane(),
                color: 0,
            }],
        };
        let violations = find_post_sliver_violations(&mesh);
        assert_eq!(
            violations,
            vec![PostSliverViolation::AdjacentDuplicate {
                poly_idx: 0,
                index: 1,
                vertex_id: 1,
            }]
        );
    }

    #[test]
    fn post_sliver_wraparound_duplicate_surfaces() {
        // (0, 1, 2, 0) — last == first, wraparound duplicate at index 3.
        let mesh = IndexedMesh {
            vertices: vec![pt(0, 0, 0), pt(1, 0, 0), pt(0, 1, 0)],
            polygons: vec![IndexedPolygon {
                vertices: vec![0, 1, 2, 0],
                plane: xy_plane(),
                color: 0,
            }],
        };
        let violations = find_post_sliver_violations(&mesh);
        assert_eq!(
            violations,
            vec![PostSliverViolation::AdjacentDuplicate {
                poly_idx: 0,
                index: 3,
                vertex_id: 0,
            }]
        );
    }
}
