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

use std::sync::Arc;

use aether_math::Vec3;

use crate::deform::{BONE_LIMIT, Rigid, Skin};

const LEAF_SIZE: usize = 8;

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
    half_angle: f32,
}

#[derive(Clone)]
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
    order: Vec<u32>,
}

#[derive(Clone, Copy, Default)]
struct Binding {
    bones: u8,
    weight_error: f32,
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
            return Cone { axis: Vec3::Y, half_angle: f32::INFINITY };
        }
        sum += normal;
        held += 1;
    }
    if held == 0 || !finite(sum) || sum.length_squared() < 1e-12 {
        return Cone { axis: Vec3::Y, half_angle: f32::INFINITY };
    }

    let axis = sum.normalize();
    let half_angle = order
        .iter()
        .flat_map(|&face| faces[face as usize])
        .map(|corner| axis.dot(normals[corner as usize]).clamp(-1.0, 1.0).acos())
        .fold(0.0f32, f32::max);

    Cone { axis, half_angle: half_angle + 16.0 * f32::EPSILON }
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
        let empty_sphere = Sphere { centre: Vec3::ZERO, radius: 0.0 };
        let empty_cone = Cone { axis: Vec3::Y, half_angle: f32::INFINITY };
        nodes.push(Node {
            min: Vec3::splat(0.0),
            max: Vec3::splat(0.0),
            start: 0,
            count: 0,
            sphere: empty_sphere,
            cone: empty_cone,
        });

        // Explicit stack rather than recursion: the depth is data-driven and
        // a degenerate split pattern should not be able to blow the real one.
        let mut stack = vec![(0usize, 0usize, order.len())];
        while let Some((node, start, end)) = stack.pop() {
            let slice = &mut order[start..end];
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

        Self { topology: Arc::new(Topology { nodes, order }), tris }
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

    pub(crate) fn faces(&self, eye: Vec3, transforms: &[Rigid]) -> Vec<usize> {
        candidate_faces(&self.topology, &self.bindings, eye, transforms)
    }
}

fn merge_spheres(a: Sphere, b: Sphere) -> Option<Sphere> {
    if !finite(a.centre)
        || !finite(b.centre)
        || !a.radius.is_finite()
        || !b.radius.is_finite()
        || a.radius < 0.0
        || b.radius < 0.0
    {
        return None;
    }

    let between = b.centre - a.centre;
    let distance = between.length();
    if !distance.is_finite() {
        return None;
    }
    if a.radius >= distance + b.radius {
        return Some(a);
    }
    if b.radius >= distance + a.radius {
        return Some(b);
    }
    if distance <= 0.0 {
        return Some(Sphere { centre: a.centre, radius: a.radius.max(b.radius) });
    }

    let radius = (distance + a.radius + b.radius) * 0.5;
    let centre = a.centre + between * ((radius - a.radius) / distance);
    Some(Sphere { centre, radius: radius * (1.0 + 8.0 * f32::EPSILON) })
}

fn posed_sphere(rest: Sphere, binding: Binding, transforms: &[Rigid]) -> Option<Sphere> {
    let mut posed = None;
    for bone in 0..BONE_LIMIT {
        if binding.bones & (1 << bone) == 0 {
            continue;
        }
        let (centre, radius) = transforms.get(bone)?.bound_sphere(rest.centre, rest.radius)?;
        let transformed = Sphere { centre, radius };
        posed = Some(match posed {
            None => transformed,
            Some(held) => merge_spheres(held, transformed)?,
        });
    }

    let mut posed = posed.unwrap_or(rest);
    if !binding.weight_error.is_finite() || binding.weight_error < 0.0 {
        return None;
    }
    let reach = posed.centre.length() + posed.radius;
    posed.radius += binding.weight_error * reach;
    posed.radius *= 1.0 + 8.0 * f32::EPSILON;

    finite(posed.centre).then_some(posed).filter(|sphere| sphere.radius.is_finite())
}

fn posed_cone(rest: Cone, binding: Binding, transforms: &[Rigid]) -> Option<Cone> {
    if !finite(rest.axis) || !rest.half_angle.is_finite() || rest.half_angle < 0.0 {
        return None;
    }

    // The rest axis is included because `normalize_or(rest_normal)` is
    // the exact live fallback when a blended normal cancels.
    let mut axes = vec![rest.axis];
    for bone in 0..BONE_LIMIT {
        if binding.bones & (1 << bone) == 0 {
            continue;
        }
        let axis = transforms.get(bone)?.direction(rest.axis);
        if !finite(axis) || axis.length_squared() < 1e-12 {
            return None;
        }
        axes.push(axis.normalize());
    }

    let sum = axes.iter().copied().fold(Vec3::ZERO, |sum, axis| sum + axis);
    if !finite(sum) || sum.length_squared() < 1e-12 {
        return None;
    }
    let axis = sum.normalize();
    let half_angle = axes
        .iter()
        .map(|&candidate| axis.dot(candidate).clamp(-1.0, 1.0).acos())
        .fold(rest.half_angle, |wide, angle| wide.max(rest.half_angle + angle))
        + 32.0 * f32::EPSILON;

    (half_angle < core::f32::consts::FRAC_PI_2).then_some(Cone { axis, half_angle })
}

