//! The authored half of the face: eye, brow, mouth and nose as marks.
//!
//! The rule the source spike settled on is that a feature belongs to the
//! geometry when it obeys occlusion and lighting, and to the chart when it
//! violates either. The mouth was ruled chart-side because it violates
//! lighting — the field crosses two surfaces through it, so there is
//! nothing coherent to shade. A line renderer never shades anything, so
//! only the occlusion half of that test still applies, and these marks are
//! planted on the face and run through the same visibility pass as every
//! other stroke. Turn her head and the mouth is occluded like anything
//! else.
//!
//! The eye arrives at the chart from both ends. A pupil is painted texture
//! over a smooth ball, so there is no relief under it and the crease pass
//! finds a lid rim and nothing else — and a lid rim alone is two thin arcs
//! that stop reading the moment she turns. Gaze settles it from the other
//! side: a pupil has to move against the skull, and no bone can move it
//! since there is no eyeball to weight.
//!
//! Each submodule is one feature, authored in chart space — the model's
//! frontal plane, in model units, around an [`Anchor`] someone else
//! measured. None of them casts a ray or knows what a mesh is: shape is all
//! they decide. Assembling the face and dropping it on the surface is this
//! module's job, and `plant` does the dropping.

pub mod brow;
pub mod eye;
pub mod mouth;
pub mod nose;

use aether_math::{Vec2, Vec3};

use crate::anchor::{Anchor, Anchors};
use crate::deform::Rigid;
use crate::extract::Settings;
use crate::feature::{Curve3, FeatureClass, Pen};
use crate::mesh::Mesh;
use crate::plant::{self, INTO};

pub use eye::Style;
pub use nose::Nose;

/// How far each mark stands proud of the surface it is planted on.
///
/// Not one number for all of them. A mark sits on a plane fitted to the
/// surface under it, and its standoff has to clear whatever that fit threw
/// away. For a brow that is the fringe — thick, and the thing the brow is
/// meant to read straight through, which is how the convention draws a brow
/// over hair regardless. An eye has nothing to clear but its own socket,
/// and standoff costs parallax there: a plate floating far in front of a
/// socket slides across it as she turns, and the eye is small enough that
/// the slide is measured in eye-widths.
pub const STANDOFF_BROW: f32 = 0.045;
pub const STANDOFF_MOUTH: f32 = 0.018;
pub const STANDOFF_EYE: f32 = 0.012;
pub const STANDOFF_NOSE: f32 = 0.014;

/// A mark in chart space, ready to be dropped onto the face.
pub struct Mark {
    pub points: Vec<Vec2>,
    /// Per-point width multiplier; `1.0` is the class's base weight. The
    /// taper is half of what makes a mark read as drawn rather than
    /// plotted — a lip line that does not thin at its corners and a brow
    /// that does not thin outward are both wire.
    pub weights: Vec<f32>,
    pub pen: Pen,
    pub class: FeatureClass,
    /// How far in front of the fitted surface plane this mark sits.
    pub standoff: f32,
}

/// A whole face: what the mouth does, what the brows do, what the eyes do,
/// and where she is looking.
///
/// Brows are the other half of every expression — a smile under angry brows
/// is a threat, and the mouth alone cannot say which. `brow` is `(raise,
/// tilt, arch, skew)`: lift off the eye, inner-end angle, how curved, and
/// how much the two disagree. `eye` is `(openness, lash, tilt)`, and `gaze`
/// is a direction both eyes share so they converge rather than wandering
/// independently.
///
/// This is the state a control surface addresses
/// (iamacoffeepot/aether#4338). Until then every subject wears [`REST`].
///
/// [`REST`]: Face::REST
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Face {
    pub mouth: [f32; 5],
    pub brow: [f32; 4],
    pub eye: [f32; 3],
    pub gaze: Vec2,
}

impl Face {
    /// Level brows, open eyes, a closed mouth, looking straight out.
    pub const REST: Self =
        Self { mouth: mouth::REST, brow: [0.00, 0.00, 0.55, 0.00], eye: [1.00, 1.00, 0.00], gaze: Vec2::ZERO };
}

