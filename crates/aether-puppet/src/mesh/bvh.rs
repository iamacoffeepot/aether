//! A bounding-volume hierarchy over the subject's triangles.
//!
//! Every visible point has to prove nothing stands between it and the
//! eye, and against 868k triangles the linear answer is hopeless. Only
//! *any*-hit is ever asked, never nearest-hit, so traversal returns the
//! moment it finds a blocker.
//!
//! Binned surface-area heuristic on the widest centroid axis, falling back
//! to a median split where the heuristic has nothing to say. The query
//! *is* the bottleneck — a frame casts tens of thousands of these rays, and
//! what they cost is the number of nodes they open, so the tree earns a
//! slower build.

use std::{
    array,
    f32::consts::{FRAC_PI_2, PI},
    sync::Arc,
};

use aether_math::{Rigid, Vec3};

use crate::deform::{BONE_LIMIT, RigidBounds, RigidDelta, Skin, relative_bounds, silhouette_bounds};

const LEAF_SIZE: usize = 8;
const SILHOUETTE_DIRECT_SIZE: u32 = 256;

/// Buckets the split candidates are drawn from. Sixteen is the usual
/// knee: the tree stops improving measurably past it and the build is
/// linear in this.
const BINS: usize = 16;

/// Half the surface area of a box — the relative term is all the
/// heuristic compares, so the halving cancels. Negative extents (an empty
/// bin) clamp to zero rather than reading as area.
fn half_area(min: Vec3, max: Vec3) -> f32 {
    let (x, y, z) = ((max.x - min.x).max(0.0), (max.y - min.y).max(0.0), (max.z - min.z).max(0.0));

    x * y + y * z + z * x
}

fn expand(bounds: (Vec3, Vec3), with: (Vec3, Vec3)) -> (Vec3, Vec3) {
    let (min, max) = bounds;
    let (lo, hi) = with;

    (
        Vec3::new(min.x.min(lo.x), min.y.min(lo.y), min.z.min(lo.z)),
        Vec3::new(max.x.max(hi.x), max.y.max(hi.y), max.z.max(hi.z)),
    )
}

const EMPTY: (Vec3, Vec3) = (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN));

#[derive(Clone, Copy)]
struct Sphere {
    centre: Vec3,
    radius: f32,
}

#[derive(Clone, Copy)]
struct Cone {
    axis: Vec3,
    sin_half_angle: f32,
    cos_half_angle: f32,
}

impl Cone {
    fn around(axis: Vec3, half_angle: f32) -> Self {
        if !finite(axis) || !half_angle.is_finite() || half_angle < 0.0 {
            return Self::uncertain();
        }
        let (sin_half_angle, cos_half_angle) = half_angle.min(PI).sin_cos();

        Self { axis, sin_half_angle, cos_half_angle }
    }

    fn uncertain() -> Self {
        Self { axis: Vec3::Y, sin_half_angle: 1.0, cos_half_angle: 0.0 }
    }
}

#[derive(Clone, Copy)]
struct Node {
    min: Vec3,
    max: Vec3,
    /// For a leaf, the first index into `order`; for an interior node,
    /// the index of the left child.
    start: u32,
    /// Triangle count for a leaf, `0` for an interior node.
    count: u32,
    sphere: Sphere,
    cone: Cone,
}

#[derive(Clone)]
struct Topology {
    nodes: Vec<Node>,
    spans: Vec<Span>,
    order: Vec<u32>,
}

#[derive(Clone, Copy)]
struct Span {
    start: u32,
    count: u32,
}

#[derive(Clone, Copy, Default)]
struct Binding {
    bones: u8,
    weight_error: f32,
}

#[derive(Clone, Copy)]
struct MaskBounds {
    valid: bool,
    reference: Rigid,
    stretch: f32,
    displacement: f32,
    translation: f32,
    sin_rotation: f32,
    cos_rotation: f32,
}

struct PoseBounds {
    masks: [MaskBounds; 1 << BONE_LIMIT],
}

impl PoseBounds {
    fn new(transforms: &[Rigid]) -> Self {
        let bones: [Option<(Rigid, RigidBounds)>; BONE_LIMIT] = array::from_fn(|bone| {
            transforms
                .get(bone)
                .copied()
                .and_then(|transform| silhouette_bounds(&transform).map(|bounds| (transform, bounds)))
        });
        let options: [Option<Rigid>; BONE_LIMIT] = array::from_fn(|bone| bones[bone].map(|v| v.0));
        let deltas: [[Option<RigidDelta>; BONE_LIMIT]; BONE_LIMIT] = array::from_fn(|from| {
            array::from_fn(|to| options[from].zip(options[to]).and_then(|(from, to)| relative_bounds(&from, &to)))
        });
        let masks = array::from_fn(|mask| {
            if mask == 0 {
                return MaskBounds {
                    valid: true,
                    reference: Rigid::IDENTITY,
                    stretch: 1.0,
                    displacement: 0.0,
                    translation: 0.0,
                    sin_rotation: 0.0,
                    cos_rotation: 1.0,
                };
            }
            if (0..BONE_LIMIT).any(|bone| mask & (1 << bone) != 0 && bones[bone].is_none()) {
                return MaskBounds {
                    valid: false,
                    reference: Rigid::IDENTITY,
                    stretch: 0.0,
                    displacement: 0.0,
                    translation: 0.0,
                    sin_rotation: 1.0,
                    cos_rotation: 0.0,
                };
            }

            let active = |bone: usize| mask & (1 << bone) != 0;
            let mut best = (0usize, f32::INFINITY);
            for reference in (0..BONE_LIMIT).filter(|&bone| active(bone)) {
                let mut spread = 0.0f32;
                for other in (0..BONE_LIMIT).filter(|&bone| active(bone)) {
                    spread = spread.max(deltas[reference][other].expect("active transforms were validated").rotation);
                }
                if spread < best.1 {
                    best = (reference, spread);
                }
            }
            let (sin_rotation, cos_rotation) = best.1.min(FRAC_PI_2).sin_cos();
            let mut stretch = 0.0f32;
            let mut displacement = 0.0f32;
            let mut translation = 0.0f32;
            for bone in (0..BONE_LIMIT).filter(|&bone| active(bone)) {
                let delta = deltas[best.0][bone].expect("active transforms were validated");
                stretch = stretch.max(bones[bone].expect("active bone").1.stretch);
                displacement = displacement.max(delta.displacement);
                translation = translation.max(delta.translation);
            }

            MaskBounds {
                valid: true,
                reference: options[best.0].expect("best reference came from the active mask"),
                stretch,
                displacement,
                translation,
                sin_rotation,
                cos_rotation,
            }
        });

        Self { masks }
    }

