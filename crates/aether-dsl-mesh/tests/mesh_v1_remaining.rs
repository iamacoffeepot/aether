//! Tests for the v1 vocabulary completion: cylinder, cone, wedge,
//! sphere, extrude, mirror, array. Promoted alongside ADR-0051's
//! formalization of the v1 vocabulary.
//!
//! Each mesher gets a triangle-count check + an outward-winding check
//! (where the centroid-vs-normal test is well-defined for the shape).

use aether_dsl_mesh::{mesh, parse};
use aether_math::Vec3;

fn tri_normal(tri: &aether_dsl_mesh::Triangle) -> Vec3 {
    let a = tri.vertices[0];
    let b = tri.vertices[1];
    let c = tri.vertices[2];
    (b - a).cross(c - a)
}

fn tri_centroid(tri: &aether_dsl_mesh::Triangle) -> Vec3 {
    (tri.vertices[0] + tri.vertices[1] + tri.vertices[2]) * (1.0 / 3.0)
}

// ---------- cylinder ----------

#[test]
fn cylinder_has_28_triangles_after_cap_merge() {
    // Lathed from a 4-point profile with two pole edges + one side
    // edge: raw fan emits 1 (bottom cap) + 2 (side) + 1 (top cap) = 4
    // triangles per segment = 32 for n=8. The cleanup pass groups
    // coplanar same-color fragments and CDT re-tessellates: each cap
    // (8 axis-fan triangles, all coplanar same-color) collapses to an
    // octagon and CDT emits n−2 = 6 triangles. Sides stay at 16.
    // Total: 6 + 6 + 16 = 28.
    let ast = parse("(cylinder 1 2 8 :color 0)").unwrap();
    assert_eq!(mesh(&ast).unwrap().len(), 28);
}

#[test]
fn cylinder_outward_normals() {
    // For a cylinder centered at origin, side-face normals point away
    // from the Y axis; cap normals point along ±Y. The centroid-vs-
    // normal test works because the centroid of every face has a
    // strictly positive component in the outward direction.
    let ast = parse("(cylinder 1 2 12 :color 0)").unwrap();
    let tris = mesh(&ast).unwrap();
    for tri in &tris {
        let n = tri_normal(tri);
        let c = tri_centroid(tri);
        // Pick the dominant axis component of the centroid as the
        // expected outward direction.
        let radial_len = (c.x * c.x + c.z * c.z).sqrt();
        let outward = if c.y.abs() > radial_len {
            [0.0, c.y.signum(), 0.0]
        } else {
            [c.x, 0.0, c.z]
        };
        let dot = n.x * outward[0] + n.y * outward[1] + n.z * outward[2];
        assert!(
            dot > 0.0,
            "cylinder face normal points inward for triangle {tri:?}"
        );
    }
}

// ---------- cone ----------

#[test]
fn cone_has_10_triangles_after_cap_merge() {
    // 3-point profile, two pole edges (first edge: axis-to-base = bottom
    // cap; last edge: rim-to-apex = sloped sides). Raw fan emits 2 tris
    // per segment = 12 for n=6. Cleanup merges the bottom cap's 6
    // coplanar same-color tris into a hexagon, CDT re-emits as n−2 = 4
    // triangles. Sloped sides are at 6 distinct planes (different
    // angular orientation), unaffected. Total: 4 + 6 = 10.
    let ast = parse("(cone 1 2 6 :color 0)").unwrap();
    assert_eq!(mesh(&ast).unwrap().len(), 10);
}

#[test]
fn cone_outward_normals() {
    // Use a known deeply-interior reference point and check
    // (centroid - interior) · normal > 0 for every face. For a cone
    // with base at y=-1 and apex at y=+1, (0, -0.9, 0) is inside.
    let ast = parse("(cone 1 2 12 :color 0)").unwrap();
    let tris = mesh(&ast).unwrap();
    let interior = Vec3::new(0.0, -0.9, 0.0);
    for tri in &tris {
        let n = tri_normal(tri);
        let c = tri_centroid(tri);
        let v = c - interior;
        let dot = n.dot(v);
        assert!(
            dot > 0.0,
            "cone face normal points inward for triangle {tri:?}"
        );
    }
}

// ---------- wedge ----------

