//! The displacement field `u(v, t)` that animates the figure.
//!
//! Nothing here reads or writes voxels. The field is a pure function of a
//! material-space position and a phase, so the same lattice that was extracted
//! at `init` produces a different pose every frame purely by being read
//! through a different map.
//!
//! The field is a ramp along `+X`. Its weight is zero at the shoulder plane
//! and one at the hand, smoothstepped between — and because the shoulder plane
//! is the only place the arm meets the torso, a weight that vanishes there is
//! automatically zero everywhere off the arm. Corners on that plane are shared
//! by torso faces and sleeve faces alike and do not move, so the arm elongates
//! out of a joint that stays welded. The cells between shoulder and hand carry
//! intermediate weights, so the space between them expands rather than the
//! hand detaching and sliding away.
//!
//! Stretch conserves the arm's rough volume by thinning its cross-section
//! toward the arm axis, blended in by the same weight so the shoulder end
//! keeps its girth.

use aether_math::Vec3;

use crate::figure::{ARM_AXIS_CELL, ARM_SHOULDER_CELL, CELLS_X, world_from_cell};

/// Seconds for one full reach-and-return.
pub const PERIOD_SECONDS: f32 = 4.0;

/// World `x` of the shoulder plane, in material space.
#[allow(clippy::cast_precision_loss)] // Lattice extents are small integers.
const SHOULDER_X: f32 = world_from_cell([ARM_SHOULDER_CELL as f32, 0.0, 0.0]).x;

/// Rest length of the arm along `x`, in world units. The hand block runs to
/// the lattice edge, so the far end of the arm is the far end of the lattice.
#[allow(clippy::cast_precision_loss)] // Lattice extents are small integers.
const ARM_LENGTH: f32 = world_from_cell([CELLS_X as f32, 0.0, 0.0]).x - SHOULDER_X;

/// World-space distance the hand travels at full reach — one arm length, so
/// the arm doubles.
pub const MAX_STRETCH: f32 = ARM_LENGTH;

/// World-space centerline the arm's cross-section thins toward under stretch.
const ARM_AXIS: Vec3 = world_from_cell(ARM_AXIS_CELL);

/// The field frozen at one instant. `stretch` is how far the hand's anchor has
/// travelled along `+X`; `thin` is the cross-section scale that reach implies.
/// Both are phase-derived, so they are computed once per frame and applied to
/// every corner.
pub struct Warp {
    stretch: f32,
    thin: f32,
}

impl Warp {
    /// The field at `phase` radians. Reach follows `0.5 - 0.5·cos(phase)`, so
    /// it rests at both ends of the travel rather than snapping around.
    #[must_use]
    pub fn at_phase(phase: f32) -> Self {
        let stretch = MAX_STRETCH * 0.5 * (1.0 - phase.cos());
        Self { stretch, thin: 1.0 / (1.0 + stretch / ARM_LENGTH).sqrt() }
    }

    /// Map one material-space position into its warped world position.
    #[must_use]
    pub fn apply(&self, v: Vec3) -> Vec3 {
        let w = weight_at(v.x);
        let squash = (self.thin - 1.0).mul_add(w, 1.0);
        Vec3::new(
            w.mul_add(self.stretch, v.x),
            (v.y - ARM_AXIS.y).mul_add(squash, ARM_AXIS.y),
            (v.z - ARM_AXIS.z).mul_add(squash, ARM_AXIS.z),
        )
    }
}

/// The ramp `w(v)`: zero at and inboard of the shoulder, one at and beyond the
/// hand, smoothstepped across the arm.
fn weight_at(x: f32) -> f32 {
    smoothstep((x - SHOULDER_X) / ARM_LENGTH)
}

/// The canonical `3t² − 2t³` smoothstep, clamped at both ends.
fn smoothstep(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * 2.0f32.mul_add(-t, 3.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tripwire: the whole watertightness argument is that the weight is
    /// exactly zero at and inboard of the shoulder plane and exactly one at
    /// the hand. A ramp that leaked a nonzero weight into the torso would tear
    /// the figure in half on the first frame; one that saturated early would
    /// translate the hand rigidly instead of stretching the space.
    #[test]
    fn weight_pins_the_shoulder_and_saturates_at_the_hand() {
        assert!((weight_at(SHOULDER_X) - 0.0).abs() < 1e-6);
        assert!((weight_at(SHOULDER_X - 1.0) - 0.0).abs() < 1e-6);
        assert!((weight_at(SHOULDER_X + ARM_LENGTH) - 1.0).abs() < 1e-6);
        assert!((weight_at(SHOULDER_X + 10.0) - 1.0).abs() < 1e-6);

        let midpoint = weight_at(ARM_LENGTH.mul_add(0.5, SHOULDER_X));
        assert!(midpoint > 0.0 && midpoint < 1.0, "the ramp should be graded across the arm, not a step");
    }

    /// Tripwire: a corner inboard of the shoulder must land on itself for
    /// every phase. Torso and sleeve faces share those corners, so any motion
    /// there is a seam.
    #[test]
    fn torso_corners_are_fixed_points_of_the_field() {
        let torso = Vec3::new(SHOULDER_X - 0.3, 1.2, 0.15);
        for step in 0..16 {
            #[allow(clippy::cast_precision_loss)] // Sixteen phase samples.
            let warped = Warp::at_phase(step as f32 * 0.4).apply(torso);
            assert!((warped - torso).length() < 1e-6, "the field moved a torso corner");
        }
    }
}
