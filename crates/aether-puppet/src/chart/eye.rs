//! The eye: the mark that carries the face, and the one the sculpt could
//! never supply.
//!
//! What makes a drawn eye read is the dark mass, and a dark mass has to be
//! authored — there is no relief under a painted pupil for the crease pass
//! to find. So the design is held as numbers rather than as six copies of
//! one function: an archetype is a row of constants, and a new one costs a
//! line.

use aether_math::Vec2;

use crate::anchor::Anchor;
use crate::feature::{FeatureClass, Pen};

use super::{Mark, STANDOFF_EYE};

use core::f32::consts::{FRAC_PI_2, TAU};
use core::mem;

/// What separates one eye design from another.
///
/// The drawn eye is almost entirely convention, so nearly every one of
/// these is a choice with a name attached in the tradition: which way the
/// outer corner runs, how flat the lid sits over the iris, whether the
/// pupil is a circle or a slit.
#[derive(Clone, Copy, Debug)]
pub struct Style {
    /// Overall aperture, as multiples of the measured half-extents. The
    /// label class is about twice as wide as it is tall, a life-drawing
    /// eye; where a design wants a rounder one, it buys the height here.
    pub scale: Vec2,
    /// How far the outer corner rides above the inner, in half-heights.
    /// Positive is the upswept eye, which is the fox reading.
    pub sweep: f32,
    /// Where along inner-to-outer the upper lid peaks. Under a half is a
    /// lid heavy at the inner corner; over, heavy at the outer.
    pub peak: f32,
    /// Fullness of the upper arc. Low is a flat almond, high a dome.
    pub dome: f32,
    /// How far the lower lid drops, in half-heights.
    pub floor: f32,
    /// Iris radius as a fraction of the half-height. Zero draws no iris at
    /// all, which also means that design cannot hold a gaze.
    pub iris: f32,
    /// Where the iris outline starts and stops, in turns counterclockwise
    /// from her outer side.
    ///
    /// Not a full circle. A closed ring around a closed pupil reads as a
    /// target rather than an eye — concentric is the one thing an iris must
    /// not be — so the outline is a hook, heavy where the lid shadows it
    /// and simply absent where the light is.
    pub arc: (f32, f32),
    /// Pupil size as a fraction of the iris, width then height. A tall
    /// narrow one is the vertical slit an animal has.
    pub pupil: Vec2,
    /// How much of the iris outline is left open at top and bottom, in
    /// radians, so the lid reads as passing in front of it.
    pub open: f32,
    /// Overlapping passes on the upper lid. One is a drawn line; three is a
    /// sketched bundle that never quite commits to an edge.
    pub passes: usize,
    /// Lash flick past the outer corner, in half-heights.
    pub lash: f32,
    /// How heavy the upper lid runs at its thickest, over the class base.
    ///
    /// The lid is the one line on the face that is allowed to be fat. In
    /// the drawn grammar it is not an edge between two surfaces, it is the
    /// lash mass seen end-on, and mass is what it has to look like — a lid
    /// at outline weight reads as a wire.
    pub weight: f32,
    /// Lash spikes rising off the outer half of the upper lid, and how far
    /// each reaches in half-heights. These are drawn lashes rather than
    /// anatomical ones: a real lid grows them all the way along and evenly,
    /// and the convention keeps three or four on the outer half because
    /// that is what says which way the eye is pointing.
    pub wings: usize,
    pub wing: f32,
    /// Lower lid as separate dashes rather than one stroke.
    pub broken: bool,
}

