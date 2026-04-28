//! Pass 4: sliver-edge removal.
//!
//! After T-junction repair, off-axis CSG can still leave polygons with
//! near-coincident vertex pairs that bound a short ("sliver") edge.
//! These vertices are too far apart for the welding pass (PR 301:
//! `WELD_TOLERANCE_FIXED_UNITS = 4` Chebyshev) but too close to be
//! distinct features in the output mesh — they're symptoms of BSP
//! producing slightly different snapped intersection points for what
//! should be the same true point under cascaded cuts.
//!
//! Algorithm: find every (a, b) edge whose Euclidean length squared is
//! below [`SLIVER_THRESHOLD_FIXED_SQUARED`]. Merge `max(a, b)` into
//! `min(a, b)` everywhere in the mesh by rewriting vertex ids, then
//! drop any consecutive duplicates that result. Iterate to fixed point
//! (since one merge can create a new sliver edge by shortening an
//! adjacent edge).
//!
//! ### Why edge-triggered, not coordinate-triggered
//!
//! Bumping `WELD_TOLERANCE_FIXED_UNITS` to ~60 would also merge these
//! pairs but would simultaneously merge any two vertices that happen
//! to be close in coords, regardless of whether they share an edge.
//! Sliver removal only fires on vertices that the BSP+cleanup output
//! has *already* placed adjacent in some polygon — a much more
//! constrained trigger that doesn't risk colliding distinct features.
//!
//! ### Threshold
//!
//! `4096` fixed units squared = `64` units Euclidean ≈ `1e-3` world
//! units, matching the geometric validator's `SliverEdge` threshold so
//! every edge the validator would flag also fires this pass. Well
//! above the [`super::weld::WELD_TOLERANCE_FIXED_UNITS`] = 4 Chebyshev
//! bound (4 × √3 ≈ 6.9 units Euclidean), and well below the
//! next-nearest distinct-feature spacing in practical CSG output
//! (sphere/cylinder facet vertex separation ≥ 7000 fixed units for a
//! 12-segment radius-0.4 sphere — the smallest reasonable input — see
//! [`super::weld`] for the same argument).

use super::mesh::{IndexedMesh, VertexId};
use crate::point::Point3;
use std::collections::HashMap;

/// Squared Euclidean length below which an edge is a sliver.
/// `64 * 64 = 4096` fixed units squared (= 1e-3 world units edge
/// length) matches the validator's flag threshold so cleanup always
/// fires when the validator would.
const SLIVER_THRESHOLD_FIXED_SQUARED: i128 = 64 * 64;

/// Defensive iteration cap. Each iteration strictly removes at least
/// one vertex from at least one polygon, so termination is guaranteed
/// in O(V) iterations; the cap protects against logic errors that
/// would otherwise hang mesh authoring.
const MAX_SLIVER_ITERATIONS: usize = 64;

impl IndexedMesh {
    pub(super) fn remove_slivers(self) -> Self {
        let IndexedMesh {
            vertices,
            mut polygons,
        } = self;

        for _ in 0..MAX_SLIVER_ITERATIONS {
            let mut merges: HashMap<VertexId, VertexId> = HashMap::new();
            for poly in &polygons {
                let n = poly.vertices.len();
                for i in 0..n {
                    let a = poly.vertices[i];
                    let b = poly.vertices[(i + 1) % n];
                    if a == b {
                        continue;
                    }
                    if edge_len_sq(&vertices, a, b) >= SLIVER_THRESHOLD_FIXED_SQUARED {
                        continue;
                    }
                    // Resolve to roots in case either endpoint is already
                    // queued for merge into something else this iteration.
                    let ra = follow(&merges, a);
                    let rb = follow(&merges, b);
                    if ra == rb {
                        continue;
                    }
                    let (keep, drop) = if ra < rb { (ra, rb) } else { (rb, ra) };
                    merges.insert(drop, keep);
                }
            }

            if merges.is_empty() {
                break;
            }

            // Rewrite all polygon vertex ids through the merge map,
            // then collapse consecutive duplicates within each loop
            // (and drop polygons that fell below 3 distinct vertices).
            polygons.retain_mut(|poly| {
                for v in &mut poly.vertices {
                    *v = follow(&merges, *v);
                }
                dedup_consecutive_and_self_close(&mut poly.vertices);
                poly.vertices.len() >= 3
            });
        }

        IndexedMesh { vertices, polygons }
    }
}

