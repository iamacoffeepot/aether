//! Debug + invariant-checking tools for the polygon-domain mesh
//! pipeline (ADR-0057). Used by regression tests and on-demand from
//! the editor to diagnose topology issues without round-tripping
//! through the renderer.
//!
//! The most useful one is [`validate_manifold`]: a closed manifold
//! mesh has every directed edge appearing exactly once, with its
//! reverse appearing exactly once in an adjacent face. Boundary edges
//! (count=1) mean the mesh has holes (CSG output is broken).
//! Singular edges (count>2) mean non-manifold topology (multiple
//! faces meet at the same edge in the same direction).
//!
//! `validate_manifold` is the single best invariant-check for the
//! end-to-end pipeline: any CSG bug that drops a face, double-emits
//! a face, or produces inconsistent winding will show up as a
//! violation with concrete vertex coords pointing at the failure.

use crate::polygon::Polygon;
use std::collections::HashMap;

/// A grid-snapped 3D vertex used as a stable hash key. f32 coords get
/// snapped to the same 16:16 fixed-point grid the CSG core uses, so
/// vertices that should be identical (e.g. two faces meeting at an
/// edge) hash to the same key even after f32 round-tripping.
type VertKey = (i32, i32, i32);

fn vert_key(v: [f32; 3]) -> VertKey {
    use crate::csg::fixed::f32_to_fixed;
    (
        f32_to_fixed(v[0]).unwrap_or(0),
        f32_to_fixed(v[1]).unwrap_or(0),
        f32_to_fixed(v[2]).unwrap_or(0),
    )
}

fn from_key(k: VertKey) -> [f32; 3] {
    use crate::csg::fixed::fixed_to_f32;
    [fixed_to_f32(k.0), fixed_to_f32(k.1), fixed_to_f32(k.2)]
}

#[derive(Debug, Clone, PartialEq)]
pub enum ManifoldViolation {
    /// Edge appears in only one direction with no reverse twin —
    /// the mesh has a hole (boundary) where this edge sits. For CSG
    /// output (which should be closed), this means a face is missing.
    BoundaryEdge { v0: [f32; 3], v1: [f32; 3] },
    /// Edge (or its reverse) appears more than 2 times total —
    /// non-manifold topology. Multiple faces share this edge.
    SingularEdge {
        v0: [f32; 3],
        v1: [f32; 3],
        forward_count: usize,
        reverse_count: usize,
    },
    /// Edge appears the right number of times (2 total) but both in
    /// the same direction — adjacent faces don't have opposite
    /// winding, so the surface orientation is inconsistent.
    InconsistentWinding { v0: [f32; 3], v1: [f32; 3] },
}

/// Walk every directed edge across every polygon (outer + holes) and
/// flag manifold violations. A closed manifold mesh should produce an
/// empty Vec.
pub fn validate_manifold(polygons: &[Polygon]) -> Vec<ManifoldViolation> {
    let mut directed: HashMap<(VertKey, VertKey), usize> = HashMap::new();

    let record_loop = |loop_: &[[f32; 3]], directed: &mut HashMap<(VertKey, VertKey), usize>| {
        let n = loop_.len();
        if n < 2 {
            return;
        }
        for i in 0..n {
            let a = vert_key(loop_[i]);
            let b = vert_key(loop_[(i + 1) % n]);
            if a == b {
                // Degenerate edge (same vertex); skip.
                continue;
            }
            *directed.entry((a, b)).or_insert(0) += 1;
        }
    };

    for poly in polygons {
        record_loop(&poly.vertices, &mut directed);
        for hole in &poly.holes {
            record_loop(hole, &mut directed);
        }
    }

    let mut violations = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for &(a, b) in directed.keys() {
        let canonical = if a < b { (a, b) } else { (b, a) };
        if !seen.insert(canonical) {
            continue;
        }
        let forward = directed.get(&(a, b)).copied().unwrap_or(0);
        let reverse = directed.get(&(b, a)).copied().unwrap_or(0);
        let total = forward + reverse;
        let v0 = from_key(canonical.0);
        let v1 = from_key(canonical.1);
        if total == 1 {
            violations.push(ManifoldViolation::BoundaryEdge { v0, v1 });
        } else if total > 2 {
            violations.push(ManifoldViolation::SingularEdge {
                v0,
                v1,
                forward_count: forward,
                reverse_count: reverse,
            });
        } else if forward != 1 || reverse != 1 {
            // Total is 2 but not 1+1 — both same direction.
            violations.push(ManifoldViolation::InconsistentWinding { v0, v1 });
        }
    }
    // Sort for deterministic output order.
    violations.sort_by(|a, b| format!("{:?}", a).cmp(&format!("{:?}", b)));
    violations
}