/// The archetypes, in the order they are worth looking at. The first is the
/// default, so the house design is the one a subject wears unasked.
pub const ARCHETYPES: [(&str, Style); 7] = [
    // The house design, and the one worth defending: the sketched lid over
    // the fox's aperture.
    //
    // The bundle is what makes the drawing look drawn. One clean arc reads
    // as vector art no matter how good the arc is, because a hand does not
    // find an edge in a single pass — three overlapping ones read as a hand
    // deciding, and that is the whole register this renderer is aiming at.
    // Under it the narrow upswept opening and the tall pupil say fox before
    // any other mark on the page does, which is work the ears should not
    // have to do alone.
    (
        "kitsune",
        Style {
            scale: Vec2::new(1.20, 1.26),
            sweep: 0.46,
            peak: 0.64,
            dome: 0.52,
            floor: 0.58,
            iris: 0.76,
            arc: (0.08, 0.78),
            pupil: Vec2::new(0.26, 0.90),
            open: 0.34,
            passes: 3,
            lash: 1.05,
            weight: 1.95,
            wings: 3,
            wing: 0.62,
            broken: true,
        },
    ),
    // Upswept and narrow, with a tall pupil. The one that argues she is a
    // fox before anything else on the page does.
    (
        "vulpine",
        Style {
            scale: Vec2::new(1.18, 1.30),
            sweep: 0.52,
            peak: 0.66,
            dome: 0.50,
            floor: 0.60,
            iris: 0.74,
            arc: (0.06, 0.94),
            pupil: Vec2::new(0.26, 0.92),
            open: 0.42,
            passes: 1,
            lash: 0.95,
            weight: 1.55,
            wings: 1,
            wing: 0.50,
            broken: false,
        },
    ),
    // The reference: nothing committed to, everything implied. The lid is a
    // bundle of passes rather than an edge and the lower one is dashes, so
    // the eye is assembled by the reader.
    (
        "sketch",
        Style {
            scale: Vec2::new(1.15, 1.20),
            sweep: 0.16,
            peak: 0.55,
            dome: 0.55,
            floor: 0.66,
            iris: 0.60,
            arc: (0.10, 0.76),
            pupil: Vec2::new(0.44, 0.26),
            open: 0.58,
            passes: 3,
            lash: 0.50,
            weight: 1.20,
            wings: 2,
            wing: 0.44,
            broken: true,
        },
    ),
    // Half-lidded under a flat top. Gives away nothing, which under a level
    // brow reads as bored and under a raised one as unimpressed.
    (
        "cool",
        Style {
            scale: Vec2::new(1.22, 1.05),
            sweep: 0.34,
            peak: 0.60,
            dome: 0.26,
            floor: 0.46,
            iris: 0.86,
            arc: (0.04, 0.80),
            pupil: Vec2::new(0.34, 0.40),
            open: 0.30,
            passes: 2,
            lash: 0.72,
            weight: 1.75,
            wings: 0,
            wing: 0.00,
            broken: true,
        },
    ),
    // Downturned outer corner, full lower lid. The approachable one.
    (
        "soft",
        Style {
            scale: Vec2::new(1.10, 1.45),
            sweep: -0.36,
            peak: 0.42,
            dome: 0.76,
            floor: 0.88,
            iris: 0.84,
            arc: (0.08, 0.86),
            pupil: Vec2::new(0.32, 0.34),
            open: 0.34,
            passes: 1,
            lash: 0.34,
            weight: 1.05,
            wings: 3,
            wing: 0.52,
            broken: true,
        },
    ),
    // Round and open, the iris nearly filling it.
    (
        "wide",
        Style {
            scale: Vec2::new(1.06, 1.70),
            sweep: 0.0,
            peak: 0.50,
            dome: 0.94,
            floor: 1.02,
            iris: 0.88,
            arc: (0.02, 0.96),
            pupil: Vec2::new(0.34, 0.38),
            open: 0.24,
            passes: 1,
            lash: 0.38,
            weight: 1.15,
            wings: 0,
            wing: 0.00,
            broken: false,
        },
    ),
    // Lid and flick, nothing inside. The most graphic of the set and the
    // only one that cannot look anywhere, since there is no pupil to move —
    // worth keeping precisely because it names that cost.
    (
        "mask",
        Style {
            scale: Vec2::new(1.24, 1.10),
            sweep: 0.48,
            peak: 0.62,
            dome: 0.44,
            floor: 0.50,
            iris: 0.0,
            arc: (0.0, 1.0),
            pupil: Vec2::new(0.0, 0.0),
            open: 0.0,
            passes: 1,
            lash: 1.25,
            weight: 2.20,
            wings: 4,
            wing: 0.78,
            broken: false,
        },
    ),
];

pub fn style(name: &str) -> Option<Style> {
    ARCHETYPES.iter().find(|(known, _)| *known == name).map(|&(_, style)| style)
}

impl Default for Style {
    fn default() -> Self {
        ARCHETYPES[0].1
    }
}

/// Openness under which the aperture has no room left for an iris, and the
/// eye collapses to the one bowed arc that reads as a blink.
const SHUT: f32 = 0.12;

/// The pair of curves the aperture is bounded by.
///
/// One place rather than two. The ink and any paint clipping to it have to
/// agree exactly on where the lids run — a paint layer clipped to the label
/// volume instead of to these curves crops short of the ink, and the two
/// visibly disagree.
struct Lids {
    centre: Vec2,
    half_width: f32,
    half_height: f32,
    side: f32,
    style: Style,
    tilt: f32,
    gaze: Vec2,
}

