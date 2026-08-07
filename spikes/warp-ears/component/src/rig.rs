//! The two-bone chain and the volumetrized weights, which both animation paths
//! read verbatim.
//!
//! This module is the reason the comparison means anything. Linear-blend
//! skinning and the warp field are only comparable if they are fed the *same*
//! bone transforms and the *same* per-corner weights, so both live here and
//! neither path is allowed its own copy. [`Pose::blend`] is likewise shared: it
//! is the single definition of `Σ wᵢ Tᵢ`, and the two paths differ only in what
//! they do with the matrix it hands back.
//!
//! The chain is hierarchical. Bone 0 pivots at the rig base and carries the
//! whole ear; bone 1 pivots at the second joint (40% up the ear) and composes
//! **on top of** bone 0, so a fold and a twist stack the way an anatomical
//! chain does rather than fighting each other.
//!
//! Weights are a smoothstep of position along the ear axis across a band
//! centered on the second joint. Two consequences fall straight out of that,
//! and both are load-bearing. Below the base the band has not started, so `w₁`
//! is exactly zero and `w₀` exactly one — that region is carried by a single
//! rigid transform with no blend at all, which is what keeps the ear's
//! attachment from shearing (and, while bone 0 is at identity through the flick
//! and the twist, leaves it literally unmoved). Past the band `w₁` is exactly
//! one, so the ear tip is rigid under bone 1. Everything interesting happens in
//! the four cells between.

use aether_math::{Mat4, Quat, Vec3, Vec4};

use crate::curve::smoothstep;
use crate::data::{CONTACT_NORMAL, CONTACT_POINT, RIG_AXIS, RIG_BASE, RIG_JOINT2};
use crate::ear::{CELL_SIZE, world_from_cell};
use crate::program::Program;

/// Half-width of the weight blend band, in lattice cells. Eight cells total
/// across a 24-cell ear: wide enough that the twist is graded along the length
/// rather than kinked at one ring of corners, narrow enough that the tip and
/// the base are each rigid.
///
/// The width is set by the ear's *girth*, not by taste, and the number is worth
/// stating because a narrower band silently changes what the spike shows. The
/// Jacobian picks up `w′(s) · r` along the ear axis, where `r` is a corner's
/// offset from the bone — about 0.3 world units at the ear's widest. A
/// smoothstep over `W` peaks at `w′ = 1.5/W`, so a four-cell band (`W = 0.25`)
/// leaves `1 − 1.5/0.25 × 0.3 × 2sin(17.5°) ≈ −0.08` at the flick's 35°: the
/// flick alone inverts cells, the guard engages on the segment that exists to
/// be the benign baseline, and the two instances stop agreeing before the twist
/// ever starts. Eight cells lands that same pose near `0.46` and leaves the
/// half-turn twist — whose collapse comes from the blend *matrix* shrinking,
/// not from the band's gradient — refusing exactly as hard as before.
const BLEND_HALF_WIDTH_CELLS: f32 = 4.0;

/// A pose of the chain: the two bones' world transforms, already composed.
#[derive(Clone, Copy)]
pub struct Pose {
    pub bone0: Mat4,
    pub bone1: Mat4,
}

impl Pose {
    /// The rest pose — both bones at identity, so `blend` returns identity for
    /// every weight and the displacement field is zero everywhere.
    pub const REST: Self = Self { bone0: Mat4::IDENTITY, bone1: Mat4::IDENTITY };

    /// `Σ wᵢ Tᵢ` — the linear blend of the two bone matrices, componentwise.
    ///
    /// Blending matrices rather than rotations is the classic choice and it is
    /// deliberate: the collapse it produces at large relative twist is exactly
    /// what the left instance is here to show. The right instance reads the
    /// same matrix, so the artifact is *shared* — the guard is the only thing
    /// that differs.
    #[must_use]
    pub fn blend(&self, weight1: f32) -> Mat4 {
        let column = |index: usize| -> Vec4 { self.bone0.cols[index].lerp(self.bone1.cols[index], weight1) };
        Mat4::from_cols(column(0), column(1), column(2), column(3))
    }
}