/// Aggregate stats about a polygon mesh. Useful for spotting
/// gross differences between baseline and broken pipelines.
#[derive(Debug, Clone)]
pub struct Summary {
    pub polygon_count: usize,
    pub triangle_count_after_fan: usize,
    pub vertex_count_min: usize,
    pub vertex_count_max: usize,
    pub vertex_count_avg: f32,
    pub hole_count_total: usize,
    /// Polygons grouped by canonical plane direction signature.
    /// (sign(n_x), sign(n_y), sign(n_z)) → count. For axis-aligned
    /// meshes like cubes, expect 6 distinct entries.
    pub by_plane_direction: HashMap<(i8, i8, i8), usize>,
}

pub fn summary(polygons: &[Polygon]) -> Summary {
    let polygon_count = polygons.len();
    let triangle_count_after_fan = polygons
        .iter()
        .map(|p| {
            // Outer fan: n - 2. Each hole adds n - 2.
            let outer_tri = p.vertices.len().saturating_sub(2);
            let hole_tri: usize = p.holes.iter().map(|h| h.len().saturating_sub(2)).sum();
            outer_tri + hole_tri
        })
        .sum();
    let (vmin, vmax, vsum) =
        polygons
            .iter()
            .fold((usize::MAX, 0usize, 0usize), |(mn, mx, sum), p| {
                let n = p.vertices.len();
                (mn.min(n), mx.max(n), sum + n)
            });
    let avg = if polygon_count == 0 {
        0.0
    } else {
        vsum as f32 / polygon_count as f32
    };
    let hole_count_total = polygons.iter().map(|p| p.holes.len()).sum();
    let mut by_plane_direction: HashMap<(i8, i8, i8), usize> = HashMap::new();
    for p in polygons {
        let key = (
            p.plane_normal[0].signum() as i8,
            p.plane_normal[1].signum() as i8,
            p.plane_normal[2].signum() as i8,
        );
        *by_plane_direction.entry(key).or_insert(0) += 1;
    }
    Summary {
        polygon_count,
        triangle_count_after_fan,
        vertex_count_min: if polygon_count == 0 { 0 } else { vmin },
        vertex_count_max: vmax,
        vertex_count_avg: avg,
        hole_count_total,
        by_plane_direction,
    }
}

/// Human-readable per-polygon dump. One line per polygon: index,
/// color, plane normal, vertex count, hole count, centroid.
pub fn dump(polygons: &[Polygon]) -> String {
    let mut out = String::new();
    for (i, p) in polygons.iter().enumerate() {
        let cx: f32 = p.vertices.iter().map(|v| v[0]).sum::<f32>() / p.vertices.len() as f32;
        let cy: f32 = p.vertices.iter().map(|v| v[1]).sum::<f32>() / p.vertices.len() as f32;
        let cz: f32 = p.vertices.iter().map(|v| v[2]).sum::<f32>() / p.vertices.len() as f32;
        out.push_str(&format!(
            "[{:>3}] color={} normal=({:+.3},{:+.3},{:+.3}) verts={:>2} holes={} centroid=({:+.3},{:+.3},{:+.3})\n",
            i, p.color,
            p.plane_normal[0], p.plane_normal[1], p.plane_normal[2],
            p.vertices.len(), p.holes.len(),
            cx, cy, cz,
        ));
    }
    out
}

