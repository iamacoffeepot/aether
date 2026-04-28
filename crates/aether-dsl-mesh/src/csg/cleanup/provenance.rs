//! Boundary-edge provenance tracing — diagnostic.
//!
//! When `validate_manifold` reports `BoundaryEdge` violations on the
//! cleanup output, this module identifies which cleanup stage left the
//! reverse edge unaccounted for. For each unmatched directed edge in
//! the final mesh, [`analyze_unmatched_boundaries`] reports:
//!
//! - the owning polygon's index, plane, and color (in the post-cleanup
//!   mesh);
//! - the reverse edge's count in each of the four stage snapshots
//!   (post-weld, post-tjunctions, post-merge, post-slivers).
//!
//! Stage-snapshot ids are stable through weld → t-junctions → merge
//! (none of those modify the vertex pool). Sliver removal may merge
//! pool ids, so a reverse-count of `0` at post-merge that becomes
//! non-zero at post-slivers is the signature of a sliver merge that
//! "fixed" the imbalance by collapsing one endpoint into another.
//!
//! The diagnostic re-runs cleanup with snapshots at each stage —
//! production callers go through [`super::run_to_indexed`] which
//! doesn't pay the snapshot cost. Designed to debug issue 370
//! (inter-bucket rim mismatch on curved × sphere CSG); not stable API.

use super::mesh::{IndexedMesh, IndexedPolygon, VertexId};
use crate::csg::plane::Plane3;
use crate::csg::polygon::Polygon;
use aether_math::Vec3;
use std::collections::HashMap;

/// Per-edge provenance record for one unmatched directed edge in the
/// post-cleanup mesh.
#[derive(Debug, Clone)]
pub struct BoundaryEdgeProvenance {
    /// Unmatched directed edge `(a, b)` in post-sliver `VertexId` space.
    pub edge: (VertexId, VertexId),
    /// f32 world-space coordinates of `a` and `b`. Diagnostic only.
    pub coords: (Vec3, Vec3),
    /// Index of the owning polygon in the final cleanup mesh.
    pub polygon_idx: usize,
    /// Plane and color of the owning polygon — together they identify
    /// which `(plane, color)` bucket the edge belongs to and whether
    /// the matching reverse should have lived in the same bucket or a
    /// neighbouring one.
    pub plane: Plane3,
    pub color: u32,
    /// Count of the reverse edge `(b, a)` in the directed-edge multiset
    /// at the end of each cleanup stage. Ids are stable through
    /// weld → t-junctions → merge; sliver removal may merge ids, so
    /// a `0` at post-merge that becomes non-zero at post-slivers
    /// signals that sliver collapsed an endpoint.
    pub reverse_post_weld: u32,
    pub reverse_post_tjunctions: u32,
    pub reverse_post_merge: u32,
    pub reverse_post_slivers: u32,
}

/// Run the cleanup pipeline with a directed-edge snapshot at each
/// stage boundary, then for every unmatched directed edge in the
/// post-cleanup mesh emit a [`BoundaryEdgeProvenance`] record.
///
/// Returns an empty `Vec` for a watertight mesh.
pub fn analyze_unmatched_boundaries(input: Vec<Polygon>) -> Vec<BoundaryEdgeProvenance> {
    let welded = IndexedMesh::weld(input);
    let post_weld_directed = build_directed(&welded);

    let repaired = welded.repair_tjunctions();
    let post_tjunctions_directed = build_directed(&repaired);

    let merged = repaired.merge_coplanar();
    let post_merge_directed = build_directed(&merged);

    let cleaned = merged.remove_slivers();
    let post_slivers_directed = build_directed(&cleaned);

    let unmatched = unmatched_edges(&post_slivers_directed);

    let mut report: Vec<BoundaryEdgeProvenance> = Vec::with_capacity(unmatched.len());
    for (a, b) in unmatched {
        let polygon_idx = match find_polygon_with_edge(&cleaned.polygons, a, b) {
            Some(idx) => idx,
            None => continue,
        };
        let polygon = &cleaned.polygons[polygon_idx];
        let pa = cleaned.vertices[a];
        let pb = cleaned.vertices[b];
        let reverse = (b, a);

        report.push(BoundaryEdgeProvenance {
            edge: (a, b),
            coords: (pa.to_f32(), pb.to_f32()),
            polygon_idx,
            plane: polygon.plane,
            color: polygon.color,
            reverse_post_weld: post_weld_directed.get(&reverse).copied().unwrap_or(0),
            reverse_post_tjunctions: post_tjunctions_directed.get(&reverse).copied().unwrap_or(0),
            reverse_post_merge: post_merge_directed.get(&reverse).copied().unwrap_or(0),
            reverse_post_slivers: post_slivers_directed.get(&reverse).copied().unwrap_or(0),
        });
    }
    report
}