    fn at(&self, bones: u8) -> MaskBounds {
        self.masks[bones as usize]
    }
}

/// One triangle in the form the ray test actually reads it: a corner and
/// the two edges leaving it.
///
/// Held beside the nodes in leaf order rather than looked up through
/// `order` and `faces`, because the lookup is three dependent random
/// gathers — the index, the face, then each of its corners out of a
/// 2.6 MB position array — and a leaf pays them for every triangle it
/// tests. Laid out this way a leaf is two or three sequential cache
/// lines. The edges are the same subtraction the test used to do per
/// ray, hoisted to build time, so the arithmetic downstream is
/// unchanged bit for bit.
struct Tri {
    a: Vec3,
    e1: Vec3,
    e2: Vec3,
}

pub struct Bvh {
    topology: Arc<Topology>,
    tris: Vec<Tri>,
}

/// The reusable silhouette half of the face hierarchy.
///
/// Posed meshes share the rest topology and carry only the node bindings
/// needed to conservatively move its bounds. The ray-only triangle cache
/// stays on [`Bvh`], so cloning this never enables posed occlusion.
#[derive(Clone)]
pub(crate) struct SilhouetteBvh {
    topology: Arc<Topology>,
    bindings: Arc<[Binding]>,
}

pub(crate) struct PrunedFaces {
    bits: Vec<u64>,
}

impl PrunedFaces {
    pub(crate) fn contains(&self, face: usize) -> bool {
        self.bits[face / u64::BITS as usize] & (1 << (face % u64::BITS as usize)) != 0
    }
}

fn bounds_of(centres: &[Vec3], order: &[u32]) -> (Vec3, Vec3) {
    order.iter().fold((Vec3::splat(f32::MAX), Vec3::splat(f32::MIN)), |(min, max), &i| {
        let c = centres[i as usize];
        (
            Vec3::new(min.x.min(c.x), min.y.min(c.y), min.z.min(c.z)),
            Vec3::new(max.x.max(c.x), max.y.max(c.y), max.z.max(c.z)),
        )
    })
}

fn finite(v: Vec3) -> bool {
    v.x.is_finite() && v.y.is_finite() && v.z.is_finite()
}

fn node_sphere(positions: &[Vec3], faces: &[[u32; 3]], order: &[u32], min: Vec3, max: Vec3) -> Sphere {
    let centre = (min + max) * 0.5;
    if !finite(centre) {
        return Sphere { centre: Vec3::ZERO, radius: f32::INFINITY };
    }
    let mut radius = 0.0f32;
    for position in order.iter().flat_map(|&face| faces[face as usize]).map(|corner| positions[corner as usize]) {
        if !finite(position) {
            return Sphere { centre, radius: f32::INFINITY };
        }
        radius = radius.max((position - centre).length());
    }

    Sphere { centre, radius: radius * (1.0 + 8.0 * f32::EPSILON) }
}

fn node_cone(normals: &[Vec3], faces: &[[u32; 3]], order: &[u32]) -> Cone {
    let mut sum = Vec3::ZERO;
    let mut held = 0usize;
    for normal in order.iter().flat_map(|&face| faces[face as usize]).map(|corner| normals[corner as usize]) {
        if !finite(normal) {
            return Cone::uncertain();
        }
        sum += normal;
        held += 1;
    }
    if held == 0 || !finite(sum) || sum.length_squared() < 1e-12 {
        return Cone::uncertain();
    }

    let axis = sum.normalize();
    let half_angle = order
        .iter()
        .flat_map(|&face| faces[face as usize])
        .map(|corner| axis.dot(normals[corner as usize]).clamp(-1.0, 1.0).acos())
        .fold(0.0f32, f32::max);

    Cone::around(axis, half_angle + 16.0 * f32::EPSILON)
}

