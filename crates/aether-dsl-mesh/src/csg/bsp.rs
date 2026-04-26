//! Binary space partitioning tree over [`Polygon`]s.
//!
//! The tree represents a closed solid as a recursive arrangement of
//! half-spaces — each node owns a partitioner plane, coplanar polygons
//! attached to that plane, and child slot indices for polygons in front
//! of and behind the plane.
//!
//! [`BspTree`] is the public type. Tree operations (`build`, `invert`,
//! `clip_polygons`, `clip_to`) are the primitives the boolean operators
//! in [`super::ops`] compose.
//!
//! ### Determinism
//!
//! Per ADR-0054 the build order must be stable across platforms. We
//! sort the polygon list by an FNV1a hash of its plane equation + every
//! vertex before picking a splitter. Same input set → same hash order →
//! same tree shape → same triangle list out.
//!
//! ### Iterative implementation
//!
//! Every traversal here is iterative — the tree lives in an arena
//! (`Vec<NodeData>` with `Option<usize>` child indices), and walks use
//! an explicit work stack rather than the call stack. The motivating
//! issue was the snap-drift cascade in
//! [`super::plane::Plane3::coplanar_threshold`]: with a recursive
//! implementation, an unforeseen geometric edge case could blow the
//! stack with no recovery; with iteration, the same scenario hits the
//! [`MAX_WORK_QUEUE`] cap and surfaces as a clean
//! [`CsgError::RecursionLimit`]. The arena layout also keeps node data
//! contiguous, which the BSP build hammers in inner loops.
//!
//! See `CLAUDE.md`'s "Recursion in load-bearing code" guideline for the
//! project-wide direction this implements.

use super::CsgError;
use super::plane::Plane3;
use super::polygon::Polygon;

/// Cap on the iterative work-queue size. With the snap-drift tolerance
/// in [`super::plane::Plane3::coplanar_threshold`] the tree depth is
/// bounded by the number of distinct planes in the input (typically
/// <100). 65536 leaves comfortable headroom while still firing fast
/// enough to surface a residual cascade as a clean
/// [`CsgError::RecursionLimit`] rather than runaway memory use.
pub const MAX_WORK_QUEUE: usize = 65_536;

const ROOT: usize = 0;

#[derive(Debug, Clone, Default)]
struct NodeData {
    plane: Option<Plane3>,
    front: Option<usize>,
    back: Option<usize>,
    polygons: Vec<Polygon>,
}

/// Arena-backed BSP tree. The root is always node index 0.
#[derive(Debug, Clone, Default)]
pub struct BspTree {
    nodes: Vec<NodeData>,
}

impl BspTree {
    pub fn new() -> Self {
        Self {
            nodes: vec![NodeData::default()],
        }
    }

    /// Insert `polygons` into the tree, splitting against each visited
    /// node's plane (or adopting the first polygon's plane if the node
    /// is fresh). Polygons crossing a splitter are partitioned into
    /// front/back fragments and pushed to the work queue.
    pub fn build(&mut self, polygons: Vec<Polygon>) -> Result<(), CsgError> {
        if polygons.is_empty() {
            return Ok(());
        }
        let mut work: Vec<(usize, Vec<Polygon>)> = vec![(ROOT, polygons)];
        while let Some((node_idx, mut polys)) = work.pop() {
            if work.len() >= MAX_WORK_QUEUE {
                return Err(CsgError::RecursionLimit {
                    limit: MAX_WORK_QUEUE,
                });
            }
            if polys.is_empty() {
                continue;
            }
            // Stable polygon ordering — see module docs.
            polys.sort_by_key(polygon_sort_key);

            if self.nodes[node_idx].plane.is_none() {
                self.nodes[node_idx].plane = Some(polys[0].plane);
            }
            let partitioner = self.nodes[node_idx].plane.unwrap();

            let mut front_polys = Vec::new();
            let mut back_polys = Vec::new();
            let mut coplanar_front = Vec::new();
            let mut coplanar_back = Vec::new();
            for poly in polys {
                poly.split(
                    &partitioner,
                    &mut coplanar_front,
                    &mut coplanar_back,
                    &mut front_polys,
                    &mut back_polys,
                );
            }
            {
                let node = &mut self.nodes[node_idx];
                node.polygons.extend(coplanar_front);
                node.polygons.extend(coplanar_back);
            }
            if !front_polys.is_empty() {
                let child = self.ensure_front(node_idx);
                work.push((child, front_polys));
            }
            if !back_polys.is_empty() {
                let child = self.ensure_back(node_idx);
                work.push((child, back_polys));
            }
        }
        Ok(())
    }

