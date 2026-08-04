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

/// Page pixels per radian of arc: the one bridge between the offline
/// renderer's page-space measurements and the angular units used here.
/// The offline renderer draws on a 900 px page at this field of view,
/// which works out to ~1410 px per radian. Any figure quoted in page
/// pixels — a width, a wobble, a length floor — divides by this to cross.
const PAGE_PIXELS_PER_RADIAN: f32 = 1410.0;

/// Half-width of a stroke at unit distance, in radians. Multiplied by the
/// distance to the eye it becomes a world half-width that projects to the
/// same size wherever the subject sits.
/// Derived, not guessed: half of the offline silhouette's 2 px, divided by
/// the conversion above, divided again by the silhouette's own `base_width`
/// of 2.0 — since this is multiplied by the class weight.
const ANGULAR_HALF_WIDTH: f32 = 0.00035;

/// One page pixel of hand-wander, in the same angular units. Bare, because
/// `ribbon` multiplies it by the class's own `wobble_amplitude` — which is
/// already in page pixels — so the class amplitude lands exactly once.
const ANGULAR_WOBBLE: f32 = 1.0 / PAGE_PIXELS_PER_RADIAN;

/// One point's rail pair, solved but not yet widened.
///
/// `offset` reaches from the centre to the right rail at full pressure,
/// so a consumer scales it by whatever taper it believes in and mirrors
/// it for the left. That is the whole difference between the two
/// consumers: [`ribbon`] multiplies by the `taper` solved here from the
/// run it was handed, and the ink pass' vertex stage multiplies by one
/// read from the visibility field instead.
#[derive(Clone, Copy)]
pub struct Rail {
    pub centre: Vec3,
    pub offset: Vec3,
    pub taper: f32,
}

pub fn ink(pen: Pen) -> Rgb {
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

/// One curve's rail pairs, or nothing if it is too short to read once
/// projected.
///
/// Everything here is view-dependent and so is re-solved whenever the
/// eye moves — the perpendicular, the depth weighting, the wobble's
/// world argument. The taper is solved too, but only against the arc of
/// the curve it was handed: a caller that split the curve into visible
/// runs first gets ends anchored on those runs, and a caller that hands
/// the whole curve over gets ends anchored on the curve. The ink pass
/// is the second kind and overrides the taper from the field.
pub fn rails(curve: &Curve3, eye: Vec3, jitter: u64, out: &mut Vec<Rail>) -> bool {
    if curve.points.len() < 2 {
        return false;
    }

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
    // The floor does not reach an authored mark, and that exemption is not a
    // nicety. It is a noise rejector, and the chart draws no noise: a cupid's
    // bow is a fifth of a mouth wide, a lip corner tick is smaller still, and
    // the iris hook arrives as two short arcs because a lid crosses it. Every
    // one of those is under the floor and every one of them was silently
    // missing — the mouth came out as two bare lines and the eyes as blanks,
    // with nothing having errored.
    if !curve.authored && length < curve.class.min_length() / PAGE_PIXELS_PER_RADIAN {
        return false;
    }

    // The wobble's phase is per-stroke and its argument is world position,
    // so a stroke bends the same way from every angle and stays continuous
    // across a weld. Neither is true of anything keyed to the page.
    let seed = curve.seed ^ jitter;

    // The stroke's own average distance, against which each of its points is
    // near or far. Taken per stroke rather than per scene so the cue reads as
    // one line turning through depth, not as a global fog.
    let reference_depth =
        curve.points.iter().map(|point| (point.pos - eye).length()).sum::<f32>() / curve.points.len() as f32;

    let rail = |index: usize| -> Rail {
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
        // Nearer stroke points are bolder — the one cue that keeps a flat
        // line drawing from reading as a decal on glass. The angular
        // half-width above holds screen width constant through depth, so
        // this rides on top of it rather than being cancelled by it.
        let depth_weight = (reference_depth / depth).clamp(0.82, 1.22);
        let half = ANGULAR_HALF_WIDTH * depth * weight * depth_weight * point.weight;
        let drift = wander(seed, point.pos) * ANGULAR_WOBBLE * depth * amplitude;
        let centre = point.pos + across * drift;

        Rail { centre, offset: across * half, taper }
    };

    out.extend((0..curve.points.len()).map(rail));

    true
}

/// One visible run as a triangle strip, or nothing if it is too short to
/// read once projected.
///
/// The CPU path, and after ADR-0172 the parity oracle rather than the
/// per-frame producer: the ink the frame shows is rasterized from
/// [`rails`] by the stroke program instead.
pub fn ribbon(curve: &Curve3, eye: Vec3, jitter: u64, out: &mut Vec<DrawTriangle>) {
    let mut solved = Vec::with_capacity(curve.points.len());
    if !rails(curve, eye, jitter, &mut solved) {
        return;
    }

    let colour = ink(curve.pen);
    let widened = |rail: &Rail| (rail.centre - rail.offset * rail.taper, rail.centre + rail.offset * rail.taper);

    let mut previous = widened(&solved[0]);
    for rail in &solved[1..] {
        let current = widened(rail);
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