/// Partition `slice` in place and return how many triangles went left.
///
/// The candidates are the [`BINS`] equal slices of the node's centroid
/// extent — `from` its start, `extent` its width, `key` the coordinate
/// being cut — each scored by the surface-area heuristic:
/// the area a side's box sweeps times how many triangles it holds, which
/// is what a ray's chance of opening that side is proportional to. The
/// median split this replaces halves the *count* and ignores the area, so
/// it happily cut a long thin node across its short axis and left both
/// children nearly as large as the parent for every ray to open.
///
/// Falls back to the median where the heuristic cannot choose — a node
/// whose centroids coincide, or a winning plane that everything lands on
/// one side of — so a degenerate patch still halves rather than looping.
fn split(slice: &mut [u32], boxes: &[(Vec3, Vec3)], key: impl Fn(u32) -> f32, from: f32, extent: f32) -> usize {
    let median = |slice: &mut [u32]| {
        let middle = slice.len() / 2;
        slice.select_nth_unstable_by(middle, |&a, &b| key(a).total_cmp(&key(b)));
        middle
    };
    // A flat or non-finite axis has no bins to draw candidates from.
    if extent.is_nan() || extent <= 0.0 {
        return median(slice);
    }

    let scale = BINS as f32 / extent;
    let bin_of = |i: u32| (((key(i) - from) * scale) as usize).min(BINS - 1);

    let mut counts = [0u32; BINS];
    let mut bounds = [EMPTY; BINS];
    for &i in slice.iter() {
        let bin = bin_of(i);
        counts[bin] += 1;
        bounds[bin] = expand(bounds[bin], boxes[i as usize]);
    }

    // Sweep from each end so every plane's two sides are known in one
    // pass apiece rather than rebuilt per candidate.
    let mut left_area = [0.0f32; BINS];
    let mut left_count = [0u32; BINS];
    let (mut running, mut held) = (EMPTY, 0);
    for bin in 0..BINS {
        running = expand(running, bounds[bin]);
        held += counts[bin];
        left_area[bin] = half_area(running.0, running.1);
        left_count[bin] = held;
    }

    let (mut best, mut best_cost) = (None, f32::MAX);
    let (mut running, mut held) = (EMPTY, 0);
    for bin in (1..BINS).rev() {
        running = expand(running, bounds[bin]);
        held += counts[bin];
        let (left, right) = (left_count[bin - 1], held);
        if left == 0 || right == 0 {
            continue;
        }

        let cost = left_area[bin - 1] * left as f32 + half_area(running.0, running.1) * right as f32;
        if cost < best_cost {
            best_cost = cost;
            best = Some(bin);
        }
    }

    let Some(best) = best else {
        return median(slice);
    };

    // Two-pointer partition: everything below the winning plane to the
    // front, the rest to the back.
    let mut at = 0;
    let mut end = slice.len();
    while at < end {
        if bin_of(slice[at]) < best {
            at += 1;
        } else {
            end -= 1;
            slice.swap(at, end);
        }
    }

    if at == 0 || at == slice.len() {
        return median(slice);
    }

    at
}

impl Bvh {
    pub fn build(positions: &[Vec3], normals: &[Vec3], faces: &[[u32; 3]]) -> Self {
        let centres: Vec<Vec3> = faces
            .iter()
            .map(|f| (positions[f[0] as usize] + positions[f[1] as usize] + positions[f[2] as usize]) / 3.0)
            .collect();
        // Each triangle's own box, once. The heuristic reads it for every
        // triangle at every level, and recomputing it from three corners
        // each time is the build's whole cost.
        let boxes: Vec<(Vec3, Vec3)> = faces
            .iter()
            .map(|f| {
                f.iter().fold(EMPTY, |bounds, &corner| {
                    expand(bounds, (positions[corner as usize], positions[corner as usize]))
                })
            })
            .collect();

        let mut order: Vec<u32> = (0..faces.len() as u32).collect();
        let mut nodes = Vec::with_capacity(faces.len() / 4);
        let mut spans = Vec::with_capacity(faces.len() / 4);
        let empty_sphere = Sphere { centre: Vec3::ZERO, radius: 0.0 };
        let empty_cone = Cone::uncertain();
        nodes.push(Node {
            min: Vec3::splat(0.0),
            max: Vec3::splat(0.0),
            start: 0,
            count: 0,
            sphere: empty_sphere,
            cone: empty_cone,
        });
        spans.push(Span { start: 0, count: 0 });

        // Explicit stack rather than recursion: the depth is data-driven and
        // a degenerate split pattern should not be able to blow the real one.
        let mut stack = vec![(0usize, 0usize, order.len())];
        while let Some((node, start, end)) = stack.pop() {
            let slice = &mut order[start..end];
            spans[node] = Span { start: start as u32, count: slice.len() as u32 };
            let (min, max) = bounds_of(&centres, slice);

            // Grow to the true triangle bounds, not just the centroids —
            // a centroid box would clip geometry out of its own node.
            let (lo, hi) = slice.iter().fold((min, max), |bounds, &i| expand(bounds, boxes[i as usize]));
            let sphere = node_sphere(positions, faces, slice, lo, hi);
            let cone = node_cone(normals, faces, slice);

            if slice.len() <= LEAF_SIZE {
                nodes[node] = Node { min: lo, max: hi, start: start as u32, count: slice.len() as u32, sphere, cone };
                continue;
            }

            let extent = max - min;
            let axis = if extent.x > extent.y && extent.x > extent.z {
                0
            } else if extent.y > extent.z {
                1
            } else {
                2
            };
            let key = |i: u32| centres[i as usize].to_array()[axis];

            let middle = split(slice, &boxes, key, min.to_array()[axis], extent.to_array()[axis]);

            let left = nodes.len();
            nodes.push(Node { min: lo, max: hi, start: 0, count: 0, sphere, cone });
            nodes.push(Node { min: lo, max: hi, start: 0, count: 0, sphere, cone });
            spans.push(Span { start: 0, count: 0 });
            spans.push(Span { start: 0, count: 0 });
            nodes[node] = Node { min: lo, max: hi, start: left as u32, count: 0, sphere, cone };

            stack.push((left, start, start + middle));
            stack.push((left + 1, start + middle, end));
        }

        let tris = order
            .iter()
            .map(|&i| {
                let [a, b, c] = faces[i as usize].map(|corner| positions[corner as usize]);

                Tri { a, e1: b - a, e2: c - a }
            })
            .collect();

        Self { topology: Arc::new(Topology { nodes, spans, order }), tris }
    }

    pub(crate) fn silhouette(&self, skin: &Skin, faces: &[[u32; 3]]) -> SilhouetteBvh {
        SilhouetteBvh::bound(Arc::clone(&self.topology), skin, faces)
    }

