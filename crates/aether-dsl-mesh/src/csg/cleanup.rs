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
//!    re-triangulates each single-loop component via 2D ear clipping.
//!    Multi-loop components (faces with holes) currently pass through
//!    unmerged — hole bridging is a follow-up.
//! 3. **T-junction removal** — *not yet implemented.*
//!
//! [`run`] is the single entry point — `csg::ops` calls it on every
//! boolean operation's result so callers see cleaned polygons
//! unconditionally.

mod merge;
mod mesh;
mod weld;

use crate::csg::polygon::Polygon;

/// Run the cleanup pipeline on a polygon list.
///
/// Currently runs Pass 1 (vertex welding) and Pass 2 (coplanar merging);
/// T-junction repair lands under the same entry point in a follow-up PR.
pub fn run(polygons: Vec<Polygon>) -> Vec<Polygon> {
    mesh::IndexedMesh::weld(polygons)
        .merge_coplanar()
        .into_polygons()
}
