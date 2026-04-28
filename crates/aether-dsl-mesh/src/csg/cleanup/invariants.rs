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
use crate::csg::plane::Plane3;

/// One twin-edge violation surfaced by [`find_twin_edges`].
#[derive(Debug, Clone)]
pub(in crate::csg) struct TwinEdgeViolation {
    pub plane: Plane3,
    pub color: u32,
    pub edge: (VertexId, VertexId),
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
}