    pub(crate) fn silhouette_faces(&self, eye: Vec3) -> Vec<usize> {
        candidate_faces(&self.topology, &[], eye, &[])
    }

    /// Is anything hit strictly between `t_min` and `t_max` along the ray?
    ///
    /// Both children go on the stack and each is tested as it comes off.
    /// Testing them where they are found instead — so a missed child is
    /// never pushed, and the nearer hit is descended first — is the
    /// textbook shape and measurably the slower one here: it costs a
    /// second slab test and a four-armed branch at every interior node to
    /// buy an early return that only an occluded ray collects, and most of
    /// these rays are cast from points that turn out to be visible.
    pub fn occluded(&self, origin: Vec3, dir: Vec3, t_min: f32, t_max: f32) -> bool {
        let inv = Vec3::new(1.0 / dir.x, 1.0 / dir.y, 1.0 / dir.z);
        let mut stack = [0u32; 64];
        let mut depth = 1;

        while depth > 0 {
            depth -= 1;
            let node = &self.topology.nodes[stack[depth] as usize];

            if slab_entry(node.min, node.max, origin, inv, t_min, t_max).is_none() {
                continue;
            }

            if node.count == 0 {
                stack[depth] = node.start;
                stack[depth + 1] = node.start + 1;
                depth += 2;
                continue;
            }

            for tri in &self.tris[node.start as usize..(node.start + node.count) as usize] {
                if triangle_t(tri.a, tri.e1, tri.e2, origin, dir, t_min, t_max).is_some() {
                    return true;
                }
            }
        }

        false
    }
}

impl SilhouetteBvh {
    fn bound(topology: Arc<Topology>, skin: &Skin, faces: &[[u32; 3]]) -> Self {
        let vertex_bindings: Vec<Binding> = (0..skin.vertices())
            .map(|vertex| {
                let (bones, weight_error) = skin.silhouette_binding(vertex);
                Binding { bones, weight_error }
            })
            .collect();
        let mut bindings = vec![Binding::default(); topology.nodes.len()];

        // Children are always appended after their parent, so a reverse
        // pass can fold leaves into interiors without another stack.
        for node_index in (0..topology.nodes.len()).rev() {
            let node = &topology.nodes[node_index];
            bindings[node_index] = if node.count == 0 {
                let left = node.start as usize;
                Binding {
                    bones: bindings[left].bones | bindings[left + 1].bones,
                    weight_error: bindings[left].weight_error.max(bindings[left + 1].weight_error),
                }
            } else {
                let mut binding = Binding::default();
                for &face in &topology.order[node.start as usize..(node.start + node.count) as usize] {
                    for corner in faces[face as usize] {
                        let vertex = vertex_bindings[corner as usize];
                        binding.bones |= vertex.bones;
                        binding.weight_error = binding.weight_error.max(vertex.weight_error);
                    }
                }
                binding
            };
        }

        Self { topology, bindings: bindings.into() }
    }

    pub(crate) fn faces(&self, eye: Vec3, transforms: &[Rigid]) -> PrunedFaces {
        pruned_faces(&self.topology, &self.bindings, eye, transforms)
    }
}

fn posed_sphere(rest: Sphere, binding: Binding, pose: &PoseBounds) -> Option<Sphere> {
    if !finite(rest.centre)
        || !rest.radius.is_finite()
        || rest.radius < 0.0
        || !binding.weight_error.is_finite()
        || binding.weight_error < 0.0
    {
        return None;
    }
    if binding.bones == 0 {
        return (binding.weight_error == 0.0).then_some(rest);
    }

    let bounds = pose.at(binding.bones);
    if !bounds.valid {
        return None;
    }
    let centre = bounds.reference.point(rest.centre);
    let rest_reach = rest.centre.x.abs() + rest.centre.y.abs() + rest.centre.z.abs();
    let posed_reach = centre.x.abs() + centre.y.abs() + centre.z.abs();
    let radius = bounds.stretch * rest.radius + bounds.displacement * rest_reach + bounds.translation;
    let radius = (1.0 + binding.weight_error) * radius + binding.weight_error * posed_reach;
    let radius = radius * (1.0 + 16.0 * f32::EPSILON);

    (finite(centre) && radius.is_finite()).then_some(Sphere { centre, radius })
}

fn posed_cone(rest: Cone, binding: Binding, pose: &PoseBounds) -> Option<Cone> {
    if !finite(rest.axis)
        || !rest.sin_half_angle.is_finite()
        || !rest.cos_half_angle.is_finite()
        || rest.cos_half_angle <= 0.0
    {
        return None;
    }
    let bounds = pose.at(binding.bones);
    if !bounds.valid {
        return None;
    }

    // Each mask chooses the pose transform with the smallest worst-case
    // angular distance to its bones. Recenter the rest axis there, then
    // widen by that precomputed spread: every rotated normal and every
    // positive blend remains contained without per-node trigonometry. An
    // acute result cannot cancel; a wide result returns `None` below and
    // descends, preserving the exact skin fallback.
    let axis = bounds.reference.direction(rest.axis);
    if !finite(axis) || axis.length_squared() < 1e-12 {
        return None;
    }
    let axis = axis.normalize();
    let sin_half_angle = rest.sin_half_angle * bounds.cos_rotation + rest.cos_half_angle * bounds.sin_rotation;
    let cos_half_angle = rest.cos_half_angle * bounds.cos_rotation - rest.sin_half_angle * bounds.sin_rotation;
    let sin_half_angle = (sin_half_angle + 32.0 * f32::EPSILON).min(1.0);
    let cos_half_angle = cos_half_angle - 32.0 * f32::EPSILON;

    (sin_half_angle.is_finite() && cos_half_angle.is_finite() && cos_half_angle > 0.0).then_some(Cone {
        axis,
        sin_half_angle,
        cos_half_angle,
    })
}

