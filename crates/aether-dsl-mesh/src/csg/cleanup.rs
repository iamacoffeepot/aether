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

use crate::csg::polygon::Polygon;

/// Run the cleanup pipeline on a polygon list, returning triangulated
/// polygons (3 vertices each) for the wire `Vec<Triangle>` path.
pub fn run(polygons: Vec<Polygon>) -> Vec<Polygon> {
    mesh::IndexedMesh::weld(polygons)
        .merge_coplanar()
        .repair_tjunctions()
        .cdt_triangulate()
}
