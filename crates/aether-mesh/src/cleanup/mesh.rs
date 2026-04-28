//! Indexed-mesh intermediate representation used by the cleanup pipeline.
//!
//! Polygons own indices into a shared vertex pool rather than vertex
//! coordinates. Two adjacent polygons that share a corner share the
//! same `VertexId` — coplanar merging (Pass 2) detects shared edges by
//! `VertexId` equality, T-junction repair (Pass 3) detects collinear
//! interior vertices by walking the pool. Both rely on canonical vertex
//! identity that the welding pass establishes.
//!
//! Visible to the rest of `csg` (not just `cleanup`) so
//! [`super::super::tessellate`] can consume the indexed mesh as input
//! to triangulation without round-tripping through `Vec<Polygon>`.

use crate::loop_polygon::Polygon;
use crate::plane::Plane3;
use crate::point::Point3;

pub(crate) type VertexId = usize;

pub(crate) struct IndexedMesh {
    pub(crate) vertices: Vec<Point3>,
    pub(crate) polygons: Vec<IndexedPolygon>,
}

#[derive(Clone)]
pub(crate) struct IndexedPolygon {
    pub(crate) vertices: Vec<VertexId>,
    pub(crate) plane: Plane3,
    pub(crate) color: u32,
}

impl IndexedMesh {
    /// Convert back to the owned-vertex polygon form (n-gon). The entry
    /// point for the polygon-domain public API per ADR-0057 — used by
    /// `cleanup::run_to_loops` and by pipeline tests inspecting the
    /// post-merge / post-tjunction state.
    pub(crate) fn into_polygons(self) -> Vec<Polygon> {
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