    /// Flip every polygon, plane, and tree pointer. The resulting tree
    /// represents the complement of the original solid.
    pub fn invert(&mut self) {
        let mut stack: Vec<usize> = vec![ROOT];
        while let Some(idx) = stack.pop() {
            let node = &mut self.nodes[idx];
            for poly in &mut node.polygons {
                poly.invert();
            }
            if let Some(plane) = node.plane.as_mut() {
                *plane = plane.invert();
            }
            std::mem::swap(&mut node.front, &mut node.back);
            let front = node.front;
            let back = node.back;
            if let Some(c) = front {
                stack.push(c);
            }
            if let Some(c) = back {
                stack.push(c);
            }
        }
    }

    /// Filter `polygons` to only the parts that lie outside the volume
    /// represented by `self`. Polygons crossing a splitter may be split
    /// across multiple planes; fragments routed into a missing back
    /// subtree are dropped (they're inside the volume), fragments
    /// routed into a missing front subtree are kept (outside).
    pub fn clip_polygons(&self, polygons: Vec<Polygon>) -> Result<Vec<Polygon>, CsgError> {
        let mut output = Vec::new();
        let mut work: Vec<(usize, Vec<Polygon>)> = vec![(ROOT, polygons)];
        while let Some((node_idx, polys)) = work.pop() {
            if work.len() >= MAX_WORK_QUEUE {
                return Err(CsgError::RecursionLimit {
                    limit: MAX_WORK_QUEUE,
                });
            }
            let node = &self.nodes[node_idx];
            let Some(plane) = node.plane else {
                // Empty node — nothing to classify against, pass through.
                output.extend(polys);
                continue;
            };

            let mut front = Vec::new();
            let mut back = Vec::new();
            let mut coplanar_front = Vec::new();
            let mut coplanar_back = Vec::new();
            for poly in polys {
                poly.split(
                    &plane,
                    &mut coplanar_front,
                    &mut coplanar_back,
                    &mut front,
                    &mut back,
                );
            }
            // Coplanar polygons get grouped with whichever side they
            // face, so shared boundaries are processed by the
            // appropriate subtree.
            front.extend(coplanar_front);
            back.extend(coplanar_back);

            // Front polys → descend; if no front subtree, keep.
            if !front.is_empty() {
                if let Some(child) = node.front {
                    work.push((child, front));
                } else {
                    output.extend(front);
                }
            }
            // Back polys → descend; if no back subtree, drop (inside).
            if !back.is_empty()
                && let Some(child) = node.back
            {
                work.push((child, back));
            }
        }
        Ok(output)
    }

    /// Clip `self`'s polygons against `other`, removing parts inside it.
    pub fn clip_to(&mut self, other: &BspTree) -> Result<(), CsgError> {
        let mut stack: Vec<usize> = vec![ROOT];
        while let Some(idx) = stack.pop() {
            if stack.len() >= MAX_WORK_QUEUE {
                return Err(CsgError::RecursionLimit {
                    limit: MAX_WORK_QUEUE,
                });
            }
            let owned = std::mem::take(&mut self.nodes[idx].polygons);
            self.nodes[idx].polygons = other.clip_polygons(owned)?;
            let front = self.nodes[idx].front;
            let back = self.nodes[idx].back;
            if let Some(c) = front {
                stack.push(c);
            }
            if let Some(c) = back {
                stack.push(c);
            }
        }
        Ok(())
    }

