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
//!    integer equality. Foundation for the other two passes; on its
//!    own it is semantically a no-op for the wire output.
//! 2. **Coplanar polygon merging** — *not yet implemented.*
//! 3. **T-junction removal** — *not yet implemented.*
//!
//! [`run`] is the single entry point — `csg::ops` calls it on every
//! boolean operation's result so callers see cleaned polygons
//! unconditionally.

mod mesh;
mod weld;

use crate::csg::polygon::Polygon;

/// Run the cleanup pipeline on a polygon list.
///
/// Currently only Pass 1 (vertex welding) is wired up; subsequent PRs
/// extend this entry point with coplanar merging and T-junction repair.
/// The wire-format output is unchanged in this PR — welding is an
/// internal-representation pass.
pub fn run(polygons: Vec<Polygon>) -> Vec<Polygon> {
    mesh::IndexedMesh::weld(polygons).into_polygons()
}