fn uniformly_signed_bounds(sphere: Sphere, cone: Cone, binding: Binding, eye: Vec3, pose: &PoseBounds) -> bool {
    if !finite(eye) {
        return false;
    }
    let Some(sphere) = posed_sphere(sphere, binding, pose) else {
        return false;
    };
    let to_centre = sphere.centre - eye;
    let distance_squared = to_centre.length_squared();
    let radius_squared = sphere.radius * sphere.radius;
    if !distance_squared.is_finite()
        || !radius_squared.is_finite()
        || distance_squared <= radius_squared
        || distance_squared <= 0.0
    {
        return false;
    }
    let Some(normal) = posed_cone(cone, binding, pose) else {
        return false;
    };
    let projected = to_centre.dot(normal.axis);
    let margin = 256.0 * f32::EPSILON * distance_squared.max(1.0);
    // The combined view and normal cones stay inside a hemisphere exactly
    // when `distance * cos(normal) > radius`. Keep that comparison and the
    // final sign proof squared: it avoids two roots at every visited node
    // while equality and rounding uncertainty still descend.
    let hemisphere = distance_squared * normal.cos_half_angle * normal.cos_half_angle - radius_squared;
    if !hemisphere.is_finite() || hemisphere <= margin {
        return false;
    }
    let remaining = projected.abs() - sphere.radius * normal.cos_half_angle;
    if !remaining.is_finite() || remaining <= 0.0 {
        return false;
    }
    let signed =
        remaining * remaining - (distance_squared - radius_squared) * normal.sin_half_angle * normal.sin_half_angle;

    projected.is_finite() && signed.is_finite() && signed > margin
}

fn uniformly_signed(node: &Node, binding: Binding, eye: Vec3, pose: &PoseBounds) -> bool {
    uniformly_signed_bounds(node.sphere, node.cone, binding, eye, pose)
}

fn pruned_faces(topology: &Topology, bindings: &[Binding], eye: Vec3, transforms: &[Rigid]) -> PrunedFaces {
    pruned_faces_observed(topology, bindings, eye, transforms, SILHOUETTE_DIRECT_SIZE, |_| {})
}

fn pruned_faces_observed(
    topology: &Topology,
    bindings: &[Binding],
    eye: Vec3,
    transforms: &[Rigid],
    direct_size: u32,
    mut observe_pruned: impl FnMut(bool),
) -> PrunedFaces {
    let pose = PoseBounds::new(transforms);
    let mut pruned_faces = vec![0u64; topology.order.len().div_ceil(u64::BITS as usize)];
    let mut stack = vec![0u32];

    while let Some(node_index) = stack.pop() {
        let node = &topology.nodes[node_index as usize];
        let span = topology.spans[node_index as usize];
        if span.count <= direct_size {
            continue;
        }
        let binding = bindings.get(node_index as usize).copied().unwrap_or_default();
        let pruned = uniformly_signed(node, binding, eye, &pose);
        observe_pruned(pruned);
        if pruned {
            for &face in &topology.order[span.start as usize..(span.start + span.count) as usize] {
                pruned_faces[face as usize / u64::BITS as usize] |= 1 << (face % u64::BITS);
            }
            continue;
        }
        if node.count == 0 {
            stack.push(node.start + 1);
            stack.push(node.start);
        }
    }

    PrunedFaces { bits: pruned_faces }
}

fn candidate_faces(topology: &Topology, bindings: &[Binding], eye: Vec3, transforms: &[Rigid]) -> Vec<usize> {
    candidate_faces_observed(topology, bindings, eye, transforms, SILHOUETTE_DIRECT_SIZE, |_| {})
}

fn candidate_faces_observed(
    topology: &Topology,
    bindings: &[Binding],
    eye: Vec3,
    transforms: &[Rigid],
    direct_size: u32,
    mut observe_pruned: impl FnMut(bool),
) -> Vec<usize> {
    let pose = PoseBounds::new(transforms);
    let mut active = vec![0u64; topology.order.len().div_ceil(u64::BITS as usize)];
    let mut stack = vec![0u32];

    while let Some(node_index) = stack.pop() {
        let node = &topology.nodes[node_index as usize];
        let span = topology.spans[node_index as usize];
        if span.count <= direct_size {
            for &face in &topology.order[span.start as usize..(span.start + span.count) as usize] {
                active[face as usize / u64::BITS as usize] |= 1 << (face % u64::BITS);
            }
            continue;
        }
        let binding = bindings.get(node_index as usize).copied().unwrap_or_default();
        let pruned = uniformly_signed(node, binding, eye, &pose);
        observe_pruned(pruned);
        if pruned {
            continue;
        }
        if node.count == 0 {
            stack.push(node.start + 1);
            stack.push(node.start);
            continue;
        }
        for &face in &topology.order[node.start as usize..(node.start + node.count) as usize] {
            active[face as usize / u64::BITS as usize] |= 1 << (face % u64::BITS);
        }
    }

    let mut candidates = Vec::with_capacity(active.iter().map(|word| word.count_ones() as usize).sum());
    for (word_index, mut word) in active.into_iter().enumerate() {
        while word != 0 {
            let bit = word.trailing_zeros();
            candidates.push(word_index * u64::BITS as usize + bit as usize);
            word &= word - 1;
        }
    }
    candidates
}

