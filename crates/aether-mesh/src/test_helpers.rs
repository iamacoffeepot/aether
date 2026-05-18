//! Shared fixtures for `aether-mesh` unit tests.
//!
//! `pt(x, y, z)` constructs a `Point3` from `f32` literals, encoding each
//! coordinate through `f32_to_fixed` and unwrapping. Inline-construction
//! convenience for tests; not part of the public API.

use crate::cleanup::mesh::{IndexedMesh, IndexedPolygon, VertexId};
use crate::fixed::f32_to_fixed;
use crate::loop_polygon::Polygon;
use crate::plane::Plane3;
use crate::point::Point3;

pub fn pt(x: f32, y: f32, z: f32) -> Point3 {
    Point3 {
        x: f32_to_fixed(x).expect("test setup: x in fixed-point range"),
        y: f32_to_fixed(y).expect("test setup: y in fixed-point range"),
        z: f32_to_fixed(z).expect("test setup: z in fixed-point range"),
    }
}

/// Build a list of triangles by `Polygon::from_triangle`-ing each
/// `(a, b, c)` triple. Hoisted from the cleanup tests because every
/// shattered-quad / L-shape / fan scenario repeated the same 4-line
/// vec of `from_triangle(..).expect(..)` calls.
pub fn triangle_fan(triangles: &[(Point3, Point3, Point3)], color: u32) -> Vec<Polygon> {
    triangles
        .iter()
        .map(|&(a, b, c)| {
            Polygon::from_triangle(a, b, c, color).expect("test setup: non-degenerate triangle")
        })
        .collect()
}

/// Build an `IndexedMesh` whose every polygon carries the same
/// `plane` and `color`. Hoisted out of the cleanup fixtures that
/// otherwise share a long `.map(|verts| IndexedPolygon { vertices,
/// plane, color }).collect()` chain after their vertex / index
/// literals.
pub fn indexed_mesh_on(
    plane: Plane3,
    color: u32,
    vertices: Vec<Point3>,
    polygons: impl IntoIterator<Item = Vec<VertexId>>,
) -> IndexedMesh {
    IndexedMesh {
        vertices,
        polygons: polygons
            .into_iter()
            .map(|verts| IndexedPolygon {
                vertices: verts,
                plane,
                color,
            })
            .collect(),
    }
}
