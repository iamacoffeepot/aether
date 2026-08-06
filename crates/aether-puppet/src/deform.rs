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
#[cfg(test)]
use core::iter;
#[cfg(test)]
use core::mem::align_of;
use core::mem::size_of;
use serde::{Deserialize, Serialize};

use crate::Pose;
use crate::extract::{Settings, tone_gate};
use crate::feature::Curve3;
use crate::mesh::{Anchorage, Mesh};
use crate::npy;

/// Half the arc an ear can actually turn through, in degrees. A fox aims
/// an ear by rotating it about its own long axis; past this the blade is
/// doing something an ear does not do.
pub const TWIST_LIMIT: f32 = 22.5;

/// Bone slots the uniform blob carries, and so the most a rig may bind.
///
/// The blob is `BONE_LIMIT` affine maps at three `vec4` rows each — 384
/// bytes, which is what "the pose rides the uniform" costs. Eight is the
/// bust rig's six with room for a brow pair; a descriptor asking for more
/// is refused by [`Skin::parse`] rather than silently posing a prefix of
/// itself.
pub const BONE_LIMIT: usize = 8;

/// Bone influences one vertex carries into the vertex stage — the width
/// of the `Uint8x4` / `Unorm8x4` attribute pair the closed vertex format
/// set names for skinning.
///
/// Four is a checked property of every loaded rig. The decoder refuses a
/// row with a fifth meaningful influence instead of silently truncating and
/// renormalising it to fit this format. See [`RigWeights::decode_npy`].
pub const INFLUENCES: usize = 4;

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

    /// The three rows a uniform block carries this map as: each the
    /// linear part's row followed by that axis' translation, so a shader
    /// poses a point by three dot products against `vec4(p, 1)` and a
    /// direction by three against `vec4(v, 0)`.
    ///
    /// Rows rather than the columns the struct holds, because a row is
    /// what a dot product wants and the transposition is free here and
    /// per-vertex there.
    fn rows(&self) -> [[f32; 4]; 3] {
        let [x, y, z] = self.columns;
        let t = self.translation;

        [[x.x, y.x, z.x, t.x], [x.y, y.y, z.y, t.y], [x.z, y.z, z.z, t.z]]
    }
}

/// The bone table a uniform blob carries, as little-endian `f32` lanes:
/// [`BONE_LIMIT`] maps at three `vec4` rows each, identity past the end
/// of the rig.
///
/// Identity rather than zero for the unbound slots. A vertex bound to a
/// slot no rig fills would otherwise collapse to the origin, which is a
/// figure with a spike through it rather than a figure that did not move
/// — and the packer writes slot zero into the joint lanes an influence
/// does not use, so those slots are read on every vertex.
#[must_use]
pub fn bone_uniform(transforms: &[Rigid]) -> [f32; BONE_LIMIT * 12] {
    let mut lanes = [0.0f32; BONE_LIMIT * 12];
    for bone in 0..BONE_LIMIT {
        let rows = transforms.get(bone).copied().unwrap_or(Rigid::IDENTITY).rows();
        for (row, values) in rows.into_iter().enumerate() {
            lanes[bone * 12 + row * 4..bone * 12 + row * 4 + 4].copy_from_slice(&values);
        }
    }

    lanes
}

/// A subject and the rig that binds it — everything a packer needs to
/// hand a rest curve to a vertex stage that will pose it.
///
/// Borrowed for the pack. The mesh is the *rest* sculpt, because that is
/// what an [`Anchorage`] addresses and what the shipped vertex buffer
/// stands in: the pose reaches the GPU as a uniform, so the geometry is
/// the sculpt and never travels again.
#[derive(Clone, Copy)]
pub struct Bound<'a> {
    pub rest: &'a Mesh,
    pub skin: &'a Skin,
}

/// One curve point's binding, as the vertex stage takes it: two corners
/// of the face it sits in, and where between them it sits.
///
/// Two rather than the face's three because every curve carried to the
/// GPU as *rest* geometry is a level set of a per-vertex scalar, and a
/// level set crosses a face along an edge — so the third corner's
/// barycentric share is exactly zero and carrying it would be carrying a
/// zero. A point that genuinely sits inside a face is planted rather
/// than extracted, is re-solved every frame anyway, and arrives through
/// [`Anchored::posed`] with no binding at all.
#[derive(Clone, Copy)]
pub struct Anchored {
    pub positions: [Vec3; 2],
    pub normals: [Vec3; 2],
    pub joints: [[u8; INFLUENCES]; 2],
    pub shares: [[f32; INFLUENCES]; 2],
    /// The second corner's barycentric share; the first takes the rest.
    pub between: f32,
}

impl Anchored {
    /// A point the CPU already posed, standing for itself.
    ///
    /// Both corners are the point and neither carries a share of any
    /// bone, which is what an empty share row means to the vertex stage:
    /// this position is the answer, do not pose it again.
    #[must_use]
    pub fn posed(pos: Vec3, normal: Vec3) -> Self {
        Self {
            positions: [pos; 2],
            normals: [normal; 2],
            joints: [[0; INFLUENCES]; 2],
            shares: [[0.0; INFLUENCES]; 2],
            between: 0.0,
        }
    }
}

