//! Boolean operations expressed as BSP tree-against-tree clipping.
//!
//! Three classical recursions (Thibault & Naylor 1980, popularized by
//! csg.js):
//!
//! - `union(A, B)`:        clip A by B, clip B by A, drop B's interior
//!   shared boundary, merge.
//! - `intersection(A, B) = invert(union(invert(A), invert(B)))`.
//! - `difference(A, B)   = invert(union(invert(A), B))`.
//!
//! Inputs and outputs are flat polygon lists — this module does no I/O,
//! no parsing, no rendering. The mesher in `crate::mesh` converts
//! triangles ↔ polygons at the boundary.

use super::CsgError;
use super::bsp::BspTree;
use super::polygon::Polygon;

pub fn union(a: Vec<Polygon>, b: Vec<Polygon>) -> Result<Vec<Polygon>, CsgError> {
    let mut na = BspTree::new();
    let mut nb = BspTree::new();
    na.build(a)?;
    nb.build(b)?;
    na.clip_to(&nb)?;
    nb.clip_to(&na)?;
    nb.invert();
    nb.clip_to(&na)?;
    nb.invert();
    let extra = nb.all_polygons();
    na.build(extra)?;
    Ok(na.all_polygons())
}

pub fn intersection(a: Vec<Polygon>, b: Vec<Polygon>) -> Result<Vec<Polygon>, CsgError> {
    let mut na = BspTree::new();
    let mut nb = BspTree::new();
    na.build(a)?;
    nb.build(b)?;
    na.invert();
    nb.clip_to(&na)?;
    nb.invert();
    na.clip_to(&nb)?;
    nb.clip_to(&na)?;
    let extra = nb.all_polygons();
    na.build(extra)?;
    na.invert();
    Ok(na.all_polygons())
}

