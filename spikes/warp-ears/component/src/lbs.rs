//! Linear-blend skinning: the left instance's path, and the whole of it.
//!
//! Blend the two bone matrices by the corner's weight, multiply the rest-pose
//! corner through the result, done. No guard, no Jacobian, no bookkeeping of
//! any kind — that absence is the point of the comparison. Whatever the blend
//! produces is what gets drawn, including the collapsed cross-sections at large
//! relative twist and the ear passing straight through the contact slab.
//!
//! This is deliberately the textbook formulation, artifacts included. The warp
//! side is fed the same [`Pose`] and the same weights and computes the same
//! product; the two are algebraically identical at full application, and the
//! test that asserts so is the honesty check on this whole spike.

use aether_math::Vec3;

use crate::rig::Pose;

/// Pose every corner of the lattice by linear-blend skinning.
///
/// `out` is written in place and must be the same length as `rest`.
pub fn pose_corners(rest: &[Vec3], weights: &[f32], pose: &Pose, out: &mut [Vec3]) {
    debug_assert_eq!(rest.len(), out.len(), "the posed lattice must match the rest lattice");
    for ((slot, &corner), &weight1) in out.iter_mut().zip(rest).zip(weights) {
        *slot = (pose.blend(weight1) * corner.extend(1.0)).truncate();
    }
}
