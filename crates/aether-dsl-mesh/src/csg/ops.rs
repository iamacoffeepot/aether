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
//! no parsing, no rendering.
//!
//! ### Output shape
//!
//! [`union`] / [`intersection`] / [`difference`] return n-gon boundary
//! loops (no triangulation) — the canonical mesh form per ADR-0057.
//! BSP composition is shared with the no-cleanup `_raw` helpers
//! ([`union_raw`], [`intersection_raw`], [`difference_raw`]) which the
//! AST mesh evaluator uses to chain CSG operations without inter-op
//! cleanup. The wire `Vec<Triangle>` path triangulates only at the
//! root via [`super::tessellate::run`].

use super::CsgError;
use super::bsp::BspTree;
use super::cleanup;
use super::polygon::Polygon;

pub fn union(a: Vec<Polygon>, b: Vec<Polygon>) -> Result<Vec<Polygon>, CsgError> {
    Ok(cleanup::run_to_loops(union_raw(a, b)?))
}

pub fn intersection(a: Vec<Polygon>, b: Vec<Polygon>) -> Result<Vec<Polygon>, CsgError> {
    Ok(cleanup::run_to_loops(intersection_raw(a, b)?))
}

pub fn difference(a: Vec<Polygon>, b: Vec<Polygon>) -> Result<Vec<Polygon>, CsgError> {
    Ok(cleanup::run_to_loops(difference_raw(a, b)?))
}

/// `union` minus the cleanup pass — the raw polygon stream coming
/// out of the BSP composition. Used by diagnostic tests that want to
/// compare BSP-only vs full-pipeline output to localize bugs.
pub(crate) fn union_raw(a: Vec<Polygon>, b: Vec<Polygon>) -> Result<Vec<Polygon>, CsgError> {
    let mut na = BspTree::new();
    let mut nb = BspTree::new();
    na.build(a)?;
    nb.build(b)?;
    na.clip_to(&nb)?;
    nb.clip_to(&na)?;
    nb.invert();
    nb.clip_to(&na)?;
    nb.invert();
    let extra = nb.all_polygons()?;
    na.build(extra)?;
    na.all_polygons()
}

/// `intersection` minus the cleanup pass — see [`union_raw`].
pub(crate) fn intersection_raw(a: Vec<Polygon>, b: Vec<Polygon>) -> Result<Vec<Polygon>, CsgError> {
    let mut na = BspTree::new();
    let mut nb = BspTree::new();
    na.build(a)?;
    nb.build(b)?;
    na.invert();
    nb.clip_to(&na)?;
    nb.invert();
    na.clip_to(&nb)?;
    nb.clip_to(&na)?;
    let extra = nb.all_polygons()?;
    na.build(extra)?;
    na.invert();
    na.all_polygons()
}

