//! Which way the strokes run, and which of them a view draws.
//!
//! A hatch family is the level sets of `position . axis`, so its strokes
//! run along `axis x normal` — across the axis on the surface, turning
//! with the surface as they go — and reach the page through the eye. That
//! makes the drawing's stroke directions a property of the axes, of the
//! subject, and of where the viewer stands, and it is a property that can
//! fail: two families can rule the same screen direction, and a family
//! whose axis stands along the surface's own turn cuts it in closed rings
//! with no direction at all. Either way the cross-hatch stops crossing and
//! the subject reads as a wireframe.
//!
//! Three axes fixed in the world cannot avoid this on a turntable. The
//! views where a pair collapses form a great circle per pair, and an
//! orbit misses all three only if every pair's cross product lies near
//! the orbit axis — which forces all three axes near-horizontal, and
//! near-horizontal axes project to nearly the same screen angle anyway
//! (iamacoffeepot/aether#5877).
//!
//! So the subject carries [`AXES`] of them instead, spread over the
//! sphere rather than laid out in one plane, and each view draws the
//! three that cross best from where it stands. The level sets are still
//! extracted once at load — a resident axis is resident geometry — and
//! the choice is a per-view verdict in the tone gate, which is the same
//! place the tone ramp already decides what a point draws.

use aether_math::Vec3;

use crate::extract::Settings;
use crate::math3::camera_frame;

/// How many axes the subject carries level sets for.
///
/// Six, as three pairs: one pair per rank of the tone ramp, so whichever
/// of a pair a view picks, the family it draws was extracted at that
/// rank's own spacing. That is what keeps the ramp's shape a constant of
/// the style rather than of the azimuth — every view draws one tight
/// family, one middle and one sparse, and only their directions change.
pub const AXES: usize = 6;

/// How many families one view draws — the ranks of the tone ramp.
pub const RANKS: usize = 3;

/// The golden ratio, which is what puts the six axes on an icosahedron.
const PHI: f32 = 1.618_034;

/// How many of the subject's own normals the choice weighs its answer
/// over.
///
/// A sample rather than the whole surface, because the answer is a mean
/// over an area and a few hundred points estimate that as well as a
/// hundred thousand do — and this runs every frame the eye moves.
const SURVEYED: usize = 512;

/// How much better a challenger has to score before a view changes which
/// axes it draws.
///
/// Two triples trade places smoothly as the eye turns, and near the
/// crossing they are worth the same to a rounding error — so without a
/// margin a camera resting near one would swap the drawing's stroke
/// directions back and forth every frame. The margin is in the units
/// [`separation`] reports, and small against the spread those take over a
/// turn: swept through the demo's own turntable it changes no verdict,
/// which is what it is for — it holds a choice at a crossing, not against
/// a turn.
const HYSTERESIS: f32 = 0.015;

/// The axes before [`crate::extract::Settings::hatch_tilt`] turns them.
///
/// The six axes of an icosahedron — the most evenly spread six directions
/// there are, once opposite directions are counted as one plane normal:
/// no two are closer than 63 degrees.
///
/// The pairing is the load-bearing part, and it is about the *vertical*.
/// A subject on a turntable is very often a body of revolution about the
/// up axis, and a hatch axis near that axis cuts it in horizontal rings —
/// contour banding, from every azimuth, which is precisely the reading
/// this module exists to avoid. Two of the six sit close to the vertical,
/// and pairing those two together would leave one rank with no answer but
/// a ring. So they are split across two ranks and each is paired with one
/// of the two axes that lie flat in the horizontal plane, leaving every
/// rank an upright option and a level one to choose between.
fn resident() -> [Vec3; AXES] {
    [
        Vec3::new(0.0, 1.0, PHI),
        Vec3::new(0.0, -1.0, PHI),
        Vec3::new(1.0, PHI, 0.0),
        Vec3::new(PHI, 0.0, 1.0),
        Vec3::new(-1.0, PHI, 0.0),
        Vec3::new(PHI, 0.0, -1.0),
    ]
    .map(Vec3::normalize)
}

/// The resident axes, turned about the model's up axis by `tilt`.
///
/// About the vertical rather than about `z`, which is where the knob
/// pointed while there were three axes in one plane to be the angle of.
/// A turn about the vertical is a phase in the turntable's own turn: it
/// slides where on the subject the strokes fall without changing how any
/// axis stands against the up axis, so no tilt can tip an axis into the
/// vertical and turn its family into rings. A turn about anything else
/// can, which makes the knob a way to break the drawing rather than to
/// place it.
#[must_use]
pub fn axes(tilt: f32) -> [Vec3; AXES] {
    let (sin, cos) = tilt.sin_cos();

    resident().map(|axis| Vec3::new(cos * axis.x + sin * axis.z, axis.y, cos * axis.z - sin * axis.x))
}

/// Which rank of the tone ramp an axis draws at — its spacing multiplier,
/// its threshold, and its stroke weight all key off this.
#[must_use]
pub const fn rank(axis: u8) -> usize {
    axis as usize / 2
}