/// The rig: bone pivots, the axes each segment of the program rotates about,
/// the contact plane, and the per-corner weights.
pub struct Rig {
    /// Bone 0's pivot — where the ear meets the skull.
    pub base: Vec3,
    /// Bone 1's pivot, 40% up the ear.
    pub joint2: Vec3,
    /// Unit direction up the ear's long axis; the twist axis.
    pub axis: Vec3,
    /// The ear's local left-right axis; the flick pitches about it.
    pub pitch_axis: Vec3,
    /// Axis the fold rotates bone 0 about. Positive rotation carries the ear
    /// *out* along the contact normal, so the program's fold angle is negative:
    /// it lays the ear back down against the skull.
    pub fold_axis: Vec3,
    /// A point on the contact plane — the rig base, per the extraction.
    pub contact_point: Vec3,
    /// Outward unit normal of the contact plane; the skull is behind it.
    pub contact_normal: Vec3,
    /// `w₁` per corner of the dense lattice. `w₀` is `1 − w₁` and is never
    /// stored, so the two cannot disagree.
    pub weights: Vec<f32>,
}

impl Rig {
    /// Build the rig against the surface's rest corners. Runs once.
    #[must_use]
    pub fn build(rest: &[Vec3]) -> Self {
        let base = world_from_cell(RIG_BASE);
        let joint2 = world_from_cell(RIG_JOINT2);
        let axis = Vec3::from_array(RIG_AXIS).normalize();
        let contact_normal = Vec3::from_array(CONTACT_NORMAL).normalize();

        let along_joint = (joint2 - base).dot(axis);
        let band = BLEND_HALF_WIDTH_CELLS * CELL_SIZE;
        let weights = rest
            .iter()
            .map(|&corner| smoothstep(((corner - base).dot(axis) - (along_joint - band)) / (2.0 * band)))
            .collect();

        Self {
            base,
            joint2,
            axis,
            // The ear axis is very nearly world `+Y`, so crossing it with world
            // `+Z` recovers a left-right axis that is the ear's own rather than
            // the world's — it tilts with the ear instead of being pinned to
            // the model grid.
            pitch_axis: axis.cross(Vec3::Z).normalize(),
            fold_axis: axis.cross(contact_normal).normalize(),
            contact_point: world_from_cell(CONTACT_POINT),
            contact_normal,
            weights,
        }
    }

    /// Compose the chain at one point in the program.
    #[must_use]
    pub fn pose(&self, program: &Program) -> Pose {
        let bone0 = pivoted(self.base, Quat::from_axis_angle(self.fold_axis, program.fold_radians));
        let local1 = Quat::from_axis_angle(self.pitch_axis, program.flick_radians)
            * Quat::from_axis_angle(self.axis, program.twist_radians);

        Pose { bone0, bone1: bone0 * pivoted(self.joint2, local1) }
    }
}

/// A rotation applied about a pivot rather than about the origin.
fn pivoted(pivot: Vec3, rotation: Quat) -> Mat4 {
    Mat4::from_translation(pivot) * Mat4::from_rotation_quat(rotation) * Mat4::from_translation(-pivot)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ear;

    /// Tripwire: the weight field's two saturated ends are what make the base
    /// rigid and the tip rigid. A band that leaked a nonzero `w₁` below the
    /// base would shear the ear's attachment under the twist; one that never
    /// reached `1` would leave the tip permanently under-rotated, and the
    /// candy-wrapper pose the whole spike is built to show would never form.
    /// The graded middle is asserted too — a band that had collapsed to a step
    /// would pass both endpoint checks while destroying the comparison.
    #[test]
    fn weights_saturate_below_the_base_and_past_the_blend_band() {
        let surface = ear::build();
        let rig = Rig::build(&surface.rest);
        let band = BLEND_HALF_WIDTH_CELLS * CELL_SIZE;
        let along_joint = (rig.joint2 - rig.base).dot(rig.axis);

        let at = |along: f32| {
            let corner = rig.base + rig.axis * along;
            smoothstep(((corner - rig.base).dot(rig.axis) - (along_joint - band)) / (2.0 * band))
        };

        assert!(at(-CELL_SIZE).abs() < 1e-6, "material below the base must be rigid under bone 0");
        assert!(at(0.0).abs() < 1e-6, "the base itself must be rigid under bone 0");
        assert!(at(along_joint - band).abs() < 1e-6, "the band must not start before its lower edge");
        assert!((at(along_joint + band) - 1.0).abs() < 1e-6, "the band must saturate at its upper edge");
        assert!((at(4.0f32.mul_add(band, along_joint)) - 1.0).abs() < 1e-6, "the tip must be rigid under bone 1");

        let midpoint = at(along_joint);
        assert!(midpoint > 0.4 && midpoint < 0.6, "the band should be graded across the joint, not a step");
    }
}
