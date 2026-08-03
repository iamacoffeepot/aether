//! Strokes to triangles.
//!
//! The offline renderer ends at SVG, where a stroke is a centreline and a
//! width and the backend does the rest. A substrate has no such backend —
//! `aether.draw_triangle` takes triangles — so the ribbon has to be built
//! here, two per segment, the same shape `aether.kit.mesh` already uses for
//! its polygon outlines.
//!
//! Two things differ from that outline, and both come from wanting a pen
//! rather than a wire.
//!
//! **The ribbon faces the eye.** An outline offset in the surface plane
//! disappears edge-on; a drawn line never does, because a pen line is not a
//! thing in the world with a side to it. Offsetting perpendicular to the
//! stroke *and* to the view keeps the full width from every angle.
//!
//! **Width is angular, not absolute.** A plotter draws at one pen width
//! wherever the subject is on the page, so a world-space width — which
//! thins with distance like real geometry — reads as wire, not ink.
//! Scaling the half-width by the distance to the eye holds it constant on
//! screen, and needs no viewport: the angle is the parameter.

use aether_math::{Rgb, Vec3};
use aether_render::{DrawTriangle, Vertex};

use crate::feature::{Curve3, Pen};
use crate::style::{pressure, wander};

/// Half-width of a stroke at unit distance, in radians. Multiplied by the
/// distance to the eye it becomes a world half-width that projects to the
/// same size wherever the subject sits.
/// Derived, not guessed: the offline renderer draws a silhouette 2 px wide
/// on a 900 px page at this field of view, which is ~1410 px per radian.
/// Half of 2 px, divided by that scale, divided again by the silhouette's
/// own `base_width` of 2.0 — since this is multiplied by the class weight.
const ANGULAR_HALF_WIDTH: f32 = 0.00035;

/// How far the hand wanders, in the same angular units.
/// The same conversion applied to the offline silhouette wobble of 0.8 px.
const ANGULAR_WOBBLE: f32 = 0.00057;

/// Shortest *extracted* stroke worth drawing, in radians of arc. Detail too
/// small to read is dropped rather than inked as noise — a relief field on a
/// reconstruction throws off specks, and a speck at full weight reads as
/// dirt on the paper.
///
/// It does not reach an authored mark, and that exemption is not a nicety.
/// The floor is a noise rejector, and the chart draws no noise: a cupid's
/// bow is a fifth of a mouth wide, a lip corner tick is smaller still, and
/// the iris hook arrives as two short arcs because a lid crosses it. Every
/// one of those is under the floor and every one of them was silently
/// missing — the mouth came out as two bare lines and the eyes as blanks,
/// with nothing having errored.
const MIN_ANGULAR_LENGTH: f32 = 0.004;

fn ink(pen: Pen) -> Rgb {
    match pen {
        Pen::Ink => Rgb::new(0.106, 0.106, 0.122),
        Pen::Accent => Rgb::new(0.247, 0.498, 0.816),
        Pen::Pale => Rgb::new(0.490, 0.490, 0.525),
        // Diagnostic only. One hue per bone for the weight-paint view.
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

/// One visible run as a triangle strip, or nothing if it is too short to
/// read once projected.
pub fn ribbon(curve: &Curve3, eye: Vec3, jitter: u64, out: &mut Vec<DrawTriangle>) {
    if curve.points.len() < 2 {
        return;
    }

    let colour = ink(curve.pen);
    let weight = curve.class.base_width();
    let amplitude = curve.class.wobble_amplitude();

    // Arc measured in radians rather than world units, so the length floor
    // and the pressure ramp both mean the same thing at any distance.
    let angular: Vec<f32> = curve
        .points
        .windows(2)
        .scan(0.0f32, |total, pair| {
            let span = (pair[1].pos - pair[0].pos).length();
            let depth = (pair[0].pos - eye).length().max(1e-4);
            *total += span / depth;
            Some(*total)
        })
        .collect();
    let length = angular.last().copied().unwrap_or(0.0);
    if !curve.authored && length < MIN_ANGULAR_LENGTH {
        return;
    }

    // The wobble's phase is per-stroke and its argument is world position,
    // so a stroke bends the same way from every angle and stays continuous
    // across a weld. Neither is true of anything keyed to the page.
    let seed = curve.seed ^ jitter;
    let rail = |index: usize| -> (Vec3, Vec3) {
        let point = curve.points[index];
        let previous = curve.points[index.saturating_sub(1)].pos;
        let next = curve.points[(index + 1).min(curve.points.len() - 1)].pos;

        let to_eye = eye - point.pos;
        let depth = to_eye.length().max(1e-4);
        let along = next - previous;
        // Perpendicular to the stroke and to the view at once, which is
        // what keeps a line from vanishing when it turns edge-on.
        let across = along.cross(to_eye);
        let across = if across.length() < 1e-9 {
            Vec3::new(0.0, 0.0, 0.0)
        } else {
            across.normalize()
        };

        let taper = pressure(angular.get(index.saturating_sub(1)).copied().unwrap_or(0.0), length);
        let half = ANGULAR_HALF_WIDTH * depth * weight * taper * point.weight;
        let drift = wander(seed, point.pos) * ANGULAR_WOBBLE * depth * amplitude;
        let centre = point.pos + across * drift;

        (centre - across * half, centre + across * half)
    };

    let mut previous = rail(0);
    for index in 1..curve.points.len() {
        let current = rail(index);
        out.push(triangle(previous.0, current.0, current.1, colour));
        out.push(triangle(previous.0, current.1, previous.1, colour));
        previous = current;
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