impl Bound<'_> {
    /// The two-corner binding at an address on the sculpt: the face's two
    /// heaviest corners, each with its own bone row, and the share
    /// between them.
    ///
    /// Ordered heaviest first and the share renormalised over the pair,
    /// so an edge crossing — where the third share is zero — reproduces
    /// its own rest position exactly under an identity pose.
    #[must_use]
    pub fn anchored(&self, at: Anchorage) -> Anchored {
        let corners = self.rest.faces[at.face as usize];
        let mut ranked: [(u32, f32); 3] = [0, 1, 2].map(|lane| (corners[lane], at.barycentric()[lane]));
        ranked.sort_unstable_by(|a, b| b.1.total_cmp(&a.1));

        let [(first, weight_first), (second, weight_second), ..] = ranked;
        let pair = weight_first + weight_second;
        let between = if pair > 0.0 {
            weight_second / pair
        } else {
            0.0
        };
        let (first, second) = (first as usize, second as usize);
        let (first_joints, first_shares) = self.skin.influences(first);
        let (second_joints, second_shares) = self.skin.influences(second);

        Anchored {
            positions: [self.rest.positions[first], self.rest.positions[second]],
            normals: [self.rest.normals[first], self.rest.normals[second]],
            joints: [first_joints, second_joints],
            shares: [first_shares, second_shares],
            between,
        }
    }
}

/// One declared bone in a rig descriptor.
///
/// `segment` is bake metadata rather than a runtime motor today, but it is
/// still part of `rig.txt`: accepting it without declaring and validating it
/// would preserve the old parser's unknown-data hole.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, aether_data::Schema)]
pub struct RigBone {
    pub name: String,
    pub pivot: Option<[f32; 3]>,
    pub axis: Option<[f32; 3]>,
    pub segment: Option<[[f32; 3]; 2]>,
}

/// The validated in-memory form of `rig.txt`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, aether_data::Kind, aether_data::Schema)]
#[kind(name = "aether.puppet.rig_descriptor")]
pub struct RigDescriptor {
    pub bones: Vec<RigBone>,
    /// How much of the head's rotation the neck takes, so a turn reads as
    /// a neck carrying a head rather than a head swivelling on a post.
    pub neck_share: f32,
}

impl RigDescriptor {
    /// Decode and validate the authored `rig.txt` format.
    ///
    /// Empty lines are harmless. Every non-empty line must be one of the
    /// format's declared records, name a declared bone where applicable,
    /// and contain finite numeric data. Missing `neck_share` retains the
    /// historical authored default of `0.35`; a malformed value never does.
    pub fn decode_text(text: &str) -> Result<Self, String> {
        let mut bones: Option<Vec<RigBone>> = None;
        let mut neck_share = None;

        for (line_index, line) in text.lines().enumerate() {
            let line_number = line_index + 1;
            let fields = line.split_whitespace().collect::<Vec<_>>();
            if fields.is_empty() {
                continue;
            }

            match fields.as_slice() {
                ["bones", names @ ..] if !names.is_empty() => {
                    if bones.is_some() {
                        return Err(format!("rig descriptor line {line_number} repeats the bones table"));
                    }
                    if names.len() > BONE_LIMIT {
                        return Err(format!(
                            "rig descriptor names {} bones, but the puppet carries at most {BONE_LIMIT}",
                            names.len()
                        ));
                    }
                    let mut declared = Vec::with_capacity(names.len());
                    for &name in names {
                        if declared.iter().any(|bone: &RigBone| bone.name == name) {
                            return Err(format!("rig descriptor line {line_number} repeats bone '{name}'"));
                        }
                        declared.push(RigBone { name: name.to_owned(), pivot: None, axis: None, segment: None });
                    }
                    bones = Some(declared);
                }
                ["neck_share", value] => {
                    if neck_share.is_some() {
                        return Err(format!("rig descriptor line {line_number} repeats neck_share"));
                    }
                    let value = descriptor_float(value, line_number, "neck_share")?;
                    if !(0.0..=1.0).contains(&value) {
                        return Err(format!(
                            "rig descriptor line {line_number} neck_share is {value}, expected a share from 0 to 1"
                        ));
                    }
                    neck_share = Some(value);
                }
                [record @ ("pivot" | "axis"), name, x, y, z] => {
                    let values = [
                        descriptor_float(x, line_number, record)?,
                        descriptor_float(y, line_number, record)?,
                        descriptor_float(z, line_number, record)?,
                    ];
                    let bone = descriptor_bone(&mut bones, name, line_number, record)?;
                    let slot = if *record == "pivot" {
                        &mut bone.pivot
                    } else {
                        &mut bone.axis
                    };
                    if slot.is_some() {
                        return Err(format!("rig descriptor line {line_number} repeats {record} for bone '{name}'"));
                    }
                    if *record == "axis" && values.iter().map(|value| value * value).sum::<f32>() <= f32::EPSILON {
                        return Err(format!("rig descriptor line {line_number} gives bone '{name}' a zero axis"));
                    }
                    *slot = Some(values);
                }
                ["seg", name, ax, ay, az, bx, by, bz] => {
                    let segment = [
                        [
                            descriptor_float(ax, line_number, "seg")?,
                            descriptor_float(ay, line_number, "seg")?,
                            descriptor_float(az, line_number, "seg")?,
                        ],
                        [
                            descriptor_float(bx, line_number, "seg")?,
                            descriptor_float(by, line_number, "seg")?,
                            descriptor_float(bz, line_number, "seg")?,
                        ],
                    ];
                    let bone = descriptor_bone(&mut bones, name, line_number, "seg")?;
                    if bone.segment.replace(segment).is_some() {
                        return Err(format!("rig descriptor line {line_number} repeats seg for bone '{name}'"));
                    }
                }
                [record, ..] if matches!(*record, "bones" | "neck_share" | "pivot" | "axis" | "seg") => {
                    return Err(format!("rig descriptor line {line_number} has malformed {record} data"));
                }
                [record, ..] => {
                    return Err(format!("rig descriptor line {line_number} has unknown record '{record}'"));
                }
                [] => unreachable!("empty descriptor lines were skipped"),
            }
        }

        let descriptor = Self {
            bones: bones.ok_or_else(|| "rig descriptor names no bones".to_owned())?,
            neck_share: neck_share.unwrap_or(0.35),
        };
        descriptor.validate()?;

        Ok(descriptor)
    }

