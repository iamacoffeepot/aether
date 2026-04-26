//! Regression test suite for the polygon-domain mesh pipeline
//! (ADR-0057). Every visual bug we've caught becomes a fixture here
//! so a future change that re-introduces it fails `cargo test`
//! instead of needing a live MCP capture to spot.
//!
//! The pattern: parse a DSL string, mesh it via `mesh_polygons`, run
//! `validate_manifold` against the output, fail with `report` if any
//! invariant is violated. Each test docstring records the bug it
//! pins down.

use aether_dsl_mesh::debug::{report, validate_manifold};
use aether_dsl_mesh::{mesh_polygons, parse};

fn assert_watertight(dsl: &str) {
    let ast = parse(dsl).expect("parse failed");
    let polys = mesh_polygons(&ast).expect("mesh failed");
    let violations = validate_manifold(&polys);
    assert!(
        violations.is_empty(),
        "DSL `{}` produced a non-watertight mesh:\n\n{}",
        dsl,
        report(&polys)
    );
}

#[test]
fn plain_box_is_watertight() {
    assert_watertight("(box 1.5 1.5 1.5 :color 0)");
}

#[test]
fn plain_cylinder_is_watertight() {
    assert_watertight("(cylinder 0.5 1.5 12 :color 0)");
}

#[test]
fn plain_sphere_is_watertight() {
    assert_watertight("(sphere 0.5 12 :color 0)");
}

/// **Regression**: cube with a fully-enclosed inner box should be
/// a hollow cube — the outer cube surface intact, an inner cubical
/// cavity. Every edge of every polygon should be shared by exactly
/// two polygons. (Caught visually 2026-04-26: the broken pipeline
/// rendered the inner geometry visible from outside.)
#[test]
fn box_minus_enclosed_box_is_watertight() {
    assert_watertight("(difference (box 1.5 1.5 1.5 :color 0) (box 0.6 0.6 0.6 :color 1))");
}

/// **Known-failing**: BSP CSG produces ~36 boundary edges on cube
/// face planes when a sphere is subtracted. Originally hypothesized
/// as a Plane3::coplanar_threshold L1-vs-L2 issue, but that fix
/// (landed in this PR) didn't reduce the count — the bug was
/// localized via the diagnostics in csg::ops to *BSP fragmentation
/// asymmetry* between cube-clipped-by-sphere (axis-aligned
/// partitioners) and sphere-clipped-by-cube (sphere facet
/// partitioners). See csg::ops::tests::diagnostic_box_minus_sphere*
/// for the localization. Follow-up PR un-ignores once fixed.
#[test]
fn box_minus_enclosed_sphere_is_watertight() {
    assert_watertight("(difference (box 1.5 1.5 1.5 :color 0) (sphere 0.5 12 :color 1))");
}

/// **Known-failing — blocked on polygon-throughout migration.** The two
/// SingularEdges (forward=2, reverse=2) on the cube's z=-0.75 face are
/// caused by the triangle round-trip in `mesh_polygons`: BSP CSG output
/// is correct (zero reversed-orientation cube fragments), but the wire
/// `Vec<Triangle>` boundary fan-triangulates n-gons and `from_triangle`
/// re-derives each plane from three vertices via cross product. For
/// CDT-output sliver triangles (near-collinear vertices), the cross
/// product is dominated by sub-fixed-point noise and `n_z` flips sign
/// on ~20 cube-color triangles per protruding face. Those reversed
/// triangles re-enter cleanup as a separate coplanar bucket and emit
/// as a real second mesh layer — the visible singular edges. The fix
/// is the polygon-throughout migration: skip the triangle round-trip
/// entirely (n-gon loops travel from CSG cleanup straight into
/// `mesh_polygons`, no plane re-derivation). Tracked separately.
#[test]
#[ignore = "blocked on polygon-throughout migration — triangle round-trip flips sliver normals"]
fn box_minus_protruding_sphere_is_watertight() {
    assert_watertight("(difference (box 1.5 1.5 1.5 :color 0) (sphere 0.95 12 :color 1))");
}

/// **Known-failing**: same root cause — cylinder side facets
/// near-cube-face planes trigger the same fragmentation asymmetry
/// in BSP composition.
#[test]
fn box_minus_cylinder_is_watertight() {
    assert_watertight("(difference (box 1.5 1.5 1.5 :color 0) (cylinder 0.3 2.0 16 :color 1))");
}

/// **Known-failing**: the 3-cut box compounds two cylinder cuts —
/// same BSP fragmentation issue as the single-cylinder case.
#[test]
fn three_cut_box_is_watertight() {
    assert_watertight(
        "(difference \
         (box 3.0 1.0 1.5 :color 0) \
         (translate (-0.9 0 0) (cylinder 0.3 1.5 16 :color 1)) \
         (translate (0 0 0) (box 0.4 1.5 0.4 :color 2)) \
         (translate (0.9 0 0) (cylinder 0.3 1.5 16 :color 3)))",
    );
}

#[test]
fn union_of_disjoint_boxes_is_watertight() {
    assert_watertight("(union (box 1 1 1 :color 0) (translate (3 0 0) (box 1 1 1 :color 1)))");
}

#[test]
fn union_of_overlapping_boxes_is_watertight() {
    assert_watertight("(union (box 1 1 1 :color 0) (translate (0.5 0 0) (box 1 1 1 :color 1)))");
}
