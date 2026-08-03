//! A bounding-volume hierarchy over the triangles, for occlusion only.
//!
//! Every visible point has to prove nothing stands between it and the
//! eye, and against 868k triangles the linear answer is hopeless. Only
//! *any*-hit is ever asked, never nearest-hit, so traversal returns the
//! moment it finds a blocker.
//!
//! Median split on the widest axis. Not the best tree a SAH build would
//! give, but it builds in a second and the query is not the bottleneck.

use aether_math::Vec3;

const LEAF_SIZE: usize = 8;

struct Node {
    min: Vec3,
    max: Vec3,
    /// For a leaf, the first index into `order`; for an interior node,
    /// the index of the left child.
    start: u32,
    /// Triangle count for a leaf, `0` for an interior node.
    count: u32,
}

pub struct Bvh {
    nodes: Vec<Node>,
    order: Vec<u32>,
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

impl Bvh {
    pub fn build(positions: &[Vec3], faces: &[[u32; 3]]) -> Self {
        let centres: Vec<Vec3> = faces
            .iter()
            .map(|f| (positions[f[0] as usize] + positions[f[1] as usize] + positions[f[2] as usize]) / 3.0)
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
            let (mut lo, mut hi) = (min, max);
            for &i in slice.iter() {
                for corner in faces[i as usize] {
                    let p = positions[corner as usize];
                    lo = Vec3::new(lo.x.min(p.x), lo.y.min(p.y), lo.z.min(p.z));
                    hi = Vec3::new(hi.x.max(p.x), hi.y.max(p.y), hi.z.max(p.z));
                }
            }

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

            let middle = slice.len() / 2;
            slice.select_nth_unstable_by(middle, |&a, &b| key(a).total_cmp(&key(b)));

            let left = nodes.len();
            nodes.push(Node { min: lo, max: hi, start: 0, count: 0 });
            nodes.push(Node { min: lo, max: hi, start: 0, count: 0 });
            nodes[node] = Node { min: lo, max: hi, start: left as u32, count: 0 };

            stack.push((left, start, start + middle));
            stack.push((left + 1, start + middle, end));
        }

        Self { nodes, order }
    }

    /// Is anything hit strictly between `t_min` and `t_max` along the ray?
    pub fn occluded(
        &self,
        positions: &[Vec3],
        faces: &[[u32; 3]],
        origin: Vec3,
        dir: Vec3,
        t_min: f32,
        t_max: f32,
    ) -> bool {
        let inv = Vec3::new(1.0 / dir.x, 1.0 / dir.y, 1.0 / dir.z);
        let mut stack = [0u32; 64];
        let mut depth = 1;

        while depth > 0 {
            depth -= 1;
            let node = &self.nodes[stack[depth] as usize];

            if !slab_hit(node.min, node.max, origin, inv, t_min, t_max) {
                continue;
            }

            if node.count == 0 {
                stack[depth] = node.start;
                stack[depth + 1] = node.start + 1;
                depth += 2;
                continue;
            }

            for &i in &self.order[node.start as usize..(node.start + node.count) as usize] {
                let [a, b, c] = faces[i as usize];
                if triangle_hit(
                    positions[a as usize],
                    positions[b as usize],
                    positions[c as usize],
                    origin,
                    dir,
                    t_min,
                    t_max,
                ) {
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
    pub fn nearest(
        &self,
        positions: &[Vec3],
        faces: &[[u32; 3]],
        origin: Vec3,
        dir: Vec3,
        t_min: f32,
        t_max: f32,
    ) -> Option<(f32, usize)> {
        let inv = Vec3::new(1.0 / dir.x, 1.0 / dir.y, 1.0 / dir.z);
        let mut stack = [0u32; 64];
        let mut depth = 1;
        let mut best: Option<(f32, usize)> = None;

        while depth > 0 {
            depth -= 1;
            let node = &self.nodes[stack[depth] as usize];
            let limit = best.map_or(t_max, |(t, _)| t);

            if !slab_hit(node.min, node.max, origin, inv, t_min, limit) {
                continue;
            }

            if node.count == 0 {
                stack[depth] = node.start;
                stack[depth + 1] = node.start + 1;
                depth += 2;
                continue;
            }

            for &i in &self.order[node.start as usize..(node.start + node.count) as usize] {
                let [a, b, c] = faces[i as usize];
                let hit = triangle_t(
                    positions[a as usize],
                    positions[b as usize],
                    positions[c as usize],
                    origin,
                    dir,
                    t_min,
                    best.map_or(t_max, |(t, _)| t),
                );
                if let Some(t) = hit {
                    best = Some((t, i as usize));
                }
            }
        }

        best
    }
}

fn slab_hit(min: Vec3, max: Vec3, origin: Vec3, inv: Vec3, t_min: f32, t_max: f32) -> bool {
    let lo = min - origin;
    let hi = max - origin;
    let (mut near, mut far) = (t_min, t_max);

    for (l, h, i) in [(lo.x, hi.x, inv.x), (lo.y, hi.y, inv.y), (lo.z, hi.z, inv.z)] {
        let (t0, t1) = (l * i, h * i);
        near = near.max(t0.min(t1));
        far = far.min(t0.max(t1));
    }

    near <= far
}

fn triangle_hit(a: Vec3, b: Vec3, c: Vec3, origin: Vec3, dir: Vec3, t_min: f32, t_max: f32) -> bool {
    triangle_t(a, b, c, origin, dir, t_min, t_max).is_some()
}

/// Möller-Trumbore.
fn triangle_t(a: Vec3, b: Vec3, c: Vec3, origin: Vec3, dir: Vec3, t_min: f32, t_max: f32) -> Option<f32> {
    let (e1, e2) = (b - a, c - a);
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
