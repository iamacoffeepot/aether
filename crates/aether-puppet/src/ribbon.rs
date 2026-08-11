//! Puppet policy and renderer packing for shared eye-facing stroke geometry.
//!
//! `aether_mesh::stroke` owns the numeric solve. This module retains the
//! puppet's public API, resolves feature policy into angular parameters, and
//! packs renderer-neutral positions into coloured `DrawTriangle`s.

use aether_math::{Rgb, Vec3};
use aether_mesh::stroke::{self, StrokeParameters, StrokePoint};
use aether_render::{DrawTriangle, Vertex};

use crate::feature::{Curve3, Pen};

pub use aether_mesh::stroke::{Anchor, NOT_DRAWN, Rail};

/// Page pixels per radian of arc: the bridge between the offline renderer's
/// page-space measurements and the angular units used by the shared solve.
const PAGE_PIXELS_PER_RADIAN: f32 = 1410.0;

/// Half-width of a stroke at unit distance, in radians.
const ANGULAR_HALF_WIDTH: f32 = 0.00035;

/// One page pixel of hand-wander, in angular units.
const ANGULAR_WOBBLE: f32 = 1.0 / PAGE_PIXELS_PER_RADIAN;

/// Minimum angular arc a curve must span to read as ink.
#[must_use]
pub fn minimum_angular_length(curve: &Curve3) -> f32 {
    if curve.authored {
        0.0
    } else {
        curve.class.min_length() / PAGE_PIXELS_PER_RADIAN
    }
}

/// Colour selected by the puppet pen policy.
#[must_use]
pub fn ink(pen: Pen) -> Rgb {
    match pen {
        Pen::Ink => Rgb::new(0.106, 0.106, 0.122),
        Pen::Accent => Rgb::new(0.247, 0.498, 0.816),
        Pen::Pale => Rgb::new(0.490, 0.490, 0.525),
        Pen::Bone(index) => {
            const WHEEL: [Rgb; 6] = [
                Rgb::new(0.73, 0.71, 0.66),
                Rgb::new(0.06, 0.61, 0.56),
                Rgb::new(0.18, 0.44, 0.82),
                Rgb::new(0.84, 0.25, 0.18),
                Rgb::new(0.29, 0.66, 0.20),
                Rgb::new(0.88, 0.54, 0.12),
            ];
            WHEEL[usize::from(index) % WHEEL.len()]
        }
    }
}

/// Append the curve's eye-free anchors through the shared solve.
pub fn anchors(curve: &Curve3, jitter: u64, out: &mut Vec<Anchor>) -> bool {
    stroke::anchors(stroke_points(curve), parameters(curve), curve.seed, jitter, out)
}

/// Return the curve's average eye distance, or [`NOT_DRAWN`] if it is too short.
pub fn reference_depth(curve: &Curve3, eye: Vec3) -> f32 {
    stroke::reference_depth(stroke_points(curve), parameters(curve), eye)
}

/// Solve one anchor against an eye into its centre and full-pressure offset.
#[must_use]
pub fn rail(anchor: &Anchor, reference: f32, eye: Vec3) -> (Vec3, Vec3) {
    stroke::rail(anchor, reference, eye)
}

/// Append the curve's solved rails through the shared solve.
pub fn rails(curve: &Curve3, eye: Vec3, jitter: u64, out: &mut Vec<Rail>) -> bool {
    stroke::rails(stroke_points(curve), parameters(curve), eye, curve.seed, jitter, out)
}

/// Append one visible run as coloured renderer triangles.
pub fn ribbon(curve: &Curve3, eye: Vec3, jitter: u64, out: &mut Vec<DrawTriangle>) {
    let colour = ink(curve.pen);
    out.extend(
        stroke::ribbon(stroke_points(curve), parameters(curve), eye, curve.seed, jitter)
            .into_iter()
            .map(|[a, b, c]| triangle(a, b, c, colour)),
    );
}

fn stroke_points(curve: &Curve3) -> impl Iterator<Item = StrokePoint> + Clone + '_ {
    curve.points.iter().map(|point| StrokePoint { pos: point.pos, weight: point.weight })
}

fn parameters(curve: &Curve3) -> StrokeParameters {
    StrokeParameters {
        angular_half_width: ANGULAR_HALF_WIDTH * curve.class.base_width(),
        angular_wobble: ANGULAR_WOBBLE,
        wobble_scale: curve.class.wobble_amplitude(),
        minimum_angular_length: minimum_angular_length(curve),
    }
}

