//! Pass 1: vertex welding — owned-vertex polygons → indexed mesh.
//!
//! Hashes vertices by exact `Point3` integer equality and replaces
//! polygon vertex lists with indices into a shared pool. Polygons that
//! collapse to fewer than three distinct vertices after welding are
//! dropped — they were degenerate slivers from a CSG split that
//! produced near-coincident edges (the snap-to-grid round-trip in
//! `compute_intersection` occasionally produces these).
//!
//! Determinism: the vertex pool is built in input traversal order, so
//! identical input polygon lists produce identical pools and identical
//! indexed-polygon lists across runs / platforms / threads.

use super::mesh::{IndexedMesh, IndexedPolygon, VertexId};
use crate::csg::point::Point3;
use crate::csg::polygon::Polygon;
use std::collections::HashMap;

impl IndexedMesh {
    pub(super) fn weld(polygons: Vec<Polygon>) -> Self {
        let mut vertex_pool: Vec<Point3> = Vec::new();
        let mut vertex_index: HashMap<Point3, VertexId> = HashMap::new();
        let mut indexed = Vec::with_capacity(polygons.len());

        for poly in polygons {
            let mut ids: Vec<VertexId> = Vec::with_capacity(poly.vertices.len());
            for v in &poly.vertices {
                let id = *vertex_index.entry(*v).or_insert_with(|| {
                    let next = vertex_pool.len();
                    vertex_pool.push(*v);
                    next
                });
                if ids.last() != Some(&id) {
                    ids.push(id);
                }
            }
            // Wrap-around duplicate: last vertex equal to first (an explicit
            // closed-polygon form, or a sliver where the loop folds back).
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
}
