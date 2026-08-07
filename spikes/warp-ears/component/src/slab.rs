//! The contact slab: a thin neutral plate standing in for the skull surface at
//! the ear's attachment.
//!
//! Its plane is the extraction's contact plane verbatim — the rig base for a
//! point, the outward radial direction at that point for a normal — and the
//! plate is offset back along the normal by its own half-thickness, so the face
//! that reads as the skull *is* the plane rather than a hair in front of it.
//!
//! The slab draws nothing and stops nothing. It exists so that the fold's
//! interpenetration has a surface to be visible against: both instances drive
//! the ear through it identically, because neither path knows the plate is
//! there. A displacement field can refuse to fold a cell inside out; it has no
//! opinion whatever about two surfaces occupying the same space. Reading that
//! off the picture is the point of putting the plate in it.
//!
//! One consequence of taking the extraction's numbers literally is worth
//! stating: at rest the ear's long axis already sits about 17° *behind* this
//! plane, because a tangent plane at the base of an ear on a curved skull is
//! very nearly the plane the ear grows along. So the plate cuts the ear's
//! attachment from the first frame. That is anatomically right — the skull
//! surface does pass through where an ear joins it — and the fold is what turns
//! a plausible intersection into an unmistakable one.

use aether_math::{Rgb, Vec3};

use crate::ear::FACES;

/// Half-width of the plate in its own plane, in world units. Sized to cover
/// where the folded ear lands and no further: the fold turns the ear to about
/// 77° off the plane, so its in-plane reach from the base collapses to roughly
/// a quarter of its length plus its own girth. A plate much wider than that
/// starts dominating the frame it exists to be a backdrop in.
const HALF_EXTENT: f32 = 0.8;

/// Plate thickness along the normal. Thin enough to read as a surface, thick
/// enough that its edge is visible when the plane is seen close to edge-on.
const THICKNESS: f32 = 0.05;

/// Neutral grey — deliberately outside the ear's palette and outside the
/// det-J tint's warm/cool axis, so nothing about the plate can be mistaken for
/// instrumentation.
pub const COLOR: Rgb = Rgb::new(0.44, 0.44, 0.47);

/// The winding table's corner offsets are 0/1 per axis; this turns one into the
/// ∓ half-extent to step, so a plate corner is a single expression rather than
/// a fold with a branch inside it.
const SIGN: [f32; 2] = [-1.0, 1.0];

/// Triangles making up the plate, in an instance's local world space.
///
/// Twelve — a closed box, so the plate has a silhouette from any angle instead
/// of vanishing when the camera crosses its plane.
#[must_use]
pub fn build(point: Vec3, normal: Vec3) -> Vec<[Vec3; 3]> {
    // A right-handed frame with the normal as its third axis, so the cube-face
    // winding table from the extractor applies unchanged: `tangent × binormal`
    // is the normal by construction.
    let tangent = normal.cross(Vec3::Y).normalize_or(Vec3::X);
    let binormal = normal.cross(tangent);
    let center = point - normal * (THICKNESS * 0.5);
    let half = [tangent * HALF_EXTENT, binormal * HALF_EXTENT, normal * (THICKNESS * 0.5)];

    let corner =
        |offset: [usize; 3]| center + half[0] * SIGN[offset[0]] + half[1] * SIGN[offset[1]] + half[2] * SIGN[offset[2]];

    FACES
        .iter()
        .flat_map(|(_, offsets)| {
            let quad = offsets.map(corner);
            [[quad[0], quad[1], quad[2]], [quad[0], quad[2], quad[3]]]
        })
        .collect()
}