/// How hard each lid follows the gaze, in half-heights, upper then lower.
///
/// Without this, looking down is nearly invisible: the iris fills the
/// aperture vertically so it has almost no room to travel, and the lash
/// line is heavy enough to hide the little it has. A real lid comes down
/// with the eye, and moving the aperture reads at a glance where moving the
/// pupil inside a fixed one does not.
const FOLLOW: (f32, f32) = (0.45, 0.15);

impl Lids {
    fn new(anchor: &Anchor, style: Style, [openness, _lash, tilt]: [f32; 3], gaze: Vec2, side: f32) -> Self {
        Self {
            centre: anchor.centre,
            half_width: anchor.half.x * style.scale.x,
            half_height: anchor.half.y * style.scale.y * openness.max(0.0),
            side,
            style,
            tilt,
            gaze,
        }
    }

    /// `u` runs inner corner to outer, so sweep, tilt, taper and lash all
    /// key off the same end regardless of which side of the face this is.
    fn at(&self, u: f32) -> f32 {
        self.centre.x + self.side * self.half_width * (-1.0 + 2.0 * u)
    }

    /// Sweep tips the whole aperture; tilt drops the inner corner only, and
    /// is expression rather than design — the eye's half of an angry face.
    fn rake(&self, u: f32) -> f32 {
        self.half_height * (self.style.sweep * (u - 0.5) - self.tilt * 0.85 * (1.0 - u))
    }

    /// `peak` moves the maximum along the lid by reparameterising `u`,
    /// which keeps both corners pinned where they are.
    fn upper(&self, u: f32) -> f32 {
        let t = 2.0 * u.powf(0.5 / self.style.peak.clamp(0.15, 0.85)) - 1.0;
        let arc = (1.0 - t * t).max(0.0).powf(1.0 - self.style.dome * 0.62);

        self.centre.y + self.half_height * arc + self.rake(u) + self.gaze.y * self.half_height * FOLLOW.0
    }

    fn lower(&self, u: f32) -> f32 {
        let t = 2.0 * u - 1.0;
        let arc = (1.0 - t * t).max(0.0).powf(0.75);

        self.centre.y - self.half_height * self.style.floor * arc
            + self.rake(u)
            + self.gaze.y * self.half_height * FOLLOW.1
    }

    /// The aperture as a function of world `x`, which is what the iris ring
    /// needs: it has to know where each column is cut off.
    fn aperture(&self, x: f32) -> Option<(f32, f32)> {
        let u = (((x - self.centre.x) / (self.side * self.half_width)) + 1.0) * 0.5;

        (u > 0.02 && u < 0.98).then(|| (self.lower(u), self.upper(u)))
    }

    /// The aperture as a closed loop, sampled inner-to-outer along the
    /// upper lid and back along the lower.
    fn outline(&self, samples: usize) -> Vec<Vec2> {
        let u = |i: usize| i as f32 / (samples - 1) as f32;
        let mut outline: Vec<Vec2> = (0..samples).map(|i| Vec2::new(self.at(u(i)), self.upper(u(i)))).collect();
        outline.extend((0..samples).rev().map(|i| Vec2::new(self.at(u(i)), self.lower(u(i)))));

        outline
    }
}

/// Where the iris sits inside the aperture once the gaze has moved it.
///
/// One place rather than two, for the same reason the lid pair is one
/// place:
/// the ink draws its ring on this ellipse and a painted iris fills the same
/// one, so a second copy of the reach formula would let the two drift apart
/// the moment either changed.
pub struct Iris {
    pub centre: Vec2,
    pub radius: f32,
    /// Pupil half-axes as fractions of `radius`, width then height.
    pub pupil: Vec2,
}

impl Lids {
    /// The iris this aperture holds, or `None` for a design that draws
    /// none — which also means that design cannot hold a gaze.
    fn iris(&self) -> Option<Iris> {
        if self.style.iris <= 0.0 {
            return None;
        }

        let radius = self.half_height * self.style.iris;
        let reach = Vec2::new((self.half_width - radius * 1.02).max(0.0) * 0.72, self.half_height * 0.52);

        Some(Iris {
            centre: Vec2::new(self.centre.x + self.gaze.x * reach.x, self.centre.y + self.gaze.y * reach.y),
            radius,
            pupil: self.style.pupil,
        })
    }
}

