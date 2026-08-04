//! Posing the subject, and carrying the drawing along with it.
//!
//! The rig is a bust: chest pedestal, neck rotor, three-axis head, jaw
//! hinge, two ear rotors, with harmonic weights solved over a lattice and
//! sampled at the sculpt's own vertices. Every bone in the set is
//! load-bearing even when only the ears move — harmonic weights have
//! global support, so with no head bone competing for it the cranium binds
//! to whatever seed is nearest, and the never-posed bones are what hold
//! the skull still while an ear swings.
//!
//! # Why the curves are skinned rather than re-extracted
//!
//! [`extract::surface`](crate::extract::surface) marches the whole sculpt
//! and takes the better part of a second. Re-running it per frame on a
//! posed mesh is not slow, it is impossible. So the level sets are solved
//! once in the rest pose and the *resulting curve points* are posed
//! instead — tens of thousands of points rather than a march over a
//! million faces.
//!
//! That works because a curve point is not a free-floating position: it is
//! a barycentric address inside one face ([`Anchorage`]), recorded where
//! the level set crossed. Posing it is the same blend the surrounding
//! surface gets, evaluated at the same place, so a hatch line stays welded
//! to the skin it was cut into instead of being re-cut by a world-space
//! plane every frame.
//!
//! Hatch and crease improve under this rather than merely surviving. The
//! silhouette does not: it is the zero set of `view . normal` and depends
//! on the pose *and* the eye, so it alone is genuinely re-extracted, off
//! the posed surface.

use aether_math::Vec3;

use crate::Pose;
use crate::extract::{Settings, tone_gate};
use crate::feature::Curve3;
use crate::mesh::{Anchorage, Mesh};

/// Half the arc an ear can actually turn through, in degrees. A fox aims
/// an ear by rotating it about its own long axis; past this the blade is
/// doing something an ear does not do.
pub const TWIST_LIMIT: f32 = 22.5;

/// Below this share of a vertex a bone is not blended in.
///
/// Harmonic weights have global support, so every bone owns a little of
/// every vertex and the honest sum is over the whole set. At this
/// threshold what is dropped moves a vertex by well under a thousandth of
/// the subject's height — under the width of the thinnest stroke the
/// drawing can lay down — and the blend loop is the per-pose cost that
/// scales with the mesh.
const MINIMUM_SHARE: f32 = 1.0e-4;

/// An affine map, held as the three images of the basis and the image of
/// the origin.
///
/// Sampled out of a composition of rotations rather than multiplied
/// together: each bone's rule is written the way the rig describes it —
/// rotate about this pivot, then that one — and an affine map is fully
/// determined by where it sends four points. So the rule stays readable
/// and the matrix falls out of it, instead of the rule being rewritten as
/// a product nobody can check against the reference.
#[derive(Clone, Copy, Debug)]
pub struct Rigid {
    columns: [Vec3; 3],
    translation: Vec3,
}

impl Rigid {
    pub const IDENTITY: Self = Self { columns: [Vec3::X, Vec3::Y, Vec3::Z], translation: Vec3::ZERO };

    /// The affine map `send` performs, read off its action on the origin
    /// and the three basis vectors.
    fn sample(send: impl Fn(Vec3) -> Vec3) -> Self {
        let translation = send(Vec3::ZERO);

        Self { columns: [Vec3::X, Vec3::Y, Vec3::Z].map(|axis| send(axis) - translation), translation }
    }

    /// Where this map sends a point.
    pub fn point(&self, p: Vec3) -> Vec3 {
        self.direction(p) + self.translation
    }

    /// Where this map sends a direction — a normal, or the offset a decal
    /// stands off its surface by.
    pub fn direction(&self, v: Vec3) -> Vec3 {
        self.columns[0] * v.x + self.columns[1] * v.y + self.columns[2] * v.z
    }

