//! Constrained Delaunay triangulation for the tessellation pass (ADR-0056).
//!
//! Replaces the ear-clipping + hole-bridging step that ADR-0055 ships;
//! produces sliver-free, locally-Delaunay triangulations of polygons-
//! with-holes by enforcing boundary edges as constraints rather than
//! splicing holes into the outer loop with a slit.
//!
//! Module layout:
//!
//! - [`predicates`]: exact integer in-circle and orient2d tests in
//!   i128. Foundation for everything else.
//! - [`bowyer_watson`]: incremental Delaunay triangulation.
//! - [`triangulate`]: constraint enforcement + inside/outside marking
//!   + the public entry point that `triangulate_indexed` calls.

pub(super) mod bowyer_watson;
pub(super) mod predicates;
pub(super) mod triangulate;

pub(super) use triangulate::triangulate as triangulate_loops;