/// The iris the ink draws, for paint that has to land inside it.
///
/// `None` for a design with no iris and for an eye shut far enough that
/// the ink has collapsed to the blink arc, where there is nothing left to
/// paint inside.
pub fn iris_frame(anchor: &Anchor, style: Style, shape: [f32; 3], gaze: Vec2, side: f32) -> Option<Iris> {
    (shape[0] >= SHUT).then(|| Lids::new(anchor, style, shape, gaze, side).iris()).flatten()
}

/// The aperture the drawn lids enclose — the clip a painted iris needs.
pub fn aperture_outline(
    anchor: &Anchor,
    style: Style,
    shape: [f32; 3],
    gaze: Vec2,
    side: f32,
    samples: usize,
) -> Vec<Vec2> {
    Lids::new(anchor, style, shape, gaze, side).outline(samples)
}

/// One eye. `side` is `-1` for her right, `+1` for her left.
///
/// `shape` is `(openness, lash, tilt)` — how far the aperture opens, how
/// far the lash flicks past the outer corner, and how much the inner end
/// drops. `gaze` is in her own frame, `+x` toward her left.
pub fn draw(anchor: &Anchor, style: Style, shape: [f32; 3], gaze: Vec2, side: f32) -> Vec<Mark> {
    let lids = Lids::new(anchor, style, shape, gaze, side);
    let [openness, lash, _tilt] = shape;

    // Shut. One arc bowed upward, which is the drawn convention for both a
    // blink and the happy squint, and is what the aperture collapses to
    // anyway once there is no room left to put an iris in.
    if openness < SHUT {
        return vec![blink(anchor, &lids)];
    }

    let mut marks = iris(&lids);
    marks.extend(lower_lid(&lids));
    marks.extend(upper_lid(&lids, lash));
    marks.extend(wings(&lids));

    marks
}

fn blink(anchor: &Anchor, lids: &Lids) -> Mark {
    let arc: Vec<Vec2> = (0..40)
        .map(|i| {
            let u = i as f32 / 39.0;
            let bow = anchor.half.y * 0.9 * (1.0 - (2.0 * u - 1.0).powi(2));

            Vec2::new(lids.at(u), lids.centre.y + bow)
        })
        .collect();

    Mark {
        weights: vec![1.3; arc.len()],
        points: arc,
        pen: Pen::Ink,
        class: FeatureClass::Silhouette,
        standoff: STANDOFF_EYE,
    }
}

/// Outline, not mass, and one grey rather than a palette.
///
/// A filled iris is a block of tone in a drawing that has been line the
/// whole way up to here, and it drags the page toward a different medium;
/// the open crescent says the same thing with the pen already in use.
/// Colour comes back later, over this, not instead of it.
fn iris(lids: &Lids) -> Vec<Mark> {
    let Some(iris) = lids.iris() else {
        return Vec::new();
    };
    let pupil = iris.pupil * iris.radius;

    let mut marks = ring(lids, iris.centre, Vec2::splat(iris.radius), lids.style.arc, lids.style.open, 1.0);
    marks.extend(ring(lids, iris.centre, pupil, (0.0, 1.0), 0.0, 0.85));

    marks
}

/// Closed, an eye reads as a hole cut in a mask — the same failure the
/// source spike names for the mouth — and an unbroken lower line reads as
/// makeup besides, so the lower lid stops short of both corners either way.
fn lower_lid(lids: &Lids) -> Vec<Mark> {
    let dashes: &[(f32, f32)] = if lids.style.broken {
        &[(0.20, 0.42), (0.50, 0.66), (0.74, 0.86)]
    } else {
        &[(0.18, 0.88)]
    };

    dashes
        .iter()
        .map(|&(from, to)| {
            let (points, weights) = (0..12)
                .map(|i| {
                    let u = from + (to - from) * i as f32 / 11.0;
                    (Vec2::new(lids.at(u), lids.lower(u)), 0.45)
                })
                .unzip();

            Mark { points, weights, pen: Pen::Pale, class: FeatureClass::Decal, standoff: STANDOFF_EYE }
        })
        .collect()
}

/// Thickest over the outer third and thinning hard toward the inner corner.
///
/// Not a taper from one end to the other: a lash line is a mass that swells
/// and then comes to a point, so the heavy part sits where the lashes are
/// longest and both ends run out of it.
fn swell(lids: &Lids, u: f32) -> f32 {
    let off = ((u - 0.68) / 0.66).clamp(-1.0, 1.0);

    0.34 + lids.style.weight * (1.0 - off * off).powf(0.7)
}

