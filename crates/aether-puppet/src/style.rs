//! What makes it look drawn rather than plotted by a machine.
//!
//! Two effects carry almost all of it. **Wobble** bends the line off true;
//! **pressure** swells and tapers its width. Both are seeded functions of
//! stroke identity and the curve's own intrinsic parameter, never of screen
//! position or a frame counter — so a stroke wobbles the same way from
//! every camera angle and the drawing does not boil when the camera moves.
//! That determinism is the difference between a sketch and a shimmer.
//!
//! The offline renderer's page-space emitter lived here too. It is gone:
//! `ribbon` builds geometry directly, so there is no page to project onto.

use aether_math::{TAU, Vec3};

use crate::math3::hash_unit;

/// Two incommensurate sine terms sampled against *world position* rather
/// than distance along the stroke.
///
/// Sampling a noise field in space instead of along the line buys two
/// things at once. The wobble is identical from every camera angle, so an
/// orbit does not make the lines crawl; and it is continuous across a weld,
/// so a stroke assembled from forty arcs wanders like one stroke instead of
/// restarting its phase at every join.
pub fn wander(seed: u64, at: Vec3) -> f32 {
    let (p1, p2) = (hash_unit(seed) * TAU, hash_unit(seed ^ 0x5bf0_3635) * TAU);
    let (f1, f2) = (5.5 + hash_unit(seed ^ 0x1234_9abc) * 2.5, 15.0 + hash_unit(seed ^ 0xfeed_beef) * 6.0);
    let (a, b) = (Vec3::new(0.71, 0.52, 0.47), Vec3::new(-0.44, 0.63, 0.64));

    (at.dot(a) * f1 + p1).sin() * 0.72 + (at.dot(b) * f2 + p2).sin() * 0.28
}

/// Pencil pressure: light at the entry, full through the middle, tapering
/// out at the exit.
///
/// The ramp is a fixed amount of arc rather than a fraction of the stroke,
/// which matters more than it sounds. A proportional taper turns every
/// short stroke into a lens — and a hatch field is mostly short strokes, so
/// the whole drawing would read as dots.
pub fn pressure(travelled: f32, total: f32) -> f32 {
    /// In radians of arc, matching the units `ribbon` measures length in.
    const RAMP: f32 = 0.010;

    let ramp = RAMP.min(total * 0.45);
    if ramp <= 1e-6 {
        return 1.0;
    }

    let ends = (travelled / ramp).min((total - travelled) / ramp).clamp(0.0, 1.0);
    0.42 + 0.58 * ends.sqrt()
}
