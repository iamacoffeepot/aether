//! Regression suite for the polygon-domain mesh pipeline (ADR-0057).
//! Each fixture parses a DSL string, meshes it, and runs the manifold /
//! geometric validators against the result.
//!
//! Boolean composition was retired by ADR-0062; the dense CSG fixture
//! corpus that lived here has moved to `archive/csg-bsp`. What remains
//! pins the non-boolean primitive + transform paths that production
//! still exercises.

use aether_mesh::debug::{report, validate_geometry, validate_manifold};
use aether_mesh::{mesh_polygons, parse};

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

/// Stronger than `assert_watertight`: also requires planarity, polygon
/// quality, normal coherence, and T-junction validators to pass.
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

/// Box rotated 30° around Y — every face plane off-grid. Catches BSP
/// plane-snap drift on the primitive itself; without CSG this stays
/// useful as a simple non-axis-aligned regression.
#[test]
fn rotated_box_is_geometric() {
    assert_geometric("(rotate (0 1 0) 0.5236 (box 1.5 1.5 1.5 :color 0))");
}

/// Cylinder tilted 30° from vertical — sides no longer axis-parallel.
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
