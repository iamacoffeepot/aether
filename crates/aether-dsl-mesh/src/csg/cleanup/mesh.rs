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

use super::cdt;
use crate::csg::plane::Plane3;
use crate::csg::point::Point3;
use crate::csg::polygon::Polygon;
use std::collections::HashMap;

pub(super) type VertexId = usize;

pub(super) struct IndexedMesh {
    pub(super) vertices: Vec<Point3>,
    pub(super) polygons: Vec<IndexedPolygon>,
}

#[derive(Clone)]
pub(super) struct IndexedPolygon {
    pub(super) vertices: Vec<VertexId>,
    pub(super) plane: Plane3,
    pub(super) color: u32,
}

type PlaneKey = (i64, i64, i64, i128);

fn plane_key(p: &Plane3) -> PlaneKey {
    (p.n_x, p.n_y, p.n_z, p.d)
}

impl IndexedMesh {
    /// Convert back to the owned-vertex polygon form (n-gon). Used by
    /// pipeline tests to inspect the post-merge / post-tjunction state;
    /// will become the entry point for the polygon-domain public API
    /// in a follow-on PR.
    #[cfg(test)]
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

    /// Pass 4 (per ADR-0057): triangulate the n-gon loops for the wire
    /// `Vec<Triangle>` path. Polygons sharing a `Plane3` are grouped
    /// together so multi-loop faces (outer + holes from CSG-cut
    /// regions) feed into a single CDT call — the hole appears as
    /// constraint loops the CDT routes around, with even-odd inside
    /// marking selecting the correct triangles.
    ///
    /// Output is one `Polygon` per triangle (3 vertices each), color
    /// inherited from the first polygon in each plane group.
    pub(super) fn cdt_triangulate(self) -> Vec<Polygon> {
        let IndexedMesh { vertices, polygons } = self;

        // Group polygon indices by their plane signature.
        let mut groups: HashMap<PlaneKey, Vec<usize>> = HashMap::new();
        for (i, poly) in polygons.iter().enumerate() {
            groups.entry(plane_key(&poly.plane)).or_default().push(i);
        }
        let mut sorted_keys: Vec<&PlaneKey> = groups.keys().collect();
        sorted_keys.sort();

        let mut out: Vec<Polygon> = Vec::with_capacity(polygons.len());
        for key in sorted_keys {
            let group = &groups[key];
            let plane = polygons[group[0]].plane;
            let color = polygons[group[0]].color;
            let loops: Vec<Vec<VertexId>> =
                group.iter().map(|&pid| polygons[pid].vertices.clone()).collect();

            match cdt::triangulate_loops(&vertices, &loops, &plane) {
                Some(triangles) => {
                    for tri in triangles {
                        out.push(Polygon {
                            vertices: vec![
                                vertices[tri[0]],
                                vertices[tri[1]],
                                vertices[tri[2]],
                            ],
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
}