/// Every way a view could pick one axis per rank.
///
/// Eight of them, which is small enough to score whole rather than
/// searched: the low three bits of a counter say which of each rank's
/// pair the triple takes.
fn choices() -> impl Iterator<Item = [u8; RANKS]> {
    (0..8u8).map(|bits| [bits & 1, 2 + ((bits >> 1) & 1), 4 + ((bits >> 2) & 1)])
}

/// Where a family's strokes run on the page at one point of the surface.
///
/// Along `axis x normal` — across the axis *on the surface*, which is not
/// the same as across the axis' own screen projection. On anything curved
/// that direction turns with the surface, and two axes whose projections
/// sit well apart can still rule parallel strokes over a whole band of
/// normals. That band is most of a body of revolution seen side on, which
/// is exactly where the banding shows — so the question is asked of the
/// surface the subject actually has rather than of the axes alone.
fn along(axis: Vec3, normal: Vec3, right: Vec3, up: Vec3) -> (f32, f32) {
    let running = axis.cross(normal);

    (running.dot(right), running.dot(up))
}

/// How well a triple's families cross over the surface this eye sees.
///
/// Per pair and per surveyed normal, the magnitude of their stroke
/// directions' cross product — `|a||b| sin t`. One number for the three
/// ways a pair fails: it falls to zero when the two directions line up,
/// which is the same stroke drawn twice, and when either family's level
/// set degenerates there, which is a ring carrying no direction to cross
/// with. A triple scores at a point as its worst pair, because a
/// cross-hatch is only as good as the pair it crosses worst; over the
/// survey it scores as the mean weighted by how much page each point
/// stands for, because the drawing is the whole visible surface and not
/// one patch of it.
fn separation(surveyed: &[(Vec3, f32)], axes: &[Vec3; AXES], view: (Vec3, Vec3), triple: [u8; RANKS]) -> f32 {
    let (right, up) = view;
    let cross = |a: (f32, f32), b: (f32, f32)| (a.0 * b.1 - a.1 * b.0).abs();

    let (mut crossing, mut page) = (0.0, 0.0);
    for (normal, share) in surveyed {
        let running = triple.map(|axis| along(axes[usize::from(axis)], *normal, right, up));
        let worst = cross(running[0], running[1]).min(cross(running[0], running[2])).min(cross(running[1], running[2]));

        crossing += worst * share;
        page += share;
    }

    if page <= 0.0 {
        return 0.0;
    }

    crossing / page
}

/// Which three axes the drawing is currently hatching with.
///
/// Held across views rather than recomputed clean, because the answer is
/// a maximum over a small set and a maximum can change under a rounding
/// error. See [`HYSTERESIS`].
#[derive(Clone, Debug)]
pub struct Choice {
    chosen: [u8; RANKS],
    /// A stride through the subject's own vertex normals — the surface the
    /// verdict is taken over. Empty until a subject lands, and the choice
    /// then stands where it is: with no surface to score against there is
    /// nothing to prefer.
    surveyed: Vec<Vec3>,
}

impl Default for Choice {
    /// One axis per rank, before any subject or eye has had an opinion.
    fn default() -> Self {
        Self { chosen: [0, 2, 4], surveyed: Vec::new() }
    }
}

impl Choice {
    /// Take the surface the choice is scored over off a new subject.
    pub fn subject_changed(&mut self, normals: &[Vec3]) {
        let stride = normals.len().div_ceil(SURVEYED).max(1);

        self.surveyed = normals.iter().step_by(stride).copied().collect();
    }

    /// The three axes this view draws, keeping the standing choice unless
    /// a challenger is clearly better from here.
    ///
    /// `settings` is the style this view shades with —
    /// [`Settings::resolved`]'s, whose key light is already a world
    /// direction — because where the drawing hatches is where the choice
    /// has to be right.
    pub fn choose(&mut self, settings: &Settings, eye: Vec3, target: Vec3) -> [u8; RANKS] {
        let (right, up, back) = camera_frame(eye, target);
        let turned = axes(settings.hatch_tilt);
        // How much page each surveyed point stands for: nothing where it
        // faces away, and foreshortened by how far it has turned. The
        // subject is opaque, so the half of it that is not facing the eye
        // carries no strokes to cross.
        //
        // Not weighted toward the dark passages where all three families
        // draw, though that is where a collapse shows: measured on the
        // demo's own sweep, weighting by the ramp reads a narrow band and
        // chooses for it at the expense of the rest of the figure, and the
        // whole turn comes out worse.
        let surveyed: Vec<(Vec3, f32)> =
            self.surveyed.iter().map(|normal| (*normal, normal.dot(back).max(0.0))).collect();

        // Seeded with the standing choice's score plus the margin, so a
        // challenger has to clear the margin to enter the running at all
        // and then only has to be the best of those that did.
        let standing = separation(&surveyed, &turned, (right, up), self.chosen) + HYSTERESIS;
        self.chosen = choices()
            .map(|triple| (separation(&surveyed, &turned, (right, up), triple), triple))
            .fold((standing, self.chosen), |held, challenger| {
                if challenger.0 > held.0 {
                    challenger
                } else {
                    held
                }
            })
            .1;

        self.chosen
    }
}
