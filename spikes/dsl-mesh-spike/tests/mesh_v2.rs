//! Tests for the v2 vocabulary additions: torus + sweep-along-path.

use dsl_mesh_spike::{mesh, parse};

#[test]
fn torus_triangle_count_is_two_per_quad() {
    // 8×6 = 48 quads → 96 triangles.
    let text = "(torus 1.0 0.25 8 6 :color 0)";
    let ast = parse(text).unwrap();
    let tris = mesh(&ast).unwrap();
    assert_eq!(tris.len(), 8 * 6 * 2);
}

#[test]
fn torus_face_normals_point_outward() {
    // Outward direction at a torus vertex is (vertex - tube_center)
    // where tube_center is the projection of the vertex onto the
    // major circle (i.e., scaled to major_radius in the XZ plane).
    let major_radius: f32 = 1.0;
    let text = "(torus 1.0 0.25 12 8 :color 0)";
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
        // Centroid in XZ; project onto the major circle.
        let cent = [
            (a[0] + b[0] + c[0]) / 3.0,
            (a[1] + b[1] + c[1]) / 3.0,
            (a[2] + b[2] + c[2]) / 3.0,
        ];
        let radial_xz = (cent[0] * cent[0] + cent[2] * cent[2]).sqrt();
        let tube_center = if radial_xz < 1e-6 {
            [0.0, 0.0, 0.0]
        } else {
            [
                cent[0] / radial_xz * major_radius,
                0.0,
                cent[2] / radial_xz * major_radius,
            ]
        };
        let outward = [
            cent[0] - tube_center[0],
            cent[1] - tube_center[1],
            cent[2] - tube_center[2],
        ];
        let dot = normal[0] * outward[0] + normal[1] * outward[1] + normal[2] * outward[2];
        assert!(
            dot > 0.0,
            "torus face normal points inward for triangle {tri:?}"
        );
    }
}

#[test]
fn torus_with_under_three_segments_emits_nothing() {
    let ast = parse("(torus 1.0 0.25 2 6 :color 0)").unwrap();
    assert_eq!(mesh(&ast).unwrap().len(), 0);
    let ast = parse("(torus 1.0 0.25 6 2 :color 0)").unwrap();
    assert_eq!(mesh(&ast).unwrap().len(), 0);
}

#[test]
fn sweep_straight_path_is_an_extruded_polygon() {
    // 4-point square profile swept along a 2-point straight path
    // should produce a square tube: 4 sides × 1 segment × 2 triangles
    // = 8 triangles. No caps in v1.
    let text = "(sweep
        ((-0.1 -0.1) (0.1 -0.1) (0.1 0.1) (-0.1 0.1))
        ((0 0 0) (1 0 0))
        :color 0)";
    let ast = parse(text).unwrap();
    let tris = mesh(&ast).unwrap();
    assert_eq!(tris.len(), 4 * 2);
}

#[test]
fn sweep_curved_path_keeps_profile_perpendicular() {
    // A path that turns 90° (going +x then +y) — verify all profile
    // ring vertices land at the correct distance from each waypoint.
    let radius: f32 = 0.1;
    let text = "(sweep
        ((-0.1 -0.1) (0.1 -0.1) (0.1 0.1) (-0.1 0.1))
        ((0 0 0) (1 0 0) (1 1 0))
        :color 0)";
    let ast = parse(text).unwrap();
    let tris = mesh(&ast).unwrap();
    // Square profile, 3 path points → 2 tube segments → 4 quads each
    // → 8 triangles per segment → 16 triangles.
    assert_eq!(tris.len(), 16);
    // Sanity: every vertex should be within `radius * sqrt(2)` of a
    // waypoint (corners of the square profile). Conservative bound.
    let path = [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [1.0, 1.0, 0.0]];
    for tri in &tris {
        for v in tri.vertices {
            let mut min_dist = f32::MAX;
            for p in &path {
                let dx = v[0] - p[0];
                let dy = v[1] - p[1];
                let dz = v[2] - p[2];
                let d = (dx * dx + dy * dy + dz * dz).sqrt();
                if d < min_dist {
                    min_dist = d;
                }
            }
            assert!(
                min_dist <= radius * std::f32::consts::SQRT_2 + 1e-4,
                "swept vertex too far from any waypoint: {v:?} (dist {min_dist})"
            );
        }
    }
}

#[test]
fn sweep_with_short_path_emits_nothing() {
    let ast =
        parse("(sweep ((-0.1 -0.1) (0.1 -0.1) (0.1 0.1) (-0.1 0.1)) ((0 0 0)) :color 0)").unwrap();
    assert_eq!(mesh(&ast).unwrap().len(), 0);
}

#[test]
fn sweep_with_under_three_profile_points_emits_nothing() {
    let ast = parse("(sweep ((-0.1 0) (0.1 0)) ((0 0 0) (1 0 0)) :color 0)").unwrap();
    assert_eq!(mesh(&ast).unwrap().len(), 0);
}

#[test]
fn round_trip_torus_and_sweep() {
    let text = "(composition
        (torus 1.0 0.25 16 8 :color 5)
        (sweep ((0 0.05) (0.05 0) (0 -0.05) (-0.05 0))
               ((0 0.5 0) (0.3 0.6 0) (0.5 0.8 0))
               :color 7))";
    let ast1 = parse(text).unwrap();
    let serialized = dsl_mesh_spike::serialize(&ast1);
    let ast2 = parse(&serialized).unwrap();
    assert_eq!(ast1, ast2);
}