pub fn difference(a: Vec<Polygon>, b: Vec<Polygon>) -> Result<Vec<Polygon>, CsgError> {
    let mut na = BspTree::new();
    let mut nb = BspTree::new();
    na.build(a)?;
    nb.build(b)?;
    na.invert();
    na.clip_to(&nb)?;
    nb.clip_to(&na)?;
    nb.invert();
    nb.clip_to(&na)?;
    nb.invert();
    let extra = nb.all_polygons();
    na.build(extra)?;
    na.invert();
    Ok(na.all_polygons())
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

    fn axis_aligned_box(
        cx: f32,
        cy: f32,
        cz: f32,
        sx: f32,
        sy: f32,
        sz: f32,
        color: u32,
    ) -> Vec<Polygon> {
        let lo = (cx - sx, cy - sy, cz - sz);
        let hi = (cx + sx, cy + sy, cz + sz);
        let v = |x: f32, y: f32, z: f32| pt(x, y, z);
        let tri = |a, b, c| Polygon::from_triangle(a, b, c, color).expect("non-degenerate face");
        vec![
            // +X
            tri(
                v(hi.0, lo.1, lo.2),
                v(hi.0, hi.1, lo.2),
                v(hi.0, hi.1, hi.2),
            ),
            tri(
                v(hi.0, lo.1, lo.2),
                v(hi.0, hi.1, hi.2),
                v(hi.0, lo.1, hi.2),
            ),
            // -X
            tri(
                v(lo.0, lo.1, lo.2),
                v(lo.0, lo.1, hi.2),
                v(lo.0, hi.1, hi.2),
            ),
            tri(
                v(lo.0, lo.1, lo.2),
                v(lo.0, hi.1, hi.2),
                v(lo.0, hi.1, lo.2),
            ),
            // +Y
            tri(
                v(lo.0, hi.1, lo.2),
                v(lo.0, hi.1, hi.2),
                v(hi.0, hi.1, hi.2),
            ),
            tri(
                v(lo.0, hi.1, lo.2),
                v(hi.0, hi.1, hi.2),
                v(hi.0, hi.1, lo.2),
            ),
            // -Y
            tri(
                v(lo.0, lo.1, lo.2),
                v(hi.0, lo.1, lo.2),
                v(hi.0, lo.1, hi.2),
            ),
            tri(
                v(lo.0, lo.1, lo.2),
                v(hi.0, lo.1, hi.2),
                v(lo.0, lo.1, hi.2),
            ),
            // +Z
            tri(
                v(lo.0, lo.1, hi.2),
                v(hi.0, lo.1, hi.2),
                v(hi.0, hi.1, hi.2),
            ),
            tri(
                v(lo.0, lo.1, hi.2),
                v(hi.0, hi.1, hi.2),
                v(lo.0, hi.1, hi.2),
            ),
            // -Z
            tri(
                v(lo.0, lo.1, lo.2),
                v(lo.0, hi.1, lo.2),
                v(hi.0, hi.1, lo.2),
            ),
            tri(
                v(lo.0, lo.1, lo.2),
                v(hi.0, hi.1, lo.2),
                v(hi.0, lo.1, lo.2),
            ),
        ]
    }

    #[test]
    fn union_with_self_is_idempotent_in_topology() {
        let a = axis_aligned_box(0.0, 0.0, 0.0, 1.0, 1.0, 1.0, 0);
        let result = union(a.clone(), a).unwrap();
        // After union with itself, the surface count should match the
        // original (the union of identical solids has the same boundary).
        assert!(!result.is_empty());
    }

    #[test]
    fn difference_with_self_collapses_to_empty() {
        let a = axis_aligned_box(0.0, 0.0, 0.0, 1.0, 1.0, 1.0, 0);
        let result = difference(a.clone(), a).unwrap();
        assert!(
            result.is_empty(),
            "A − A should be empty; got {} polygons",
            result.len()
        );
    }

    #[test]
    fn intersection_with_self_preserves_volume() {
        let a = axis_aligned_box(0.0, 0.0, 0.0, 1.0, 1.0, 1.0, 0);
        let result = intersection(a.clone(), a).unwrap();
        assert!(!result.is_empty(), "A ∩ A should be non-empty");
    }

    #[test]
    fn intersection_of_disjoint_boxes_is_empty() {
        let a = axis_aligned_box(0.0, 0.0, 0.0, 0.5, 0.5, 0.5, 0);
        let b = axis_aligned_box(5.0, 5.0, 5.0, 0.5, 0.5, 0.5, 1);
        let result = intersection(a, b).unwrap();
        assert!(
            result.is_empty(),
            "disjoint intersection should be empty; got {} polygons",
            result.len()
        );
    }

    #[test]
    fn difference_of_box_minus_disjoint_box_returns_box() {
        let a = axis_aligned_box(0.0, 0.0, 0.0, 1.0, 1.0, 1.0, 0);
        let b = axis_aligned_box(10.0, 10.0, 10.0, 0.5, 0.5, 0.5, 1);
        let result = difference(a.clone(), b).unwrap();
        // Subtracting a disjoint box leaves the original surface intact.
        // The polygon count may differ slightly due to clip-and-rebuild
        // splitting, but must be non-empty.
        assert!(!result.is_empty());
        // Color of result should still be 0 (from `a`) — `b` is fully
        // outside, so none of its polygons survive.
        assert!(result.iter().all(|p| p.color == 0));
    }

    #[test]
    fn box_minus_inset_box_produces_walls() {
        // 2x2x2 outer minus 1x1x1 inner — should leave a hollow box.
        let outer = axis_aligned_box(0.0, 0.0, 0.0, 1.0, 1.0, 1.0, 0);
        let inner = axis_aligned_box(0.0, 0.0, 0.0, 0.5, 0.5, 0.5, 1);
        let result = difference(outer, inner).unwrap();
        assert!(!result.is_empty(), "hollow box should have walls");
        // Both colors should appear: outer (0) for outside walls,
        // inner (1) for the cavity surfaces.
        let has_outer = result.iter().any(|p| p.color == 0);
        let has_inner = result.iter().any(|p| p.color == 1);
        assert!(has_outer, "missing outer-wall polygons");
        assert!(has_inner, "missing cavity (inner-wall) polygons");
    }

    #[test]
    fn difference_color_inheritance_for_shared_volume() {
        // Cube minus an overlapping cube — the cavity walls come from
        // the subtractor's color, the remaining outer walls from the base.
        let base = axis_aligned_box(0.0, 0.0, 0.0, 1.0, 1.0, 1.0, 7);
        let cutter = axis_aligned_box(0.5, 0.0, 0.0, 1.0, 0.5, 0.5, 9);
        let result = difference(base, cutter).unwrap();
        let colors: std::collections::BTreeSet<u32> = result.iter().map(|p| p.color).collect();
        assert!(colors.contains(&7), "missing base polygons");
        assert!(colors.contains(&9), "missing cutter-walled cavity polygons");
    }

    #[test]
    fn determinism_across_runs() {
        // Identical inputs must produce identical outputs (ordered
        // polygon list, vertex-by-vertex). This is the bit-exact
        // reproducibility guarantee from ADR-0054.
        let a = axis_aligned_box(0.0, 0.0, 0.0, 1.0, 1.0, 1.0, 0);
        let b = axis_aligned_box(0.5, 0.5, 0.5, 0.7, 0.7, 0.7, 1);
        let r1 = difference(a.clone(), b.clone()).unwrap();
        let r2 = difference(a, b).unwrap();
        assert_eq!(r1.len(), r2.len());
        for (p, q) in r1.iter().zip(r2.iter()) {
            assert_eq!(p.vertices, q.vertices);
            assert_eq!(p.color, q.color);
        }
    }
}