fn build_directed(mesh: &IndexedMesh) -> HashMap<(VertexId, VertexId), u32> {
    let mut directed: HashMap<(VertexId, VertexId), u32> = HashMap::new();
    for poly in &mesh.polygons {
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
}

/// Surviving directed edges after twin cancellation. Same shape as
/// `merge::boundary_edges_after_twin_cancellation` but global (across
/// the whole mesh, not per bucket): the count imbalance survives in
/// the dominant direction, with multiplicity preserved.
fn unmatched_edges(directed: &HashMap<(VertexId, VertexId), u32>) -> Vec<(VertexId, VertexId)> {
    let mut out: Vec<(VertexId, VertexId)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut keys: Vec<(VertexId, VertexId)> = directed.keys().copied().collect();
    keys.sort();
    for (a, b) in keys {
        let canonical = if a < b { (a, b) } else { (b, a) };
        if !seen.insert(canonical) {
            continue;
        }
        let forward = directed.get(&(a, b)).copied().unwrap_or(0);
        let reverse = directed.get(&(b, a)).copied().unwrap_or(0);
        match forward.cmp(&reverse) {
            std::cmp::Ordering::Greater => {
                for _ in 0..(forward - reverse) {
                    out.push((a, b));
                }
            }
            std::cmp::Ordering::Less => {
                for _ in 0..(reverse - forward) {
                    out.push((b, a));
                }
            }
            std::cmp::Ordering::Equal => {}
        }
    }
    out
}

fn find_polygon_with_edge(polygons: &[IndexedPolygon], a: VertexId, b: VertexId) -> Option<usize> {
    for (idx, poly) in polygons.iter().enumerate() {
        let n = poly.vertices.len();
        for i in 0..n {
            if poly.vertices[i] == a && poly.vertices[(i + 1) % n] == b {
                return Some(idx);
            }
        }
    }
    None
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

    #[test]
    fn watertight_mesh_has_no_provenance_records() {
        // Two triangles sharing an edge with opposite winding form a
        // closed (degenerate) double-sided lamina — every directed edge
        // is twin-paired, so analyze finds no boundaries.
        let t1 = Polygon::from_triangle(pt(0.0, 0.0, 0.0), pt(1.0, 0.0, 0.0), pt(0.0, 1.0, 0.0), 0)
            .unwrap();
        let t2 = Polygon::from_triangle(pt(0.0, 0.0, 0.0), pt(0.0, 1.0, 0.0), pt(1.0, 0.0, 0.0), 0)
            .unwrap();
        let report = analyze_unmatched_boundaries(vec![t1, t2]);
        assert!(report.is_empty(), "double-sided lamina is closed");
    }

    #[test]
    fn single_triangle_emits_three_unmatched_edges() {
        // A bare triangle has three boundary edges, no opposing
        // neighbours — every edge is unmatched.
        let t = Polygon::from_triangle(pt(0.0, 0.0, 0.0), pt(1.0, 0.0, 0.0), pt(0.0, 1.0, 0.0), 7)
            .unwrap();
        let report = analyze_unmatched_boundaries(vec![t]);
        assert_eq!(report.len(), 3);
        for r in &report {
            assert_eq!(r.color, 7);
            assert_eq!(r.polygon_idx, 0);
            // Reverse edge never existed at any stage — single triangle.
            assert_eq!(r.reverse_post_weld, 0);
            assert_eq!(r.reverse_post_tjunctions, 0);
            assert_eq!(r.reverse_post_merge, 0);
            assert_eq!(r.reverse_post_slivers, 0);
        }
    }
}
