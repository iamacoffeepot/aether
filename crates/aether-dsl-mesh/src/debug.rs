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
use std::collections::{HashMap, HashSet};

/// Default tolerances for geometric validators. All in world units.
///
/// The CSG core snaps coordinates to a 16:16 fixed-point grid (`1 unit
/// ≈ 1.5e-5`, see [`crate::csg::fixed`]); these defaults sit a few snap
/// units above that floor so f32 reconstruction noise and plane
/// re-derivation drift don't trip the validators on legitimate output.
pub mod tol {
    /// Vertex-to-stored-plane distance allowed in [`validate_planarity`].
    /// ~33 fixed-point units — enough for f32 round-trip drift on cleanup
    /// output, tight enough to catch a polygon that legitimately bends.
    pub const PLANARITY: f32 = 5e-4;

    /// Edge length below which an edge is "sliver" in
    /// [`validate_polygon_quality`]. ~65 fixed-point units — smaller
    /// than any usable face edge but well above snap noise.
    pub const SLIVER_EDGE: f32 = 1e-3;

    /// Polygon area below which the polygon is degenerate.
    pub const DEGENERATE_AREA: f32 = 1e-6;

    /// Edge-length aspect ratio (longest / shortest) above which the
    /// polygon is shape-pathological.
    pub const ASPECT_RATIO: f32 = 1000.0;

    /// Cosine of the fold angle between adjacent face normals below
    /// which they're "flipped." `-1.0` is a 180° fold (back-to-back
    /// faces — a CSG sign error). `-0.99` is ~8° from full inversion,
    /// which still leaves headroom for sharp concave dihedrals.
    pub const FOLD_COS: f32 = -0.99;

    /// Distance from a vertex to a non-incident edge below which the
    /// vertex sits *on* the edge — a T-junction.
    pub const T_JUNCTION: f32 = 1e-4;
}

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

