//! Post-CSG mesh cleanup pipeline (ADR-0055).
//!
//! Runs on the polygon stream produced by `ops::{union, intersection,
//! difference}` before triangulation back to the wire `Vec<Triangle>`
//! format. The pipeline operates in the same fixed-point integer domain
//! as the BSP CSG core; passes are pure and exact.
//!
//! Three passes (composed in order):
//!
//! 1. **Vertex welding** — converts owned-vertex polygons into an
//!    indexed-mesh representation, deduplicating vertices by exact
//!    integer equality. Foundation for the other two passes.
//! 2. **Coplanar polygon merging** — groups polygons by exact `Plane3`
//!    signature, finds connected components by shared edges, and
//!    re-triangulates each component via 2D ear clipping. Multi-loop
//!    components (faces with holes) are bridged into a single slit
//!    polygon before clipping.
//! 3. **T-junction removal** — finds vertices in the welded pool that
//!    lie strictly on an edge of a polygon, and subdivides the edge
//!    so the vertex becomes part of the polygon's vertex list. Loops
//!    to fixed point.
//!
//! [`run`] is the single entry point — `csg::ops` calls it on every
//! boolean operation's result so callers see cleaned polygons
//! unconditionally.

mod cdt;
mod merge;
mod mesh;
mod tjunctions;
mod weld;

use crate::csg::polygon::Polygon;

/// Run the cleanup pipeline on a polygon list.
pub fn run(polygons: Vec<Polygon>) -> Vec<Polygon> {
    mesh::IndexedMesh::weld(polygons)
        .merge_coplanar()
        .repair_tjunctions()
        .into_polygons()
}