impl Default for Face {
    fn default() -> Self {
        Self::REST
    }
}

/// The named expressions, as `(name, viseme, brow, eye)`.
///
/// Eyes carry expression at least as hard as brows do, and they carry a
/// different part of it: the brow says what she thinks, the aperture says
/// how much she is giving away. Half-lidded under a level brow is smug;
/// wide under the same brow is alarmed.
pub const EXPRESSIONS: [(&str, &str, [f32; 4], [f32; 3]); 8] = [
    ("rest", "rest", [0.00, 0.00, 0.55, 0.00], [1.00, 1.00, 0.00]),
    ("happy", "smile", [0.14, -0.12, 0.75, 0.00], [0.74, 1.00, -0.18]),
    ("grin", "grin", [0.20, -0.16, 0.80, 0.00], [0.62, 1.15, -0.22]),
    ("angry", "frown", [-0.22, 0.48, 0.18, 0.00], [0.88, 1.20, 0.52]),
    ("surprised", "O", [0.46, -0.18, 0.92, 0.00], [1.34, 0.85, -0.10]),
    ("smug", "smirk", [0.10, 0.06, 0.50, 0.40], [0.66, 1.10, 0.30]),
    ("sad", "pout", [0.12, -0.42, 0.38, 0.00], [0.94, 0.90, -0.46]),
    ("speaking", "A", [0.06, -0.05, 0.60, 0.00], [1.00, 1.00, 0.00]),
];

pub fn face(name: &str) -> Option<Face> {
    let &(_, viseme, brow, eye) = EXPRESSIONS.iter().find(|(known, ..)| *known == name)?;

    Some(Face { mouth: mouth::shape(viseme)?, brow, eye, gaze: Vec2::ZERO })
}

/// Seed base for authored strokes. Each mark takes its index off this, so
/// the wobble is stable across frames and across a re-solve — never keyed
/// to traversal order or to a clock.
const CHART_SEED: u64 = 0x5a17_0000;

/// Draw the face and plant it, so the marks wrap the surface and are
/// occluded by the same pass as everything else.
///
/// `eye` is an input rather than an afterthought: the nose bar has to ask
/// whether the profile is already drawing her nose before deciding whether
/// it is still wanted.
pub fn marks(mesh: &Mesh, anchors: &Anchors, face: Face, settings: &Settings, eye: Vec3) -> Vec<Curve3> {
    let mut marks = mouth::draw(&anchors.mouth, face.mouth);
    for (side, at) in &anchors.eyes {
        marks.push(brow::draw(at, face.brow, *side));
        marks.extend(eye::draw(at, settings.eye_style, face.eye, face.gaze, *side));
    }
    if let Some(at) = anchors.nose.as_ref().filter(|at| !in_profile(mesh, at, eye)) {
        // The mark goes on the flank the light is not on, so it moves with
        // the lamp rather than being baked to one side of her face.
        marks.extend(nose::draw(at, settings.nose, -settings.light.x.signum(), settings.nose_bend));
    }

    let front = anchors.front();
    marks
        .into_iter()
        .enumerate()
        .flat_map(|(index, mark)| {
            plant::mark(mesh, &mark, front).into_iter().map(move |points| Curve3 {
                points,
                class: mark.class,
                pen: mark.pen,
                seed: CHART_SEED | index as u64,
                authored: true,
            })
        })
        .collect()
}

/// One eye's frame, planted: the chart geometry a painted accent reads
/// instead of re-deriving anything from the label field.
///
/// Everything here rests on the same fitted plane at the same standoff the
/// ink's own marks do, so paint and ink cannot skew apart wherever the
/// surface curves — an iris centred on the label blob instead looks past
/// the viewer the moment she turns.
#[derive(Clone)]
pub struct EyeFrame {
    /// The iris centre, and the tips of its two radii. Projected, the pair
    /// of tips becomes the axes paint measures in, so foreshortening rides
    /// along with them rather than being modelled a second time.
    pub centre: Vec3,
    pub width_tip: Vec3,
    pub height_tip: Vec3,
    /// Pupil half-axes as fractions of the iris radius.
    pub pupil: Vec2,
    /// The aperture as a closed loop — the lid curves the ink draws, which
    /// is the only clip a painted iris may use.
    pub aperture: Vec<Vec3>,
}

