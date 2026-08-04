//! A bounding-volume hierarchy over the triangles, for occlusion only.
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

use aether_math::Vec3;

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

struct Node {
    min: Vec3,
    max: Vec3,
    /// For a leaf, the first index into `order`; for an interior node,
    /// the index of the left child.
    start: u32,
    /// Triangle count for a leaf, `0` for an interior node.
    count: u32,
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
    nodes: Vec<Node>,
    order: Vec<u32>,
    tris: Vec<Tri>,
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
    pub fn build(positions: &[Vec3], faces: &[[u32; 3]]) -> Self {
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
        nodes.push(Node { min: Vec3::splat(0.0), max: Vec3::splat(0.0), start: 0, count: 0 });

        // Explicit stack rather than recursion: the depth is data-driven and
        // a degenerate split pattern should not be able to blow the real one.
        let mut stack = vec![(0usize, 0usize, order.len())];
        while let Some((node, start, end)) = stack.pop() {
            let slice = &mut order[start..end];
            let (min, max) = bounds_of(&centres, slice);

            // Grow to the true triangle bounds, not just the centroids —
            // a centroid box would clip geometry out of its own node.
            let (lo, hi) = slice.iter().fold((min, max), |bounds, &i| expand(bounds, boxes[i as usize]));

            if slice.len() <= LEAF_SIZE {
                nodes[node] = Node { min: lo, max: hi, start: start as u32, count: slice.len() as u32 };
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
            nodes.push(Node { min: lo, max: hi, start: 0, count: 0 });
            nodes.push(Node { min: lo, max: hi, start: 0, count: 0 });
            nodes[node] = Node { min: lo, max: hi, start: left as u32, count: 0 };

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

        Self { nodes, order, tris }
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
            let node = &self.nodes[stack[depth] as usize];

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
            let node = &self.nodes[stack[depth] as usize];
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
                    best = Some((t, self.order[slot] as usize));
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
        let bvh = Bvh::build(&positions, &faces);

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
        let bvh = Bvh::build(&positions, &faces);

        assert!(!bvh.occluded(Vec3::new(-0.5, 4.0, 0.0), Vec3::new(0.0, 0.0, 1.0), 1e-4, 2.0), "left of the wall");
        assert!(!bvh.occluded(Vec3::new(4.0, 4.0, 0.0), Vec3::new(0.0, 0.0, 1.0), 1e-4, 0.5), "stops short of it");
    }
}
