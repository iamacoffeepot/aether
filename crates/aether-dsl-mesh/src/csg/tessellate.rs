//! Polygon → triangle conversion for the wire `Vec<Triangle>` path.
//!
//! Splits cleanly off the `cleanup` module: cleanup's job is to *fix*
//! the polygon stream (welding, coplanar merging, T-junction repair,
//! sliver removal) so that downstream consumers see a well-formed
//! polygon mesh. Tessellation's job is then to *render* that polygon
//! mesh as triangles for the GPU. The two were grouped together
//! historically because both ran on the post-CSG polygon stream, but
//! they answer different questions and have different consumers — the
//! polygon-domain public API ([`super::cleanup::run_to_loops`] +
//! [`crate::polygon::mesh_polygons`]) skips tessellation entirely
//! because n-gon polygons are the canonical mesh form per ADR-0057.
//!
//! Two entry points:
//!
//! - [`run`]: full pipeline for the legacy triangle-domain ops in
//!   [`crate::csg::ops`] — runs cleanup and then triangulates.
//! - [`tessellate_polygon_integer`]: display-time triangulation of a
//!   single polygon-with-holes for the GPU upload step the polygon-
//!   domain public API uses. Operates entirely in fixed-point
//!   integers — the f32 conversion happens at the GPU upload site,
//!   not here.
//!
//! Triangulation algorithm: constrained Delaunay (ADR-0056) per
//! (plane, color) group, with even-odd inside marking. CDT failure
//! falls back to fan triangulation per polygon so geometry isn't
//! dropped silently.

mod cdt;

use super::cleanup;
use super::cleanup::mesh::{IndexedMesh, VertexId};
use crate::csg::plane::Plane3;
use crate::csg::point::Point3;
use crate::csg::polygon::Polygon;
use std::collections::HashMap;

/// Plane + color — the CDT groups by this so each group produces one
/// color-consistent batch of triangles. Two disjoint same-plane
/// polygons with different colors must triangulate independently or
/// the second's color gets steamrolled by the first.
type PlaneColorKey = (i64, i64, i64, i128, u32);

fn plane_color_key(p: &Plane3, color: u32) -> PlaneColorKey {
    (p.n_x, p.n_y, p.n_z, p.d, color)
}

/// Run cleanup + CDT triangulation. Returns one `Polygon` per
/// triangle (3 vertices each) in `Vec<Polygon>` form so the caller can
/// fan-flatten to `Vec<Triangle>` via [`super::polygons_to_triangles`].
pub fn run(polygons: Vec<Polygon>) -> Vec<Polygon> {
    let cleaned = cleanup::run_to_indexed(polygons);
    triangulate_indexed(cleaned)
}

/// Display-time tessellation for the polygon-domain public API
/// (ADR-0057). Takes a polygon-with-holes in fixed-point integer
/// coordinates and returns integer triangles — same coordinate type
/// the BSP CSG core and cleanup pipeline use, so no f32 round-trip
/// happens inside the mesh pipeline. The f32 conversion happens at
/// the GPU upload site (`aether-mesh-editor-component`).
///
/// `outer` is the CCW outer boundary; `holes` are CW inner boundaries.
/// Returns `None` if the inputs collapse to fewer than 3 unique
/// vertices or CDT fails to enforce a constraint.
///
/// Callers should fall back to fan triangulation on `None` so geometry
/// isn't dropped silently.
pub fn tessellate_polygon_integer(
    outer: &[Point3],
    holes: &[Vec<Point3>],
) -> Option<Vec<[Point3; 3]>> {
    if outer.len() < 3 {
        return None;
    }

    // Build a flat vertex pool — CDT's `triangulate_loops` takes
    // (pool, loops as VertexId sequences, plane).
    let total = outer.len() + holes.iter().map(|h| h.len()).sum::<usize>();
    let mut vertices: Vec<Point3> = Vec::with_capacity(total);
    let mut all_loops: Vec<Vec<usize>> = Vec::with_capacity(1 + holes.len());

    let mut outer_indices: Vec<usize> = Vec::with_capacity(outer.len());
    for &p in outer {
        outer_indices.push(vertices.len());
        vertices.push(p);
    }
    all_loops.push(outer_indices);

    for hole in holes {
        let mut indices = Vec::with_capacity(hole.len());
        for &p in hole {
            indices.push(vertices.len());
            vertices.push(p);
        }
        all_loops.push(indices);
    }

    // Compute the plane from the outer loop's first three vertices.
    // CDT uses it for axis selection only; the CCW outer assumption
    // gives a normal pointing "outward" by construction.
    let plane = Plane3::from_points(
        vertices[all_loops[0][0]],
        vertices[all_loops[0][1]],
        vertices[all_loops[0][2]],
    );
    if plane.is_degenerate() {
        return None;
    }

    let triangles = cdt::triangulate_loops(&vertices, &all_loops, &plane)?;
    Some(
        triangles
            .into_iter()
            .map(|tri| [vertices[tri[0]], vertices[tri[1]], vertices[tri[2]]])
            .collect(),
    )
}

