//! The nose, as one mark on the shadow side.
//!
//! The sculpt has a nose — the midline profile bulges a little proud
//! between the eye and lip bands — but it is far too gentle to draw itself.
//! The normal barely turns across it, so no hatch family's tone threshold
//! is crossed, and the relief band-pass does not clear the crease threshold
//! either. Nothing in the pipeline was ever drawing it, which is why
//! lifting the face out of the hatching cost nothing here.
//!
//! So it is charted, and the convention is on our side: a drawn nose is one
//! mark on the shadow side, not a rendered form.

use aether_math::Vec2;

use crate::anchor::Anchor;
use crate::feature::{FeatureClass, Pen};

use super::{Mark, STANDOFF_NOSE};

/// Which mark stands in for the nose. Enumerated rather than a set of flags
/// because these are the whole vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Nose {
    /// Nothing charted. The face carries on the eyes and mouth alone, and
    /// the sculpt's own relief is left to draw whatever it can reach.
    None,
    /// One short stroke down the shadow side. The default of the idiom.
    #[default]
    Tick,
    /// The tick plus the shadow under the tip, meeting it at an angle.
    Wedge,
}

pub const KINDS: [(&str, Nose); 3] = [("none", Nose::None), ("tick", Nose::Tick), ("wedge", Nose::Wedge)];

pub fn kind(name: &str) -> Option<Nose> {
    KINDS.iter().find(|(known, _)| *known == name).map(|&(_, kind)| kind)
}

/// The nose. `shadow` is the side the light is *not* on, `-1` for her right
/// and `+1` for her left.
///
/// Drawn down the shaded flank of the bridge rather than centred on it: a
/// mark on the midline reads as a seam splitting the face, and the whole
/// point of the single stroke is that it implies a form by describing only
/// where the light stops.
pub fn draw(anchor: &Anchor, kind: Nose, shadow: f32, bend: f32) -> Vec<Mark> {
    if kind == Nose::None {
        return Vec::new();
    }

    let (centre, half) = (anchor.centre, anchor.half);

    // One short bar down the shaded flank, tapering out at the bottom.
    //
    // `bend` kicks the lower end away from the midline, which is the one
    // piece of nose form that stays true from any angle: the shadow edge
    // runs down the bridge and turns out where the nostril wing begins.
    // Bowing it the other way reads as a crease in her cheek. It wants to
    // stay small — the mark's job is to say nose without claiming to
    // describe one, since the profile takes that job over as she turns, and
    // two lines describing the same form compete.
    let (points, weights) = (0..12)
        .map(|i| {
            let t = i as f32 / 11.0;
            let x = centre.x + shadow * half.x * (0.34 + bend * t * t);

            (Vec2::new(x, centre.y + half.y * (0.34 - 0.92 * t)), 1.25 * (1.0 - t * t * t).max(0.0).powf(0.5))
        })
        .unzip();

    let mut marks = vec![Mark { points, weights, pen: Pen::Ink, class: FeatureClass::Decal, standoff: STANDOFF_NOSE }];

    if kind == Nose::Wedge {
        // The underside, turning back toward the midline so the two marks
        // meet at the tip in a shallow hook rather than crossing.
        let (points, weights) = (0..10)
            .map(|i| {
                let t = i as f32 / 9.0;
                let x = centre.x + shadow * half.x * (0.18 - 0.52 * t);

                (Vec2::new(x, centre.y - half.y * (0.83 + 0.16 * t * t)), 0.85 * (1.0 - t).powf(0.6))
            })
            .unzip();

        marks.push(Mark { points, weights, pen: Pen::Ink, class: FeatureClass::Decal, standoff: STANDOFF_NOSE });
    }

    marks
}