fn triangle(a: Vec3, b: Vec3, c: Vec3, colour: Rgb) -> DrawTriangle {
    DrawTriangle {
        verts: [
            Vertex { x: a.x, y: a.y, z: a.z, color: colour },
            Vertex { x: b.x, y: b.y, z: b.z, color: colour },
            Vertex { x: c.x, y: c.y, z: c.z, color: colour },
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feature::{FeatureClass, SurfacePoint};

    #[test]
    fn fixed_seed_ribbon_preserves_position_and_colour_bits() {
        let mut points = vec![
            SurfacePoint::on_surface(Vec3::new(-0.7, 0.2, 0.4), Vec3::new(0.0, 0.0, 1.0)),
            SurfacePoint::on_surface(Vec3::new(0.1, 0.8, 0.6), Vec3::new(0.0, 0.0, 1.0)),
            SurfacePoint::on_surface(Vec3::new(0.9, 0.3, 1.1), Vec3::new(0.0, 0.0, 1.0)),
        ];
        points[0].weight = 0.65;
        points[1].weight = 1.2;
        points[2].weight = 0.8;
        let curve = Curve3 {
            points,
            class: FeatureClass::Hatch { level: 1 },
            pen: Pen::Accent,
            seed: 0x1234_5678_9abc_def0,
            authored: false,
        };
        let mut triangles = Vec::new();

        ribbon(&curve, Vec3::new(2.4, -1.3, 5.7), 0x0fed_cba9_8765_4321, &mut triangles);

        let actual: Vec<_> = triangles
            .iter()
            .map(|triangle| {
                triangle.verts.map(|vertex| {
                    [
                        vertex.x.to_bits(),
                        vertex.y.to_bits(),
                        vertex.z.to_bits(),
                        vertex.color.r.to_bits(),
                        vertex.color.g.to_bits(),
                        vertex.color.b.to_bits(),
                    ]
                })
            })
            .collect();
        let expected = vec![
            [
                [0xbf33_9e78, 0x3e4e_8b25, 0x3ecd_8973, 0x3e7c_ed91, 0x3efe_f9db, 0x3f50_e560],
                [0x3dc9_6911, 0x3f4e_339e, 0x3f1a_5e44, 0x3e7c_ed91, 0x3efe_f9db, 0x3f50_e560],
                [0x3dcb_5a47, 0x3f4d_6604, 0x3f19_ed94, 0x3e7c_ed91, 0x3efe_f9db, 0x3f50_e560],
            ],
            [
                [0xbf33_9e78, 0x3e4e_8b25, 0x3ecd_8973, 0x3e7c_ed91, 0x3efe_f9db, 0x3f50_e560],
                [0x3dcb_5a47, 0x3f4d_6604, 0x3f19_ed94, 0x3e7c_ed91, 0x3efe_f9db, 0x3f50_e560],
                [0xbf33_50b8, 0x3e4d_479f, 0x3ecd_00b5, 0x3e7c_ed91, 0x3efe_f9db, 0x3f50_e560],
            ],
            [
                [0x3dc9_6911, 0x3f4e_339e, 0x3f1a_5e44, 0x3e7c_ed91, 0x3efe_f9db, 0x3f50_e560],
                [0x3f66_50ef, 0x3e99_45bd, 0x3f8c_c902, 0x3e7c_ed91, 0x3efe_f9db, 0x3f50_e560],
                [0x3f66_3261, 0x3e98_ce5f, 0x3f8c_c39c, 0x3e7c_ed91, 0x3efe_f9db, 0x3f50_e560],
            ],
            [
                [0x3dc9_6911, 0x3f4e_339e, 0x3f1a_5e44, 0x3e7c_ed91, 0x3efe_f9db, 0x3f50_e560],
                [0x3f66_3261, 0x3e98_ce5f, 0x3f8c_c39c, 0x3e7c_ed91, 0x3efe_f9db, 0x3f50_e560],
                [0x3dcb_5a47, 0x3f4d_6604, 0x3f19_ed94, 0x3e7c_ed91, 0x3efe_f9db, 0x3f50_e560],
            ],
        ];

        assert_eq!(actual, expected);
    }
}