impl Bvh {
    /// Nearest hit along the ray, as `(t, face)`. Used to drop authored
    /// face marks onto whatever surface lies behind them.
    pub fn nearest(&self, origin: Vec3, dir: Vec3, t_min: f32, t_max: f32) -> Option<(f32, usize)> {
        let inv = Vec3::new(1.0 / dir.x, 1.0 / dir.y, 1.0 / dir.z);
        let mut stack = [0u32; 64];
        let mut depth = 1;
        let mut best: Option<(f32, usize)> = None;

        while depth > 0 {
            depth -= 1;
            let node = &self.topology.nodes[stack[depth] as usize];
            let limit = best.map_or(t_max, |(t, _)| t);

            if slab_entry(node.min, node.max, origin, inv, t_min, limit).is_none() {
                continue;
            }

            if node.count == 0 {
                stack[depth] = node.start;
                stack[depth + 1] = node.start + 1;
                depth += 2;
                continue;
            }

            let leaf = node.start as usize..(node.start + node.count) as usize;
            for (slot, tri) in leaf.clone().zip(&self.tris[leaf]) {
                let hit = triangle_t(tri.a, tri.e1, tri.e2, origin, dir, t_min, best.map_or(t_max, |(t, _)| t));
                if let Some(t) = hit {
                    best = Some((t, self.topology.order[slot] as usize));
                }
            }
        }

        best
    }
}

/// Where the ray enters the box, or `None` if it never does. The distance
/// is what orders two children; whether it is `Some` is the hit test.
fn slab_entry(min: Vec3, max: Vec3, origin: Vec3, inv: Vec3, t_min: f32, t_max: f32) -> Option<f32> {
    // Spelled out per axis rather than looped over a three-element array
    // of tuples: this is the hottest arithmetic in the frame, run tens of
    // times per ray and tens of thousands of rays deep, and the array form
    // is only reliably flattened when the optimiser feels like it.
    let (lo_x, hi_x) = ((min.x - origin.x) * inv.x, (max.x - origin.x) * inv.x);
    let (lo_y, hi_y) = ((min.y - origin.y) * inv.y, (max.y - origin.y) * inv.y);
    let (lo_z, hi_z) = ((min.z - origin.z) * inv.z, (max.z - origin.z) * inv.z);

    let near = t_min.max(lo_x.min(hi_x)).max(lo_y.min(hi_y)).max(lo_z.min(hi_z));
    let far = t_max.min(lo_x.max(hi_x)).min(lo_y.max(hi_y)).min(lo_z.max(hi_z));

    (near <= far).then_some(near)
}

