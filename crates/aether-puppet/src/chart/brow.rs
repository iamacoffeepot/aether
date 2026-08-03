//! The brow, which carries half of every expression the mouth carries the
//! other half of.

use aether_math::Vec2;

use crate::anchor::Anchor;
use crate::feature::{FeatureClass, Pen};

use super::{Mark, STANDOFF_BROW};

/// Samples along the stroke. Enough that the arch reads as a curve rather
/// than as a chain of straights at the width a brow is drawn.
const SAMPLES: usize = 32;

/// One brow. `side` is `-1` for her right, `+1` for her left.
///
/// Drawn as a tapered stroke: thick at the inner end and thinning outward,
/// which is how a brow actually grows and what stops it reading as a
/// pencilled arc.
pub fn draw(anchor: &Anchor, [raise, tilt, arch, skew]: [f32; 4], side: f32) -> Mark {
    let span = anchor.half.x;
    // The eye half-height is only a few hundredths, so a brow sitting one
    // eye above it needs a multiple of about three, not two. Under that it
    // lands on the lid line and reads as a thicker lash rather than a brow.
    let lift = anchor.half.y * (3.4 + raise * 4.4);
    let angle = (tilt + skew * side) * anchor.half.y * 3.0;

    let (points, weights) = (0..SAMPLES)
        .map(|i| {
            // `u` runs inner to outer, so the taper and the tilt both key
            // off the same end.
            let u = i as f32 / (SAMPLES - 1) as f32;
            // Spanning the eye, inner corner to just past the outer — the
            // anchor is the eye's centre, so this has to reach back across
            // it, not start there.
            let x = anchor.centre.x + side * span * (-0.85 + 1.95 * u);
            let curve = arch * anchor.half.y * 2.1 * (1.0 - (2.0 * u - 1.0).powi(2));
            let y = anchor.centre.y + lift + curve - angle * (1.0 - u);

            (Vec2::new(x, y), 1.25 - 0.95 * u * u)
        })
        .unzip();

    Mark { points, weights, pen: Pen::Ink, class: FeatureClass::Silhouette, standoff: STANDOFF_BROW }
}