#[test]
fn wedge_has_eight_triangles() {
    // Two quads (bottom, back, hypotenuse) → 6 tris; two triangles
    // (left, right side) → 2 tris; total 8.
    let ast = parse("(wedge 2 1 1 :color 0)").unwrap();
    assert_eq!(mesh(&ast).unwrap().len(), 8);
}

#[test]
fn wedge_outward_normals() {
    // The wedge's geometric centroid lies on the hypotenuse face plane,
    // so a centroid-as-interior test is degenerate. Use a known point
    // strictly inside (low-Y, low-Z corner of the wedge volume).
    let ast = parse("(wedge 2 2 2 :color 0)").unwrap();
    let tris = mesh(&ast).unwrap();
    let interior = Vec3::new(0.0, -0.5, -0.5);
    for tri in &tris {
        let n = tri_normal(tri);
        let c = tri_centroid(tri);
        let v = c - interior;
        let dot = n.dot(v);
        assert!(
            dot > 0.0,
            "wedge face normal points inward for triangle {tri:?}"
        );
    }
}

#[test]
fn wedge_uses_six_unique_vertices() {
    let ast = parse("(wedge 2 2 2 :color 0)").unwrap();
    let tris = mesh(&ast).unwrap();
    let mut seen = std::collections::BTreeSet::<[i32; 3]>::new();
    for tri in &tris {
        for v in tri.vertices {
            seen.insert([v.x as i32, v.y as i32, v.z as i32]);
        }
    }
    assert_eq!(seen.len(), 6, "expected 6 unique corners, got {seen:?}");
}

// ---------- sphere ----------

#[test]
fn sphere_triangle_count_matches_lathe_pole_collapse() {
    // n+1 profile points, n profile edges. Two pole edges (first +
    // last) emit 1 tri/segment; the remaining n-2 edges emit
    // 2 tris/segment. Total = (2*(n-2) + 2) * segments = (2n - 2) * n.
    // For subdivisions = 8: (16 - 2) * 8 = 112.
    let ast = parse("(sphere 1 8 :color 0)").unwrap();
    assert_eq!(mesh(&ast).unwrap().len(), (2 * 8 - 2) * 8);
}

#[test]
fn sphere_vertices_lie_on_radius() {
    let radius: f32 = 1.5;
    let ast = parse("(sphere 1.5 12 :color 0)").unwrap();
    let tris = mesh(&ast).unwrap();
    for tri in &tris {
        for v in tri.vertices {
            let r = (v.x * v.x + v.y * v.y + v.z * v.z).sqrt();
            assert!(
                (r - radius).abs() < 1e-4,
                "sphere vertex off-radius: r={r}, expected {radius}"
            );
        }
    }
}

#[test]
fn sphere_outward_normals() {
    let ast = parse("(sphere 1 12 :color 0)").unwrap();
    let tris = mesh(&ast).unwrap();
    for tri in &tris {
        let n = tri_normal(tri);
        let c = tri_centroid(tri);
        // Centroid is inside the sphere shell; outward = c (radial).
        let dot = n.x * c.x + n.y * c.y + n.z * c.z;
        assert!(
            dot > 0.0,
            "sphere face normal points inward for triangle {tri:?}"
        );
    }
}

// ---------- extrude ----------

#[test]
fn extrude_square_produces_walls_and_caps() {
    // 4-edge square profile, depth 1: 4 side quads (8 tris) + 2 caps,
    // each fan-triangulated into n-2 = 2 tris. Total 8 + 4 = 12.
    let ast = parse(
        "(extrude
            ((-0.5 -0.5) (0.5 -0.5) (0.5 0.5) (-0.5 0.5))
            1
            :color 0)",
    )
    .unwrap();
    assert_eq!(mesh(&ast).unwrap().len(), 12);
}

#[test]
fn extrude_cap_normals_face_along_z() {
    // For a square extruded by depth 1, all triangles should be
    // axis-aligned faces. Verify outward direction by centroid sign.
    let ast = parse(
        "(extrude
            ((-0.5 -0.5) (0.5 -0.5) (0.5 0.5) (-0.5 0.5))
            1
            :color 0)",
    )
    .unwrap();
    let tris = mesh(&ast).unwrap();
    for tri in &tris {
        let n = tri_normal(tri);
        let c = tri_centroid(tri);
        // Front cap z=0 (centroid z=0) faces -Z; back cap z=1
        // (centroid z=1) faces +Z; sides (centroid z=0.5) face
        // outward in XY.
        let outward = if c.z < 0.01 {
            Vec3::new(0.0, 0.0, -1.0)
        } else if c.z > 0.99 {
            Vec3::new(0.0, 0.0, 1.0)
        } else {
            Vec3::new(c.x, c.y, 0.0)
        };
        let dot = n.dot(outward);
        assert!(
            dot > 0.0,
            "extrude face normal points inward for triangle {tri:?}"
        );
    }
}