/// The upper lid carries the lash line with it, so it is the heaviest
/// stroke on the face — silhouette weight, not decal. Everything else here
/// is drawn to sit under it. More than one pass and it stops being an edge
/// and becomes a search for one, which is the sketched reading.
fn upper_lid(lids: &Lids, lash: f32) -> Vec<Mark> {
    let passes = lids.style.passes.max(1);
    let flick = lids.half_height * lids.style.lash * lash;

    (0..passes)
        .map(|pass| {
            let spread = pass as f32 - (passes as f32 - 1.0) * 0.5;
            let drift = spread * lids.half_height * 0.11;
            let (from, to) = (0.04 * pass as f32, 1.0 - 0.07 * pass as f32);
            // Only the leading pass carries the mass. The others are the
            // hand searching for the edge, and a bundle of three fat lines
            // is a smear rather than a sketch.
            let share = if pass == 0 {
                1.0
            } else {
                0.30
            };

            let (mut points, mut weights): (Vec<Vec2>, Vec<f32>) = (0..44)
                .map(|i| {
                    let u = from + (to - from) * i as f32 / 43.0;
                    (Vec2::new(lids.at(u), lids.upper(u) + drift), swell(lids, u) * share)
                })
                .unzip();

            // Past the outer corner the lash keeps going and lifts, coming
            // to a point. That flick is the single most recognisable thing
            // about a drawn eye, and it is pure convention — no lash on a
            // real lid does this.
            if pass == 0 {
                for i in 1..10 {
                    let t = i as f32 / 9.0;
                    points.push(Vec2::new(lids.at(1.0 + 0.32 * t), lids.upper(1.0) + flick * t * (2.0 - t)));
                    weights.push(swell(lids, 1.0) * (1.0 - t).powf(0.85));
                }
            }

            Mark { points, weights, pen: Pen::Ink, class: FeatureClass::Silhouette, standoff: STANDOFF_EYE }
        })
        .collect()
}

/// Lash spikes off the outer half, each a hook rather than a spine: it
/// leaves the lid heading up and out, then bends back over as it thins. A
/// straight spike reads as a comb.
fn wings(lids: &Lids) -> Vec<Mark> {
    (0..lids.style.wings)
        .map(|k| {
            let u = 0.46 + 0.17 * k as f32;
            let root = Vec2::new(lids.at(u), lids.upper(u));
            let reach = lids.half_height * lids.style.wing * (1.0 - 0.16 * k as f32);
            let lean = Vec2::new(lids.side * (0.40 + 0.18 * k as f32), 1.0);

            let (points, weights) = (0..10)
                .map(|i| {
                    let t = i as f32 / 9.0;
                    let bend = Vec2::new(lids.side * 0.50 * t * t, -0.30 * t * t);
                    (root + (lean + bend) * reach * t, swell(lids, u) * 0.85 * (1.0 - t).powf(0.6))
                })
                .unzip();

            Mark { points, weights, pen: Pen::Ink, class: FeatureClass::Silhouette, standoff: STANDOFF_EYE }
        })
        .collect()
}

/// An ellipse outline, broken wherever a lid crosses it and deliberately
/// left open at its own top and bottom.
///
/// `gap` is how much of the arc, in radians either side of vertical, the
/// pen skips. A closed circle sitting inside a lid reads as a drawn ball;
/// the same circle with its poles missing reads as an iris the lid is
/// passing in front of, which is the whole difference.
fn ring(lids: &Lids, centre: Vec2, radii: Vec2, span: (f32, f32), gap: f32, weight: f32) -> Vec<Mark> {
    let mut marks = Vec::new();
    let mut run: Vec<Vec2> = Vec::new();
    let flush = |run: &mut Vec<Vec2>, marks: &mut Vec<Mark>| {
        if run.len() >= 2 {
            marks.push(Mark {
                weights: vec![weight; run.len()],
                points: mem::take(run),
                pen: Pen::Ink,
                class: FeatureClass::Decal,
                standoff: STANDOFF_EYE,
            });
        }
        run.clear();
    };

    for i in 0..=96 {
        let angle = TAU * (span.0 + (span.1 - span.0) * i as f32 / 96.0);
        let (sin, cos) = angle.sin_cos();
        let point = Vec2::new(centre.x + radii.x * cos, centre.y + radii.y * sin);
        let polar = (angle - FRAC_PI_2).abs().min((angle - 3.0 * FRAC_PI_2).abs()) < gap;
        let inside = lids.aperture(point.x).is_some_and(|(floor, ceiling)| point.y > floor && point.y < ceiling);

        if polar || !inside {
            flush(&mut run, &mut marks);
        } else {
            run.push(point);
        }
    }

    flush(&mut run, &mut marks);
    marks
}
