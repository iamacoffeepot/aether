//! Indexed-mesh intermediate representation used by the cleanup pipeline.
//!
//! Polygons own indices into a shared vertex pool rather than vertex
//! coordinates. Two adjacent polygons that share a corner share the
//! same `VertexId` — coplanar merging (Pass 2) detects shared edges by
//! `VertexId` equality, T-junction repair (Pass 3) detects collinear
//! interior vertices by walking the pool. Both rely on canonical vertex
//! identity that the welding pass establishes.
//!
//! Module-private; callers see only the `Vec<Polygon>` round-trip
//! through `cleanup::run`.

use crate::csg::plane::Plane3;
use crate::csg::point::Point3;
use crate::csg::polygon::Polygon;

pub(super) type VertexId = usize;

pub(super) struct IndexedMesh {
    pub(super) vertices: Vec<Point3>,
    pub(super) polygons: Vec<IndexedPolygon>,
}

pub(super) struct IndexedPolygon {
    pub(super) vertices: Vec<VertexId>,
    pub(super) plane: Plane3,
    pub(super) color: u32,
}

impl IndexedMesh {
    /// Convert back to the owned-vertex polygon form for the wire output.
    /// Polygon order, vertex order within each polygon, plane, and color
    /// are preserved exactly.
    pub(super) fn into_polygons(self) -> Vec<Polygon> {
        let IndexedMesh { vertices, polygons } = self;
        let mut out = Vec::with_capacity(polygons.len());
        for poly in polygons {
            let verts = poly.vertices.iter().map(|&i| vertices[i]).collect();
            out.push(Polygon {
                vertices: verts,
                plane: poly.plane,
                color: poly.color,
            });
        }
        out
    }
}
