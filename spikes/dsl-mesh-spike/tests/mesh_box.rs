//! Verify the box mesher produces 12 triangles at the expected corners.

use dsl_mesh_spike::{mesh, parse};

#[test]
fn unit_box_has_twelve_triangles() {
    let ast = parse("(box 1 1 1 :color 0)").unwrap();
    let tris = mesh(&ast).unwrap();
    assert_eq!(tris.len(), 12);
}

#[test]
fn unit_box_corners_are_at_half_extents() {
    let ast = parse("(box 2 2 2 :color 5)").unwrap();
    let tris = mesh(&ast).unwrap();
    let mut seen_corners = std::collections::BTreeSet::<[i32; 3]>::new();
    for tri in &tris {
        for v in tri.vertices {
            // box 2 2 2 → corners at ±1 on each axis
            seen_corners.insert([v[0] as i32, v[1] as i32, v[2] as i32]);
        }
    }
    assert_eq!(
        seen_corners.len(),
        8,
        "expected 8 unique corners, got {seen_corners:?}"
    );
    for &x in &[-1, 1] {
        for &y in &[-1, 1] {
            for &z in &[-1, 1] {
                assert!(
                    seen_corners.contains(&[x, y, z]),
                    "missing corner ({x}, {y}, {z})"
                );
            }
        }
    }
}

#[test]
fn translated_box_is_offset() {
    let ast = parse("(translate (5 0 0) (box 1 1 1 :color 0))").unwrap();
    let tris = mesh(&ast).unwrap();
    for tri in &tris {
        for v in tri.vertices {
            assert!(v[0] >= 4.49 && v[0] <= 5.51, "vertex x out of range: {v:?}");
        }
    }
}

#[test]
fn composition_concatenates_triangles() {
    let ast = parse(
        "(composition
            (box 1 1 1 :color 0)
            (translate (3 0 0) (box 1 1 1 :color 1)))",
    )
    .unwrap();
    let tris = mesh(&ast).unwrap();
    assert_eq!(tris.len(), 24);
    let color_0 = tris.iter().filter(|t| t.color == 0).count();
    let color_1 = tris.iter().filter(|t| t.color == 1).count();
    assert_eq!(color_0, 12);
    assert_eq!(color_1, 12);
}

#[test]
fn box_face_normals_point_outward() {
    // For each triangle, (b - a) × (c - a) must point away from the box center
    // (which is at origin for an untranslated box).
    let ast = parse("(box 2 2 2 :color 0)").unwrap();
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
        // Centroid of the triangle, treated as a vector from origin.
        let centroid = [
            (a[0] + b[0] + c[0]) / 3.0,
            (a[1] + b[1] + c[1]) / 3.0,
            (a[2] + b[2] + c[2]) / 3.0,
        ];
        let dot = normal[0] * centroid[0] + normal[1] * centroid[1] + normal[2] * centroid[2];
        assert!(
            dot > 0.0,
            "face winding wrong — normal points inward for triangle {tri:?}"
        );
    }
}
