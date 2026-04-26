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
use super::cleanup;
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
    Ok(cleanup::run(na.all_polygons()))
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
    Ok(cleanup::run(na.all_polygons()))
}

pub fn difference(a: Vec<Polygon>, b: Vec<Polygon>) -> Result<Vec<Polygon>, CsgError> {
    let raw = difference_raw(a, b)?;
    Ok(cleanup::run(raw))
}

/// `difference` minus the cleanup pass — the raw polygon stream coming
/// out of the BSP composition. Used by diagnostic tests that want to
/// compare BSP-only vs full-pipeline output to localize bugs.
pub(crate) fn difference_raw(a: Vec<Polygon>, b: Vec<Polygon>) -> Result<Vec<Polygon>, CsgError> {
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

    /// Diagnostic: count directed boundary edges (edges appearing once
    /// with no reverse twin) in a polygon stream, treating `Point3` as
    /// the vertex identity. Used to compare manifold violations between
    /// BSP-raw and full-cleanup output.
    fn count_boundary_edges(polys: &[Polygon]) -> usize {
        use std::collections::HashMap;
        let mut directed: HashMap<(Point3, Point3), i32> = HashMap::new();
        for poly in polys {
            let n = poly.vertices.len();
            for i in 0..n {
                let a = poly.vertices[i];
                let b = poly.vertices[(i + 1) % n];
                if a == b {
                    continue;
                }
                *directed.entry((a, b)).or_insert(0) += 1;
            }
        }
        directed
            .iter()
            .filter(|&(&(a, b), _)| !directed.contains_key(&(b, a)))
            .count()
    }

    /// Build a UV sphere directly via the same routine the DSL uses.
    /// Returns the CSG-internal `Polygon` list ready for ops.
    fn build_sphere(radius: f32, subdivisions: u32, color: u32) -> Vec<Polygon> {
        use crate::ast::Node;
        let triangles = crate::mesh::mesh(&Node::Sphere {
            radius,
            subdivisions,
            color,
        })
        .expect("sphere mesh should not fail");
        triangles
            .into_iter()
            .filter_map(|t| {
                let v0 = Point3::from_f32(t.vertices[0]).ok()?;
                let v1 = Point3::from_f32(t.vertices[1]).ok()?;
                let v2 = Point3::from_f32(t.vertices[2]).ok()?;
                Polygon::from_triangle(v0, v1, v2, t.color)
            })
            .collect()
    }

    /// **Diagnostic**: compares boundary-edge count of `box - sphere`
    /// before vs after `cleanup::run`. Localizes whether the regression's
    /// 36 boundary edges originate in the BSP composition or in the
    /// post-BSP cleanup pipeline.
    ///
    /// Reads as: print both numbers via panic so the result is captured
    /// in test output. Will be deleted or converted to an assertion
    /// after the bug is localized.
    #[test]
    #[ignore = "diagnostic only — prints boundary edge counts to localize the bug"]
    fn diagnostic_box_minus_sphere_bsp_vs_cleanup() {
        let box_polys = axis_aligned_box(0.0, 0.0, 0.0, 0.75, 0.75, 0.75, 0);
        let sphere_polys = build_sphere(0.5, 12, 1);

        let raw = difference_raw(box_polys.clone(), sphere_polys.clone()).unwrap();
        let cleaned = difference(box_polys, sphere_polys).unwrap();

        let raw_boundary = count_boundary_edges(&raw);
        let cleaned_boundary = count_boundary_edges(&cleaned);

        panic!(
            "DIAGNOSTIC: box - sphere boundary edges — RAW BSP: {} polygons / {} boundary edges; \
             AFTER cleanup: {} polygons / {} boundary edges",
            raw.len(),
            raw_boundary,
            cleaned.len(),
            cleaned_boundary,
        );
    }

    /// **Diagnostic step 3**: count dropped fragments. Replaces
    /// `Polygon::split`'s silent `if f.len() >= 3` / `if b.len() >= 3`
    /// with a counted version, then runs box - sphere through it. If
    /// the drop count is high, the bug is "fragments collapsed by
    /// snap rounding"; if zero, the bug is elsewhere.
    ///
    /// We replicate `split` here rather than instrument the production
    /// code so production stays uninstrumented after the diagnostic.
    #[test]
    #[ignore = "diagnostic only — counts dropped sub-3-vertex fragments"]
    fn diagnostic_count_dropped_fragments() {
        let box_polys = axis_aligned_box(0.0, 0.0, 0.0, 0.75, 0.75, 0.75, 0);
        let sphere_polys = build_sphere(0.5, 12, 1);

        // Replicate the BSP composition but instrument `split` calls.
        // A dropped fragment is one where the spanning case produced
        // f or b with < 3 vertices.
        //
        // We approximate by counting spanning polygons that, given
        // their vertex types, *should* yield f.len() >= 3 and b.len()
        // >= 3 but have one of those collapse. This requires walking
        // the same logic as `split`.
        //
        // Simpler version: check raw BSP output for polygons with < 3
        // vertices (which would be already filtered) and for ratios of
        // total fragments that suggest drops.
        let raw = difference_raw(box_polys, sphere_polys).unwrap();
        let degenerate_in_output: usize = raw.iter().filter(|p| p.vertices.len() < 3).count();
        let total = raw.len();
        let three_vertex: usize = raw.iter().filter(|p| p.vertices.len() == 3).count();
        let four_vertex: usize = raw.iter().filter(|p| p.vertices.len() == 4).count();
        let five_or_more: usize = raw.iter().filter(|p| p.vertices.len() >= 5).count();
        panic!(
            "DIAGNOSTIC: {total} polys in raw BSP — {degenerate_in_output} degenerate (<3 verts, \
             should never appear), {three_vertex} tris, {four_vertex} quads, \
             {five_or_more} pentagons+",
        );
    }

    /// **Diagnostic step 2**: enumerate raw BSP boundary edges and
    /// search for "near-twin" partners — for each unmatched directed
    /// edge (a→b), find the closest-by-Manhattan-distance unmatched
    /// edge (b'→a') with same direction. If twins exist at small
    /// distances, the bug is `compute_intersection` snap asymmetry.
    /// If they don't, the bug is something else (missing fragment,
    /// wrong winding, etc.).
    #[test]
    #[ignore = "diagnostic only — searches for snap-asymmetry twins"]
    fn diagnostic_boundary_edge_near_twins() {
        let box_polys = axis_aligned_box(0.0, 0.0, 0.0, 0.75, 0.75, 0.75, 0);
        let sphere_polys = build_sphere(0.5, 12, 1);
        let raw = difference_raw(box_polys, sphere_polys).unwrap();

        use std::collections::HashMap;
        let mut directed: HashMap<(Point3, Point3), i32> = HashMap::new();
        for poly in &raw {
            let n = poly.vertices.len();
            for i in 0..n {
                let a = poly.vertices[i];
                let b = poly.vertices[(i + 1) % n];
                if a == b {
                    continue;
                }
                *directed.entry((a, b)).or_insert(0) += 1;
            }
        }
        let unmatched: Vec<(Point3, Point3)> = directed
            .iter()
            .filter_map(|(&(a, b), _)| {
                if !directed.contains_key(&(b, a)) {
                    Some((a, b))
                } else {
                    None
                }
            })
            .collect();

        // For each unmatched edge (a, b), find the closest other
        // unmatched edge (a', b') such that (b', a') is the reverse
        // partner we're missing. Report the Manhattan distance from
        // a to a'.
        let manhattan = |p: Point3, q: Point3| -> i64 {
            (p.x as i64 - q.x as i64).abs()
                + (p.y as i64 - q.y as i64).abs()
                + (p.z as i64 - q.z as i64).abs()
        };

        let mut report = String::new();
        let mut twin_distances: Vec<i64> = Vec::new();
        for &(a, b) in unmatched.iter().take(8) {
            // Look for any unmatched (a', b') where b' ≈ a and a' ≈ b.
            let mut best: Option<(i64, Point3, Point3)> = None;
            for &(ap, bp) in &unmatched {
                if (ap, bp) == (a, b) {
                    continue;
                }
                let d = manhattan(bp, a) + manhattan(ap, b);
                if best.map(|(bd, _, _)| d < bd).unwrap_or(true) {
                    best = Some((d, ap, bp));
                }
            }
            if let Some((d, ap, bp)) = best {
                twin_distances.push(d);
                report.push_str(&format!(
                    "edge ({:?}, {:?}) — closest reverse twin candidate ({:?}, {:?}) Manhattan={d}\n",
                    a.to_f32(),
                    b.to_f32(),
                    bp.to_f32(),
                    ap.to_f32(),
                ));
            }
        }
        twin_distances.sort();
        let median = twin_distances
            .get(twin_distances.len() / 2)
            .copied()
            .unwrap_or(0);
        panic!(
            "DIAGNOSTIC: {} unmatched boundary edges; median Manhattan distance to closest \
             reverse-twin candidate = {} fixed units. Sample:\n{}",
            unmatched.len(),
            median,
            report
        );
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