    fn validate(&self) -> Result<(), String> {
        if self.bones.is_empty() {
            return Err("rig descriptor names no bones".to_owned());
        }
        if self.bones.len() > BONE_LIMIT {
            return Err(format!(
                "rig descriptor names {} bones, but the puppet carries at most {BONE_LIMIT}",
                self.bones.len()
            ));
        }
        if !self.neck_share.is_finite() || !(0.0..=1.0).contains(&self.neck_share) {
            return Err(format!(
                "rig descriptor neck_share is {}, expected a finite share from 0 to 1",
                self.neck_share
            ));
        }

        for (index, bone) in self.bones.iter().enumerate() {
            if bone.name.is_empty() {
                return Err(format!("rig descriptor bone {index} has an empty name"));
            }
            if self.bones[..index].iter().any(|earlier| earlier.name == bone.name) {
                return Err(format!("rig descriptor repeats bone '{}'", bone.name));
            }
            if bone.pivot.is_some_and(|values| values.iter().any(|value| !value.is_finite())) {
                return Err(format!("rig descriptor bone '{}' has a non-finite pivot", bone.name));
            }
            if bone.axis.is_some_and(|values| values.iter().any(|value| !value.is_finite())) {
                return Err(format!("rig descriptor bone '{}' has a non-finite axis", bone.name));
            }
            if bone.segment.is_some_and(|segment| segment.iter().flatten().any(|value| !value.is_finite())) {
                return Err(format!("rig descriptor bone '{}' has a non-finite segment", bone.name));
            }
            if bone.axis.is_some_and(|axis| axis.iter().map(|value| value * value).sum::<f32>() <= f32::EPSILON) {
                return Err(format!("rig descriptor bone '{}' has a zero axis", bone.name));
            }
        }

        Ok(())
    }
}

fn descriptor_float(value: &str, line: usize, record: &str) -> Result<f32, String> {
    value
        .parse::<f32>()
        .ok()
        .filter(|value| value.is_finite())
        .ok_or_else(|| format!("rig descriptor line {line} has non-finite or malformed {record} number '{value}'"))
}

fn descriptor_bone<'a>(
    bones: &'a mut Option<Vec<RigBone>>,
    name: &str,
    line: usize,
    record: &str,
) -> Result<&'a mut RigBone, String> {
    bones
        .as_mut()
        .ok_or_else(|| format!("rig descriptor line {line} declares {record} before its bones table"))?
        .iter_mut()
        .find(|bone| bone.name == name)
        .ok_or_else(|| format!("rig descriptor line {line} declares {record} for unknown bone '{name}'"))
}

/// Validated per-vertex rig weights.
///
/// `data` keeps the dense little-endian `f32` rows from the authored `NumPy`
/// array behind the declared bytes contract. [`Skin::from_kinds`] decodes the
/// blob once into aligned floats; the pose loop never reads transport bytes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, aether_data::Kind, aether_data::Schema)]
#[kind(name = "aether.puppet.rig_weights")]
pub struct RigWeights {
    pub vertices: u32,
    pub bones: u32,
    pub influences: u32,
    #[serde(with = "aether_data::bytes")]
    pub data: Vec<u8>,
}

impl RigWeights {
    /// Decode the dense `.npy` asset into the declared bytes form.
    ///
    /// Only a `NumPy` 1.0 `<f4`, C-order array shaped exactly
    /// `(vertices, descriptor bones)` is accepted. A row with more than
    /// [`INFLUENCES`] shares above `MINIMUM_SHARE` is refused rather than
    /// truncated.
    pub fn decode_npy(bytes: &[u8], vertices: usize, bones: usize) -> Result<Self, String> {
        let array = npy::parse(bytes).map_err(|error| format!("rig weights refused: {error}"))?;
        if array.descr != "<f4" {
            return Err(format!("rig weights dtype is '{}', expected '<f4'", array.descr));
        }
        if array.fortran_order {
            return Err("rig weights are Fortran-order, expected C-order".to_owned());
        }
        if array.shape.as_slice() != [vertices, bones] {
            return Err(format!(
                "rig weights shape is {:?}, expected ({vertices}, {bones}) for this subject and descriptor",
                array.shape
            ));
        }

        for (vertex, row) in array.payload.chunks_exact(bones * 4).enumerate() {
            let mut live = 0usize;
            let mut total = 0.0f32;
            for (bone, word) in row.chunks_exact(4).enumerate() {
                let weight = f32::from_le_bytes([word[0], word[1], word[2], word[3]]);
                if !weight.is_finite() || weight < 0.0 {
                    return Err(format!(
                        "rig weights vertex {vertex} bone {bone} has invalid weight {weight}; expected a finite non-negative share"
                    ));
                }
                if weight > MINIMUM_SHARE {
                    live += 1;
                }
                total += weight;
            }
            if live == 0 {
                return Err(format!("rig weights vertex {vertex} has no share above the minimum {MINIMUM_SHARE}"));
            }
            if live > INFLUENCES {
                return Err(format!(
                    "rig weights vertex {vertex} has {live} influences above the minimum {MINIMUM_SHARE}, but the puppet carries at most {INFLUENCES}"
                ));
            }
            if (total - 1.0).abs() > 1.0e-5 {
                return Err(format!("rig weights vertex {vertex} shares sum to {total}, expected 1"));
            }
        }

        Ok(Self {
            vertices: u32::try_from(vertices).map_err(|_| "rig weights vertex count exceeds u32".to_owned())?,
            bones: u32::try_from(bones).map_err(|_| "rig weights bone count exceeds u32".to_owned())?,
            influences: INFLUENCES as u32,
            data: array.payload.to_vec(),
        })
    }

