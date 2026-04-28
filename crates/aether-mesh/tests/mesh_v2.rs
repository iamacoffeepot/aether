//! Tests for the v2 vocabulary additions: torus + sweep-along-path.

use aether_math::Vec3;
use aether_mesh::{ParseError, mesh, parse};

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
        let normal = (b - a).cross(c - a);
        // Centroid in XZ; project onto the major circle.
        let cent = (a + b + c) * (1.0 / 3.0);
        let radial_xz = (cent.x * cent.x + cent.z * cent.z).sqrt();
        let tube_center = if radial_xz < 1e-6 {
            Vec3::ZERO
        } else {
            Vec3::new(
                cent.x / radial_xz * major_radius,
                0.0,
                cent.z / radial_xz * major_radius,
            )
        };
        let outward = cent - tube_center;
        assert!(
            normal.dot(outward) > 0.0,
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
    // = 8 triangles. The `:open true` opt-out keeps this test focused
    // on the side stitching — see `sweep_default_is_capped` below for
    // the closed-solid default counts (issue 352).
    let text = "(sweep
        ((-0.1 -0.1) (0.1 -0.1) (0.1 0.1) (-0.1 0.1))
        ((0 0 0) (1 0 0))
        :open true
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
        :open true
        :color 0)";
    let ast = parse(text).unwrap();
    let tris = mesh(&ast).unwrap();
    // Square profile, 3 path points → 2 tube segments → 4 quads each
    // → 8 triangles per segment → 16 triangles. `:open true` skips
    // caps so the count is side-only — issue 352.
    assert_eq!(tris.len(), 16);
    // Sanity: every vertex should be within `radius * sqrt(2)` of a
    // waypoint (corners of the square profile). Conservative bound.
    let path = [
        Vec3::new(0.0, 0.0, 0.0),
        Vec3::new(1.0, 0.0, 0.0),
        Vec3::new(1.0, 1.0, 0.0),
    ];
    for tri in &tris {
        for v in tri.vertices {
            let mut min_dist = f32::MAX;
            for p in &path {
                let dx = v.x - p.x;
                let dy = v.y - p.y;
                let dz = v.z - p.z;
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
fn sweep_scales_length_must_equal_path_length() {
    // ADR-0051 normative: scales-length-mismatched sweeps are a parse-time error.
    let text = "(sweep ((-0.1 -0.1) (0.1 -0.1) (0.1 0.1) (-0.1 0.1))
                       ((0 0 0) (1 0 0) (2 0 0))
                       :scales (1.0 0.5)
                       :color 0)";
    let err = parse(text).unwrap_err();
    assert!(
        matches!(
            err,
            ParseError::SweepScalesLengthMismatch {
                scales_len: 2,
                path_len: 3
            }
        ),
        "expected SweepScalesLengthMismatch, got {err:?}"
    );
}

#[test]
fn round_trip_torus_and_sweep() {
    let text = "(composition
        (torus 1.0 0.25 16 8 :color 5)
        (sweep ((0 0.05) (0.05 0) (0 -0.05) (-0.05 0))
               ((0 0.5 0) (0.3 0.6 0) (0.5 0.8 0))
               :color 7))";
    let ast1 = parse(text).unwrap();
    let serialized = aether_mesh::serialize(&ast1);
    let ast2 = parse(&serialized).unwrap();
    assert_eq!(ast1, ast2);
}

/// Issue 352: `:open true` round-trips through serialize → parse. The
/// closed default is the absent-keyword form so it's covered by every
/// other sweep round-trip test in the suite.
#[test]
fn round_trip_open_sweep() {
    let text = "(sweep ((0 0) (1 0) (1 1) (0 1))
                       ((0 0 0) (0 1 0))
                       :open true
                       :color 3)";
    let ast1 = parse(text).unwrap();
    let serialized = aether_mesh::serialize(&ast1);
    let ast2 = parse(&serialized).unwrap();
    assert_eq!(ast1, ast2);
    // Pin the AST shape so a regression in the parser silently
    // dropping `:open` would surface as a structural mismatch.
    use aether_math::Vec3;
    use aether_mesh::ast::Node;
    assert_eq!(
        ast1,
        Node::Sweep {
            profile: vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]],
            path: vec![Vec3::new(0.0, 0.0, 0.0), Vec3::new(0.0, 1.0, 0.0)],
            scales: None,
            open: true,
            color: 3,
        }
    );
}