/// Walk the merge chain to the root id (the smallest id in the
/// equivalence class). Path compression isn't worth it at this scale —
/// chains are at most a handful of hops since merges are
/// strictly-smaller-id-wins.
fn follow(merges: &HashMap<VertexId, VertexId>, mut id: VertexId) -> VertexId {
    while let Some(&next) = merges.get(&id) {
        if next == id {
            break;
        }
        id = next;
    }
    id
}

fn edge_len_sq(vertices: &[Point3], a: VertexId, b: VertexId) -> i128 {
    let pa = vertices[a];
    let pb = vertices[b];
    let dx = (pb.x - pa.x) as i128;
    let dy = (pb.y - pa.y) as i128;
    let dz = (pb.z - pa.z) as i128;
    dx * dx + dy * dy + dz * dz
}

fn dedup_consecutive_and_self_close(loop_: &mut Vec<VertexId>) {
    if loop_.is_empty() {
        return;
    }
    // Drop adjacent duplicates (a, a, b) → (a, b).
    let mut write = 0;
    for read in 1..loop_.len() {
        if loop_[read] != loop_[write] {
            write += 1;
            loop_[write] = loop_[read];
        }
    }
    loop_.truncate(write + 1);
    // Drop loop-closing duplicate (a, ..., a) → (a, ...).
    while loop_.len() >= 2 && loop_.first() == loop_.last() {
        loop_.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cleanup::mesh::IndexedPolygon;
    use crate::plane::Plane3;

    fn xy_plane() -> Plane3 {
        Plane3 {
            n_x: 0,
            n_y: 0,
            n_z: 1,
            d: 0,
        }
    }

    fn poly(vertices: Vec<VertexId>) -> IndexedPolygon {
        IndexedPolygon {
            vertices,
            plane: xy_plane(),
            color: 0,
        }
    }

    fn p(x: i32, y: i32, z: i32) -> Point3 {
        Point3 { x, y, z }
    }

    #[test]
    fn sliver_pair_is_merged_and_polygon_collapses_correctly() {
        // Triangle (A, B, C) plus a near-duplicate B' sitting 30 fixed
        // units from B. Edge (B, B') is a sliver — len² = 900 < 4096.
        // Polygon walks A → B → B' → C. After merge: A → B → C.
        let vertices = vec![
            p(0, 0, 0),       // 0: A
            p(10000, 0, 0),   // 1: B
            p(10030, 0, 0),   // 2: B' (30 units from B)
            p(5000, 8000, 0), // 3: C
        ];
        let polygons = vec![poly(vec![0, 1, 2, 3])];
        let mesh = IndexedMesh { vertices, polygons };
        let cleaned = mesh.remove_slivers();
        assert_eq!(cleaned.polygons.len(), 1);
        // 2 (B') merged into 1 (B); polygon's loop collapses the
        // consecutive duplicate.
        assert_eq!(cleaned.polygons[0].vertices, vec![0, 1, 3]);
    }

    #[test]
    fn polygon_collapsed_below_three_vertices_is_dropped() {
        // Triangle whose two edges are both slivers (len ≤ ~28 fixed
        // units → len² ≤ 784 < 4096) — collapses to a single vertex.
        // The polygon must be removed entirely so downstream passes
        // don't see a degenerate primitive.
        let vertices = vec![p(0, 0, 0), p(20, 0, 0), p(0, 20, 0)];
        let polygons = vec![poly(vec![0, 1, 2])];
        let mesh = IndexedMesh { vertices, polygons };
        let cleaned = mesh.remove_slivers();
        assert!(cleaned.polygons.is_empty());
    }

    #[test]
    fn non_sliver_edges_are_left_alone() {
        // Edge length² = 10000² = 1e8 >> 4096. Triangle should pass
        // through unchanged.
        let vertices = vec![p(0, 0, 0), p(10000, 0, 0), p(0, 10000, 0)];
        let polygons = vec![poly(vec![0, 1, 2])];
        let mesh = IndexedMesh {
            vertices: vertices.clone(),
            polygons: polygons.clone(),
        };
        let cleaned = mesh.remove_slivers();
        assert_eq!(cleaned.polygons.len(), 1);
        assert_eq!(cleaned.polygons[0].vertices, vec![0, 1, 2]);
    }

    #[test]
    fn shared_sliver_merges_consistently_across_polygons() {
        // Two quads sharing the sliver edge (B, B') in opposite walking
        // directions. After merge both quads collapse to triangles
        // with B in the same role; B' is eliminated everywhere.
        let vertices = vec![
            p(0, 0, 0),         // 0: A
            p(10000, 0, 0),     // 1: B
            p(10030, 0, 0),     // 2: B' (30 units → len²=900 < 4096)
            p(20000, 8000, 0),  // 3: C
            p(0, 8000, 0),      // 4: D
            p(0, -8000, 0),     // 5: E
            p(20000, -8000, 0), // 6: F
        ];
        let polygons = vec![
            poly(vec![0, 1, 2, 3, 4]), // top quad walks B → B'
            poly(vec![5, 6, 2, 1]),    // bottom quad walks B' → B
        ];
        let mesh = IndexedMesh { vertices, polygons };
        let cleaned = mesh.remove_slivers();
        assert_eq!(cleaned.polygons.len(), 2);
        for poly in &cleaned.polygons {
            assert!(!poly.vertices.contains(&2));
            assert!(poly.vertices.contains(&1));
        }
    }

    #[test]
    fn cascading_slivers_converge_within_iteration_cap() {
        // Three vertices in a row, each 30 units apart. Sliver pass
        // merges 1↔2 first (len²=900 < 4096); polygon becomes
        // (0, 1, 3, 4). New edge 1→3 is now 60 units long → len²=3600
        // < 4096 → still a sliver, merged in iter 2. Final loop
        // should reduce to (0, 1, 4).
        let vertices = vec![
            p(0, 0, 0),       // 0: A
            p(10000, 0, 0),   // 1: B
            p(10030, 0, 0),   // 2: B'  (30 units from B)
            p(10060, 0, 0),   // 3: B'' (60 units from B)
            p(5000, 8000, 0), // 4: C
        ];
        let polygons = vec![poly(vec![0, 1, 2, 3, 4])];
        let mesh = IndexedMesh { vertices, polygons };
        let cleaned = mesh.remove_slivers();
        assert_eq!(cleaned.polygons.len(), 1);
        assert_eq!(cleaned.polygons[0].vertices, vec![0, 1, 4]);
    }

    #[test]
    fn dedup_consecutive_handles_loop_closure() {
        // Pin: (a, b, c, a) is a closed-loop encoding sometimes
        // produced upstream. Must reduce to (a, b, c) so the polygon
        // walk doesn't double-count the closing edge.
        let mut loop_ = vec![5, 7, 9, 5];
        dedup_consecutive_and_self_close(&mut loop_);
        assert_eq!(loop_, vec![5, 7, 9]);
    }

    #[test]
    fn empty_mesh_passes_through_unchanged() {
        let mesh = IndexedMesh {
            vertices: vec![],
            polygons: vec![],
        };
        let cleaned = mesh.remove_slivers();
        assert!(cleaned.vertices.is_empty());
        assert!(cleaned.polygons.is_empty());
    }
}
