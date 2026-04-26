//! Pass 1: vertex welding — owned-vertex polygons → indexed mesh.
//!
//! Welds vertices by Chebyshev-distance ≤ [`WELD_TOLERANCE_FIXED_UNITS`]
//! (the single-pass snap drift bound from `compute_intersection`).
//! Polygons that collapse to fewer than three distinct vertices after
//! welding are dropped.
//!
//! ### Why tolerance and not exact equality
//!
//! BSP-side [`crate::csg::vertex_pool::SharedVertexPool`] already
//! catches snap-drift duplicates that share a *line key* (same
//! partitioner × polygon-plane pair). But duplicates can also arise
//! across different line keys — the most common case is two adjacent
//! cylinder/sphere facets sharing an edge, where the BSP splits each
//! facet against the same partitioner; the shared edge produces the
//! same true intersection point twice but the pool keys differ
//! (different facet planes). Welding with the snap-drift tolerance
//! catches these as a final dedup before downstream cleanup runs.
//!
//! ### Tolerance derivation
//!
//! `compute_intersection` rounds each output axis by up to 0.5 fixed
//! units. Two same-true-point intersections, each independently
//! snapped, can land up to 0.5 + 0.5 = 1 fixed unit apart in
//! Chebyshev distance. Tolerance = 1 catches every legitimate
//! same-vertex case — and the next-nearest distinct point in
//! practical CSG inputs (sphere/cylinder facet vertex spacing on a
//! cube face) is far above this (typically 0.05+ float = 3000+ fixed
//! units), so there are no false positives.
//!
//! ### Determinism
//!
//! Pool order is input traversal order. The spatial bucket lookup
//! visits 27 neighboring cells in (z,y,x) order so the "first match
//! wins" rule is reproducible across runs.

use super::mesh::{IndexedMesh, IndexedPolygon, VertexId};
use crate::csg::point::Point3;
use crate::csg::polygon::Polygon;
use std::collections::HashMap;

/// Snap-drift bound for near-duplicate vertex welding. Single-pass
/// drift is 0.5 fixed units per axis from `compute_intersection`'s
/// rounding step. Two same-true-point intersections each independently
/// snapped can land up to 1 unit apart in Chebyshev distance.
///
/// We use `2` (not `1`) to absorb the accumulated drift across BSP
/// passes for compositions with deeply overlapping cuts (e.g.,
/// `three_cut_box_is_watertight`). The per-line
/// [`crate::csg::vertex_pool::SharedVertexPool`] catches all
/// single-pass snap-drift duplicates that share a line key; the
/// remaining ≤2-unit gaps come from cross-pass drift between
/// non-line-key-sharing computations and need to be welded here.
///
/// The next-nearest distinct-point spacing in practical CSG inputs
/// (sphere/cylinder facet vertex spacing on a cube face, typically
/// 0.05+ float = 3000+ fixed units) is far above 2, so there are no
/// false positives.
const WELD_TOLERANCE_FIXED_UNITS: i32 = 2;

/// Spatial-bucket cell size: 2 × tolerance so any two points within
/// tolerance land in the same or adjacent buckets.
const BUCKET_SIZE: i32 = WELD_TOLERANCE_FIXED_UNITS * 2;

type BucketKey = (i32, i32, i32);

fn bucket_of(p: Point3) -> BucketKey {
    (
        p.x.div_euclid(BUCKET_SIZE),
        p.y.div_euclid(BUCKET_SIZE),
        p.z.div_euclid(BUCKET_SIZE),
    )
}

impl IndexedMesh {
    pub(super) fn weld(polygons: Vec<Polygon>) -> Self {
        let mut vertex_pool: Vec<Point3> = Vec::new();
        let mut buckets: HashMap<BucketKey, Vec<VertexId>> = HashMap::new();
        let mut indexed = Vec::with_capacity(polygons.len());

        for poly in polygons {
            let mut ids: Vec<VertexId> = Vec::with_capacity(poly.vertices.len());
            for v in &poly.vertices {
                let id = lookup_or_insert(&mut vertex_pool, &mut buckets, *v);
                if ids.last() != Some(&id) {
                    ids.push(id);
                }
            }
            if ids.len() >= 2 && ids.first() == ids.last() {
                ids.pop();
            }
            if ids.len() < 3 {
                continue;
            }
            indexed.push(IndexedPolygon {
                vertices: ids,
                plane: poly.plane,
                color: poly.color,
            });
        }

        IndexedMesh {
            vertices: vertex_pool,
            polygons: indexed,
        }
    }
}

