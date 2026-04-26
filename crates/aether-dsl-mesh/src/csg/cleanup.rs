//! Post-CSG mesh cleanup pipeline (ADR-0055, refactored under ADR-0057).
//!
//! Runs on the polygon stream produced by `ops::{union, intersection,
//! difference}` before triangulation back to the wire `Vec<Triangle>`
//! format. The pipeline operates in the same fixed-point integer domain
//! as the BSP CSG core; passes are pure and exact.
//!
//! Four passes (composed in order):
//!
//! 1. **Vertex welding** — converts owned-vertex polygons into an
//!    indexed-mesh representation, deduplicating vertices by exact
//!    integer equality. Foundation for the other passes.
//! 2. **Coplanar polygon merging** — groups polygons by exact `Plane3`
//!    signature, finds connected components by shared edges, extracts
//!    each component's boundary loop(s), and emits one indexed polygon
//!    per loop (no triangulation here per ADR-0057 — the canonical
//!    intermediate is n-gon loops).
//! 3. **T-junction removal** — finds vertices in the welded pool that
//!    lie strictly on an edge of a polygon, and subdivides the edge
//!    so the vertex becomes part of the polygon's vertex list. Loops
//!    to fixed point. Operates on n-gons.
//! 4. **CDT triangulation for the wire** — groups loops by plane,
//!    runs constrained Delaunay triangulation per group (ADR-0056),
//!    and emits triangle polygons. Multi-loop groups (faces with
//!    holes) are triangulated as a single CDT call so the hole is
//!    cut out cleanly.
//!
//! Pass 4 is what makes [`run`] return the same `Vec<Polygon>` shape
//! `csg::ops` expects today. ADR-0057 follow-on PRs will add a
//! polygon-domain entry point that stops at pass 3 and returns n-gons.

mod cdt;
mod merge;
mod mesh;
mod tjunctions;
mod weld;

use crate::csg::plane::Plane3;
use crate::csg::point::Point3;
use crate::csg::polygon::Polygon;

/// Run the cleanup pipeline on a polygon list, returning triangulated
/// polygons (3 vertices each) for the wire `Vec<Triangle>` path.
pub fn run(polygons: Vec<Polygon>) -> Vec<Polygon> {
    mesh::IndexedMesh::weld(polygons)
        .merge_coplanar()
        .repair_tjunctions()
        .cdt_triangulate()
}

/// Run the cleanup pipeline on a polygon list, stopping after pass 3 so
/// the output stays in n-gon-loop form (one polygon per boundary loop).
/// This is the entry point for the polygon-domain public API per
/// ADR-0057 — `mesh_polygons` calls this and groups loops into outer +
/// holes by signed area.
///
/// Annular faces (e.g. the top of a cube with a hole bored through it)
/// emit two polygons sharing a `Plane3` and color: the CCW outer loop
/// plus the CW hole loop. Callers responsible for grouping by plane
/// when they want the polygon-with-holes shape.
pub fn run_to_loops(polygons: Vec<Polygon>) -> Vec<Polygon> {
    mesh::IndexedMesh::weld(polygons)
        .merge_coplanar()
        .repair_tjunctions()
        .into_polygons()
}

/// Display-time tessellation for the polygon-domain public API
/// (ADR-0057). Takes a polygon-with-holes in f32 coords, runs the
/// internal CDT against the integer fixed-point pool, and returns
/// triangles in f32 for GPU upload.
///
/// `outer` is the CCW outer boundary; `holes` are CW inner boundaries.
/// Returns `None` if the inputs collapse to fewer than 3 unique
/// vertices, fall outside the integer fixed-point coordinate budget
/// (ADR-0054 ±256 unit cap), or CDT fails to enforce a constraint.
///
/// Callers should fall back to fan triangulation on `None` so geometry
/// isn't dropped silently.
pub fn tessellate_polygon_f32(
    outer: &[[f32; 3]],
    holes: &[Vec<[f32; 3]>],
) -> Option<Vec<[[f32; 3]; 3]>> {
    if outer.len() < 3 {
        return None;
    }

    // Convert to integer fixed-point and build a flat vertex pool.
    let mut vertices: Vec<Point3> =
        Vec::with_capacity(outer.len() + holes.iter().map(|h| h.len()).sum::<usize>());
    let mut outer_indices: Vec<usize> = Vec::with_capacity(outer.len());
    for v in outer {
        let p = Point3::from_f32(*v).ok()?;
        outer_indices.push(vertices.len());
        vertices.push(p);
    }
    let mut hole_index_loops: Vec<Vec<usize>> = Vec::with_capacity(holes.len());
    for hole in holes {
        let mut indices = Vec::with_capacity(hole.len());
        for v in hole {
            let p = Point3::from_f32(*v).ok()?;
            indices.push(vertices.len());
            vertices.push(p);
        }
        hole_index_loops.push(indices);
    }

    // Compute the plane from the outer loop's first three integer
    // vertices. The CDT uses this for axis selection only; the CCW
    // outer assumption gives a normal pointing "outward" by construction.
    if outer_indices.len() < 3 {
        return None;
    }
    let plane = Plane3::from_points(
        vertices[outer_indices[0]],
        vertices[outer_indices[1]],
        vertices[outer_indices[2]],
    );
    if plane.is_degenerate() {
        return None;
    }

    let mut all_loops: Vec<Vec<usize>> = Vec::with_capacity(1 + holes.len());
    all_loops.push(outer_indices);
    all_loops.extend(hole_index_loops);

    let triangles = cdt::triangulate_loops(&vertices, &all_loops, &plane)?;
    Some(
        triangles
            .into_iter()
            .map(|tri| {
                [
                    vertices[tri[0]].to_f32(),
                    vertices[tri[1]].to_f32(),
                    vertices[tri[2]].to_f32(),
                ]
            })
            .collect(),
    )
}