/// Möller-Trumbore, over the corner-and-edges form the leaf stores.
fn triangle_t(a: Vec3, e1: Vec3, e2: Vec3, origin: Vec3, dir: Vec3, t_min: f32, t_max: f32) -> Option<f32> {
    let h = dir.cross(e2);
    let det = e1.dot(h);
    if det.abs() < 1e-12 {
        return None;
    }

    let inv_det = 1.0 / det;
    let s = origin - a;
    let u = s.dot(h) * inv_det;
    if !(0.0..=1.0).contains(&u) {
        return None;
    }

    let q = s.cross(e1);
    let v = dir.dot(q) * inv_det;
    if v < 0.0 || u + v > 1.0 {
        return None;
    }

    let t = e2.dot(q) * inv_det;
    (t > t_min && t < t_max).then_some(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::{env, f32::consts::PI, fs, hint::black_box, path::PathBuf, time::Instant};

    use crate::Pose;
    use crate::deform::npy;
    use crate::mesh::Mesh;

    /// A wall of `across * across` unit quads in the `z = 1` plane, its
    /// corner at the origin. Every triangle's centroid shares a `z`, so
    /// the build meets a flat axis on every descent — and there are enough
    /// of them to force a dozen splits.
    fn wall(across: usize) -> (Vec<Vec3>, Vec<[u32; 3]>) {
        let mut positions = Vec::new();
        let mut faces = Vec::new();

        for row in 0..=across {
            for column in 0..=across {
                positions.push(Vec3::new(column as f32, row as f32, 1.0));
            }
        }
        let corner = |row: usize, column: usize| (row * (across + 1) + column) as u32;
        for row in 0..across {
            for column in 0..across {
                faces.push([corner(row, column), corner(row, column + 1), corner(row + 1, column + 1)]);
                faces.push([corner(row, column), corner(row + 1, column + 1), corner(row + 1, column)]);
            }
        }

        (positions, faces)
    }

    /// Tripwire: every triangle handed to the build is still reachable by
    /// a ray afterwards.
    ///
    /// The split partitions an index array in place and falls back to a
    /// median where the heuristic cannot choose. Both halves of that can
    /// lose triangles silently — a partition that miscounts its boundary
    /// leaves a slice out of the tree, and the leaves it did build stay
    /// perfectly valid, so nothing errors and no test that asks only
    /// whether a ray hits *something* notices. On the page it reads as a
    /// hole in the occlusion: strokes drawn straight through a patch of
    /// the subject that should have hidden them. So this asks after every
    /// triangle individually, aiming a ray at each one.
    #[test]
    fn no_triangle_falls_out_of_the_tree() {
        let (positions, faces) = wall(8);
        let normals = vec![Vec3::Z; positions.len()];
        let bvh = Bvh::build(&positions, &normals, &faces);

        for face in &faces {
            let centre = face.iter().fold(Vec3::splat(0.0), |sum, &i| sum + positions[i as usize]) / 3.0;
            let origin = Vec3::new(centre.x, centre.y, 0.0);

            assert!(
                bvh.occluded(origin, Vec3::new(0.0, 0.0, 1.0), 1e-4, 2.0),
                "the wall covers {centre:?}, so a ray to it is blocked",
            );
        }
    }

    /// Tripwire: the tree does not answer for geometry that is not there.
    ///
    /// The complement of the sweep above — a bounds-only traversal passes
    /// anything inside the root box, and the leaf test is what makes the
    /// answer about triangles rather than about boxes.
    #[test]
    fn a_ray_past_the_wall_s_edge_is_clear() {
        let (positions, faces) = wall(8);
        let normals = vec![Vec3::Z; positions.len()];
        let bvh = Bvh::build(&positions, &normals, &faces);

        assert!(!bvh.occluded(Vec3::new(-0.5, 4.0, 0.0), Vec3::new(0.0, 0.0, 1.0), 1e-4, 2.0), "left of the wall");
        assert!(!bvh.occluded(Vec3::new(4.0, 4.0, 0.0), Vec3::new(0.0, 0.0, 1.0), 1e-4, 0.5), "stops short of it");
    }

    #[test]
    fn posed_spheres_and_cones_contain_blended_bone_images() {
        let rest_sphere = Sphere { centre: Vec3::new(0.2, -0.1, 0.4), radius: 0.75 };
        let rest_half_angle = 0.2;
        let rest_cone = Cone::around(Vec3::Z, rest_half_angle);
        let transforms = [
            Rigid::sample(|p| p.rotate_axis_angle(Vec3::Y, 0.35) + Vec3::new(0.2, 0.0, 0.0)),
            Rigid::sample(|p| p.rotate_axis_angle(Vec3::X, -0.45) + Vec3::new(-0.1, 0.15, 0.0)),
        ];
        let binding = Binding { bones: 0b11, weight_error: 0.0 };
        let pose = PoseBounds::new(&transforms);
        let sphere = posed_sphere(rest_sphere, binding, &pose).expect("finite transformed sphere");
        let cone = posed_cone(rest_cone, binding, &pose).expect("narrow transformed cone");

        for share in [0.0, 0.2, 0.5, 0.8, 1.0] {
            for direction in [Vec3::X, Vec3::Y, Vec3::Z, -Vec3::X, -Vec3::Y, -Vec3::Z] {
                let point = rest_sphere.centre + direction * rest_sphere.radius;
                let posed = transforms[0].point(point) * share + transforms[1].point(point) * (1.0 - share);
                assert!((posed - sphere.centre).length() <= sphere.radius, "posed point escaped its node sphere");
            }

            let normal = Vec3::Z.rotate_axis_angle(Vec3::Y, rest_half_angle);
            let posed =
                (transforms[0].direction(normal) * share + transforms[1].direction(normal) * (1.0 - share)).normalize();
            assert!(
                cone.axis.dot(posed) >= cone.cos_half_angle - 64.0 * f32::EPSILON,
                "posed normal escaped its node cone"
            );
        }
    }

    #[test]
    fn posed_node_bounds_contain_every_live_corner() {
        let (mut positions, faces) = wall(3);
        for position in &mut positions {
            *position -= Vec3::new(1.5, 1.5, 0.0);
        }
        let normals: Vec<Vec3> =
            positions.iter().map(|position| Vec3::new(position.x * 0.45, position.y * 0.35, 1.0).normalize()).collect();
        let weights: Vec<f32> = (0..positions.len())
            .flat_map(|vertex| match vertex % 3 {
                0 => [1.0, 0.0],
                1 => [0.0, 1.0],
                _ => [0.35, 0.65],
            })
            .collect();
        let skin = Skin::parse(
            &npy(&weights, (positions.len(), 2)),
            "bones chest head\npivot head 0.0 0.0 0.0\n",
            positions.len(),
        )
        .expect("two meaningful bone lanes");
        let transforms = skin.transforms(&Pose { yaw: 31.0, pitch: -11.0, roll: 7.0, ..Pose::default() });
        let mut rest = Mesh::build(positions, faces, 0);
        rest.normals = normals;
        rest.bvh = Some(Bvh::build(&rest.positions, &rest.normals, &rest.faces));
        let mut posed = rest.deformable(&skin);
        skin.pose_surface(&transforms, &rest, &mut posed.positions, &mut posed.normals);
        let silhouette = rest.bvh.as_ref().expect("rest accelerator").silhouette(&skin, &rest.faces);
        let pose = PoseBounds::new(&transforms);

        for (node_index, node) in silhouette.topology.nodes.iter().enumerate() {
            let sphere = posed_sphere(node.sphere, silhouette.bindings[node_index], &pose).expect("posed sphere");
            let cone = posed_cone(node.cone, silhouette.bindings[node_index], &pose);
            let mut stack = vec![node_index];
            while let Some(descendant) = stack.pop() {
                let descendant = &silhouette.topology.nodes[descendant];
                if descendant.count == 0 {
                    stack.push(descendant.start as usize);
                    stack.push(descendant.start as usize + 1);
                    continue;
                }
                for &face in &silhouette.topology.order
                    [descendant.start as usize..(descendant.start + descendant.count) as usize]
                {
                    for corner in rest.faces[face as usize] {
                        let corner = corner as usize;
                        assert!(
                            (posed.positions[corner] - sphere.centre).length() <= sphere.radius,
                            "node {node_index} sphere lost face {face} corner {corner}",
                        );
                        if let Some(cone) = cone {
                            assert!(
                                cone.axis.dot(posed.normals[corner]) >= cone.cos_half_angle,
                                "node {node_index} cone lost face {face} corner {corner}",
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    #[ignore = "diagnostic instrument; needs the shipped subject and rig"]
    #[allow(clippy::disallowed_methods, clippy::print_stderr)]
    fn canonical_posed_traversal_census() {
        const REPEATS: u32 = 8;

        let dir = PathBuf::from(env::var("AETHER_CROSSFEED_DIR").expect("canonical subject directory"));
        let rest =
            Mesh::from_obj_bytes(&fs::read(dir.join("subject.obj")).expect("read subject"), 2).expect("parse subject");
        let skin = Skin::parse(
            &fs::read(dir.join("rig/weights.npy")).expect("read weights"),
            &fs::read_to_string(dir.join("rig/rig.txt")).expect("read rig"),
            rest.positions.len(),
        )
        .expect("parse rig");
        let transforms = skin.transforms(&Pose { yaw: 18.0, ..Pose::default() });
        let mut posed = rest.deformable(&skin);
        skin.pose_surface(&transforms, &rest, &mut posed.positions, &mut posed.normals);
        posed.rebound(&transforms);
        let silhouette = posed.silhouette.as_ref().expect("posed silhouette accelerator");
        let elevation = 3.0f32.to_radians();
        let eye = Vec3::new(0.0, elevation.sin(), elevation.cos()) * 5.4;
        let (mut visited, mut pruned) = (0usize, 0usize);
        let candidates = pruned_faces_observed(
            &silhouette.topology,
            &silhouette.bindings,
            eye,
            &transforms,
            SILHOUETTE_DIRECT_SIZE,
            |uniform| {
                visited += 1;
                pruned += usize::from(uniform);
            },
        );

        eprintln!(
            "silhouette census: {visited} nodes visited, {pruned} pruned, {} of {} faces survived",
            rest.faces.len() - candidates.bits.iter().map(|word| word.count_ones() as usize).sum::<usize>(),
            rest.faces.len(),
        );
        let started = Instant::now();
        for _ in 0..REPEATS {
            black_box(pruned_faces(&silhouette.topology, &silhouette.bindings, eye, &transforms));
        }
        eprintln!(
            "silhouette census: traversal {:.3} ms",
            started.elapsed().as_secs_f64() * 1000.0 / f64::from(REPEATS),
        );
        let started = Instant::now();
        for _ in 0..REPEATS {
            black_box(posed.silhouette_level_set(eye));
        }
        eprintln!(
            "silhouette census: traversal plus exact leaves {:.3} ms",
            started.elapsed().as_secs_f64() * 1000.0 / f64::from(REPEATS),
        );
        let started = Instant::now();
        for _ in 0..REPEATS {
            black_box(posed.facing(eye));
        }
        eprintln!(
            "silhouette census: linear facing field {:.3} ms",
            started.elapsed().as_secs_f64() * 1000.0 / f64::from(REPEATS),
        );
        let facing = posed.facing(eye);
        let started = Instant::now();
        for _ in 0..REPEATS {
            black_box(posed.level_set(&facing, &[], 0.0));
        }
        eprintln!(
            "silhouette census: linear exact march {:.3} ms",
            started.elapsed().as_secs_f64() * 1000.0 / f64::from(REPEATS),
        );
    }

    #[test]
    fn merged_nodes_keep_every_meaningful_bone() {
        let (positions, faces) = wall(3);
        let normals = vec![Vec3::Z; positions.len()];
        let weights: Vec<f32> = (0..positions.len())
            .flat_map(|vertex| {
                if vertex % 2 == 0 {
                    [1.0, 0.0]
                } else {
                    [0.0, 1.0]
                }
            })
            .collect();
        let skin =
            Skin::parse(&npy(&weights, (positions.len(), 2)), "bones chest head\npivot head 0 0 0\n", positions.len())
                .expect("two bone wall");
        let silhouette = Bvh::build(&positions, &normals, &faces).silhouette(&skin, &faces);

        assert_eq!(silhouette.bindings[0].bones, 0b11, "the root merges both child masks");
        assert!(silhouette.bindings.iter().all(|binding| binding.bones != 0), "every occupied node has a bone");
    }

    #[test]
    fn every_uncertain_bound_descends() {
        let identity = PoseBounds::new(&[]);
        let ordinary = Node {
            min: Vec3::splat(-1.0),
            max: Vec3::splat(1.0),
            start: 0,
            count: 1,
            sphere: Sphere { centre: Vec3::X, radius: 0.25 },
            cone: Cone::around(Vec3::Y, 0.0),
        };

        assert!(!uniformly_signed(&ordinary, Binding::default(), Vec3::X, &identity), "eye on sphere centre");
        assert!(
            !uniformly_signed(
                &Node { sphere: Sphere { centre: Vec3::X, radius: 1.0 }, ..ordinary },
                Binding::default(),
                Vec3::ZERO,
                &identity,
            ),
            "eye on sphere boundary"
        );
        assert!(!uniformly_signed(&ordinary, Binding::default(), Vec3::splat(f32::NAN), &identity), "non-finite eye");
        assert!(
            !uniformly_signed(
                &Node { cone: Cone::around(Vec3::Y, FRAC_PI_2), ..ordinary },
                Binding::default(),
                Vec3::ZERO,
                &identity,
            ),
            "hemisphere-wide cone"
        );
        assert!(
            !uniformly_signed(
                &Node { cone: Cone::around(Vec3::X, 80.0f32.to_radians()), ..ordinary },
                Binding::default(),
                Vec3::ZERO,
                &identity,
            ),
            "combined view and normal cones cross the hemisphere"
        );
        assert!(!uniformly_signed(&ordinary, Binding::default(), Vec3::ZERO, &identity), "exact perpendicularity");

        let opposite = Rigid::sample(|p| p.rotate_axis_angle(Vec3::Y, PI));
        let opposite_pose = PoseBounds::new(&[Rigid::IDENTITY, opposite]);
        assert!(
            posed_cone(Cone::around(Vec3::X, 0.0), Binding { bones: 0b11, weight_error: 0.0 }, &opposite_pose)
                .is_none(),
            "cancelling axes are uncertifiable"
        );

        let signed = Node { cone: Cone::around(Vec3::X, 0.0), ..ordinary };
        assert!(uniformly_signed(&signed, Binding::default(), Vec3::ZERO, &identity), "a strict positive bound prunes");
    }
}