#[test]
fn extrude_with_under_three_profile_points_emits_nothing() {
    let ast = parse("(extrude ((0 0) (1 0)) 1 :color 0)").unwrap();
    assert_eq!(mesh(&ast).unwrap().len(), 0);
}

// ---------- mirror ----------

#[test]
fn mirror_x_reflects_box_across_yz_plane() {
    // Box centered at (5, 0, 0), mirrored across YZ plane → centered
    // at (-5, 0, 0).
    let ast = parse("(mirror x (translate (5 0 0) (box 1 1 1 :color 0)))").unwrap();
    let tris = mesh(&ast).unwrap();
    assert_eq!(tris.len(), 12);
    for tri in &tris {
        for v in tri.vertices {
            assert!(
                v.x >= -5.51 && v.x <= -4.49,
                "mirror-x vertex x out of range: {v:?}"
            );
        }
    }
}

#[test]
fn mirror_preserves_outward_winding() {
    // After reflection + winding swap, normals should still point
    // outward of the reflected box (toward the new centroid at -5).
    let ast = parse("(mirror x (translate (5 0 0) (box 2 2 2 :color 0)))").unwrap();
    let tris = mesh(&ast).unwrap();
    for tri in &tris {
        let n = tri_normal(tri);
        let c = tri_centroid(tri);
        // Reflected box center is at (-5, 0, 0); outward = c - center.
        let outward = [c.x + 5.0, c.y, c.z];
        let dot = n.x * outward[0] + n.y * outward[1] + n.z * outward[2];
        assert!(
            dot > 0.0,
            "mirror face normal points inward for triangle {tri:?}"
        );
    }
}

// ---------- array ----------

#[test]
fn array_produces_count_copies() {
    let ast = parse("(array 4 (2 0 0) (box 1 1 1 :color 0))").unwrap();
    let tris = mesh(&ast).unwrap();
    assert_eq!(tris.len(), 12 * 4);
}

#[test]
fn array_copies_are_translated_correctly() {
    // 3 copies of a unit box at spacing (2, 0, 0): copies sit at
    // x=0, x=2, x=4.
    let ast = parse("(array 3 (2 0 0) (box 1 1 1 :color 0))").unwrap();
    let tris = mesh(&ast).unwrap();
    let mut x_centers = std::collections::BTreeSet::<i32>::new();
    for tri in &tris {
        let c = tri_centroid(tri);
        x_centers.insert(c.x.round() as i32);
    }
    assert!(x_centers.contains(&0));
    assert!(x_centers.contains(&2));
    assert!(x_centers.contains(&4));
}

#[test]
fn array_zero_count_emits_nothing() {
    let ast = parse("(array 0 (1 0 0) (box 1 1 1 :color 0))").unwrap();
    assert_eq!(mesh(&ast).unwrap().len(), 0);
}

// ---------- round-trip across the full v1 vocabulary ----------

#[test]
fn round_trip_full_v1_vocab() {
    let text = "(composition
        (cylinder 1 2 12 :color 0)
        (cone 0.5 1 8 :color 1)
        (wedge 1 1 1 :color 2)
        (sphere 0.7 8 :color 3)
        (extrude ((-1 -1) (1 -1) (1 1) (-1 1)) 0.5 :color 4)
        (mirror x (translate (2 0 0) (box 1 1 1 :color 5)))
        (array 3 (1.5 0 0) (box 0.5 0.5 0.5 :color 6)))";
    let ast1 = parse(text).unwrap();
    let serialized = aether_dsl_mesh::serialize(&ast1);
    let ast2 = parse(&serialized).unwrap();
    assert_eq!(ast1, ast2);
    // And the whole composition meshes without error.
    let _ = mesh(&ast1).unwrap();
}
