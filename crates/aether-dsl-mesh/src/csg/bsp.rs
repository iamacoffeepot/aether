//! Binary space partitioning tree over [`Polygon`]s.
//!
//! The tree's job is to represent a closed solid as a recursive
//! arrangement of half-spaces — each node owns a partitioner plane,
//! coplanar polygons attached to that plane, and child nodes for
//! polygons in front of and behind the plane.
//!
//! Node operations (`build`, `invert`, `clip_polygons`, `clip_to`) are
//! the primitives the boolean operators in [`super::ops`] compose.
//!
//! ### Determinism
//!
//! Per ADR-0054 the build order must be stable across platforms. We
//! sort the polygon list by an FNV1a hash of its plane equation + first
//! vertex before picking a splitter. Same input set → same hash order →
//! same tree shape → same triangle list out.

use super::plane::Plane3;
use super::polygon::Polygon;

#[derive(Debug, Clone, Default)]
pub struct Node {
    pub plane: Option<Plane3>,
    pub front: Option<Box<Node>>,
    pub back: Option<Box<Node>>,
    pub polygons: Vec<Polygon>,
}

impl Node {
    pub fn new() -> Self {
        Node::default()
    }

    /// Insert `polygons` into the tree, splitting against the current
    /// node's plane (or adopting the first polygon's plane if the node
    /// is fresh). Polygons crossing the splitter are partitioned into
    /// front/back fragments and recursed.
    pub fn build(&mut self, mut polygons: Vec<Polygon>) {
        if polygons.is_empty() {
            return;
        }
        // Stable polygon ordering — see module docs.
        polygons.sort_by_key(polygon_sort_key);

        if self.plane.is_none() {
            self.plane = Some(polygons[0].plane);
        }
        let partitioner = self.plane.unwrap();

        let mut front_polys = Vec::new();
        let mut back_polys = Vec::new();
        let mut coplanar_front = Vec::new();
        let mut coplanar_back = Vec::new();

        for poly in polygons {
            poly.split(
                &partitioner,
                &mut coplanar_front,
                &mut coplanar_back,
                &mut front_polys,
                &mut back_polys,
            );
        }

        self.polygons.extend(coplanar_front);
        self.polygons.extend(coplanar_back);

        if !front_polys.is_empty() {
            let front = self.front.get_or_insert_with(|| Box::new(Node::new()));
            front.build(front_polys);
        }
        if !back_polys.is_empty() {
            let back = self.back.get_or_insert_with(|| Box::new(Node::new()));
            back.build(back_polys);
        }
    }

    /// Recursively flip every polygon, plane, and tree pointer. The
    /// resulting tree represents the complement of the original solid.
    pub fn invert(&mut self) {
        for poly in &mut self.polygons {
            poly.invert();
        }
        if let Some(plane) = self.plane.as_mut() {
            *plane = plane.invert();
        }
        if let Some(front) = self.front.as_mut() {
            front.invert();
        }
        if let Some(back) = self.back.as_mut() {
            back.invert();
        }
        std::mem::swap(&mut self.front, &mut self.back);
    }

    /// Filter `polygons` to only the parts that lie outside the volume
    /// represented by `self`. Polygons may be split across multiple
    /// planes; each fragment surviving back-tree traversal is dropped.
    pub fn clip_polygons(&self, polygons: Vec<Polygon>) -> Vec<Polygon> {
        let Some(plane) = self.plane else {
            // Empty tree — nothing inside, all stay.
            return polygons;
        };

        let mut front = Vec::new();
        let mut back = Vec::new();
        let mut coplanar_front = Vec::new();
        let mut coplanar_back = Vec::new();

        for poly in polygons {
            poly.split(
                &plane,
                &mut coplanar_front,
                &mut coplanar_back,
                &mut front,
                &mut back,
            );
        }

        // Coplanar polygons get grouped with whichever side they face,
        // so shared boundaries are processed by the appropriate subtree.
        front.extend(coplanar_front);
        back.extend(coplanar_back);

        let mut front_result = if let Some(node) = &self.front {
            node.clip_polygons(front)
        } else {
            front
        };
        let back_result = if let Some(node) = &self.back {
            node.clip_polygons(back)
        } else {
            // No back subtree means everything that fell behind is
            // inside the solid; discard.
            Vec::new()
        };
        front_result.extend(back_result);
        front_result
    }

    /// Clip `self`'s polygons against `bsp`, removing parts inside it.
    pub fn clip_to(&mut self, bsp: &Node) {
        let owned = std::mem::take(&mut self.polygons);
        self.polygons = bsp.clip_polygons(owned);
        if let Some(front) = self.front.as_mut() {
            front.clip_to(bsp);
        }
        if let Some(back) = self.back.as_mut() {
            back.clip_to(bsp);
        }
    }

    /// Flatten the tree's polygons (own + front subtree + back subtree).
    pub fn all_polygons(&self) -> Vec<Polygon> {
        let mut out = self.polygons.clone();
        if let Some(front) = &self.front {
            out.extend(front.all_polygons());
        }
        if let Some(back) = &self.back {
            out.extend(back.all_polygons());
        }
        out
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
        let mut tree = Node::new();
        tree.build(polys);
        let out = tree.all_polygons();
        // Self-build can split coplanar pairs apart but never drops
        // them — the count is at least the input.
        assert!(out.len() >= n, "lost polygons: {} → {}", n, out.len());
    }

    #[test]
    fn invert_twice_is_identity_in_polygon_count() {
        let mut tree = Node::new();
        tree.build(unit_box());
        let before = tree.all_polygons().len();
        tree.invert();
        tree.invert();
        let after = tree.all_polygons().len();
        assert_eq!(before, after);
    }

    #[test]
    fn empty_tree_clip_passes_through() {
        let empty = Node::new();
        let polys = unit_box();
        let result = empty.clip_polygons(polys.clone());
        assert_eq!(result.len(), polys.len());
    }

    #[test]
    fn clip_to_self_keeps_boundary() {
        // The polygons of a closed solid lie on its own boundary, not
        // inside its volume — clip_to only drops polygons strictly
        // inside the clipping volume. csg.js relies on this same
        // invariant: A.clip_to(A) leaves A unchanged in `union(A, A)`.
        let mut tree = Node::new();
        tree.build(unit_box());
        let snapshot = tree.clone();
        let before = tree.all_polygons().len();
        tree.clip_to(&snapshot);
        let after = tree.all_polygons().len();
        assert!(after > 0, "self-clip dropped boundary polygons");
        assert_eq!(before, after, "self-clip changed polygon count");
    }

    #[test]
    fn deterministic_build_across_input_orderings() {
        let a = unit_box();
        let mut b = a.clone();
        b.reverse();
        let mut tree_a = Node::new();
        let mut tree_b = Node::new();
        tree_a.build(a.clone());
        tree_b.build(b);
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