    /// The inverse, given the linear part is a rotation: its transpose,
    /// and the translation carried back through it.
    ///
    /// Every bone rule is a composition of rotations about pivots, so the
    /// linear part is orthonormal by construction and the transpose is the
    /// inverse. Nothing here scales or shears, and if anything ever does
    /// this is the assertion that has to move first.
    pub fn inverse(&self) -> Self {
        let [x, y, z] = self.columns;
        let columns = [Vec3::new(x.x, y.x, z.x), Vec3::new(x.y, y.y, z.y), Vec3::new(x.z, y.z, z.z)];
        let inverted = Self { columns, translation: Vec3::ZERO };

        Self { columns, translation: -inverted.direction(self.translation) }
    }

    /// `self + other * weight`, the accumulation linear blend skinning is.
    ///
    /// At *one* point, blending the maps and applying the blend once is
    /// the same answer as applying each map and blending the results, so
    /// this order is free and lets a vertex's position, its normal and its
    /// standoff share a single blend.
    ///
    /// Across *several* points it is not. Interpolating transforms between
    /// three corners and applying the result to the interpolated point is
    /// a different function from posing each corner and interpolating
    /// those — see [`Skin::pose_curves`], where the difference is the
    /// drawing leaving the surface.
    fn add_scaled(self, other: &Self, weight: f32) -> Self {
        Self {
            columns: [0, 1, 2].map(|axis| self.columns[axis] + other.columns[axis] * weight),
            translation: self.translation + other.translation * weight,
        }
    }

    const ZERO: Self = Self { columns: [Vec3::ZERO; 3], translation: Vec3::ZERO };
}

/// One bone: what it is called, what it turns about, and its long axis
/// where one is meaningful.
struct Bone {
    name: String,
    pivot: Option<Vec3>,
    axis: Option<Vec3>,
}

/// The rig, and the per-vertex weights that bind the sculpt to it.
pub struct Skin {
    /// Row-major `[vertex][bone]`, as the solver normalised them.
    weights: Vec<f32>,
    bones: Vec<Bone>,
    /// How much of the head's rotation the neck takes, so a turn reads as
    /// a neck carrying a head rather than a head swivelling on a post.
    neck_share: f32,
}

/// The `f32` payload of a little-endian `.npy`, however its header is
/// sized. Same shape as the material field's reader: a guest has no
/// filesystem, so the array arrives by mail and the caller owns the bytes.
fn npy_f32(bytes: &[u8]) -> Option<Vec<f32>> {
    let header = usize::from(u16::from_le_bytes([*bytes.get(8)?, *bytes.get(9)?]));

    Some(bytes.get(10 + header..)?.chunks_exact(4).map(|w| f32::from_le_bytes([w[0], w[1], w[2], w[3]])).collect())
}

impl Skin {
    /// Read a rig from its weight array and its descriptor.
    ///
    /// `None` when the weights do not describe this subject. That check is
    /// the whole of the rig's identity — weights are per vertex of one
    /// sculpt, and a rig silently applied to a different one poses nothing
    /// recognisable while reporting success.
    pub fn parse(weights: &[u8], descriptor: &str, vertices: usize) -> Option<Self> {
        let weights = npy_f32(weights)?;

        let mut bones: Vec<Bone> = Vec::new();
        let mut neck_share = 0.35;
        let read = |value: &str| value.parse::<f32>().unwrap_or(0.0);
        let find = |bones: &[Bone], name: &str| bones.iter().position(|bone| bone.name == name);

        for line in descriptor.lines() {
            match line.split_whitespace().collect::<Vec<&str>>().as_slice() {
                ["bones", names @ ..] => {
                    bones =
                        names.iter().map(|name| Bone { name: (*name).to_owned(), pivot: None, axis: None }).collect();
                }
                ["neck_share", value] => neck_share = read(value),
                ["pivot", name, x, y, z] => {
                    if let Some(at) = find(&bones, name) {
                        bones[at].pivot = Some(Vec3::new(read(x), read(y), read(z)));
                    }
                }
                ["axis", name, x, y, z] => {
                    if let Some(at) = find(&bones, name) {
                        bones[at].axis = Some(Vec3::new(read(x), read(y), read(z)));
                    }
                }
                _ => {}
            }
        }

        (!bones.is_empty() && weights.len() == vertices * bones.len()).then_some(Self { weights, bones, neck_share })
    }

    pub fn bones(&self) -> usize {
        self.bones.len()
    }

