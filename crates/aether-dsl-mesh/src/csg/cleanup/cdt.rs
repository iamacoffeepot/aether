//! Constrained Delaunay triangulation for the cleanup pipeline (ADR-0056).
//!
//! Replaces the ear-clipping + hole-bridging step that ADR-0055 ships;
//! produces sliver-free, locally-Delaunay triangulations of polygons-
//! with-holes by enforcing boundary edges as constraints rather than
//! splicing holes into the outer loop with a slit.
//!
//! Module layout (built up across the implementation cascade):
//!
//! - [`predicates`]: exact integer in-circle and orient2d tests in
//!   i128. Foundation for everything else.
//! - `bowyer_watson`: incremental Delaunay triangulation (PR 2, not
//!   yet shipped).
//! - `triangulate`: constraint enforcement + inside/outside marking +
//!   the public entry point that `merge::process_component` will call
//!   (PR 3, not yet shipped).

pub(super) mod predicates;
// Foundation pass: bowyer_watson is wired in by PR 3. Suppress dead-code
// noise until then.
#[allow(dead_code)]
pub(super) mod bowyer_watson;
