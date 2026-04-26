//! Verify the lathe mesher: triangle counts, axis-collapse handling,
//! and outward-facing winding.

use aether_dsl_mesh::{mesh, parse};

#[test]
fn straight_cylinder_via_lathe_has_expected_triangle_count() {
    // Two profile points, both at radius 1 — produces a cylinder side
    // wall (no caps). 8 segments → 8 quads → 16 triangles.
    let text = "(lathe ((1 0) (1 2)) 8 :color 0)";
    let ast = parse(text).unwrap();
    let tris = mesh(&ast).unwrap();
    assert_eq!(tris.len(), 16);
}

#[test]
fn axis_apex_collapses_degenerate_triangles_away() {
    // Cone: profile from axis (0, 0) to rim (1, 1). The axis point
    // collapses every angular sample to the same vertex; the
    // mesher must drop the resulting zero-area triangles, leaving
    // only the slanted side wall (8 triangles for 8 segments).
    let text = "(lathe ((0 0) (1 1)) 8 :color 0)";
    let ast = parse(text).unwrap();
    let tris = mesh(&ast).unwrap();
    assert_eq!(tris.len(), 8, "cone should have one triangle per segment");
}

#[test]
fn closed_disc_via_axis_to_rim_then_rim_to_axis() {
    // Profile that opens at the axis, goes out to a rim, and closes
    // back to the axis: a 'bowl' that's actually a flat disc + an
    // inverted cone. Just sanity-checks the mesher emits triangles
    // for both edges without panicking.
    let text = "(lathe ((0 0) (1 0) (0 0.01)) 6 :color 0)";
    let ast = parse(text).unwrap();
    let tris = mesh(&ast).unwrap();
    // 6-segment lathe with two profile edges, both axis-collapsed
    // on one side: 6 + 6 = 12 triangles (one per segment per edge).
    assert_eq!(tris.len(), 12);
}

#[test]
fn lathe_face_normals_point_outward() {
    // Cylinder: every face normal should point in the +radial
    // direction (away from the Y axis). Test by dotting the
    // triangle's centroid (radial vector from y-axis) against
    // the cross-product normal.
    let text = "(lathe ((1 0) (1 2)) 12 :color 0)";
    let ast = parse(text).unwrap();
    let tris = mesh(&ast).unwrap();
    for tri in &tris {
        let a = tri.vertices[0];
        let b = tri.vertices[1];
        let c = tri.vertices[2];
        let ab = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
        let ac = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
        let normal = [
            ab[1] * ac[2] - ab[2] * ac[1],
            ab[2] * ac[0] - ab[0] * ac[2],
            ab[0] * ac[1] - ab[1] * ac[0],
        ];
        // Radial vector from Y axis at the triangle's centroid (drop y).
        let centroid = [(a[0] + b[0] + c[0]) / 3.0, (a[2] + b[2] + c[2]) / 3.0];
        let radial_dot = normal[0] * centroid[0] + normal[2] * centroid[1];
        assert!(
            radial_dot > 0.0,
            "lathe face normal points inward for triangle {tri:?}"
        );
    }
}

#[test]
fn lathe_with_translate_offsets_all_vertices() {
    let text = "(translate (5 0 0) (lathe ((1 0) (1 2)) 4 :color 0))";
    let ast = parse(text).unwrap();
    let tris = mesh(&ast).unwrap();
    for tri in &tris {
        for v in tri.vertices {
            assert!(
                v[0] >= 4.0 && v[0] <= 6.0,
                "translated cylinder x out of range: {v:?}"
            );
        }
    }
}

#[test]
fn fewer_than_three_segments_produces_no_geometry() {
    let text = "(lathe ((1 0) (1 1)) 2 :color 0)";
    let ast = parse(text).unwrap();
    let tris = mesh(&ast).unwrap();
    assert_eq!(
        tris.len(),
        0,
        "segments < 3 is degenerate, should emit nothing"
    );
}