    fn pivot(&self, name: &str) -> Vec3 {
        self.bones.iter().find(|bone| bone.name == name).and_then(|bone| bone.pivot).unwrap_or(Vec3::ZERO)
    }

    /// Where the head bone sends a point at this pose.
    ///
    /// Its inverse is what carries the eye into her own frame, which is
    /// the question the chart's turn gate and its planting rays are really
    /// asking: a face is authored in the model's frontal plane, so a head
    /// that has turned is drawn by turning the viewer, not the face.
    pub fn head(&self, pose: &Pose) -> Rigid {
        let pivot = self.pivot("head");

        Rigid::sample(|p| turn(p, pivot, pose, 1.0))
    }

    /// Where each bone sends a point at this pose.
    ///
    /// Ears compose on top of the head, so flicking an ear while she turns
    /// does both. The chest is the pedestal and never moves — it is what
    /// the rest of the figure is posed against.
    pub fn transforms(&self, pose: &Pose) -> Vec<Rigid> {
        let head_pivot = self.pivot("head");
        let head = |p: Vec3, share: f32| turn(p, head_pivot, pose, share);

        self.bones
            .iter()
            .map(|bone| {
                let pivot = bone.pivot.unwrap_or(Vec3::ZERO);
                match bone.name.as_str() {
                    "neck" => Rigid::sample(|p| head(p, self.neck_share)),
                    "head" => Rigid::sample(|p| head(p, 1.0)),
                    "jaw" => Rigid::sample(|p| head(rotate(p, pivot, Vec3::X, pose.jaw), 1.0)),
                    "ear_left" | "ear_right" => {
                        let left = bone.name == "ear_left";
                        let flick = if left {
                            pose.ear_flick_left
                        } else {
                            pose.ear_flick_right
                        };
                        let twist = if left {
                            pose.ear_twist_left
                        } else {
                            pose.ear_twist_right
                        };
                        let (twist, side) = (
                            twist.clamp(-TWIST_LIMIT, TWIST_LIMIT),
                            if left {
                                1.0
                            } else {
                                -1.0
                            },
                        );
                        let axis = bone.axis.unwrap_or(Vec3::Y);

                        // Aim first, about the blade's own axis, so the cup
                        // sweeps. Then flick, which swings the whole blade
                        // out of the midline plane.
                        Rigid::sample(|p| {
                            let aimed = rotate(p, pivot, axis, twist * side);
                            let swung = rotate(aimed, pivot, Vec3::Z, -flick * side);

                            head(rotate(swung, pivot, Vec3::X, flick * 0.35), 1.0)
                        })
                    }
                    _ => Rigid::IDENTITY,
                }
            })
            .collect()
    }

    /// The blended map at one vertex.
    fn at_vertex(&self, transforms: &[Rigid], vertex: usize) -> Rigid {
        let row = &self.weights[vertex * self.bones.len()..(vertex + 1) * self.bones.len()];

        row.iter().zip(transforms).fold(Rigid::ZERO, |blend, (&weight, transform)| {
            if weight <= MINIMUM_SHARE {
                blend
            } else {
                blend.add_scaled(transform, weight)
            }
        })
    }

    /// The blended map at a point addressed inside a face — the same blend
    /// the three corners get, interpolated where the point actually sits.
    fn at_anchorage(&self, transforms: &[Rigid], faces: &[[u32; 3]], at: Anchorage) -> Rigid {
        let corners = faces[at.face as usize];

        corners.iter().zip(at.barycentric()).fold(Rigid::ZERO, |blend, (&corner, share)| {
            if share <= 0.0 {
                blend
            } else {
                blend.add_scaled(&self.at_vertex(transforms, corner as usize), share)
            }
        })
    }

    /// Pose the surface: every vertex and its normal, written in place.
    ///
    /// Normals are carried by the blended rotation rather than recomputed
    /// off the posed faces. That is both the cheaper answer and the more
    /// faithful one — the rest normals are area-weighted and relaxed
    /// (`Mesh::build`), and a recompute would throw that relaxation away
    /// and hand the silhouette back the frayed field the relaxation exists
    /// to clean up.
    pub fn pose_surface(&self, transforms: &[Rigid], rest: &Mesh, positions: &mut [Vec3], normals: &mut [Vec3]) {
        for vertex in 0..rest.positions.len() {
            let blend = self.at_vertex(transforms, vertex);
            positions[vertex] = blend.point(rest.positions[vertex]);
            normals[vertex] = blend.direction(rest.normals[vertex]).normalize_or(rest.normals[vertex]);
        }
    }