/// Triangulate the n-gon loops in an `IndexedMesh` per (plane, color)
/// group. Multi-loop groups (faces with holes from CSG cuts) feed a
/// single CDT call so the hole appears as a constraint loop with
/// even-odd inside marking selecting the right triangles.
fn triangulate_indexed(mesh: IndexedMesh) -> Vec<Polygon> {
    let IndexedMesh { vertices, polygons } = mesh;

    let mut groups: HashMap<PlaneColorKey, Vec<usize>> = HashMap::new();
    for (i, poly) in polygons.iter().enumerate() {
        groups
            .entry(plane_color_key(&poly.plane, poly.color))
            .or_default()
            .push(i);
    }
    let mut sorted_keys: Vec<&PlaneColorKey> = groups.keys().collect();
    sorted_keys.sort();

    let mut out: Vec<Polygon> = Vec::with_capacity(polygons.len());
    for key in sorted_keys {
        let group = &groups[key];
        let plane = polygons[group[0]].plane;
        let color = polygons[group[0]].color;
        let loops: Vec<Vec<VertexId>> = group
            .iter()
            .map(|&pid| polygons[pid].vertices.clone())
            .collect();

        match cdt::triangulate_loops(&vertices, &loops, &plane) {
            Some(triangles) => {
                for tri in triangles {
                    out.push(Polygon {
                        vertices: vec![vertices[tri[0]], vertices[tri[1]], vertices[tri[2]]],
                        plane,
                        color,
                    });
                }
            }
            None => {
                // CDT couldn't enforce a constraint or hit a degenerate
                // configuration. Fall back to fan-triangulating each
                // polygon in the group so geometry isn't dropped.
                for &pid in group {
                    let poly = &polygons[pid];
                    if poly.vertices.len() < 3 {
                        continue;
                    }
                    let v0 = vertices[poly.vertices[0]];
                    for i in 1..poly.vertices.len() - 1 {
                        out.push(Polygon {
                            vertices: vec![
                                v0,
                                vertices[poly.vertices[i]],
                                vertices[poly.vertices[i + 1]],
                            ],
                            plane,
                            color,
                        });
                    }
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::cleanup::mesh::IndexedPolygon;
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

    #[test]
    fn triangulate_indexed_quad_emits_two_triangles() {
        let mesh = IndexedMesh {
            vertices: vec![
                pt(0.0, 0.0, 0.0),
                pt(1.0, 0.0, 0.0),
                pt(1.0, 1.0, 0.0),
                pt(0.0, 1.0, 0.0),
            ],
            polygons: vec![IndexedPolygon {
                vertices: vec![0, 1, 2, 3],
                plane: xy_plane(),
                color: 7,
            }],
        };
        let triangles = triangulate_indexed(mesh);
        assert_eq!(triangles.len(), 2);
        for tri in &triangles {
            assert_eq!(tri.vertices.len(), 3);
            assert_eq!(tri.color, 7);
        }
    }

    #[test]
    fn triangulate_indexed_groups_polygons_by_plane_key() {
        // Two polygons on the same plane (single CDT call) and one on
        // a different plane (separate CDT call). Three quads → six
        // triangles total.
        let yz_plane = Plane3 {
            n_x: 1,
            n_y: 0,
            n_z: 0,
            d: 0,
        };
        let mesh = IndexedMesh {
            vertices: vec![
                pt(0.0, 0.0, 0.0),
                pt(1.0, 0.0, 0.0),
                pt(1.0, 1.0, 0.0),
                pt(0.0, 1.0, 0.0),
                pt(2.0, 0.0, 0.0),
                pt(3.0, 0.0, 0.0),
                pt(3.0, 1.0, 0.0),
                pt(2.0, 1.0, 0.0),
                pt(0.0, 0.0, 0.0),
                pt(0.0, 1.0, 0.0),
                pt(0.0, 1.0, 1.0),
                pt(0.0, 0.0, 1.0),
            ],
            polygons: vec![
                IndexedPolygon {
                    vertices: vec![0, 1, 2, 3],
                    plane: xy_plane(),
                    color: 0,
                },
                IndexedPolygon {
                    vertices: vec![4, 5, 6, 7],
                    plane: xy_plane(),
                    color: 0,
                },
                IndexedPolygon {
                    vertices: vec![8, 9, 10, 11],
                    plane: yz_plane,
                    color: 0,
                },
            ],
        };
        let triangles = triangulate_indexed(mesh);
        assert_eq!(triangles.len(), 6, "3 quads should triangulate to 6 tris");
    }

    #[test]
    fn tessellate_polygon_integer_rejects_fewer_than_3_outer_vertices() {
        let outer = vec![pt(0.0, 0.0, 0.0), pt(1.0, 0.0, 0.0)];
        let result = tessellate_polygon_integer(&outer, &[]);
        assert!(result.is_none());
    }

    #[test]
    fn tessellate_polygon_integer_rejects_degenerate_collinear_outer() {
        // Three collinear points → degenerate plane → None.
        let outer = vec![pt(0.0, 0.0, 0.0), pt(1.0, 0.0, 0.0), pt(2.0, 0.0, 0.0)];
        let result = tessellate_polygon_integer(&outer, &[]);
        assert!(result.is_none());
    }

    #[test]
    fn tessellate_polygon_integer_happy_path_square_with_hole() {
        let outer = vec![
            pt(0.0, 0.0, 0.0),
            pt(4.0, 0.0, 0.0),
            pt(4.0, 4.0, 0.0),
            pt(0.0, 4.0, 0.0),
        ];
        let hole = vec![
            pt(1.0, 1.0, 0.0),
            pt(1.0, 3.0, 0.0),
            pt(3.0, 3.0, 0.0),
            pt(3.0, 1.0, 0.0),
        ];
        let tris = tessellate_polygon_integer(&outer, &[hole]).expect("annular should triangulate");
        // Topological minimum for 8-vertex annular = 8 triangles.
        assert_eq!(tris.len(), 8);
        // Total area = outer (16) - hole (4) = 12 → doubled = 24. Use
        // shoelace on each triangle (signed integer in fixed-point
        // units) and sum, then convert.
        let signed_double_area_fixed: i128 = tris
            .iter()
            .map(|tri| {
                let ax = (tri[1].x - tri[0].x) as i128;
                let ay = (tri[1].y - tri[0].y) as i128;
                let bx = (tri[2].x - tri[0].x) as i128;
                let by = (tri[2].y - tri[0].y) as i128;
                ax * by - ay * bx
            })
            .sum();
        // SCALE² because we summed products of fixed-point differences.
        let scale_sq = (1u128 << 32) as f64;
        let signed_double_area = signed_double_area_fixed as f64 / scale_sq;
        assert!(
            (signed_double_area - 24.0).abs() < 0.01,
            "annular doubled area mismatch: {signed_double_area}"
        );
    }
}