    /// Validate the declared header and blob while decoding its bytes once
    /// into the aligned dense rows the pose loop owns.
    fn decode_dense(&self, vertices: usize, bones: usize) -> Result<Vec<f32>, String> {
        if self.vertices as usize != vertices {
            return Err(format!(
                "rig weights declare {} vertices, expected {vertices} for this subject",
                self.vertices
            ));
        }
        if self.bones as usize != bones {
            return Err(format!("rig weights declare {} bones, expected {bones} from the descriptor", self.bones));
        }
        if self.influences as usize != INFLUENCES {
            return Err(format!(
                "rig weights declare {} influences, but the puppet vertex format carries {INFLUENCES}",
                self.influences
            ));
        }
        let expected = vertices
            .checked_mul(bones)
            .and_then(|values| values.checked_mul(size_of::<f32>()))
            .ok_or_else(|| "rig weights dense payload length overflows usize".to_owned())?;
        if self.data.len() != expected {
            return Err(format!("rig weights dense payload is {} bytes, expected {expected}", self.data.len()));
        }

        let mut weights = Vec::with_capacity(vertices * bones);
        for (vertex, packed) in self.data.chunks_exact(bones * size_of::<f32>()).enumerate() {
            let mut live = 0usize;
            let mut total = 0.0f32;
            for (bone, word) in packed.chunks_exact(size_of::<f32>()).enumerate() {
                let share = f32::from_le_bytes([word[0], word[1], word[2], word[3]]);
                if !share.is_finite() || share < 0.0 {
                    return Err(format!(
                        "rig weights vertex {vertex} bone {bone} has invalid weight {share}; expected a finite non-negative share"
                    ));
                }
                if share > MINIMUM_SHARE {
                    live += 1;
                }
                total += share;
                weights.push(share);
            }
            if live == 0 {
                return Err(format!("rig weights vertex {vertex} has no share above the minimum {MINIMUM_SHARE}"));
            }
            if live > INFLUENCES {
                return Err(format!(
                    "rig weights vertex {vertex} has {live} influences above the minimum {MINIMUM_SHARE}, but the puppet carries at most {INFLUENCES}"
                ));
            }
            if (total - 1.0).abs() > 1.0e-5 {
                return Err(format!("rig weights vertex {vertex} shares sum to {total}, expected 1"));
            }
        }

        Ok(weights)
    }
}

/// The rig, and the per-vertex weights that bind the sculpt to it.
pub struct Skin {
    weights: Vec<f32>,
    descriptor: RigDescriptor,
}

impl Skin {
    /// Read the two legacy disk assets into their declared in-memory kinds.
    pub fn parse(weights: &[u8], descriptor: &str, vertices: usize) -> Result<Self, String> {
        let descriptor = RigDescriptor::decode_text(descriptor)?;
        let weights = RigWeights::decode_npy(weights, vertices, descriptor.bones.len())?;

        Self::from_kinds(weights, descriptor, vertices)
    }

    /// Build a skin from already-decoded declared kinds, checking their
    /// cross-kind and subject invariants again at the ownership boundary.
    pub fn from_kinds(weights: RigWeights, descriptor: RigDescriptor, vertices: usize) -> Result<Self, String> {
        descriptor.validate()?;
        let weights = weights.decode_dense(vertices, descriptor.bones.len())?;

        Ok(Self { weights, descriptor })
    }

    pub fn descriptor(&self) -> &RigDescriptor {
        &self.descriptor
    }

    pub fn bones(&self) -> usize {
        self.descriptor.bones.len()
    }