/// Look up `v` against pool entries within tolerance (Chebyshev), via
/// the 27 neighboring spatial buckets. Returns the existing entry's
/// VertexId if any is within tolerance; otherwise inserts as new.
fn lookup_or_insert(
    pool: &mut Vec<Point3>,
    buckets: &mut HashMap<BucketKey, Vec<VertexId>>,
    v: Point3,
) -> VertexId {
    let (bx, by, bz) = bucket_of(v);
    for dz in -1..=1 {
        for dy in -1..=1 {
            for dx in -1..=1 {
                let key = (bx + dx, by + dy, bz + dz);
                if let Some(ids) = buckets.get(&key) {
                    for &id in ids {
                        let p = pool[id];
                        if (p.x - v.x).abs() <= WELD_TOLERANCE_FIXED_UNITS
                            && (p.y - v.y).abs() <= WELD_TOLERANCE_FIXED_UNITS
                            && (p.z - v.z).abs() <= WELD_TOLERANCE_FIXED_UNITS
                        {
                            return id;
                        }
                    }
                }
            }
        }
    }
    let new_id = pool.len();
    pool.push(v);
    buckets.entry((bx, by, bz)).or_default().push(new_id);
    new_id
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::csg::fixed::f32_to_fixed;
    use crate::csg::plane::Plane3;

    fn pt(x: f32, y: f32, z: f32) -> Point3 {
        Point3 {
            x: f32_to_fixed(x).unwrap(),
            y: f32_to_fixed(y).unwrap(),
            z: f32_to_fixed(z).unwrap(),
        }
    }

    #[test]
    fn empty_input_produces_empty_mesh() {
        let mesh = IndexedMesh::weld(vec![]);
        assert!(mesh.vertices.is_empty());
        assert!(mesh.polygons.is_empty());
    }

    #[test]
    fn single_triangle_keeps_three_distinct_vertices() {
        let tri =
            Polygon::from_triangle(pt(0.0, 0.0, 0.0), pt(1.0, 0.0, 0.0), pt(0.0, 1.0, 0.0), 5)
                .unwrap();
        let mesh = IndexedMesh::weld(vec![tri]);
        assert_eq!(mesh.vertices.len(), 3);
        assert_eq!(mesh.polygons.len(), 1);
        assert_eq!(mesh.polygons[0].vertices.len(), 3);
        assert_eq!(mesh.polygons[0].color, 5);
    }

    #[test]
    fn two_triangles_sharing_an_edge_share_two_vertices() {
        // Quad split into two triangles along the (0,0)-(1,1) diagonal.
        let t1 = Polygon::from_triangle(pt(0.0, 0.0, 0.0), pt(1.0, 0.0, 0.0), pt(1.0, 1.0, 0.0), 0)
            .unwrap();
        let t2 = Polygon::from_triangle(pt(0.0, 0.0, 0.0), pt(1.0, 1.0, 0.0), pt(0.0, 1.0, 0.0), 0)
            .unwrap();
        let mesh = IndexedMesh::weld(vec![t1, t2]);
        // 4 distinct corners, not 6.
        assert_eq!(mesh.vertices.len(), 4);
        assert_eq!(mesh.polygons.len(), 2);
    }

    #[test]
    fn polygon_collapsing_to_fewer_than_three_distinct_vertices_is_dropped() {
        // Degenerate: same point twice + one other → a collapsing sliver.
        // We can't go through `Polygon::from_triangle` (it rejects degenerate
        // planes), so build the polygon directly with a bogus plane — weld
        // doesn't inspect the plane, only the vertex list.
        let bogus_plane = Plane3 {
            n_x: 0,
            n_y: 0,
            n_z: 1,
            d: 0,
        };
        let degenerate = Polygon {
            vertices: vec![pt(0.0, 0.0, 0.0), pt(0.0, 0.0, 0.0), pt(1.0, 0.0, 0.0)],
            plane: bogus_plane,
            color: 0,
        };
        let mesh = IndexedMesh::weld(vec![degenerate]);
        assert!(mesh.polygons.is_empty());
    }

    #[test]
    fn explicit_closed_loop_is_unwound() {
        // Polygon expressed with a wraparound duplicate (last == first).
        let bogus_plane = Plane3 {
            n_x: 0,
            n_y: 0,
            n_z: 1,
            d: 0,
        };
        let closed = Polygon {
            vertices: vec![
                pt(0.0, 0.0, 0.0),
                pt(1.0, 0.0, 0.0),
                pt(0.0, 1.0, 0.0),
                pt(0.0, 0.0, 0.0),
            ],
            plane: bogus_plane,
            color: 0,
        };
        let mesh = IndexedMesh::weld(vec![closed]);
        assert_eq!(mesh.polygons.len(), 1);
        assert_eq!(mesh.polygons[0].vertices.len(), 3);
    }

    #[test]
    fn round_trip_preserves_vertex_coords_planes_and_colors() {
        let tri_a =
            Polygon::from_triangle(pt(0.0, 0.0, 0.0), pt(1.0, 0.0, 0.0), pt(0.0, 1.0, 0.0), 7)
                .unwrap();
        let tri_b =
            Polygon::from_triangle(pt(0.0, 0.0, 0.0), pt(0.0, 1.0, 0.0), pt(0.0, 0.0, 1.0), 9)
                .unwrap();
        let original = vec![tri_a.clone(), tri_b.clone()];
        let round_tripped = IndexedMesh::weld(original.clone()).into_polygons();

        assert_eq!(round_tripped.len(), original.len());
        for (a, b) in original.iter().zip(round_tripped.iter()) {
            assert_eq!(a.vertices, b.vertices);
            assert_eq!(a.color, b.color);
            assert_eq!(a.plane.n_x, b.plane.n_x);
            assert_eq!(a.plane.n_y, b.plane.n_y);
            assert_eq!(a.plane.n_z, b.plane.n_z);
            assert_eq!(a.plane.d, b.plane.d);
        }
    }

    #[test]
    fn welding_is_deterministic_across_runs() {
        let tri_a =
            Polygon::from_triangle(pt(0.0, 0.0, 0.0), pt(1.0, 0.0, 0.0), pt(1.0, 1.0, 0.0), 0)
                .unwrap();
        let tri_b =
            Polygon::from_triangle(pt(0.0, 0.0, 0.0), pt(1.0, 1.0, 0.0), pt(0.0, 1.0, 0.0), 0)
                .unwrap();
        let m1 = IndexedMesh::weld(vec![tri_a.clone(), tri_b.clone()]);
        let m2 = IndexedMesh::weld(vec![tri_a, tri_b]);
        assert_eq!(m1.vertices, m2.vertices);
        assert_eq!(m1.polygons.len(), m2.polygons.len());
        for (p, q) in m1.polygons.iter().zip(m2.polygons.iter()) {
            assert_eq!(p.vertices, q.vertices);
        }
    }

    /// **Welding invariant**: two polygons that share a vertex by
    /// coordinate equality must reference it via the same `VertexId`
    /// in the indexed output. This is what `merge_coplanar`,
    /// `repair_tjunctions`, and the manifold validator all assume —
    /// without it, downstream edge-walking would treat geometrically-
    /// shared edges as distinct and report phantom boundary edges.
    #[test]
    fn shared_vertex_has_identical_id_across_polygons() {
        let shared0 = pt(1.0, 0.0, 0.0);
        let shared1 = pt(1.0, 1.0, 0.0);
        let t1 = Polygon::from_triangle(pt(0.0, 0.0, 0.0), shared0, shared1, 0).unwrap();
        let t2 = Polygon::from_triangle(pt(0.0, 0.0, 0.0), shared1, pt(0.0, 1.0, 0.0), 0).unwrap();
        let mesh = IndexedMesh::weld(vec![t1, t2]);

        // Find each shared coordinate's VertexId in each polygon and
        // assert they match.
        let id_for = |poly_idx: usize, coord: Point3| -> super::VertexId {
            let poly = &mesh.polygons[poly_idx];
            for &id in &poly.vertices {
                if mesh.vertices[id] == coord {
                    return id;
                }
            }
            panic!("polygon {poly_idx} missing shared vertex {coord:?}");
        };
        assert_eq!(
            id_for(0, shared1),
            id_for(1, shared1),
            "shared vertex must have the same VertexId in both polygons \
             — manifold validator depends on this"
        );
        assert_eq!(id_for(0, pt(0.0, 0.0, 0.0)), id_for(1, pt(0.0, 0.0, 0.0)));
    }

    #[test]
    fn ngon_welding_preserves_vertex_count() {
        // A quad has 4 distinct vertices and should weld to a 4-id
        // indexed polygon. Catches a refactor that special-cases the
        // 3-vertex (triangle) path.
        let bogus_plane = Plane3 {
            n_x: 0,
            n_y: 0,
            n_z: 1,
            d: 0,
        };
        let quad = Polygon {
            vertices: vec![
                pt(0.0, 0.0, 0.0),
                pt(1.0, 0.0, 0.0),
                pt(1.0, 1.0, 0.0),
                pt(0.0, 1.0, 0.0),
            ],
            plane: bogus_plane,
            color: 0,
        };
        let mesh = IndexedMesh::weld(vec![quad]);
        assert_eq!(mesh.vertices.len(), 4);
        assert_eq!(mesh.polygons[0].vertices.len(), 4);
    }

    #[test]
    fn mid_loop_adjacent_duplicate_is_collapsed() {
        // [A, A, B, C] should weld to [A, B, C]. The existing closed-
        // loop test only covers the wraparound case [A, B, C, A].
        let bogus_plane = Plane3 {
            n_x: 0,
            n_y: 0,
            n_z: 1,
            d: 0,
        };
        let with_mid_dup = Polygon {
            vertices: vec![
                pt(0.0, 0.0, 0.0),
                pt(0.0, 0.0, 0.0), // mid-loop adjacent duplicate
                pt(1.0, 0.0, 0.0),
                pt(0.0, 1.0, 0.0),
            ],
            plane: bogus_plane,
            color: 0,
        };
        let mesh = IndexedMesh::weld(vec![with_mid_dup]);
        assert_eq!(mesh.polygons.len(), 1);
        assert_eq!(mesh.polygons[0].vertices.len(), 3);
    }

    #[test]
    fn all_same_vertex_polygon_is_dropped() {
        // [A, A, A] collapses to [A] which has <3 distinct → dropped.
        let bogus_plane = Plane3 {
            n_x: 0,
            n_y: 0,
            n_z: 1,
            d: 0,
        };
        let collapsed = Polygon {
            vertices: vec![pt(0.0, 0.0, 0.0), pt(0.0, 0.0, 0.0), pt(0.0, 0.0, 0.0)],
            plane: bogus_plane,
            color: 0,
        };
        let mesh = IndexedMesh::weld(vec![collapsed]);
        assert!(mesh.polygons.is_empty());
        // Note: the lone vertex still ends up in the pool — weld doesn't
        // garbage-collect orphan pool entries. That's documented behavior;
        // pin it here so a future "cleanup" doesn't silently change it.
        assert_eq!(mesh.vertices.len(), 1);
    }

    #[test]
    fn vertex_pool_order_matches_first_occurrence() {
        // Module-level claim: the pool is built in input traversal order.
        // Pin it so a future "sort the pool for cache locality" refactor
        // doesn't silently break determinism with downstream consumers.
        let t1 = Polygon::from_triangle(
            pt(2.0, 0.0, 0.0), // first encounter
            pt(0.0, 0.0, 0.0), // second
            pt(0.0, 2.0, 0.0), // third
            0,
        )
        .unwrap();
        let mesh = IndexedMesh::weld(vec![t1]);
        assert_eq!(mesh.vertices[0], pt(2.0, 0.0, 0.0));
        assert_eq!(mesh.vertices[1], pt(0.0, 0.0, 0.0));
        assert_eq!(mesh.vertices[2], pt(0.0, 2.0, 0.0));
    }
}