/// Geometric (non-topological) violations: shape, planarity, normal
/// continuity, T-junctions. These slip past [`validate_manifold`]
/// because they don't break edge counts — they break the *shape* of
/// the surface, which `tessellate_polygon` and the renderer notice.
#[derive(Debug, Clone, PartialEq)]
pub enum GeometryViolation {
    /// A polygon vertex sits more than `tol::PLANARITY` from the
    /// polygon's stored plane. Catches a polygon whose stored normal
    /// disagrees with its actual vertex layout (cleanup failure, plane
    /// re-derivation bug, or a genuinely non-planar n-gon).
    NonPlanar {
        polygon_index: usize,
        vertex: [f32; 3],
        distance: f32,
    },
    /// A polygon's projected signed area is below `tol::DEGENERATE_AREA`
    /// — it tessellates to ~zero pixels and is a likely source of
    /// downstream NaNs.
    DegenerateArea { polygon_index: usize, area: f32 },
    /// A polygon edge is shorter than `tol::SLIVER_EDGE`. Slivers slip
    /// through CSG cleanup and trigger CDT pathologies.
    SliverEdge {
        polygon_index: usize,
        v0: [f32; 3],
        v1: [f32; 3],
        length: f32,
    },
    /// A polygon's longest:shortest edge ratio exceeds `tol::ASPECT_RATIO`.
    ExtremeAspectRatio { polygon_index: usize, ratio: f32 },
    /// Two polygons share an edge but their stored normals point nearly
    /// opposite (`dot < tol::FOLD_COS`) — a folded surface, almost
    /// always a CSG sign error.
    FoldedNormals {
        v0: [f32; 3],
        v1: [f32; 3],
        cos_angle: f32,
    },
    /// A vertex lies on the open interior of a non-incident edge — a
    /// T-junction. Cleanup should have inserted this vertex into the
    /// edge's owning loop; missing it produces render-visible cracks.
    TJunction {
        vertex: [f32; 3],
        edge_v0: [f32; 3],
        edge_v1: [f32; 3],
        distance: f32,
    },
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn length(v: [f32; 3]) -> f32 {
    dot(v, v).sqrt()
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

/// Build a 2D basis (u, v) on the plane normal to `n` so a polygon can
/// be projected for shoelace area. Picks the world axis least aligned
/// with `n` to avoid degenerate cross products.
fn plane_basis(n: [f32; 3]) -> ([f32; 3], [f32; 3]) {
    let abs = [n[0].abs(), n[1].abs(), n[2].abs()];
    let helper = if abs[0] <= abs[1] && abs[0] <= abs[2] {
        [1.0, 0.0, 0.0]
    } else if abs[1] <= abs[2] {
        [0.0, 1.0, 0.0]
    } else {
        [0.0, 0.0, 1.0]
    };
    let u_raw = cross(n, helper);
    let u_len = length(u_raw);
    let u = if u_len > 0.0 {
        [u_raw[0] / u_len, u_raw[1] / u_len, u_raw[2] / u_len]
    } else {
        [1.0, 0.0, 0.0]
    };
    let v = cross(n, u);
    (u, v)
}

fn projected_area(verts: &[[f32; 3]], n: [f32; 3]) -> f32 {
    if verts.len() < 3 {
        return 0.0;
    }
    let (u, v) = plane_basis(n);
    let projected: Vec<(f32, f32)> = verts.iter().map(|p| (dot(*p, u), dot(*p, v))).collect();
    let mut sum = 0.0;
    for i in 0..projected.len() {
        let (x0, y0) = projected[i];
        let (x1, y1) = projected[(i + 1) % projected.len()];
        sum += x0 * y1 - x1 * y0;
    }
    0.5 * sum
}

/// Each polygon vertex (outer + holes) must lie within `tol::PLANARITY`
/// of the polygon's stored plane. Plane is taken from the stored
/// `plane_normal` plus the centroid of the outer loop as the in-plane
/// reference point — both are what downstream rendering trusts.
pub fn validate_planarity(polygons: &[Polygon]) -> Vec<GeometryViolation> {
    let mut out = Vec::new();
    for (i, poly) in polygons.iter().enumerate() {
        if poly.vertices.is_empty() {
            continue;
        }
        let n = poly.plane_normal;
        // Use a vertex as the in-plane reference (any vertex works if
        // the polygon is genuinely planar; centroid would mask a single
        // out-of-plane vertex by averaging it in).
        let p0 = poly.vertices[0];
        let check = |v: [f32; 3], out: &mut Vec<GeometryViolation>| {
            let d = dot(sub(v, p0), n).abs();
            if d > tol::PLANARITY {
                out.push(GeometryViolation::NonPlanar {
                    polygon_index: i,
                    vertex: v,
                    distance: d,
                });
            }
        };
        for &v in &poly.vertices {
            check(v, &mut out);
        }
        for hole in &poly.holes {
            for &v in hole {
                check(v, &mut out);
            }
        }
    }
    out
}

/// Polygon shape sanity: signed-area degeneracy, sliver edges, and
/// extreme aspect ratio. These don't break manifoldness on paper but
/// they're failure modes for `tessellate_polygon` and the BSP core.
pub fn validate_polygon_quality(polygons: &[Polygon]) -> Vec<GeometryViolation> {
    let mut out = Vec::new();
    for (i, poly) in polygons.iter().enumerate() {
        let area = projected_area(&poly.vertices, poly.plane_normal).abs();
        if area < tol::DEGENERATE_AREA {
            out.push(GeometryViolation::DegenerateArea {
                polygon_index: i,
                area,
            });
        }
        let mut min_edge = f32::INFINITY;
        let mut max_edge: f32 = 0.0;
        let n = poly.vertices.len();
        for j in 0..n {
            let a = poly.vertices[j];
            let b = poly.vertices[(j + 1) % n];
            let len = length(sub(b, a));
            if len < tol::SLIVER_EDGE {
                out.push(GeometryViolation::SliverEdge {
                    polygon_index: i,
                    v0: a,
                    v1: b,
                    length: len,
                });
            }
            if len > 0.0 {
                if len < min_edge {
                    min_edge = len;
                }
                if len > max_edge {
                    max_edge = len;
                }
            }
        }
        if min_edge.is_finite() && min_edge > 0.0 {
            let ratio = max_edge / min_edge;
            if ratio > tol::ASPECT_RATIO {
                out.push(GeometryViolation::ExtremeAspectRatio {
                    polygon_index: i,
                    ratio,
                });
            }
        }
    }
    out
}

/// For each manifold-shared edge (exactly two adjacent polygons in
/// opposing directions), the two stored normals must not be nearly
/// antiparallel (`dot < tol::FOLD_COS`). Antiparallel = the surface is
/// folded back on itself at this edge, which means CSG misclassified
/// inside vs outside on one of the two faces.
pub fn validate_normal_coherence(polygons: &[Polygon]) -> Vec<GeometryViolation> {
    // Map: canonical (snapped) edge → list of (polygon_index, normal).
    type EdgeIncidents = HashMap<(VertKey, VertKey), Vec<(usize, [f32; 3])>>;
    let mut edges: EdgeIncidents = HashMap::new();
    for (i, poly) in polygons.iter().enumerate() {
        let walk = |loop_: &[[f32; 3]], edges: &mut EdgeIncidents| {
            let n = loop_.len();
            for j in 0..n {
                let a = vert_key(loop_[j]);
                let b = vert_key(loop_[(j + 1) % n]);
                if a == b {
                    continue;
                }
                let canonical = if a < b { (a, b) } else { (b, a) };
                edges
                    .entry(canonical)
                    .or_default()
                    .push((i, poly.plane_normal));
            }
        };
        walk(&poly.vertices, &mut edges);
        for hole in &poly.holes {
            walk(hole, &mut edges);
        }
    }
    let mut out = Vec::new();
    let mut seen: HashSet<(usize, usize)> = HashSet::new();
    for ((a, b), incidents) in edges.iter() {
        if incidents.len() != 2 {
            continue;
        }
        let (i0, n0) = incidents[0];
        let (i1, n1) = incidents[1];
        if i0 == i1 {
            continue;
        }
        let pair = if i0 < i1 { (i0, i1) } else { (i1, i0) };
        if !seen.insert(pair) {
            continue;
        }
        let cos = dot(n0, n1);
        if cos < tol::FOLD_COS {
            out.push(GeometryViolation::FoldedNormals {
                v0: from_key(*a),
                v1: from_key(*b),
                cos_angle: cos,
            });
        }
    }
    out.sort_by(|a, b| format!("{:?}", a).cmp(&format!("{:?}", b)));
    out
}

/// For every polygon edge (A, B) and every vertex V (across all
/// polygons), if V is not a snap-key endpoint of (A, B) but lies within
/// `tol::T_JUNCTION` of the open segment interior, flag a T-junction.
/// Cleanup should have inserted V as an explicit vertex of (A, B)'s
/// owning loop; missing it produces render-visible cracks.
pub fn validate_no_t_junctions(polygons: &[Polygon]) -> Vec<GeometryViolation> {
    // Collect every distinct vertex (snap-key + f32 coords for reporting).
    let mut all_verts: HashMap<VertKey, [f32; 3]> = HashMap::new();
    for poly in polygons {
        for &v in &poly.vertices {
            all_verts.insert(vert_key(v), v);
        }
        for hole in &poly.holes {
            for &v in hole {
                all_verts.insert(vert_key(v), v);
            }
        }
    }

    // Collect every directed edge once (canonicalized).
    let mut edges: HashSet<(VertKey, VertKey)> = HashSet::new();
    for poly in polygons {
        let walk = |loop_: &[[f32; 3]], edges: &mut HashSet<(VertKey, VertKey)>| {
            let n = loop_.len();
            for j in 0..n {
                let a = vert_key(loop_[j]);
                let b = vert_key(loop_[(j + 1) % n]);
                if a == b {
                    continue;
                }
                let canonical = if a < b { (a, b) } else { (b, a) };
                edges.insert(canonical);
            }
        };
        walk(&poly.vertices, &mut edges);
        for hole in &poly.holes {
            walk(hole, &mut edges);
        }
    }

    let mut out = Vec::new();
    let tol_sq = tol::T_JUNCTION * tol::T_JUNCTION;
    for (a_key, b_key) in &edges {
        let a = from_key(*a_key);
        let b = from_key(*b_key);
        let ab = sub(b, a);
        let ab_len_sq = dot(ab, ab);
        if ab_len_sq <= 0.0 {
            continue;
        }
        for (v_key, v) in &all_verts {
            if *v_key == *a_key || *v_key == *b_key {
                continue;
            }
            let av = sub(*v, a);
            let t = dot(av, ab) / ab_len_sq;
            // Open interior — exclude endpoints by a small parametric margin
            // proportional to the snap tolerance vs. edge length.
            let margin = (tol::T_JUNCTION / ab_len_sq.sqrt()).min(0.5);
            if t <= margin || t >= 1.0 - margin {
                continue;
            }
            // Perpendicular distance from V to line AB.
            let proj = [a[0] + t * ab[0], a[1] + t * ab[1], a[2] + t * ab[2]];
            let d = sub(*v, proj);
            let d_sq = dot(d, d);
            if d_sq <= tol_sq {
                out.push(GeometryViolation::TJunction {
                    vertex: *v,
                    edge_v0: a,
                    edge_v1: b,
                    distance: d_sq.sqrt(),
                });
            }
        }
    }
    out.sort_by(|x, y| format!("{:?}", x).cmp(&format!("{:?}", y)));
    out
}

/// Run [`validate_manifold`] plus all four geometric validators.
/// Returned tuple keeps manifold and geometric violations separate so
/// callers can attribute failures cleanly.
pub fn validate_geometry(polygons: &[Polygon]) -> (Vec<ManifoldViolation>, Vec<GeometryViolation>) {
    let manifold = validate_manifold(polygons);
    let mut geom = Vec::new();
    geom.extend(validate_planarity(polygons));
    geom.extend(validate_polygon_quality(polygons));
    geom.extend(validate_normal_coherence(polygons));
    geom.extend(validate_no_t_junctions(polygons));
    (manifold, geom)
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
    let planarity = validate_planarity(polygons);
    let quality = validate_polygon_quality(polygons);
    let normals = validate_normal_coherence(polygons);
    let tjunctions = validate_no_t_junctions(polygons);
    let total_geom = planarity.len() + quality.len() + normals.len() + tjunctions.len();
    if total_geom == 0 {
        out.push_str("  geometry: OK (planar, well-shaped, coherent, no T-junctions)\n");
    } else {
        out.push_str(&format!("  geometry: {} VIOLATIONS:\n", total_geom));
        for v in planarity
            .iter()
            .chain(&quality)
            .chain(&normals)
            .chain(&tjunctions)
        {
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
    fn validate_planarity_flags_off_plane_vertex() {
        // Quad whose 4th vertex is lifted 0.01 off the z=0 plane —
        // well above the 5e-4 PLANARITY tolerance. Stored normal is
        // +Z, so the lifted vertex's distance is exactly the lift.
        let polys = vec![Polygon {
            vertices: vec![
                [0.0, 0.0, 0.0],
                [1.0, 0.0, 0.0],
                [1.0, 1.0, 0.0],
                [0.0, 1.0, 0.01],
            ],
            holes: vec![],
            plane_normal: [0.0, 0.0, 1.0],
            color: 0,
        }];
        let v = validate_planarity(&polys);
        assert_eq!(v.len(), 1, "expected 1 NonPlanar violation, got {v:#?}");
        assert!(matches!(v[0], GeometryViolation::NonPlanar { .. }));
    }

    #[test]
    fn validate_polygon_quality_flags_sliver_edge() {
        // Triangle with one edge 1e-4 long — well below the 1e-3
        // SLIVER threshold.
        let polys = vec![Polygon {
            vertices: vec![[0.0, 0.0, 0.0], [1e-4, 0.0, 0.0], [0.5, 1.0, 0.0]],
            holes: vec![],
            plane_normal: [0.0, 0.0, 1.0],
            color: 0,
        }];
        let v = validate_polygon_quality(&polys);
        assert!(
            v.iter()
                .any(|g| matches!(g, GeometryViolation::SliverEdge { .. })),
            "expected at least one SliverEdge, got {v:#?}"
        );
    }

    #[test]
    fn validate_normal_coherence_flags_folded_pair() {
        // Two coplanar quads sharing edge (a,b) but with opposite
        // stored normals (+Z and −Z). They wind opposing around the
        // shared edge so manifold check passes — only normal
        // coherence catches the fold.
        let a = [0.0, 0.0, 0.0];
        let b = [1.0, 0.0, 0.0];
        let c = [1.0, 1.0, 0.0];
        let d = [0.0, 1.0, 0.0];
        let e = [1.0, -1.0, 0.0];
        let f = [0.0, -1.0, 0.0];
        let polys = vec![
            Polygon {
                vertices: vec![a, b, c, d],
                holes: vec![],
                plane_normal: [0.0, 0.0, 1.0],
                color: 0,
            },
            Polygon {
                vertices: vec![b, a, f, e],
                holes: vec![],
                plane_normal: [0.0, 0.0, -1.0],
                color: 0,
            },
        ];
        let v = validate_normal_coherence(&polys);
        assert!(
            v.iter()
                .any(|g| matches!(g, GeometryViolation::FoldedNormals { .. })),
            "expected at least one FoldedNormals, got {v:#?}"
        );
    }

    #[test]
    fn validate_no_t_junctions_flags_vertex_on_edge_interior() {
        // Triangle (a,b,c) plus a smaller triangle whose vertex sits
        // exactly on edge (a,b) but is not in (a,b)'s loop. Classic
        // T-junction.
        let a = [0.0, 0.0, 0.0];
        let b = [1.0, 0.0, 0.0];
        let c = [0.5, 1.0, 0.0];
        let mid_ab = [0.5, 0.0, 0.0]; // on edge a→b interior
        let d = [0.5, -1.0, 0.0];
        let polys = vec![
            Polygon {
                vertices: vec![a, b, c],
                holes: vec![],
                plane_normal: [0.0, 0.0, 1.0],
                color: 0,
            },
            Polygon {
                // walks a → mid_ab → d, then mid_ab → b → d (two
                // adjacent triangles sharing mid_ab) so mid_ab is a
                // legitimate vertex elsewhere — but NOT in the first
                // polygon's loop.
                vertices: vec![a, mid_ab, d],
                holes: vec![],
                plane_normal: [0.0, 0.0, -1.0],
                color: 0,
            },
            Polygon {
                vertices: vec![mid_ab, b, d],
                holes: vec![],
                plane_normal: [0.0, 0.0, -1.0],
                color: 0,
            },
        ];
        let v = validate_no_t_junctions(&polys);
        assert!(
            v.iter()
                .any(|g| matches!(g, GeometryViolation::TJunction { .. })),
            "expected at least one TJunction on edge (a,b), got {v:#?}"
        );
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