    /// One vertex's bone binding as the vertex stage takes it: the
    /// [`INFLUENCES`] heaviest bones and their shares, renormalised so
    /// the four sum to one.
    ///
    /// Exact rather than approximate on this rig, which is why the
    /// sparse attribute pair was chosen over carrying the dense row. The
    /// solver leaves at most four non-zero weights per vertex — measured
    /// over every vertex of the shipped subject, ears and jaw included —
    /// so what is dropped here is a run of exact zeroes and the
    /// renormalisation divides by one.
    ///
    /// The shares come back as floats; the caller quantises them into
    /// the `Unorm8x4` lane and the shader renormalises again after the
    /// quantisation, which is what keeps the blend an affine partition
    /// rather than a sum that drifts off one by a part in 255.
    #[must_use]
    pub fn influences(&self, vertex: usize) -> ([u8; INFLUENCES], [f32; INFLUENCES]) {
        let row = &self.weights[vertex * self.bones()..(vertex + 1) * self.bones()];
        let mut ranked = [(0u8, 0.0f32); BONE_LIMIT];
        let held = row.len().min(BONE_LIMIT);
        for (bone, &weight) in row.iter().enumerate().take(held) {
            ranked[bone] = (bone as u8, weight);
        }
        ranked[..held].sort_unstable_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));

        let kept = held.min(INFLUENCES);
        let total: f32 = ranked[..kept].iter().map(|&(_, weight)| weight).sum();
        let (mut joints, mut shares) = ([0u8; INFLUENCES], [0.0f32; INFLUENCES]);
        for (lane, &(joint, share)) in ranked[..kept].iter().enumerate() {
            joints[lane] = joint;
            shares[lane] = if total > 0.0 {
                share / total
            } else {
                0.0
            };
        }

        (joints, shares)
    }

    /// One vertex's authored weights in bone-table order, padded to the
    /// whole table the pose uniform carries.
    ///
    /// The sparse vertex-stage binding above deliberately reorders and
    /// quantises the live shares. Silhouette classification cannot use that
    /// spelling: a last-bit normal change can move a zero crossing to the
    /// other side of an edge. The resident compute input therefore keeps
    /// the same dense, bone-order floats [`Self::at_vertex`] reads on the
    /// CPU, including sub-threshold values that the pose loop itself decides
    /// whether to skip.
    pub(super) fn dense_weights(&self, vertex: usize) -> [f32; BONE_LIMIT] {
        let mut dense = [0.0; BONE_LIMIT];
        let row = &self.weights[vertex * self.bones()..(vertex + 1) * self.bones()];
        dense[..row.len()].copy_from_slice(row);

        dense
    }

    fn pivot(&self, name: &str) -> Vec3 {
        self.descriptor
            .bones
            .iter()
            .find(|bone| bone.name == name)
            .and_then(|bone| bone.pivot)
            .map_or(Vec3::ZERO, Vec3::from_array)
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

        self.descriptor
            .bones
            .iter()
            .map(|bone| {
                let pivot = bone.pivot.map_or(Vec3::ZERO, Vec3::from_array);
                match bone.name.as_str() {
                    "neck" => Rigid::sample(|p| head(p, self.descriptor.neck_share)),
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
                        let axis = bone.axis.map_or(Vec3::Y, Vec3::from_array);

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
        let row = &self.weights[vertex * self.bones()..(vertex + 1) * self.bones()];

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

/// `.npy` framing around `values`: the magic, a version, a header length
/// and a header of that length.
///
/// Beside the reader it feeds rather than inside one test module,
/// because the packers' own tests need a rig too and a second
/// transcription of this framing is a second thing to get wrong.
#[cfg(test)]
pub(crate) fn npy(values: &[f32], shape: (usize, usize)) -> Vec<u8> {
    assert_eq!(shape.0.checked_mul(shape.1), Some(values.len()), "fixture shape must match its values");
    let dictionary = format!("{{'descr': '<f4', 'fortran_order': False, 'shape': ({}, {}), }}", shape.0, shape.1);
    let padding = (16 - ((10 + dictionary.len() + 1) % 16)) % 16;
    let mut header = dictionary;
    header.extend(iter::repeat_n(' ', padding));
    header.push('\n');

    let mut bytes = b"\x93NUMPY\x01\x00".to_vec();
    bytes.extend(u16::try_from(header.len()).expect("a short fixture header").to_le_bytes());
    bytes.extend(header.as_bytes());
    bytes.extend(values.iter().flat_map(|value| value.to_le_bytes()));

    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    use aether_data::{Kind, Schema, SchemaType};

    use crate::feature::{FeatureClass, Pen, SurfacePoint};
    use crate::weld;

    /// A rig with one posable bone, and weights binding two vertices to it
    /// at full and half share.
    fn descriptor() -> &'static str {
        "bones chest head\nneck_share 0.35\npivot head 0.0 0.0 0.0\n"
    }

    fn weight_array(descr: &str, fortran_order: bool, shape: (usize, usize), values: &[f32]) -> Vec<u8> {
        assert_eq!(shape.0.checked_mul(shape.1), Some(values.len()), "fixture shape must match its values");
        let dictionary = format!(
            "{{'descr': '{descr}', 'fortran_order': {}, 'shape': ({}, {}), }}",
            if fortran_order {
                "True"
            } else {
                "False"
            },
            shape.0,
            shape.1,
        );
        let padding = (16 - ((10 + dictionary.len() + 1) % 16)) % 16;
        let mut header = dictionary;
        header.extend(iter::repeat_n(' ', padding));
        header.push('\n');

        let mut bytes = b"\x93NUMPY\x01\x00".to_vec();
        bytes.extend(u16::try_from(header.len()).expect("a short fixture header").to_le_bytes());
        bytes.extend(header.as_bytes());
        bytes.extend(values.iter().flat_map(|value| value.to_le_bytes()));
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
        let weights = npy(&[1.0, 0.0, 0.7, 0.3, 0.4, 0.6, 0.0, 1.0], (4, 2));

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
        let weights = npy(&[1.0, 0.0, 0.5, 0.5], (2, 2));

        assert!(Skin::parse(&weights, descriptor(), 2).is_ok(), "two vertices by two bones is this subject");
        assert_eq!(
            Skin::parse(&weights, descriptor(), 3).err().as_deref(),
            Some("rig weights shape is [2, 2], expected (3, 2) for this subject and descriptor"),
        );
    }

    #[test]
    fn rig_weight_metadata_and_truncation_are_diagnostic() {
        let values = [1.0, 0.0, 0.5, 0.5];
        let wrong_dtype = weight_array(">f4", false, (2, 2), &values);
        let wrong_order = weight_array("<f4", true, (2, 2), &values);
        let wrong_shape = weight_array("<f4", false, (1, 4), &values);
        let mut truncated = npy(&values, (2, 2));
        truncated.pop();

        assert_eq!(
            Skin::parse(&wrong_dtype, descriptor(), 2).err().as_deref(),
            Some("rig weights dtype is '>f4', expected '<f4'"),
        );
        assert_eq!(
            Skin::parse(&wrong_order, descriptor(), 2).err().as_deref(),
            Some("rig weights are Fortran-order, expected C-order"),
        );
        assert_eq!(
            Skin::parse(&wrong_shape, descriptor(), 2).err().as_deref(),
            Some("rig weights shape is [1, 4], expected (2, 2) for this subject and descriptor"),
        );
        assert_eq!(
            Skin::parse(&truncated, descriptor(), 2).err().as_deref(),
            Some("rig weights refused: NumPy payload is 15 bytes, expected 16 from shape and dtype"),
        );
    }

    #[test]
    fn the_disk_descriptor_decodes_to_a_declared_kind() {
        let text = "bones chest head ear_left\n\
                    neck_share 0.4\n\
                    pivot head 0.0 0.3 -0.1\n\
                    seg head 0.0 0.2 0.0 0.0 0.8 0.0\n\
                    axis ear_left 0.0 1.0 0.0\n";
        let descriptor = RigDescriptor::decode_text(text).expect("the authored descriptor decodes");

        assert_eq!(RigDescriptor::NAME, "aether.puppet.rig_descriptor");
        assert_eq!(descriptor.neck_share, 0.4);
        assert_eq!(
            descriptor.bones.iter().map(|bone| bone.name.as_str()).collect::<Vec<_>>(),
            ["chest", "head", "ear_left"]
        );
        assert_eq!(descriptor.bones[1].pivot, Some([0.0, 0.3, -0.1]));
        assert_eq!(descriptor.bones[1].segment, Some([[0.0, 0.2, 0.0], [0.0, 0.8, 0.0]]));
        assert_eq!(descriptor.bones[2].axis, Some([0.0, 1.0, 0.0]));
    }

    #[test]
    fn the_descriptor_refuses_unknown_malformed_or_ambiguous_data() {
        let cases = [
            ("bones chest head\npivto head 0 0 0\n", "unknown record 'pivto'"),
            ("bones chest head\npivot head nope 0 0\n", "malformed pivot number 'nope'"),
            ("bones chest head\npivot missing 0 0 0\n", "pivot for unknown bone 'missing'"),
            ("pivot head 0 0 0\nbones chest head\n", "pivot before its bones table"),
            ("bones chest head\nbones chest head\n", "repeats the bones table"),
            ("bones chest chest\n", "repeats bone 'chest'"),
            ("bones chest head\npivot head 0 0 0\npivot head 0 0 0\n", "repeats pivot for bone 'head'"),
            ("bones chest ear_left\naxis ear_left 0 0 0\n", "gives bone 'ear_left' a zero axis"),
            ("bones chest head\nneck_share 1.1\n", "expected a share from 0 to 1"),
            ("bones chest head\nneck_share NaN\n", "non-finite or malformed neck_share number 'NaN'"),
            ("bones chest head\nseg head 0 0 0 1 1\n", "malformed seg data"),
        ];

        for (text, wanted) in cases {
            let error = RigDescriptor::decode_text(text).expect_err(text);
            assert!(error.contains(wanted), "{text:?} produced {error:?}, expected it to contain {wanted:?}");
        }
    }

    #[test]
    fn dense_bytes_decode_once_to_aligned_runtime_floats() {
        let row = [0.2f32, 0.6, 0.0, 0.2];
        let weights = RigWeights::decode_npy(&npy(&row, (1, row.len())), 1, row.len()).expect("a valid dense row");

        assert_eq!(RigWeights::NAME, "aether.puppet.rig_weights");
        assert_eq!((weights.vertices, weights.bones, weights.influences), (1, 4, 4));
        assert_eq!(weights.data.len(), row.len() * size_of::<f32>());

        let descriptor = RigDescriptor::decode_text("bones a b c d\n").expect("four declared bones");
        let skin = Skin::from_kinds(weights, descriptor, 1).expect("the kinds agree");
        assert_eq!(skin.weights, row);
        assert_eq!((skin.weights.as_ptr() as usize) % align_of::<f32>(), 0);

        let (joints, shares) = skin.influences(0);
        assert_eq!(joints, [1, 0, 3, 2], "heaviest first, then stable bone order");
        assert!((shares[0] - 0.6).abs() < 1e-6);
        assert!((shares[1] - 0.2).abs() < 1e-6);
        assert!((shares[2] - 0.2).abs() < 1e-6);
        assert_eq!(shares[3], 0.0);
    }

    #[test]
    fn weight_decode_refuses_invalid_or_overwide_rows() {
        let cases: [(&[f32], &str); 5] = [
            (&[0.0, 0.0], "has no share above the minimum"),
            (&[1.0, -0.1], "invalid weight -0.1"),
            (&[1.0, f32::NAN], "invalid weight NaN"),
            (&[1.0, f32::INFINITY], "invalid weight inf"),
            (&[0.3, 0.25, 0.2, 0.15, 0.1], "has 5 influences"),
        ];

        for (row, wanted) in cases {
            let error = RigWeights::decode_npy(&npy(row, (1, row.len())), 1, row.len()).expect_err(wanted);
            assert!(error.contains(wanted), "{row:?} produced {error:?}, expected it to contain {wanted:?}");
        }
    }

    #[test]
    fn typed_weight_headers_and_payloads_are_revalidated_at_the_skin_boundary() {
        let descriptor = RigDescriptor::decode_text("bones chest head\n").expect("two bones");
        let valid = RigWeights::decode_npy(&npy(&[0.25, 0.75], (1, 2)), 1, 2).expect("valid weights");

        let mut truncated = valid.clone();
        truncated.data.pop();
        assert!(
            Skin::from_kinds(truncated, descriptor.clone(), 1)
                .err()
                .expect("the truncated blob is refused")
                .contains("7 bytes, expected 8")
        );

        let mut invalid_share = valid.clone();
        invalid_share.data[..4].copy_from_slice(&f32::NAN.to_le_bytes());
        assert!(
            Skin::from_kinds(invalid_share, descriptor.clone(), 1)
                .err()
                .expect("the invalid share is refused")
                .contains("invalid weight NaN")
        );

        let mut non_normalized = valid;
        non_normalized.data[..4].copy_from_slice(&0.5f32.to_le_bytes());
        assert!(
            Skin::from_kinds(non_normalized, descriptor, 1)
                .err()
                .expect("the non-normalized row is refused")
                .contains("shares sum to 1.25")
        );
    }

    #[test]
    fn the_declared_influence_width_is_validated_at_the_skin_boundary() {
        let descriptor = RigDescriptor::decode_text("bones chest head\n").expect("two bones");
        let mut weights = RigWeights::decode_npy(&npy(&[0.25, 0.75], (1, 2)), 1, 2).expect("valid weights");
        weights.influences = 3;

        assert_eq!(
            Skin::from_kinds(weights, descriptor, 1).err().as_deref(),
            Some("rig weights declare 3 influences, but the puppet vertex format carries 4"),
        );
    }

    #[test]
    fn rig_kind_schemas_expose_the_header_blob_and_bone_table() {
        let SchemaType::Struct { fields, .. } = RigWeights::SCHEMA else {
            panic!("rig weights must be a structured kind");
        };
        assert_eq!(
            fields.iter().map(|field| field.name.as_ref()).collect::<Vec<_>>(),
            ["vertices", "bones", "influences", "data"]
        );
        assert!(matches!(fields[3].ty, SchemaType::Bytes));

        let SchemaType::Struct { fields, .. } = RigDescriptor::SCHEMA else {
            panic!("rig descriptor must be a structured kind");
        };
        assert_eq!(fields.iter().map(|field| field.name.as_ref()).collect::<Vec<_>>(), ["bones", "neck_share"]);
    }

    #[test]
    fn legacy_valid_rows_keep_their_bindings_and_pose() {
        let rows = [[0.5f32, 0.3, 0.0, 0.2, 0.0], [0.0, 0.1, 0.2, 0.3, 0.4]];
        let dense = rows.into_iter().flatten().collect::<Vec<_>>();
        let skin =
            Skin::parse(&npy(&dense, (2, 5)), "bones chest neck head jaw ear_left\n", 2).expect("valid legacy rig");
        assert_eq!(skin.weights, dense, "the declared boundary keeps the exact authored dense rows");

        for (vertex, row) in rows.iter().enumerate() {
            let mut ranked =
                row.iter().copied().enumerate().map(|(bone, weight)| (bone as u8, weight)).collect::<Vec<_>>();
            ranked.sort_unstable_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
            let kept = ranked.len().min(INFLUENCES);
            let total: f32 = ranked[..kept].iter().map(|&(_, weight)| weight).sum();
            let mut expected_joints = [0; INFLUENCES];
            let mut expected_shares = [0.0; INFLUENCES];
            for (lane, &(joint, share)) in ranked[..kept].iter().enumerate() {
                expected_joints[lane] = joint;
                expected_shares[lane] = share / total;
            }
            assert_eq!(skin.influences(vertex), (expected_joints, expected_shares));
        }

        let transforms = skin.transforms(&quarter_turn());
        let point = Vec3::new(0.2, 0.7, 1.1);
        for (vertex, row) in rows.iter().enumerate() {
            let legacy = row.iter().zip(&transforms).fold(Rigid::ZERO, |blend, (&share, transform)| {
                if share <= MINIMUM_SHARE {
                    blend
                } else {
                    blend.add_scaled(transform, share)
                }
            });
            assert!((skin.at_vertex(&transforms, vertex).point(point) - legacy.point(point)).length() < 1e-6);
        }
    }

    #[test]
    fn non_unit_dense_rows_are_refused() {
        assert_eq!(
            Skin::parse(&npy(&[2.0, 1.0], (1, 2)), "bones chest head\n", 1).err().as_deref(),
            Some("rig weights vertex 0 shares sum to 3, expected 1"),
        );
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
        let skin = Skin::parse(&npy(&[1.0, 0.0, 0.5, 0.5], (2, 2)), descriptor(), 2).expect("a two-vertex rig");
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
        let skin = Skin::parse(&npy(&[0.0, 1.0, 0.5, 0.5], (2, 2)), descriptor(), 2).expect("a two-vertex rig");
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

    /// The blend the vertex stage performs, in Rust: pose each of the two
    /// corners by its own bone row, then interpolate between them.
    ///
    /// A transcription of `skin.wgsl`'s `anchored_point`, and the only
    /// way this side can hold that side to the order it claims. The
    /// shares are the unquantised ones — what is under test is which
    /// arithmetic runs in which order, not the eight-bit lane it rides
    /// in.
    fn as_the_vertex_stage_would(anchored: &Anchored, transforms: &[Rigid]) -> Vec3 {
        let corner = |lane: usize| {
            let (joints, shares) = (anchored.joints[lane], anchored.shares[lane]);
            let total: f32 = shares.iter().sum();
            if total <= 0.0 {
                return anchored.positions[lane];
            }
            let posed = (0..INFLUENCES).fold(Vec3::ZERO, |sum, influence| {
                sum + transforms[joints[influence] as usize].point(anchored.positions[lane]) * shares[influence]
            });

            posed / total
        };

        corner(0) * (1.0 - anchored.between) + corner(1) * anchored.between
    }

    /// Tripwire: the two-corner binding the vertex stage is handed poses
    /// a curve point exactly where the CPU pass poses it.
    ///
    /// This is [`Skin::pose_curves`]' claim carried across to the GPU
    /// path, and it is two claims at once. The binding has to name the
    /// *right* two corners with the right share, or every hatch point
    /// addresses a different place on its own face — a drawing that is
    /// still a drawing and is on the wrong part of the surface. And the
    /// blend has to pose each corner before interpolating: the other
    /// order costs the same arithmetic, is exactly right at a vertex, and
    /// under a shear puts the point off the posed triangle by a visible
    /// fraction of the deformation. The strip shears rather than turning
    /// rigidly, which is what makes both bite.
    #[test]
    fn the_two_corner_binding_poses_a_point_where_the_curve_pass_does() {
        let (rest, skin) = strip();
        let transforms = skin.transforms(&quarter_turn());
        let bound = Bound { rest: &rest, skin: &skin };

        let heights: Vec<f32> = rest.positions.iter().map(|p| p.y).collect();
        let crossings = rest.level_set(&heights, &[], 0.5);
        assert!(!crossings.is_empty(), "the level set crosses the strip");

        let mut posed = rest.deformable();
        skin.pose_surface(&transforms, &rest, &mut posed.positions, &mut posed.normals);

        let mut moved = 0.0f32;
        for crossing in crossings.into_iter().flatten() {
            let anchored = bound.anchored(crossing.at);
            assert!(
                (as_the_vertex_stage_would(&anchored, &[Rigid::IDENTITY; BONE_LIMIT]) - crossing.pos).length() < 1e-5,
                "the binding does not reproduce its own rest position",
            );

            let staged = as_the_vertex_stage_would(&anchored, &transforms);
            let on_surface = posed.at(crossing.at);
            assert!(
                (staged - on_surface).length() < 1e-5,
                "the vertex stage's blend puts the point at {staged:?} while its surface went to {on_surface:?}",
            );
            moved = moved.max((staged - crossing.pos).length());
        }
        assert!(moved > 0.1, "the pose has to actually move the curve, and it moved it {moved}");
    }

    /// Tripwire: a rig with more bones than the uniform blob carries is
    /// refused.
    ///
    /// The blob is a fixed table of [`BONE_LIMIT`] maps and a joint index
    /// is an eight-bit lane, so nothing about a ninth bone fails loudly:
    /// the packer would emit index eight, the shader would read past the
    /// array's end, and WGSL's own bounds behaviour would hand back some
    /// other bone's row. That poses her by a transform nobody wrote, on
    /// whichever vertices the ninth bone happened to own.
    #[test]
    fn a_rig_past_the_uniform_s_bone_slots_is_refused() {
        let names = (0..=BONE_LIMIT).map(|bone| format!("b{bone}")).collect::<Vec<String>>().join(" ");
        let wide = format!("bones {names}\n");
        let fits = format!("bones {}\n", names.rsplit_once(' ').expect("more than one bone").0);

        let mut fitting_row = [0.0; BONE_LIMIT];
        fitting_row[0] = 1.0;
        assert!(Skin::parse(&npy(&fitting_row, (1, BONE_LIMIT)), &fits, 1).is_ok(), "a rig the blob has slots for");
        assert_eq!(
            Skin::parse(&npy(&[0.0; BONE_LIMIT + 1], (1, BONE_LIMIT + 1)), &wide, 1).err().as_deref(),
            Some("rig descriptor names 9 bones, but the puppet carries at most 8"),
        );
    }

    /// Tripwire: every influence the solver produced survives the sparse
    /// attribute.
    ///
    /// [`Skin::influences`] keeps the four heaviest bones because the
    /// vertex format carries four, and the claim that this is lossless is
    /// a claim about the *rig* rather than about the code — so it is
    /// checked against a row that fills all four and one that would
    /// overflow them. The dropped mass is what a silent truncation would
    /// cost, and the latter is refused rather than assumed.
    #[test]
    fn the_sparse_influences_carry_the_whole_row_when_it_fits() {
        let names = ["a", "b", "c", "d", "e"];
        let descriptor = format!("bones {}\n", names.join(" "));
        let row = [0.4f32, 0.3, 0.2, 0.1, 0.0];
        let skin = Skin::parse(&npy(&row, (1, row.len())), &descriptor, 1).expect("a one-vertex rig");

        let (joints, shares) = skin.influences(0);
        assert_eq!(joints, [0, 1, 2, 3], "the four heaviest bones, heaviest first");
        for (lane, share) in shares.iter().enumerate() {
            assert!((share - row[lane]).abs() < 1e-6, "share {lane} arrived as {share}");
        }

        let spread = [0.3f32, 0.25, 0.2, 0.15, 0.1];
        assert_eq!(
            Skin::parse(&npy(&spread, (1, spread.len())), &descriptor, 1).err().as_deref(),
            Some("rig weights vertex 0 has 5 influences above the minimum 0.0001, but the puppet carries at most 4"),
        );
    }
}