    /// Carry a drawing's points onto a surface `pose_surface` has already
    /// posed.
    ///
    /// The probe is read straight off `posed` at the address the point
    /// carries — the same barycentric blend of the same three corners the
    /// crossing was interpolated from — and that is what makes the claim
    /// "welded to the surface" true rather than approximately true. The
    /// tempting alternative is to blend the *bone transforms* at the
    /// anchorage and apply the blend to the point: it costs the same
    /// arithmetic, it is exactly right at a vertex, and everywhere else it
    /// puts the point off the posed triangle, because interpolating
    /// transforms and then applying them is a different function from
    /// applying them and then interpolating. Under a shear — which is what
    /// a soft weight boundary is — the two disagree by a visible fraction
    /// of the deformation.
    ///
    /// So the only thing the rig is asked for here is a *direction*: the
    /// offset an authored mark stands off its surface by. That is genuinely
    /// a rotation question, it has no corners to interpolate between, and
    /// it is asked only of the few hundred points that have one — a decal
    /// keeps its clearance instead of sinking into a cheek that turned
    /// under it.
    ///
    /// A point with no anchorage is left where it is. Nothing extracted or
    /// planted lacks one; a curve assembled by hand in a test does.
    pub fn pose_curves(&self, transforms: &[Rigid], posed: &Mesh, curves: &mut [Curve3]) {
        for point in curves.iter_mut().flat_map(|curve| &mut curve.points) {
            let Some(at) = point.anchorage else {
                continue;
            };
            let corners = posed.faces[at.face as usize];
            let shares = at.barycentric();
            let blend = |values: &[Vec3]| {
                corners
                    .iter()
                    .zip(shares)
                    .fold(Vec3::ZERO, |sum, (&corner, share)| sum + values[corner as usize] * share)
            };

            let standoff = point.pos - point.probe;
            point.probe = blend(&posed.positions);
            point.normal = blend(&posed.normals).normalize_or(point.normal);
            point.pos = if standoff == Vec3::ZERO {
                point.probe
            } else {
                point.probe + self.at_anchorage(transforms, &posed.faces, at).direction(standoff)
            };
        }
    }
}

/// Yaw, then pitch, then roll about the head pivot, at `share` of each.
fn turn(p: Vec3, pivot: Vec3, pose: &Pose, share: f32) -> Vec3 {
    let turned = rotate(p, pivot, Vec3::Y, pose.yaw * share);
    let pitched = rotate(turned, pivot, Vec3::X, pose.pitch * share);

    rotate(pitched, pivot, Vec3::Z, pose.roll * share)
}

fn rotate(p: Vec3, pivot: Vec3, axis: Vec3, degrees: f32) -> Vec3 {
    if degrees.abs() < 1e-6 {
        return p;
    }

    pivot + (p - pivot).rotate_axis_angle(axis, degrees.to_radians())
}