/// `difference` minus the cleanup pass — see [`union_raw`].
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
    let extra = nb.all_polygons()?;
    na.build(extra)?;
    na.invert();
    na.all_polygons()
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
    /// before vs after the cleanup pipeline. Localizes whether the
    /// regression's 36 boundary edges originate in the BSP composition
    /// or in the post-BSP cleanup pipeline.
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

    /// **Diagnostic step 9**: same as step 8 but for the
    /// box-minus-protruding-sphere case (sphere of radius 0.95 actually
    /// pokes through cube faces — genuine intersection geometry).
    /// What perpendicular distance would catch the remaining 2
    /// boundary edges?
    #[test]
    #[ignore = "diagnostic only — perp distance for protruding sphere"]
    fn diagnostic_protruding_sphere_perp_distances() {
        let box_polys = axis_aligned_box(0.0, 0.0, 0.0, 0.75, 0.75, 0.75, 0);
        let sphere_polys = build_sphere(0.95, 12, 1);
        let cleaned = difference(box_polys, sphere_polys).unwrap();

        use std::collections::{HashMap, HashSet};
        let mut directed: HashMap<(Point3, Point3), usize> = HashMap::new();
        let mut all_vertices: HashSet<Point3> = HashSet::new();
        for poly in &cleaned {
            let n = poly.vertices.len();
            for i in 0..n {
                let a = poly.vertices[i];
                let b = poly.vertices[(i + 1) % n];
                if a == b {
                    continue;
                }
                *directed.entry((a, b)).or_insert(0) += 1;
                all_vertices.insert(a);
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

        let mut report = String::new();
        report.push_str(&format!("POST-CLEANUP unmatched: {}\n", unmatched.len()));
        for &(a, b) in &unmatched {
            let abx = (b.x - a.x) as i128;
            let aby = (b.y - a.y) as i128;
            let abz = (b.z - a.z) as i128;
            let edge_len2 = abx * abx + aby * aby + abz * abz;
            if edge_len2 == 0 {
                continue;
            }
            // Find closest near-collinear candidate.
            let mut best: Option<(f64, Point3)> = None;
            for v in &all_vertices {
                if *v == a || *v == b {
                    continue;
                }
                let apx = (v.x - a.x) as i128;
                let apy = (v.y - a.y) as i128;
                let apz = (v.z - a.z) as i128;
                let dot = apx * abx + apy * aby + apz * abz;
                if dot <= 0 || dot >= edge_len2 {
                    continue;
                }
                let cx = apy * abz - apz * aby;
                let cy = apz * abx - apx * abz;
                let cz = apx * aby - apy * abx;
                let cross_mag2 = (cx * cx + cy * cy + cz * cz) as f64;
                let perp = (cross_mag2 / edge_len2 as f64).sqrt();
                if best.map(|(b, _)| perp < b).unwrap_or(true) {
                    best = Some((perp, *v));
                }
            }
            match best {
                Some((perp, v)) => {
                    report.push_str(&format!(
                        "  edge {:?}→{:?}: closest candidate {:?} at perp={:.3}\n",
                        a.to_f32(),
                        b.to_f32(),
                        v.to_f32(),
                        perp
                    ));
                }
                None => {
                    report.push_str(&format!(
                        "  edge {:?}→{:?}: NO collinear candidate found\n",
                        a.to_f32(),
                        b.to_f32()
                    ));
                }
            }
        }
        panic!("{report}");
    }

    /// **Diagnostic step 8**: for each unmatched boundary edge in raw
    /// BSP output, find candidate "would-be collinear" vertices from
    /// other polygons (vertices that fall geometrically between the
    /// edge's endpoints), and compute their perpendicular distance to
    /// the edge's supposed line. Reports the distribution so we know
    /// what tolerance value would catch them all.
    #[test]
    #[ignore = "diagnostic only — measures perpendicular distance of would-be collinear vertices"]
    fn diagnostic_perpendicular_distance_distribution() {
        let box_polys = axis_aligned_box(0.0, 0.0, 0.0, 0.75, 0.75, 0.75, 0);
        let sphere_polys = build_sphere(0.5, 12, 1);
        let raw = difference_raw(box_polys, sphere_polys).unwrap();

        use std::collections::{HashMap, HashSet};
        let mut directed: HashMap<(Point3, Point3), usize> = HashMap::new();
        let mut all_vertices: HashSet<Point3> = HashSet::new();
        for poly in &raw {
            let n = poly.vertices.len();
            for i in 0..n {
                let a = poly.vertices[i];
                let b = poly.vertices[(i + 1) % n];
                if a == b {
                    continue;
                }
                *directed.entry((a, b)).or_insert(0) += 1;
                all_vertices.insert(a);
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

        // For each unmatched edge, find vertices in the pool that fall
        // strictly between its endpoints (parametric t in (0, 1) when
        // projected onto edge direction). Compute perpendicular distance
        // squared and edge length squared to derive perpendicular distance.
        let mut perp_distances: Vec<f64> = Vec::new();
        let mut samples_above_threshold: Vec<String> = Vec::new();
        for &(a, b) in &unmatched {
            let abx = (b.x - a.x) as i128;
            let aby = (b.y - a.y) as i128;
            let abz = (b.z - a.z) as i128;
            let edge_len2 = abx * abx + aby * aby + abz * abz;
            if edge_len2 == 0 {
                continue;
            }
            for v in &all_vertices {
                if *v == a || *v == b {
                    continue;
                }
                let apx = (v.x - a.x) as i128;
                let apy = (v.y - a.y) as i128;
                let apz = (v.z - a.z) as i128;
                // Parametric projection: t = (a→v · a→b) / |a→b|²
                // Skip vertices outside the segment.
                let dot = apx * abx + apy * aby + apz * abz;
                if dot <= 0 || dot >= edge_len2 {
                    continue;
                }
                // Perpendicular distance = |cross(a→b, a→v)| / |a→b|
                let cx = apy * abz - apz * aby;
                let cy = apz * abx - apx * abz;
                let cz = apx * aby - apy * abx;
                let cross_mag2 = cx * cx + cy * cy + cz * cz;
                if cross_mag2 == 0 {
                    continue; // exactly collinear; t-junction would catch
                }
                let perp_sq = cross_mag2 as f64 / edge_len2 as f64;
                let perp = perp_sq.sqrt();
                // Only include "near-collinear" candidates — within ~10
                // fixed units perpendicular. Beyond that, they're not
                // mistakes from snap drift, just unrelated vertices.
                if perp < 10.0 {
                    perp_distances.push(perp);
                    if perp >= 1.0 && samples_above_threshold.len() < 5 {
                        samples_above_threshold.push(format!(
                            "  perp={perp:.3} fixed units; edge={:?}→{:?}, v={:?}",
                            a.to_f32(),
                            b.to_f32(),
                            v.to_f32()
                        ));
                    }
                }
            }
        }
        perp_distances.sort_by(|x, y| x.partial_cmp(y).unwrap());
        let buckets = [
            ("< 0.5", perp_distances.iter().filter(|&&p| p < 0.5).count()),
            (
                "0.5 - 1.0",
                perp_distances
                    .iter()
                    .filter(|&&p| (0.5..1.0).contains(&p))
                    .count(),
            ),
            (
                "1.0 - 2.0",
                perp_distances
                    .iter()
                    .filter(|&&p| (1.0..2.0).contains(&p))
                    .count(),
            ),
            (
                "2.0 - 5.0",
                perp_distances
                    .iter()
                    .filter(|&&p| (2.0..5.0).contains(&p))
                    .count(),
            ),
            (
                "5.0 - 10.0",
                perp_distances
                    .iter()
                    .filter(|&&p| (5.0..10.0).contains(&p))
                    .count(),
            ),
        ];
        let max = perp_distances.last().copied().unwrap_or(0.0);
        let median = perp_distances
            .get(perp_distances.len() / 2)
            .copied()
            .unwrap_or(0.0);
        let mut report = String::new();
        report.push_str(&format!(
            "DIAGNOSTIC: {} unmatched edges, {} candidate (vertex, edge) pairs found within 10 fixed units perpendicular.\n",
            unmatched.len(),
            perp_distances.len()
        ));
        report.push_str("Distribution of perpendicular distances (in fixed units):\n");
        for (label, count) in &buckets {
            report.push_str(&format!("  {label}: {count}\n"));
        }
        report.push_str(&format!("Median: {median:.3}, Max: {max:.3}\n"));
        report.push_str("Samples above 1.0 fixed unit:\n");
        for s in &samples_above_threshold {
            report.push_str(s);
            report.push('\n');
        }
        panic!("{report}");
    }

    /// **Diagnostic step 7**: dump all raw-BSP polygons on the +Z cube
    /// face plane to see exactly what fragments exist and what edges
    /// they have. Each fragment should be a piece of the cube face,
    /// and adjacent fragments should share edges. Unmatched edges
    /// mean two fragments that should be neighbors don't have the
    /// same vertex sequence on their shared boundary.
    #[test]
    #[ignore = "diagnostic only — dump +Z face fragments"]
    fn diagnostic_plus_z_face_fragments() {
        let box_polys = axis_aligned_box(0.0, 0.0, 0.0, 0.75, 0.75, 0.75, 0);
        let sphere_polys = build_sphere(0.5, 12, 1);
        let raw = difference_raw(box_polys, sphere_polys).unwrap();

        let cap = (0.75_f64 * 65536.0).round() as i32;
        let on_plus_z: Vec<&Polygon> = raw
            .iter()
            .filter(|p| p.vertices.iter().all(|v| v.z == cap))
            .collect();

        let mut report = String::new();
        report.push_str(&format!(
            "{} polygons on +Z (z=0.75) cube face. Fragment list:\n",
            on_plus_z.len()
        ));
        for (i, poly) in on_plus_z.iter().enumerate() {
            report.push_str(&format!("  [{i}] color={} verts:\n", poly.color));
            for v in &poly.vertices {
                let f = v.to_f32();
                let (x, y, z) = (f.x, f.y, f.z);
                report.push_str(&format!("      ({x:.6}, {y:.6}, {z:.6})\n"));
            }
        }
        panic!("{report}");
    }

    /// **Diagnostic step 6**: re-run perimeter-vs-interior classification
    /// on POST-CLEANUP output (36 boundary edges) to see what cleanup
    /// fixed and what remains. If t-junction repair handles all
    /// "interior" edges and the remaining 36 are all "perimeter", the
    /// bug is at adjacent-face snap drift. If the remaining 36 still
    /// include "interior" edges, t-junction repair has a coverage gap.
    #[test]
    #[ignore = "diagnostic only — classify post-cleanup boundary edges"]
    fn diagnostic_post_cleanup_classification() {
        let box_polys = axis_aligned_box(0.0, 0.0, 0.0, 0.75, 0.75, 0.75, 0);
        let sphere_polys = build_sphere(0.5, 12, 1);
        let cleaned = difference(box_polys, sphere_polys).unwrap();

        use std::collections::HashMap;
        let mut directed: HashMap<(Point3, Point3), usize> = HashMap::new();
        for poly in &cleaned {
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

        let cap_count = |p: Point3| -> usize {
            let cap = (0.75_f64 * 65536.0).round() as i32;
            let mut n = 0;
            if p.x.abs() == cap {
                n += 1;
            }
            if p.y.abs() == cap {
                n += 1;
            }
            if p.z.abs() == cap {
                n += 1;
            }
            n
        };
        let mut by_class: HashMap<&'static str, usize> = HashMap::new();
        for &(a, b) in &unmatched {
            let na = cap_count(a);
            let nb = cap_count(b);
            let class = match (na, nb) {
                (3, _) | (_, 3) => "corner-touching",
                (2, 2) => "perimeter (both on cube wireframe edge)",
                (2, _) | (_, 2) => "perimeter-to-interior",
                _ => "interior (within cube face)",
            };
            *by_class.entry(class).or_insert(0) += 1;
        }
        let mut report = String::new();
        for (class, count) in &by_class {
            report.push_str(&format!("  {class}: {count}\n"));
        }
        panic!(
            "POST-CLEANUP: {} unmatched edges:\n{report}",
            unmatched.len()
        );
    }

    /// **Diagnostic step 5**: classify unmatched edges by location —
    /// cube perimeter (along cube wireframe edge, where two cube faces
    /// meet) vs interior (within one cube face). Perimeter mismatches
    /// would mean adjacent cube face splits produce different snap
    /// points; interior mismatches would mean within-face fragmentation
    /// produces inconsistent splits.
    #[test]
    #[ignore = "diagnostic only — perimeter vs interior unmatched edges"]
    fn diagnostic_perimeter_vs_interior_edges() {
        let box_polys = axis_aligned_box(0.0, 0.0, 0.0, 0.75, 0.75, 0.75, 0);
        let sphere_polys = build_sphere(0.5, 12, 1);
        let raw = difference_raw(box_polys, sphere_polys).unwrap();

        use std::collections::HashMap;
        let mut directed: HashMap<(Point3, Point3), usize> = HashMap::new();
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

        // Classify each unmatched edge.
        // - "perimeter" if both endpoints have ≥2 of their coords at ±0.75 cap
        //   (i.e., they sit on a cube wireframe edge).
        // - "corner-to-edge" if one endpoint is a cube corner (3 coords at cap)
        //   and the other has 2 coords at cap.
        // - "interior" otherwise.
        let cap_count = |p: Point3| -> usize {
            let cap = (0.75_f64 * 65536.0).round() as i32;
            let mut n = 0;
            if p.x.abs() == cap {
                n += 1;
            }
            if p.y.abs() == cap {
                n += 1;
            }
            if p.z.abs() == cap {
                n += 1;
            }
            n
        };
        let mut by_class: HashMap<&'static str, usize> = HashMap::new();
        let mut samples: Vec<String> = Vec::new();
        for &(a, b) in &unmatched {
            let na = cap_count(a);
            let nb = cap_count(b);
            let class = match (na, nb) {
                (3, _) | (_, 3) => "corner-touching",
                (2, 2) => "perimeter (both on cube wireframe edge)",
                (2, _) | (_, 2) => "perimeter-to-interior",
                _ => "interior (within cube face)",
            };
            *by_class.entry(class).or_insert(0) += 1;
            if samples.len() < 6 {
                samples.push(format!("  {class}: ({:?}, {:?})", a.to_f32(), b.to_f32()));
            }
        }
        let mut report = String::new();
        for (class, count) in &by_class {
            report.push_str(&format!("  {class}: {count}\n"));
        }
        let mut sample_lines = String::new();
        for s in &samples {
            sample_lines.push_str(s);
            sample_lines.push('\n');
        }
        panic!(
            "DIAGNOSTIC: classification of {} unmatched edges:\n{report}\nSamples:\n{sample_lines}",
            unmatched.len()
        );
    }

    /// **Diagnostic step 4**: classify unmatched boundary edges by
    /// the color of the polygons containing them. Box - sphere with
    /// the sphere fully inside the box should have ZERO sphere/cube
    /// shared boundary geometrically (the sphere doesn't reach the
    /// cube faces). So all unmatched edges should be cube↔cube (color
    /// 0 ↔ 0) — meaning cube face fragments aren't pairing up.
    #[test]
    #[ignore = "diagnostic only — classifies boundary edges by polygon color"]
    fn diagnostic_boundary_edge_colors() {
        let box_polys = axis_aligned_box(0.0, 0.0, 0.0, 0.75, 0.75, 0.75, 0);
        let sphere_polys = build_sphere(0.5, 12, 1);
        let raw = difference_raw(box_polys, sphere_polys).unwrap();

        use std::collections::HashMap;
        let mut directed: HashMap<(Point3, Point3), Vec<u32>> = HashMap::new();
        for poly in &raw {
            let n = poly.vertices.len();
            for i in 0..n {
                let a = poly.vertices[i];
                let b = poly.vertices[(i + 1) % n];
                if a == b {
                    continue;
                }
                directed.entry((a, b)).or_default().push(poly.color);
            }
        }
        let mut color_pairs: HashMap<(u32, &'static str), usize> = HashMap::new();
        let mut unmatched_count = 0;
        for (&(a, b), forward_colors) in directed.iter() {
            let reverse = directed.get(&(b, a));
            if reverse.is_none() {
                unmatched_count += 1;
                for &fc in forward_colors {
                    let key = (fc, "no-reverse");
                    *color_pairs.entry(key).or_insert(0) += 1;
                }
            }
        }
        let mut report = String::new();
        for ((color, status), count) in &color_pairs {
            report.push_str(&format!("  color={color} {status}: {count}\n"));
        }
        // Also count plane locations of unmatched edges.
        let mut by_plane: HashMap<&'static str, usize> = HashMap::new();
        for (&(a, b), _) in directed.iter() {
            if directed.contains_key(&(b, a)) {
                continue;
            }
            let p = |fixed: i32| (fixed as f64 / 65536.0 * 100.0).round() / 100.0;
            let ax = p(a.x);
            let ay = p(a.y);
            let az = p(a.z);
            let bx = p(b.x);
            let by = p(b.y);
            let bz = p(b.z);
            let plane = if ax == bx && ax.abs() == 0.75 {
                if ax > 0.0 {
                    "+X (x=0.75)"
                } else {
                    "-X (x=-0.75)"
                }
            } else if ay == by && ay.abs() == 0.75 {
                if ay > 0.0 {
                    "+Y (y=0.75)"
                } else {
                    "-Y (y=-0.75)"
                }
            } else if az == bz && az.abs() == 0.75 {
                if az > 0.0 {
                    "+Z (z=0.75)"
                } else {
                    "-Z (z=-0.75)"
                }
            } else {
                "OTHER (not on cube face)"
            };
            *by_plane.entry(plane).or_insert(0) += 1;
        }
        let mut plane_lines = String::new();
        for (plane, count) in &by_plane {
            plane_lines.push_str(&format!("  {plane}: {count}\n"));
        }
        panic!(
            "DIAGNOSTIC: {unmatched_count} unmatched boundary edges. By color of containing polygon:\n{report}\nBy plane location:\n{plane_lines}"
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

    /// `union` returns n-gon boundary loops aggregated by coplanar
    /// merging — for a `2-box ∪ 2-box overlap` the result has at least
    /// one polygon with >3 vertices (the merged face quads). Verifies
    /// the cleanup pipeline composes through to the public entry.
    #[test]
    fn union_returns_n_gon_loops() {
        let a = axis_aligned_box(0.0, 0.0, 0.0, 1.0, 1.0, 1.0, 0);
        let b = axis_aligned_box(0.5, 0.0, 0.0, 1.0, 1.0, 1.0, 1);
        let loops = union(a, b).unwrap();
        assert!(
            loops.iter().any(|p| p.vertices.len() > 3),
            "expected at least one n-gon loop with >3 vertices"
        );
    }

    #[test]
    fn intersection_returns_non_empty_for_overlapping_inputs() {
        let a = axis_aligned_box(0.0, 0.0, 0.0, 1.0, 1.0, 1.0, 0);
        let b = axis_aligned_box(0.5, 0.5, 0.5, 0.7, 0.7, 0.7, 1);
        let loops = intersection(a, b).unwrap();
        assert!(
            !loops.is_empty(),
            "overlapping intersection must be non-empty"
        );
    }

    #[test]
    fn difference_returns_non_empty_for_partial_subtractor() {
        let outer = axis_aligned_box(0.0, 0.0, 0.0, 1.0, 1.0, 1.0, 0);
        let cutter = axis_aligned_box(0.0, 0.0, 0.0, 0.5, 0.5, 0.5, 1);
        let loops = difference(outer, cutter).unwrap();
        assert!(!loops.is_empty());
    }

    /// Pre-fix: this case spun the BSP build loop indefinitely. The
    /// third `BspTree::build` call in `union_raw` (the rebuild after
    /// `nb.all_polygons()`) received polygons whose accumulated snap
    /// drift from prior `clip → invert → clip → invert` passes had
    /// pushed their vertices several grid units off their stored
    /// `plane`. The per-vertex `coplanar_threshold` budget (≈ 1 grid
    /// unit) misclassified the splitter polygon itself as FRONT, and
    /// every other coplanar fragment routed forward identically, so
    /// each iteration created one new node holding the same 36-polygon
    /// list — a tower of single-child nodes growing without bound.
    ///
    /// The fix in `Polygon::split` short-circuits via `canonical_key`
    /// plane equality before the per-vertex test: a polygon with a
    /// stored plane structurally identical to the partitioner is
    /// always coplanar regardless of vertex drift. This test pins the
    /// case so a future regression here surfaces as a wallclock
    /// failure, not as silent infinite work.
    #[test]
    fn union_box_offset_sphere_does_not_hang() {
        use crate::ast::Node;
        use aether_math::Vec3;

        let box_polys = axis_aligned_box(0.0, 0.0, 0.0, 0.5, 0.5, 0.5, 0);
        let sphere_node = Node::Translate {
            offset: Vec3::new(0.3, 0.15, 0.05),
            child: std::boxed::Box::new(Node::Sphere {
                radius: 0.5,
                subdivisions: 8,
                color: 1,
            }),
        };
        let sphere_tris = crate::mesh::mesh(&sphere_node).expect("sphere mesh should not fail");
        let sphere_polys: Vec<Polygon> = sphere_tris
            .into_iter()
            .filter_map(|t| {
                let v0 = Point3::from_f32(t.vertices[0]).ok()?;
                let v1 = Point3::from_f32(t.vertices[1]).ok()?;
                let v2 = Point3::from_f32(t.vertices[2]).ok()?;
                Polygon::from_triangle(v0, v1, v2, t.color)
            })
            .collect();

        let result = union(box_polys, sphere_polys).expect("union should not error");
        assert!(
            !result.is_empty(),
            "box ∪ offset-sphere should produce non-empty geometry"
        );
    }
}