impl EyeFrame {
    /// The same frame carried through the head's own map.
    ///
    /// An eye is planted on the *rest* sculpt in the model's frontal
    /// plane, because planting one on a head that has turned casts every
    /// ray at a graze and slides the mark across her cheek. So the pose
    /// is applied afterwards, and it is the head bone's map alone rather
    /// than a skin: every point of an eye is bound wholly to the head,
    /// which makes the blend at each of them the head's own transform.
    ///
    /// Without this the wash's accents keep painting an iris where the
    /// eye used to be — invisible while the wash itself stood at rest,
    /// and the first thing on screen once it stopped
    /// (iamacoffeepot/aether#4462).
    #[must_use]
    pub fn posed(self, head: &Rigid) -> Self {
        Self {
            centre: head.point(self.centre),
            width_tip: head.point(self.width_tip),
            height_tip: head.point(self.height_tip),
            aperture: self.aperture.into_iter().map(|at| head.point(at)).collect(),
            ..self
        }
    }
}

/// How finely each lid is sampled around the aperture loop.
///
/// The loop is a clip rather than a stroke, so it is sampled for the
/// polygon's own straightness: an eye spans a couple of hundred pixels of
/// a developed sheet, and at this many rungs a chord departs from the lid
/// by well under one of them.
const APERTURE_SAMPLES: usize = 24;

/// Every eye's paint frame on this subject.
///
/// Solved per repaint rather than cached with the anchors, because gaze
/// moves the iris inside its own aperture — so a frame cached at load
/// would leave the paint looking where she used to.
pub fn eye_frames(mesh: &Mesh, anchors: &Anchors, face: Face, style: Style) -> Vec<EyeFrame> {
    let front = anchors.front();

    anchors
        .eyes
        .iter()
        .filter_map(|&(side, at)| {
            let iris = eye::iris_frame(&at, style, face.eye, face.gaze, side)?;

            let mut chart = vec![iris.centre, iris.centre + Vec2::X * iris.radius, iris.centre + Vec2::Y * iris.radius];
            chart.extend(eye::aperture_outline(&at, style, face.eye, face.gaze, side, APERTURE_SAMPLES));
            let planted = plant::points(mesh, &chart, front, STANDOFF_EYE)?;

            Some(EyeFrame {
                centre: planted[0],
                width_tip: planted[1],
                height_tip: planted[2],
                pupil: iris.pupil,
                aperture: planted[3..].to_vec(),
            })
        })
        .collect()
}

/// Rungs across the bridge the turn is measured at. A handful: the
/// question is whether a sign change exists across the nose's own width,
/// and the nose is one feature wide.
const BRIDGE_RUNGS: usize = 9;