/// Pretty-printed assertion helper. Use at the call site:
///
/// ```ignore
/// assert!(
///     validate_manifold(&polys).is_empty(),
///     "{}",
///     report(&polys)
/// );
/// ```
pub fn report(polygons: &[Polygon]) -> String {
    let violations = validate_manifold(polygons);
    let summ = summary(polygons);
    let mut out = String::new();
    out.push_str(&format!(
        "polygon mesh report: {} polygons, {} triangles after fan-tessellate, {} holes total\n",
        summ.polygon_count, summ.triangle_count_after_fan, summ.hole_count_total
    ));
    out.push_str(&format!(
        "  vertices/poly: min={} max={} avg={:.1}\n",
        summ.vertex_count_min, summ.vertex_count_max, summ.vertex_count_avg
    ));
    let mut keys: Vec<&(i8, i8, i8)> = summ.by_plane_direction.keys().collect();
    keys.sort();
    out.push_str("  by plane direction (sign x,y,z → count):\n");
    for k in keys {
        out.push_str(&format!(
            "    ({:+},{:+},{:+}) → {}\n",
            k.0, k.1, k.2, summ.by_plane_direction[k]
        ));
    }
    if violations.is_empty() {
        out.push_str("  manifold: OK (closed, consistent winding)\n");
    } else {
        out.push_str(&format!("  manifold: {} VIOLATIONS:\n", violations.len()));
        for v in &violations {
            out.push_str(&format!("    {:?}\n", v));
        }
    }
    out.push_str("\nfull dump:\n");
    out.push_str(&dump(polygons));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Polygon;

    fn quad(verts: [[f32; 3]; 4], normal: [f32; 3]) -> Polygon {
        Polygon {
            vertices: verts.to_vec(),
            holes: vec![],
            plane_normal: normal,
            color: 0,
        }
    }

    #[test]
    fn closed_unit_cube_passes_manifold_check() {
        // 6 quad faces with consistent CCW-from-outside winding.
        let h = 0.5;
        let polys = vec![
            // -X
            quad(
                [[-h, -h, -h], [-h, -h, h], [-h, h, h], [-h, h, -h]],
                [-1.0, 0.0, 0.0],
            ),
            // +X
            quad(
                [[h, -h, -h], [h, h, -h], [h, h, h], [h, -h, h]],
                [1.0, 0.0, 0.0],
            ),
            // -Y
            quad(
                [[-h, -h, -h], [h, -h, -h], [h, -h, h], [-h, -h, h]],
                [0.0, -1.0, 0.0],
            ),
            // +Y
            quad(
                [[-h, h, -h], [-h, h, h], [h, h, h], [h, h, -h]],
                [0.0, 1.0, 0.0],
            ),
            // -Z
            quad(
                [[-h, -h, -h], [-h, h, -h], [h, h, -h], [h, -h, -h]],
                [0.0, 0.0, -1.0],
            ),
            // +Z
            quad(
                [[-h, -h, h], [h, -h, h], [h, h, h], [-h, h, h]],
                [0.0, 0.0, 1.0],
            ),
        ];
        let violations = validate_manifold(&polys);
        assert!(
            violations.is_empty(),
            "unit cube should be watertight; got {:#?}",
            violations
        );
    }

    #[test]
    fn empty_input_has_no_violations() {
        // Pin: zero polygons → zero violations (vacuously closed).
        // Catches a future change that surfaces "empty mesh" as a
        // distinct error condition.
        assert!(validate_manifold(&[]).is_empty());
    }

    #[test]
    fn singular_edge_three_polygons_share_one_directed_edge() {
        // Three triangles all walking the edge (a → b) the same way.
        // The validator's directed-edge counter sees forward_count=3
        // and reverse_count=0 — should report SingularEdge with
        // forward=3, reverse=0.
        let a = [0.0, 0.0, 0.0];
        let b = [1.0, 0.0, 0.0];
        let c = [0.0, 1.0, 0.0];
        let d = [0.0, -1.0, 0.0];
        let e = [0.0, 0.5, 0.5];
        let polys = vec![
            quad([a, b, c, c], [0.0, 0.0, 1.0]),
            quad([a, b, d, d], [0.0, 0.0, -1.0]),
            quad([a, b, e, e], [0.0, 1.0, 1.0]),
        ];
        let violations = validate_manifold(&polys);
        let singular = violations
            .iter()
            .filter(|v| matches!(v, ManifoldViolation::SingularEdge { .. }))
            .count();
        assert!(
            singular >= 1,
            "expected at least one SingularEdge, got {violations:#?}"
        );
    }

    #[test]
    fn inconsistent_winding_two_polygons_walk_edge_same_direction() {
        // Two triangles that share an edge but walk it the same
        // direction (both `a → b`) instead of opposing. Should report
        // InconsistentWinding.
        let a = [0.0, 0.0, 0.0];
        let b = [1.0, 0.0, 0.0];
        let c1 = [0.5, 1.0, 0.0];
        let c2 = [0.5, -1.0, 0.0];
        let polys = vec![
            quad([a, b, c1, c1], [0.0, 0.0, 1.0]),
            quad([a, b, c2, c2], [0.0, 0.0, -1.0]),
        ];
        let violations = validate_manifold(&polys);
        let inconsistent = violations
            .iter()
            .filter(|v| matches!(v, ManifoldViolation::InconsistentWinding { .. }))
            .count();
        assert!(
            inconsistent >= 1,
            "expected at least one InconsistentWinding, got {violations:#?}"
        );
    }

    #[test]
    fn summary_polygon_and_triangle_counts_match_input() {
        // Pin the per-summary numerics so a future refactor of the
        // counting logic doesn't silently desync from input size.
        let h = 0.5;
        let polys = vec![quad(
            [[-h, -h, -h], [h, -h, -h], [h, h, -h], [-h, h, -h]],
            [0.0, 0.0, -1.0],
        )];
        let s = summary(&polys);
        assert_eq!(s.polygon_count, 1);
        // 4-vertex quad → 2 triangles after fan.
        assert_eq!(s.triangle_count_after_fan, 2);
        assert_eq!(s.hole_count_total, 0);
        assert_eq!(s.vertex_count_min, 4);
        assert_eq!(s.vertex_count_max, 4);
    }

    #[test]
    fn cube_with_one_face_missing_reports_boundary_edges() {
        let h = 0.5;
        // Same as above but drop the +Z face.
        let polys = vec![
            quad(
                [[-h, -h, -h], [-h, -h, h], [-h, h, h], [-h, h, -h]],
                [-1.0, 0.0, 0.0],
            ),
            quad(
                [[h, -h, -h], [h, h, -h], [h, h, h], [h, -h, h]],
                [1.0, 0.0, 0.0],
            ),
            quad(
                [[-h, -h, -h], [h, -h, -h], [h, -h, h], [-h, -h, h]],
                [0.0, -1.0, 0.0],
            ),
            quad(
                [[-h, h, -h], [-h, h, h], [h, h, h], [h, h, -h]],
                [0.0, 1.0, 0.0],
            ),
            quad(
                [[-h, -h, -h], [-h, h, -h], [h, h, -h], [h, -h, -h]],
                [0.0, 0.0, -1.0],
            ),
            // +Z face removed.
        ];
        let violations = validate_manifold(&polys);
        // The 4 edges around where the +Z face would have been are now boundary.
        let boundary_count = violations
            .iter()
            .filter(|v| matches!(v, ManifoldViolation::BoundaryEdge { .. }))
            .count();
        assert_eq!(
            boundary_count, 4,
            "expected 4 boundary edges around missing face, got {}: {:#?}",
            boundary_count, violations
        );
    }
}