    /// Flatten the tree's polygons into a single list (every node's
    /// `polygons` plus every descendant's, in DFS order).
    pub fn all_polygons(&self) -> Vec<Polygon> {
        let mut out = Vec::new();
        let mut stack: Vec<usize> = vec![ROOT];
        while let Some(idx) = stack.pop() {
            let node = &self.nodes[idx];
            out.extend(node.polygons.iter().cloned());
            if let Some(c) = node.front {
                stack.push(c);
            }
            if let Some(c) = node.back {
                stack.push(c);
            }
        }
        out
    }

    fn ensure_front(&mut self, parent_idx: usize) -> usize {
        if let Some(idx) = self.nodes[parent_idx].front {
            return idx;
        }
        let idx = self.nodes.len();
        self.nodes.push(NodeData::default());
        self.nodes[parent_idx].front = Some(idx);
        idx
    }

    fn ensure_back(&mut self, parent_idx: usize) -> usize {
        if let Some(idx) = self.nodes[parent_idx].back {
            return idx;
        }
        let idx = self.nodes.len();
        self.nodes.push(NodeData::default());
        self.nodes[parent_idx].back = Some(idx);
        idx
    }
}

/// FNV1a-derived stable sort key per ADR-0054. Hashes the polygon's
/// plane equation + every vertex into a 64-bit lane that's identical
/// across runs and platforms. Hashing every vertex (not just the first)
/// is required because cube-style geometry has multiple triangles
/// sharing both a plane and a first vertex — without the rest of the
/// vertex list, those polygons collide and `sort_by_key`'s stable order
/// becomes input-order-dependent.
fn polygon_sort_key(poly: &Polygon) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;
    let mut hash = FNV_OFFSET;
    let mut feed = |bytes: &[u8]| {
        for &b in bytes {
            hash ^= b as u64;
            hash = hash.wrapping_mul(FNV_PRIME);
        }
    };
    feed(&poly.plane.n_x.to_le_bytes());
    feed(&poly.plane.n_y.to_le_bytes());
    feed(&poly.plane.n_z.to_le_bytes());
    feed(&poly.plane.d.to_le_bytes());
    for v in &poly.vertices {
        feed(&v.x.to_le_bytes());
        feed(&v.y.to_le_bytes());
        feed(&v.z.to_le_bytes());
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::csg::fixed::f32_to_fixed;
    use crate::csg::point::Point3;

    fn pt(x: f32, y: f32, z: f32) -> Point3 {
        Point3 {
            x: f32_to_fixed(x).unwrap(),
            y: f32_to_fixed(y).unwrap(),
            z: f32_to_fixed(z).unwrap(),
        }
    }

    fn unit_box() -> Vec<Polygon> {
        // Six faces of a [-1, 1] cube, CCW from outside.
        let v = |sx: f32, sy: f32, sz: f32| pt(sx, sy, sz);
        let tri =
            |a, b, c| Polygon::from_triangle(a, b, c, 0).expect("non-degenerate cube triangle");
        vec![
            // +X
            tri(v(1.0, -1.0, -1.0), v(1.0, 1.0, -1.0), v(1.0, 1.0, 1.0)),
            tri(v(1.0, -1.0, -1.0), v(1.0, 1.0, 1.0), v(1.0, -1.0, 1.0)),
            // -X
            tri(v(-1.0, -1.0, -1.0), v(-1.0, -1.0, 1.0), v(-1.0, 1.0, 1.0)),
            tri(v(-1.0, -1.0, -1.0), v(-1.0, 1.0, 1.0), v(-1.0, 1.0, -1.0)),
            // +Y
            tri(v(-1.0, 1.0, -1.0), v(-1.0, 1.0, 1.0), v(1.0, 1.0, 1.0)),
            tri(v(-1.0, 1.0, -1.0), v(1.0, 1.0, 1.0), v(1.0, 1.0, -1.0)),
            // -Y
            tri(v(-1.0, -1.0, -1.0), v(1.0, -1.0, -1.0), v(1.0, -1.0, 1.0)),
            tri(v(-1.0, -1.0, -1.0), v(1.0, -1.0, 1.0), v(-1.0, -1.0, 1.0)),
            // +Z
            tri(v(-1.0, -1.0, 1.0), v(1.0, -1.0, 1.0), v(1.0, 1.0, 1.0)),
            tri(v(-1.0, -1.0, 1.0), v(1.0, 1.0, 1.0), v(-1.0, 1.0, 1.0)),
            // -Z
            tri(v(-1.0, -1.0, -1.0), v(-1.0, 1.0, -1.0), v(1.0, 1.0, -1.0)),
            tri(v(-1.0, -1.0, -1.0), v(1.0, 1.0, -1.0), v(1.0, -1.0, -1.0)),
        ]
    }

    #[test]
    fn build_and_flatten_round_trip_polygon_count() {
        let polys = unit_box();
        let n = polys.len();
        let mut tree = BspTree::new();
        tree.build(polys).unwrap();
        let out = tree.all_polygons();
        // Self-build can split coplanar pairs apart but never drops
        // them — the count is at least the input.
        assert!(out.len() >= n, "lost polygons: {} → {}", n, out.len());
    }

    #[test]
    fn invert_twice_is_identity_in_polygon_count() {
        let mut tree = BspTree::new();
        tree.build(unit_box()).unwrap();
        let before = tree.all_polygons().len();
        tree.invert();
        tree.invert();
        let after = tree.all_polygons().len();
        assert_eq!(before, after);
    }

    #[test]
    fn empty_tree_clip_passes_through() {
        let empty = BspTree::new();
        let polys = unit_box();
        let result = empty.clip_polygons(polys.clone()).unwrap();
        assert_eq!(result.len(), polys.len());
    }

    #[test]
    fn clip_to_self_keeps_boundary() {
        // The polygons of a closed solid lie on its own boundary, not
        // inside its volume — clip_to only drops polygons strictly
        // inside the clipping volume. csg.js relies on this same
        // invariant: A.clip_to(A) leaves A unchanged in `union(A, A)`.
        let mut tree = BspTree::new();
        tree.build(unit_box()).unwrap();
        let snapshot = tree.clone();
        let before = tree.all_polygons().len();
        tree.clip_to(&snapshot).unwrap();
        let after = tree.all_polygons().len();
        assert!(after > 0, "self-clip dropped boundary polygons");
        assert_eq!(before, after, "self-clip changed polygon count");
    }

    #[test]
    fn deterministic_build_across_input_orderings() {
        let a = unit_box();
        let mut b = a.clone();
        b.reverse();
        let mut tree_a = BspTree::new();
        let mut tree_b = BspTree::new();
        tree_a.build(a.clone()).unwrap();
        tree_b.build(b).unwrap();
        // The stable sort means the two trees flatten to the same
        // ordered polygon list (vertex-by-vertex equal).
        let pa = tree_a.all_polygons();
        let pb = tree_b.all_polygons();
        assert_eq!(pa.len(), pb.len());
        for (x, y) in pa.iter().zip(pb.iter()) {
            assert_eq!(x.vertices, y.vertices, "vertex order differs");
            assert_eq!(x.color, y.color);
        }
    }

    #[test]
    fn polygon_sort_key_is_stable_under_clone() {
        let polys = unit_box();
        let original: Vec<u64> = polys.iter().map(polygon_sort_key).collect();
        let cloned: Vec<u64> = polys.clone().iter().map(polygon_sort_key).collect();
        assert_eq!(original, cloned);
    }
}