/// Whether the profile already runs over the nose.
///
/// The bar stands in for a nose the drawing cannot otherwise show, so it
/// goes the moment the real one turns up: turn her far enough and the
/// silhouette breaks over her own bridge, and a tick beside a nose already
/// drawing itself reads as a blemish. Measured rather than set to an angle,
/// so it stays right for a character whose nose sits somewhere else.
///
/// Asked of the surface rather than of the extracted outline, and that is
/// the load-bearing part. The silhouette *is* the zero set of `n . v`, so
/// "does the profile cross her bridge" is exactly "does `n . v` change sign
/// across it" — which costs a row of rays, needs no threshold at all, and
/// answers about her face rather than about whatever the outline happens to
/// be made of. Searching the extracted curves instead cannot survive here:
/// the profile this engine solves per frame is a decimation, and its
/// internal loops plus the label field's ring-widening put a fringe strand
/// hanging between her eyes inside any box tight enough to be useful. That
/// misfire is total — the bar is suppressed at every angle including dead
/// ahead, and the nose simply never appears.
///
/// Measured on the subject: the sign change arrives at about thirty degrees
/// of turn either way, and nothing before twenty-five.
fn in_profile(mesh: &Mesh, nose: &Anchor, eye: Vec3) -> bool {
    let front = nose.front + 0.5;
    let facing: Vec<f32> = (0..BRIDGE_RUNGS)
        .filter_map(|i| {
            let across = -1.0 + 2.0 * i as f32 / (BRIDGE_RUNGS - 1) as f32;
            let at = Vec3::new(nose.centre.x + nose.half.x * across, nose.centre.y, front);

            mesh.hit(at, INTO).map(|c| c.normal.dot((eye - c.pos).normalize()))
        })
        .collect();

    facing.windows(2).any(|pair| pair[0] * pair[1] < 0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A ridge standing over a flat base — a bridge, and the smallest shape
    /// that has one. Two flanks meeting at `z = 1` along the midline, so
    /// the interpolated normal sweeps from leaning left at one edge to
    /// leaning right at the other.
    fn bridge() -> Mesh {
        const OBJ: &[u8] = b"v 0 -1 1\nv 0 1 1\nv -1 -1 0\nv -1 1 0\nv 1 -1 0\nv 1 1 0\n\
                             f 3 1 2\nf 3 2 4\nf 1 5 6\nf 1 6 2\n";

        Mesh::from_obj_bytes(OBJ, 0).expect("a ridge is a mesh")
    }

    /// An anchor spanning most of that ridge, the way a nose anchor spans
    /// most of a nose.
    fn anchor() -> Anchor {
        Anchor { centre: Vec2::new(0.0, 0.0), half: Vec2::new(0.9, 0.4), front: 1.0 }
    }

    /// An eye `degrees` around the ridge, far enough out that the view
    /// direction is near enough constant across it.
    fn eye(degrees: f32) -> Vec3 {
        let (sin, cos) = degrees.to_radians().sin_cos();

        Vec3::new(sin, 0.0, cos) * 20.0
    }

    /// Tripwire: the nose bar retires exactly when her face turns over its
    /// own bridge, and not before.
    ///
    /// Both edges have been the bug and both are silent and total. Firing
    /// face-on suppresses the bar at every angle, so the nose never appears
    /// at all — which is indistinguishable from the feature not having been
    /// written, and is what a search through the extracted profile does
    /// here once a fringe strand lands in its box. Never firing leaves a
    /// tick beside a nose already drawing itself, which reads as a blemish.
    ///
    /// The turn the flip happens at is the ridge's own: the outer normal
    /// leans about 40 degrees off the front, so it goes edge-on around 50
    /// and the pair below brackets that. Change the rule and the bracket
    /// moves.
    #[test]
    fn the_nose_bar_retires_when_the_bridge_breaks_the_outline() {
        let (mesh, at) = (bridge(), anchor());

        assert!(!in_profile(&mesh, &at, eye(0.0)), "face-on the bridge faces the eye; the bar stays");
        assert!(!in_profile(&mesh, &at, eye(30.0)), "a quarter turn is not enough to break the outline");
        assert!(in_profile(&mesh, &at, eye(70.0)), "turned this far the profile runs over the bridge");
    }

    /// Tripwire: the gate is even in the turn.
    ///
    /// She has two cheeks and the nose is on the midline, so a bar that
    /// retires turning one way and not the other is reading something other
    /// than the turn — a signed quantity left unabsolved, which is the
    /// shape the bug takes and is invisible until someone orbits the far
    /// way round.
    #[test]
    fn the_turn_gate_reads_the_same_either_way_round() {
        let (mesh, at) = (bridge(), anchor());

        for degrees in [30.0f32, 70.0] {
            assert_eq!(
                in_profile(&mesh, &at, eye(degrees)),
                in_profile(&mesh, &at, eye(-degrees)),
                "turning {degrees} degrees either way is the same turn",
            );
        }
    }
}