/// A whole drawing posed: the surface curves skinned, then tone-gated
/// against the normals the pose gave them.
///
/// The gate has to run here rather than at load. Which hatch families
/// survive at a point is view-independent, which is why it moved to load
/// time in the first place, but it is not *pose*-independent: it reads the
/// point's own normal, and skinning changes normals. Left at load, the
/// shading freezes to the rest pose and slides under a moving body.
pub fn posed_surface(
    skin: &Skin,
    transforms: &[Rigid],
    posed: &Mesh,
    rest: &[Curve3],
    settings: &Settings,
) -> Vec<Curve3> {
    let mut curves = rest.to_vec();
    skin.pose_curves(transforms, posed, &mut curves);

    tone_gate(curves, settings)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::feature::{FeatureClass, Pen, SurfacePoint};
    use crate::weld;

    /// A rig with one posable bone, and weights binding two vertices to it
    /// at full and half share.
    fn descriptor() -> &'static str {
        "bones chest head\nneck_share 0.35\npivot head 0.0 0.0 0.0\n"
    }

    /// `.npy` framing around `values`: the magic, a version, a header
    /// length and a header of that length.
    fn npy(values: &[f32]) -> Vec<u8> {
        let header = b"{'descr': '<f4', 'fortran_order': False, }    \n";
        let mut bytes = b"\x93NUMPY\x01\x00".to_vec();
        bytes.extend((header.len() as u16).to_le_bytes());
        bytes.extend(header);
        bytes.extend(values.iter().flat_map(|v| v.to_le_bytes()));

        bytes
    }

    fn quarter_turn() -> Pose {
        Pose { yaw: 90.0, ..Pose::default() }
    }

    /// A strip of two triangles standing in the `xy` plane, and a rig that
    /// binds each of its four corners to the head by a different share —
    /// so the strip shears rather than turning as one piece, which is the
    /// only shape in which a mis-weighted blend is visible at all.
    fn strip() -> (Mesh, Skin) {
        const OBJ: &[u8] = b"v -1 0 0\nv 1 0 0\nv -1 1 0\nv 1 1 0\nf 1 2 3\nf 2 4 3\n";
        let mesh = Mesh::from_obj_bytes(OBJ, 0).expect("a strip is a mesh");
        let weights = npy(&[1.0, 0.0, 0.7, 0.3, 0.4, 0.6, 0.0, 1.0]);

        (mesh, Skin::parse(&weights, descriptor(), 4).expect("a four-vertex rig"))
    }

    /// Tripwire: a rig whose weights are not this subject's is refused.
    ///
    /// Weights are per vertex of one sculpt and carry no identity of their
    /// own, so the count is the only thing that can catch a rig pointed at
    /// the wrong mesh. Accepted, it poses the vertices it has and reads
    /// past the end for the rest — or, at a smaller mesh, silently poses a
    /// prefix and reports success. Both are a subject that moves wrongly
    /// with nothing in the log.
    #[test]
    fn a_rig_for_another_subject_is_refused() {
        let weights = npy(&[1.0, 0.0, 0.5, 0.5]);

        assert!(Skin::parse(&weights, descriptor(), 2).is_some(), "two vertices by two bones is this subject");
        assert!(Skin::parse(&weights, descriptor(), 3).is_none(), "the same array cannot describe three vertices");
    }

    /// Tripwire: the sampled affine reproduces the rotation it was read
    /// off, including the pivot the rotation was taken about.
    ///
    /// `Rigid::sample` is the load-bearing shortcut of this module — every
    /// bone rule is written as a composition of `rotate` calls and then
    /// never evaluated again, only its sampled matrix is. A sample that
    /// dropped the pivot would still be a rotation, still be orthonormal,
    /// and still pass any check on the linear part alone; what it would do
    /// is pose her about the world origin, which reads as the whole figure
    /// swinging rather than her head turning.
    #[test]
    fn the_sampled_map_reproduces_the_rotation_it_was_read_off() {
        let pivot = Vec3::new(0.0, 2.0, 0.0);
        let sampled = Rigid::sample(|p| rotate(p, pivot, Vec3::Y, 90.0));

        let at = Vec3::new(1.0, 2.0, 0.0);
        let direct = rotate(at, pivot, Vec3::Y, 90.0);
        assert!((sampled.point(at) - direct).length() < 1e-5, "sampled {:?} against {direct:?}", sampled.point(at));
        assert!(sampled.point(pivot).length() > 1.0, "the pivot is off the origin, so the map is not linear");
    }

    /// Tripwire: the inverse carries a point back through the pose,
    /// pivot included.
    ///
    /// This is what puts the eye into her frame so the chart can plant a
    /// face on a head that has turned. A transpose without the translation
    /// term is still an exact inverse of the *rotation* and is wrong by
    /// the pivot offset, which places the viewer somewhere she is not
    /// looking — and the error is zero at the rest pose, so it only
    /// appears once she moves.
    #[test]
    fn the_inverse_carries_a_point_back_through_the_pose() {
        let pose = quarter_turn();
        let skin = Skin::parse(&npy(&[1.0, 0.0, 0.5, 0.5]), descriptor(), 2).expect("a two-vertex rig");
        let head = skin.head(&pose);

        let at = Vec3::new(0.3, 1.4, 2.0);
        let round_trip = head.inverse().point(head.point(at));
        assert!((round_trip - at).length() < 1e-4, "went out and came back to {round_trip:?}, not {at:?}");
    }

    /// Tripwire: a vertex's share of each bone decides how far it travels.
    ///
    /// Linear blend skinning is a weighted sum and the weights are the
    /// whole rig — a blend that normalised them away, or applied the
    /// dominant bone alone, moves a half-weighted vertex the full distance
    /// and turns every soft boundary on the figure into a crease. The two
    /// vertices here are the same point under a quarter turn at full and
    /// half share, so the half-weighted one has to land short of the other
    /// and off the arc between them.
    #[test]
    fn a_shared_vertex_travels_its_own_share_of_the_way() {
        let skin = Skin::parse(&npy(&[0.0, 1.0, 0.5, 0.5]), descriptor(), 2).expect("a two-vertex rig");
        let transforms = skin.transforms(&quarter_turn());

        let at = Vec3::new(0.0, 1.0, 1.0);
        let (whole, shared) = (skin.at_vertex(&transforms, 0).point(at), skin.at_vertex(&transforms, 1).point(at));

        assert!((whole - Vec3::new(1.0, 1.0, 0.0)).length() < 1e-5, "a fully bound vertex takes the whole turn");
        assert!(
            (shared - (at + whole) * 0.5).length() < 1e-5,
            "a half-bound vertex lands halfway between rest and posed, at {shared:?}",
        );
    }

    /// Tripwire: a posed curve point lands on the posed surface, not
    /// beside it.
    ///
    /// This is the whole claim the issue's second acceptance criterion
    /// makes — hatch and crease stay welded to the surface through a pose,
    /// not just through an orbit — and it is checked by two independent
    /// routes to the same point. The curve is posed by blending the bone
    /// transforms at its anchorage and applying the blend once; the
    /// surface is posed vertex by vertex and the anchorage read off the
    /// result. They agree only if both use the same corners and the same
    /// weights, and the failure they rule out is silent at rest and
    /// visible as the drawing sliding off her the moment she moves.
    ///
    /// The strip shears rather than turning rigidly, which is what makes
    /// the check bite: under a rigid transform every wrong blend is still
    /// the right answer.
    #[test]
    fn a_posed_curve_point_lands_on_the_posed_surface() {
        let (rest, skin) = strip();
        let transforms = skin.transforms(&quarter_turn());

        // A level set of height, which cuts both triangles across their
        // interiors rather than along an edge.
        let heights: Vec<f32> = rest.positions.iter().map(|p| p.y).collect();
        let mut curves = weld::curves(
            rest.level_set(&heights, &[], 0.5)
                .into_iter()
                .map(|[a, b]| [SurfacePoint::anchored(&a), SurfacePoint::anchored(&b)])
                .collect(),
            &Curve3 {
                points: Vec::new(),
                class: FeatureClass::Hatch { level: 0 },
                pen: Pen::Pale,
                seed: 0,
                authored: false,
            },
        );
        assert!(!curves.is_empty(), "the level set crosses the strip");

        let mut posed = rest.deformable();
        skin.pose_surface(&transforms, &rest, &mut posed.positions, &mut posed.normals);
        let before: Vec<Vec3> = curves.iter().flat_map(|curve| &curve.points).map(|point| point.pos).collect();
        skin.pose_curves(&transforms, &posed, &mut curves);

        let mut moved = 0.0f32;
        for (point, was) in curves.iter().flat_map(|curve| &curve.points).zip(&before) {
            let at = point.anchorage.expect("an extracted point carries its anchorage");
            let on_surface = posed.at(at);
            assert!(
                (point.pos - on_surface).length() < 1e-5,
                "a posed curve point sits at {:?} while its surface went to {on_surface:?}",
                point.pos,
            );
            moved = moved.max((point.pos - *was).length());
        }
        assert!(moved > 0.1, "the pose has to actually move the curve, and it moved it {moved}");
    }
}
