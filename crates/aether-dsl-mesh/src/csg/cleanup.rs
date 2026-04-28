//! Post-CSG mesh cleanup pipeline (ADR-0055, refactored under ADR-0057).
//!
//! Runs on the polygon stream produced by `ops::{union, intersection,
//! difference}`. The pipeline operates in the same fixed-point integer
//! domain as the BSP CSG core; passes are pure and exact.
//!
//! Four passes (composed in order):
//!
//! 1. **Vertex welding** — converts owned-vertex polygons into an
//!    indexed-mesh representation, deduplicating vertices by exact
//!    integer equality. Foundation for the other passes.
//! 2. **T-junction repair** — finds vertices in the welded pool that
//!    lie strictly on an edge of a polygon, and subdivides the edge
//!    so the vertex becomes part of the polygon's vertex list. Loops
//!    to fixed point. Runs *before* merge so adjacent BSP fragments
//!    that share a collinear-but-subdivided edge end up with matching
//!    half-edges, which is what twin cancellation in pass 3 needs to
//!    pair them.
//! 3. **Coplanar polygon merging** — groups polygons by `(Plane3,
//!    color)`, runs a single directed-edge cancellation across the
//!    whole bucket (twin pairs drop out as interior edges), then
//!    walks the surviving boundary into closed loops via angular
//!    continuation at X-junctions. Emits one indexed polygon per
//!    loop (no triangulation here per ADR-0057 — the canonical
//!    intermediate is n-gon loops).
//! 4. **Sliver removal** — collapses near-coincident vertex pairs that
//!    bound a short edge in some polygon (the symptom of off-axis BSP
//!    drifting beyond the welding tolerance). Edge-triggered, not
//!    coordinate-triggered, so it doesn't risk colliding distinct
//!    features.
//!
//! Triangulation (CDT) is the responsibility of [`super::tessellate`],
//! not cleanup. The polygon-domain public API ([`run_to_loops`] +
//! [`crate::polygon::mesh_polygons`]) skips tessellation entirely
//! because n-gon polygons are the canonical mesh form per ADR-0057;
//! the legacy triangle-domain ops in [`super::ops`] compose cleanup
//! and tessellation explicitly via [`super::tessellate::run`].

mod invariants;
mod merge;
pub(in crate::csg) mod mesh;
pub mod provenance;
mod slivers;
mod tjunctions;
mod weld;

use crate::csg::polygon::Polygon;

/// Run the cleanup pipeline and return the final indexed mesh, ready
/// for either polygon-domain consumption ([`mesh::IndexedMesh::into_polygons`])
/// or triangulation ([`super::tessellate::run`]). Internal entry point
/// that lets `tessellate` reuse the indexed representation without
/// round-tripping through `Vec<Polygon>`.
pub(in crate::csg) fn run_to_indexed(polygons: Vec<Polygon>) -> mesh::IndexedMesh {
    // T-junction repair BEFORE merge: BSP fragments share collinear-
    // but-subdivided edges (one polygon's edge `(a,b)` against the
    // neighbour's `(b,c)` + `(c,a)` where `c` lies strictly between
    // `a` and `b`). Without first inserting `c` into `(a,b)`, the
    // bucket-wide twin cancellation in `merge_coplanar` can't pair
    // these as `(a,c) ↔ (c,a)` and `(c,b) ↔ (b,c)`, so what should
    // be one annular face comes out as several small loops. Repair
    // first canonicalises the edge subdivisions so merge sees clean
    // twin pairs.
    let welded = mesh::IndexedMesh::weld(polygons);
    check_invariants_after_weld(&welded);
    check_simple_loops(&welded, "post-weld");
    let repaired = welded.repair_tjunctions();
    check_invariants_after_tjunctions(&repaired);
    check_simple_loops(&repaired, "post-tjunctions");
    let merged = repaired.merge_coplanar();
    check_invariants_after_merge(&merged);
    check_simple_loops(&merged, "post-merge");
    let cleaned = merged.remove_slivers();
    check_invariants_after_slivers(&cleaned);
    check_simple_loops(&cleaned, "post-slivers");
    cleaned
}

/// Stage-boundary invariant from the auditor's spike investigation:
/// no polygon may contain the same `VertexId` twice in its vertex loop.
/// Wired into every cleanup stage so the first pass to emit a non-simple
/// loop is identifiable from the warn stream alone. Warn-only for now;
/// promote to `debug_assert!` after a soak period.
fn check_simple_loops(mesh: &mesh::IndexedMesh, stage: &str) {
    let violations = invariants::find_non_simple_loops(mesh);
    if !violations.is_empty() {
        let preview: Vec<_> = violations.iter().take(3).collect();
        tracing::warn!(
            stage,
            count = violations.len(),
            preview = ?preview,
            "non-simple loop emitted by cleanup stage (spike or repeated vertex)"
        );
    }
}

/// Issue 337: post-weld invariant — every polygon vertex id resolves
/// inside the pool, and no two distinct ids share identical fixed-point
/// coordinates. Warn-only for now; promote to `debug_assert!` after the
/// soak period in `invariants.rs`'s module doc.
fn check_invariants_after_weld(mesh: &mesh::IndexedMesh) {
    let violations = invariants::find_post_weld_violations(mesh);
    if !violations.is_empty() {
        let preview: Vec<_> = violations.iter().take(3).collect();
        tracing::warn!(
            count = violations.len(),
            preview = ?preview,
            "post-weld invariant violated: pool integrity (issue 337)"
        );
    }
}

/// Issue 337: post-`repair_tjunctions` invariant — no vertex in the
/// pool lies strictly interior to another polygon's edge. Survivors
/// mean the repair pass exited before reaching its fixed point.
fn check_invariants_after_tjunctions(mesh: &mesh::IndexedMesh) {
    let violations = invariants::find_unrepaired_tjunctions(mesh);
    if !violations.is_empty() {
        let preview: Vec<_> = violations.iter().take(3).collect();
        tracing::warn!(
            count = violations.len(),
            preview = ?preview,
            "post-tjunctions invariant violated: unrepaired interior vertices (issue 337)"
        );
    }
}

/// Issue 337: post-merge invariant — no surviving twin edges in any
/// `(plane, color)` bucket. Warn-only for now; promote to `debug_assert!`
/// after a soak period once warns have gone quiet.
fn check_invariants_after_merge(mesh: &mesh::IndexedMesh) {
    let violations = invariants::find_twin_edges(mesh);
    if !violations.is_empty() {
        let preview: Vec<_> = violations.iter().take(3).collect();
        tracing::warn!(
            count = violations.len(),
            preview = ?preview,
            "post-merge invariant violated: surviving twin edges (issue 337)"
        );
    }
}

/// Issue 337: post-`remove_slivers` invariant — every polygon has ≥3
/// vertices and no two consecutive vertices coincide. The pass
/// guarantees both tautologically; violations here are pure regressions.
fn check_invariants_after_slivers(mesh: &mesh::IndexedMesh) {
    let violations = invariants::find_post_sliver_violations(mesh);
    if !violations.is_empty() {
        let preview: Vec<_> = violations.iter().take(3).collect();
        tracing::warn!(
            count = violations.len(),
            preview = ?preview,
            "post-slivers invariant violated: degenerate polygons (issue 337)"
        );
    }
}

/// Run the cleanup pipeline on a polygon list, stopping after pass 4 so
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
    run_to_indexed(polygons).into_polygons()
}