fn uniformly_signed(node: &Node, binding: Binding, eye: Vec3, transforms: &[Rigid]) -> bool {
    if !finite(eye) {
        return false;
    }
    let Some(sphere) = posed_sphere(node.sphere, binding, transforms) else {
        return false;
    };
    let to_centre = sphere.centre - eye;
    let distance = to_centre.length();
    if !distance.is_finite() || distance <= sphere.radius || distance <= 0.0 {
        return false;
    }
    let ratio = sphere.radius / distance;
    if !ratio.is_finite() || !(0.0..1.0).contains(&ratio) {
        return false;
    }
    let view_half_angle = ratio.asin() + 16.0 * f32::EPSILON;
    let Some(normal) = posed_cone(node.cone, binding, transforms) else {
        return false;
    };
    let view_axis = to_centre / distance;
    let between = view_axis.dot(normal.axis).clamp(-1.0, 1.0).acos();
    let uncertainty = view_half_angle + normal.half_angle;
    let boundary = core::f32::consts::FRAC_PI_2;
    let margin = 64.0 * f32::EPSILON;

    between + uncertainty < boundary - margin || between - uncertainty > boundary + margin
}

fn candidate_faces(topology: &Topology, bindings: &[Binding], eye: Vec3, transforms: &[Rigid]) -> Vec<usize> {
    let mut candidates = Vec::new();
    let mut stack = vec![0u32];

    while let Some(node_index) = stack.pop() {
        let node = &topology.nodes[node_index as usize];
        let binding = bindings.get(node_index as usize).copied().unwrap_or_default();
        if uniformly_signed(node, binding, eye, transforms) {
            continue;
        }
        if node.count == 0 {
            stack.push(node.start + 1);
            stack.push(node.start);
            continue;
        }
        candidates.extend(
            topology.order[node.start as usize..(node.start + node.count) as usize].iter().map(|&face| face as usize),
        );
    }

    candidates.sort_unstable();
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

    use crate::deform::npy;

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
        let rest_cone = Cone { axis: Vec3::Z, half_angle: 0.2 };
        let transforms = [
            Rigid::sample(|p| p.rotate_axis_angle(Vec3::Y, 0.35) + Vec3::new(0.2, 0.0, 0.0)),
            Rigid::sample(|p| p.rotate_axis_angle(Vec3::X, -0.45) + Vec3::new(-0.1, 0.15, 0.0)),
        ];
        let binding = Binding { bones: 0b11, weight_error: 0.0 };
        let sphere = posed_sphere(rest_sphere, binding, &transforms).expect("finite transformed sphere");
        let cone = posed_cone(rest_cone, binding, &transforms).expect("narrow transformed cone");

        for share in [0.0, 0.2, 0.5, 0.8, 1.0] {
            for direction in [Vec3::X, Vec3::Y, Vec3::Z, -Vec3::X, -Vec3::Y, -Vec3::Z] {
                let point = rest_sphere.centre + direction * rest_sphere.radius;
                let posed = transforms[0].point(point) * share + transforms[1].point(point) * (1.0 - share);
                assert!((posed - sphere.centre).length() <= sphere.radius, "posed point escaped its node sphere");
            }

            let normal = Vec3::Z.rotate_axis_angle(Vec3::Y, rest_cone.half_angle);
            let posed =
                (transforms[0].direction(normal) * share + transforms[1].direction(normal) * (1.0 - share)).normalize();
            let angle = cone.axis.dot(posed).clamp(-1.0, 1.0).acos();
            assert!(angle <= cone.half_angle, "posed normal escaped its node cone");
        }
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
        let ordinary = Node {
            min: Vec3::splat(-1.0),
            max: Vec3::splat(1.0),
            start: 0,
            count: 1,
            sphere: Sphere { centre: Vec3::X, radius: 0.25 },
            cone: Cone { axis: Vec3::Y, half_angle: 0.0 },
        };

        assert!(!uniformly_signed(&ordinary, Binding::default(), Vec3::X, &[]), "eye on sphere centre");
        assert!(
            !uniformly_signed(
                &Node { sphere: Sphere { centre: Vec3::X, radius: 1.0 }, ..ordinary.clone() },
                Binding::default(),
                Vec3::ZERO,
                &[],
            ),
            "eye on sphere boundary"
        );
        assert!(!uniformly_signed(&ordinary, Binding::default(), Vec3::splat(f32::NAN), &[]), "non-finite eye");
        assert!(
            !uniformly_signed(
                &Node { cone: Cone { axis: Vec3::Y, half_angle: core::f32::consts::FRAC_PI_2 }, ..ordinary.clone() },
                Binding::default(),
                Vec3::ZERO,
                &[],
            ),
            "hemisphere-wide cone"
        );
        assert!(!uniformly_signed(&ordinary, Binding::default(), Vec3::ZERO, &[]), "exact perpendicularity");

        let opposite = Rigid::sample(|p| p.rotate_axis_angle(Vec3::Y, core::f32::consts::PI));
        assert!(
            posed_cone(Cone { axis: Vec3::X, half_angle: 0.0 }, Binding { bones: 1, weight_error: 0.0 }, &[opposite])
                .is_none(),
            "cancelling axes are uncertifiable"
        );

        let signed = Node { cone: Cone { axis: Vec3::X, half_angle: 0.0 }, ..ordinary };
        assert!(uniformly_signed(&signed, Binding::default(), Vec3::ZERO, &[]), "a strict positive bound prunes");
    }
}
