//! Regression test suite for the polygon-domain mesh pipeline
//! (ADR-0057). Every visual bug we've caught becomes a fixture here
//! so a future change that re-introduces it fails `cargo test`
//! instead of needing a live MCP capture to spot.
//!
//! The pattern: parse a DSL string, mesh it via `mesh_polygons`, run
//! `validate_manifold` (or the stricter `validate_geometry`) against
//! the output, fail with `report` if any invariant is violated. Each
//! test docstring records the bug it pins down.
//!
//! Two assertion strengths: `assert_watertight` checks topology only;
//! `assert_geometric` also runs the planarity, polygon-quality,
//! normal-coherence, and T-junction validators. The off-axis tests
//! lower in the file use the stronger one because tessellation-domain
//! bugs slip past topology when geometry isn't axis-aligned.

use aether_dsl_mesh::debug::{report, validate_geometry, validate_manifold};
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

/// Stronger than `assert_watertight`: also requires the geometric
/// validators (planarity, polygon quality, normal coherence,
/// T-junctions) to pass. Use for the off-axis corpus where shape
/// failures (slivers, fold-flips, cracks) matter as much as topology.
fn assert_geometric(dsl: &str) {
    let ast = parse(dsl).expect("parse failed");
    let polys = mesh_polygons(&ast).expect("mesh failed");
    let (manifold, geom) = validate_geometry(&polys);
    assert!(
        manifold.is_empty() && geom.is_empty(),
        "DSL `{}` produced an invalid mesh:\n\n{}",
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

/// **Regression**: the protruding sphere previously surfaced two
/// SingularEdges on the cube's z=-0.75 face caused by the triangle
/// round-trip in `mesh_polygons` re-deriving plane normals via cross
/// product on CDT-output sliver triangles (`n_z` flipped sign on ~20
/// cube-color triangles per face). The polygon-throughout migration
/// (`crate::mesh::mesh_polygons_internal`) skips the round-trip — n-gon
/// loops travel from CSG cleanup straight into `mesh_polygons`.
#[test]
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

/// **Regression**: 3-cutter difference passes after primitives
/// emit n-gons natively. Pre-migration the box's 12 input triangles
/// (6 quad faces split) fed BSP enough fragments per face that
/// chained snap-drift left a residual sliver near the center
/// cutter's `z=-0.2` corner. With native quads (PR D) the cube top
/// face is one quad in, the box cutter's hole becomes a clean
/// rectangular hole, and CDT triangulates the annular region without
/// the sliver.
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

/// Box rotated 30° around Y — same topology as the axis-aligned box,
/// but every face plane is now off-grid. Catches BSP plane-snap drift
/// on the primitive itself.
#[test]
fn rotated_box_is_geometric() {
    assert_geometric("(rotate (0 1 0) 0.5236 (box 1.5 1.5 1.5 :color 0))");
}

/// Cylinder tilted 30° from vertical — sides are no longer axis-parallel.
#[test]
fn tilted_cylinder_is_geometric() {
    assert_geometric("(rotate (1 0 0) 0.5236 (cylinder 0.5 1.5 16 :color 0))");
}

/// Sphere on a non-axis-aligned position. Sphere mesher is rotation-
/// invariant; this catches downstream snap drift only if it appears.
#[test]
fn translated_sphere_is_geometric() {
    assert_geometric("(translate (0.37 0.41 -0.23) (sphere 0.5 12 :color 0))");
}

/// **Regression**: the off-axis T-junction repair fix (issue #299) —
/// `COLLINEAR_TOLERANCE_FIXED_UNITS` raised from 1 to 4 to absorb
/// snap-drift accumulated across cascaded BSP cuts. Pre-fix this
/// composition produced 3 BoundaryEdges on the cube's −Y face plus a
/// TJunction at ~2.05 fixed units perpendicular drift; the prior
/// 1-unit collinearity bound silently dropped the vertex insertion.
///
/// Box with a 30°-rotated cylinder cutter through it. Off-axis cutter
/// vs. axis-aligned solid — exercises the cube-face-clipped-by-non-
/// orthogonal-cylinder-facet path that axis-aligned tests skip.
#[test]
fn box_minus_tilted_cylinder_is_geometric() {
    assert_geometric(
        "(difference \
         (box 1.5 1.5 1.5 :color 0) \
         (rotate (1 0 0) 0.5236 (cylinder 0.3 2.0 16 :color 1)))",
    );
}

/// 45°-rotated box vs. axis-aligned sphere. Both off-grid faces and
/// curved facet faces in the same scene.
#[test]
fn rotated_box_minus_sphere_is_geometric() {
    assert_geometric(
        "(difference \
         (rotate (0 1 0) 0.7854 (box 1.5 1.5 1.5 :color 0)) \
         (sphere 0.5 12 :color 1))",
    );
}

/// 45°-rotated box vs. 30°-rotated box. No two face planes share an
/// axis — every BSP partition edge is off-grid.
#[test]
fn two_rotated_boxes_difference_is_geometric() {
    assert_geometric(
        "(difference \
         (rotate (0 1 0) 0.7854 (box 1.5 1.5 1.5 :color 0)) \
         (rotate (0 0 1) 0.5236 (box 0.6 0.6 1.5 :color 1)))",
    );
}

/// Two rotated boxes union. Catches the dual of the difference case
/// — fold-flip bugs in union are usually distinct from difference.
#[test]
fn two_rotated_boxes_union_is_geometric() {
    assert_geometric(
        "(union \
         (rotate (0 1 0) 0.5236 (box 1 1 1 :color 0)) \
         (translate (0.5 0 0) (rotate (0 0 1) 0.5236 (box 1 1 1 :color 1))))",
    );
}

/// **Regression**: same off-axis T-junction repair fix as
/// `box_minus_tilted_cylinder_is_geometric` (issue #299). Pre-fix this
/// composition produced 3 BoundaryEdges on the +Y cube face plus a
/// TJunction at ~1.05 fixed units perpendicular drift.
///
/// Mixed primitives: union of sphere and box, intersected with a
/// cylinder. Three distinct facet topologies in one BSP composition.
#[test]
fn sphere_or_box_and_cylinder_is_geometric() {
    assert_geometric(
        "(intersection \
         (union (sphere 0.6 12 :color 0) (box 1 1 1 :color 1)) \
         (cylinder 0.45 2.0 16 :color 2))",
    );
}

/// **Ignored (sliver-edge class)**: the off-axis T-junction fix
/// (issue #299) eliminated the boundary-edge cracks here, but the
/// composition still produces 2 SliverEdges (~4e-4) and 2
/// ExtremeAspectRatios (~2516:1) on a cylinder/cube near-coincident
/// facet pair. The root cause is a different bug: BSP produces two
/// distinct vertices ~9 fixed units apart in one axis (above the
/// `WELD_TOLERANCE_FIXED_UNITS = 4` weld bound, so they survive
/// cleanup as separate vertices that bound a sliver edge).
///
/// Multiple rotated cutters at distinct angles through one box. The
/// closest analogue to the live-substrate three_cut_box but with each
/// cutter coming in at its own off-axis orientation.
#[test]
#[ignore]
fn box_minus_three_rotated_cutters_is_geometric() {
    assert_geometric(
        "(difference \
         (box 3.0 1.0 1.5 :color 0) \
         (translate (-0.9 0 0) (rotate (1 0 0) 0.3 (cylinder 0.3 1.5 16 :color 1))) \
         (translate (0 0 0) (rotate (0 0 1) 0.5236 (box 0.5 1.5 0.5 :color 2))) \
         (translate (0.9 0 0) (rotate (0 1 0) 0.3 (cylinder 0.3 1.5 16 :color 3))))",
    );
}

/// **Ignored (sliver-edge class)**: same root cause as
/// `box_minus_three_rotated_cutters_is_geometric` — mesh is watertight
/// (0 manifold violations) but BSP left a near-duplicate vertex pair
/// that bounds a 2-SliverEdge sequence (~9e-4). Pre-existing before
/// the issue #299 fix; un-ignore once near-duplicate elimination
/// (separate issue) lands.
///
/// Non-uniform scale on an otherwise axis-aligned scene. Scale changes
/// edge lengths asymmetrically — a corner-case for the aspect-ratio
/// validator and for any BSP code that assumes near-unit edges.
#[test]
#[ignore]
fn nonuniform_scaled_box_minus_sphere_is_geometric() {
    assert_geometric(
        "(scale (2.5 0.6 1.0) \
         (difference (box 1 1 1 :color 0) (sphere 0.4 12 :color 1)))",
    );
}

/// A rotated CSG result wrapped in another rotation. Tests that the
/// outer transform doesn't introduce numerical error on top of an
/// already-fragile BSP output.
#[test]
fn rotated_csg_then_rotated_again_is_geometric() {
    assert_geometric(
        "(rotate (0 0 1) 0.4 \
         (rotate (1 0 0) 0.4 \
         (difference (box 1.5 1.5 1.5 :color 0) (box 0.6 0.6 0.6 :color 1))))",
    );
}

/// Lathe (curved revolution surface) minus a box. Lathe is the canonical
/// off-axis-facet primitive — every side facet is rotated by 360/segs
/// from the previous one, none are world-axis-aligned.
#[test]
fn lathe_minus_box_is_geometric() {
    assert_geometric(
        "(difference \
         (lathe ((0 -0.5) (0.5 -0.5) (0.5 0.5) (0 0.5)) 16 :color 0) \
         (box 0.4 0.4 1.5 :color 1))",
    );
}

/// **Hangs (BSP runaway)**: ignored because BSP recursion exceeds
/// 2 minutes at 100% CPU on this composition. Two facetted curved
/// primitives with no axis alignment fragment past any reasonable
/// budget. Un-ignore once BSP performance is bounded for off-axis
/// curved×curved input.
///
/// Lathe minus a tilted cylinder. Two curved-facet primitives with
/// distinct axes — the worst case for cocircular fragmentation.
#[test]
#[ignore]
fn lathe_minus_tilted_cylinder_is_geometric() {
    assert_geometric(
        "(difference \
         (lathe ((0 -0.5) (0.5 -0.5) (0.5 0.5) (0 0.5)) 16 :color 0) \
         (rotate (1 0 0) 0.4 (cylinder 0.3 1.5 16 :color 1)))",
    );
}

/// **Ignored (sliver-edge class, severe)**: the off-axis T-junction
/// fix (issue #299) reduced manifold violations from 13 to 3, but the
/// composition still produces a SingularEdge plus 2 BoundaryEdges
/// rooted in the same near-duplicate-vertex bug as
/// `box_minus_three_rotated_cutters_is_geometric` (BSP leaves vertices
/// ~9 fixed units apart in one axis, above the weld bound). The
/// remaining geometry violations (slivers, extreme aspect ratios) are
/// downstream of the same near-duplicate pair. Useful as a stress
/// oracle: any fix that reduces this count is a forward step.
///
/// Two intersecting tilted cylinders. Pure curved-on-curved CSG,
/// fully off-axis. The polygon-throughout migration's most demanding
/// shape-quality test.
#[test]
#[ignore]
fn two_tilted_cylinders_union_is_geometric() {
    assert_geometric(
        "(union \
         (rotate (1 0 0) 0.5 (cylinder 0.4 1.5 16 :color 0)) \
         (rotate (0 0 1) 0.5 (cylinder 0.4 1.5 16 :color 1)))",
    );
}

/// 45° box rotation viewed as a *baseline* for the planarity validator —
/// no CSG, just a rotated primitive. If this fails, the tolerance is
/// too tight for legitimate f32 reconstruction noise on rotated input.
#[test]
fn rotated_box_no_csg_is_geometric() {
    assert_geometric("(rotate (0 1 0) 0.7854 (box 1 1 1 :color 0))");
}
