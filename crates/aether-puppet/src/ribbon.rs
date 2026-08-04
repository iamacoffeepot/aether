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

/// Nearest and furthest a point's own depth cue is allowed to reach.
///
/// The ink pass' vertex stage clamps by the same pair, restated in its
/// WGSL because a shader constant cannot be imported. The two must move
/// together.
const DEPTH_WEIGHT_FLOOR: f32 = 0.82;
const DEPTH_WEIGHT_CEILING: f32 = 1.22;

/// What [`reference_depth`] answers for a curve that is not drawn at
/// this eye at all.
///
/// A real reference depth is a distance and so never negative, which is
/// what lets one number carry both the value and the verdict — the ink
/// pass reads it out of a single plane and collapses the curve's rails
/// where it is negative.
pub const NOT_DRAWN: f32 = -1.0;

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

/// One point's rail solve with the eye factored out.
///
/// Every field is a function of the curve alone, so a curve that has
/// not changed shape packs the same anchors from every angle — which is
/// what lets the ink pass upload a resident curve's ribbon once and
/// derive its rails per frame in the vertex stage
/// (iamacoffeepot/aether#4440).
///
/// The two scalars are the eye-free halves of their products. `half` is
/// the angular half-width before the distance to the eye scales it into
/// a world one, and `drift` is the wobble's displacement before the
/// same distance does — so the stage that has the eye multiplies each
/// by the depth it measures and nothing else has to travel.
#[derive(Clone, Copy)]
pub struct Anchor {
    /// Where the pen goes, before the wobble displaces it.
    pub pos: Vec3,
    /// The chord across the point — the next point less the previous —
    /// whose cross product with the view gives the offset's direction.
    pub along: Vec3,
    /// Angular half-width at full pressure, per unit of depth.
    pub half: f32,
    /// Hand-wander displacement, per unit of depth.
    pub drift: f32,
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

/// One curve's anchors, or nothing if it carries no segment at all.
///
/// The whole rail solve save the eye, which is exactly the part that
/// can be packed once and left on the GPU. Nothing here is a function
/// of where the viewer stands: the chord across a point is the curve's
/// own, the wobble's argument is world position (a stroke bends the
/// same way from every angle and stays continuous across a weld), and
/// the two scalars are the eye-free halves of their products.
pub fn anchors(curve: &Curve3, jitter: u64, out: &mut Vec<Anchor>) -> bool {
    if curve.points.len() < 2 {
        return false;
    }

    let (weight, amplitude) = (curve.class.base_width(), curve.class.wobble_amplitude());
    // The wobble's phase is per-stroke and its argument is world
    // position, so neither is keyed to the page.
    let seed = curve.seed ^ jitter;
    let last = curve.points.len() - 1;

    out.extend(curve.points.iter().enumerate().map(|(index, point)| Anchor {
        pos: point.pos,
        along: curve.points[(index + 1).min(last)].pos - curve.points[index.saturating_sub(1)].pos,
        half: ANGULAR_HALF_WIDTH * weight * point.weight,
        drift: wander(seed, point.pos) * ANGULAR_WOBBLE * amplitude,
    }));

    true
}

/// The stroke's own average distance to the eye, or [`NOT_DRAWN`] where
/// the curve does not read at this eye at all.
///
/// This is the whole of what the eye decides about a curve rather than
/// about one of its points, and so the whole of what the ink pass has
/// to deliver per frame — one float per curve against a rail buffer's
/// megabytes (iamacoffeepot/aether#4440). Two questions share the walk
/// because both are sums over the curve against the same distances.
///
/// The average is taken per stroke rather than per scene so the depth
/// cue reads as one line turning through depth, not as a global fog.
///
/// The length floor does not reach an authored mark, and that exemption
/// is not a nicety. It is a noise rejector, and the chart draws no
/// noise: a cupid's bow is a fifth of a mouth wide, a lip corner tick is
/// smaller still, and the iris hook arrives as two short arcs because a
/// lid crosses it. Every one of those is under the floor and every one
/// of them was silently missing — the mouth came out as two bare lines
/// and the eyes as blanks, with nothing having errored.
pub fn reference_depth(curve: &Curve3, eye: Vec3) -> f32 {
    if curve.points.len() < 2 {
        return NOT_DRAWN;
    }

    // Arc measured in radians rather than world units, so the length
    // floor means the same thing at any distance.
    let (mut total, mut arc) = (0.0f32, 0.0f32);
    for (index, point) in curve.points.iter().enumerate() {
        let depth = (point.pos - eye).length();
        total += depth;
        if let Some(next) = curve.points.get(index + 1) {
            arc += (next.pos - point.pos).length() / depth.max(1e-4);
        }
    }
    if !curve.authored && arc < curve.class.min_length() / PAGE_PIXELS_PER_RADIAN {
        return NOT_DRAWN;
    }

    total / curve.points.len() as f32
}

/// One point's rail pair against an eye: where its centre lands once
/// the wobble displaces it, and the offset reaching the right rail at
/// full pressure.
///
/// The one place the eye meets the solve, and so the one place the ink
/// pass' vertex stage has to reproduce — line for line, since a
/// disagreement here is a drawing of a different width.
#[must_use]
pub fn rail(anchor: &Anchor, reference: f32, eye: Vec3) -> (Vec3, Vec3) {
    let to_eye = eye - anchor.pos;
    let depth = to_eye.length().max(1e-4);
    // Perpendicular to the stroke and to the view at once, which is
    // what keeps a line from vanishing when it turns edge-on.
    let across = anchor.along.cross(to_eye);
    let across = if across.length() < 1e-9 {
        Vec3::new(0.0, 0.0, 0.0)
    } else {
        across.normalize()
    };

    // Nearer stroke points are bolder — the one cue that keeps a flat
    // line drawing from reading as a decal on glass. The angular
    // half-width holds screen width constant through depth, so this
    // rides on top of it rather than being cancelled by it.
    let depth_weight = (reference / depth).clamp(DEPTH_WEIGHT_FLOOR, DEPTH_WEIGHT_CEILING);

    (anchor.pos + across * (anchor.drift * depth), across * (anchor.half * depth * depth_weight))
}

/// One curve's rail pairs, or nothing if it is too short to read once
/// projected.
///
/// The three pieces above put together, and after ADR-0172 the parity
/// oracle rather than the per-frame producer: the frame's ink derives
/// its rails from the same three in the ink pass' vertex stage. The
/// taper is the one part not shared — it is solved here against the arc
/// of the curve this was handed, so a caller that split the curve into
/// visible runs first gets ends anchored on those runs, while the ink
/// pass reads its taper out of the visibility field instead.
pub fn rails(curve: &Curve3, eye: Vec3, jitter: u64, out: &mut Vec<Rail>) -> bool {
    let reference = reference_depth(curve, eye);
    let mut anchored = Vec::with_capacity(curve.points.len());
    if reference < 0.0 || !anchors(curve, jitter, &mut anchored) {
        return false;
    }

    // The taper's own parameter, walked a second time because it is a
    // per-point prefix rather than the curve's total — and because this
    // is the oracle's path, where a second walk costs nothing that the
    // frame pays for.
    let angular: Vec<f32> = curve
        .points
        .windows(2)
        .scan(0.0f32, |total, pair| {
            *total += (pair[1].pos - pair[0].pos).length() / (pair[0].pos - eye).length().max(1e-4);
            Some(*total)
        })
        .collect();
    let length = angular.last().copied().unwrap_or(0.0);

    out.extend(anchored.iter().enumerate().map(|(index, anchor)| {
        let (centre, offset) = rail(anchor, reference, eye);

        Rail { centre, offset, taper: pressure(angular.get(index.saturating_sub(1)).copied().unwrap_or(0.0), length) }
    }));

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
