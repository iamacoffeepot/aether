//! Pass 2: coplanar polygon merging — emits boundary loops as n-gons.
//!
//! Groups polygons by `(Plane3, color)` and runs a single directed-edge
//! cancellation across the whole bucket. Twin pairs (a,b) + (b,a) drop
//! out as interior edges; survivors form the boundary, walked into
//! closed loops by [`extract_loops`]. One [`IndexedPolygon`] is emitted
//! per loop (per ADR-0057). No triangulation here — CDT runs in
//! [`super::tessellate`] (pass 4) on the post-T-junction loops so
//! T-junction repair operates on n-gon edges.
//!
//! ## Why bucket-wide and not per-component
//!
//! BSP CSG output produces X-junctions where multiple polygons meet at
//! a single vertex without sharing a full edge — typically a sliver
//! triangle's apex coincident with a partition-cut vertex on a longer
//! edge. Per-component union-find by shared edge leaves these as
//! separate components, so an annular face (cube with a cylinder bore)
//! comes out as a fan of 11+ disjoint outer-only loops with no inner
//! hole loop — `group_loops` in `polygon.rs` then has nothing to attach
//! as a hole. Bucket-wide cancellation collapses the slivers and
//! surfaces the true outer + hole topology.
//!
//! ## Why the bucket key includes color
//!
//! Without color, `(composition red blue)` would steamroll color across
//! the boundary where two coplanar surfaces of different colors meet —
//! cancellation across the seam would erase the seam edges (twins from
//! each side) and produce one merged loop with whichever color won the
//! tiebreak. Bucketing by `(plane, color)` keeps color seams visible.
//!
//! ## Why `extract_loops` needs vertex coordinates
//!
//! At an X-junction with two boundary loops sharing a vertex, the
//! walker has multiple unvisited outgoing edges and must pick the one
//! that stays on the same loop. For BSP-generated geometry, the
//! correct continuation is the one most-collinear with the incoming
//! edge: rim-passes-through-J along a straight cube edge, hole-passes-
//! through-J along an almost-straight cylinder facet boundary, while
//! the cross pairs (rim-to-hole) make ~90° turns. The "go straight"
//! rule reliably picks the same-loop continuation when one pair is
//! collinear (BSP's case); for synthetic graphs with two non-collinear
//! loops touching at a corner the rule has no preferred pairing, but
//! that topology doesn't arise from BSP CSG output.
//!
//! ## Drop-axis 2D and integer comparison
//!
//! Projecting in-plane vertices to 2D for the angular pick uses the
//! drop-the-dominant-axis trick: pick the world axis with the largest
//! absolute normal component and use the other two as 2D coordinates.
//! For axis-aligned planes (the common BSP case) this is exact — no
//! shear, no precision loss. For tilted planes there's a linear shear,
//! which preserves collinearity (so a perfectly-collinear pair stays
//! perfectly collinear in drop-axis 2D) and preserves non-collinearity
//! (so cross pairs stay distinguishable from the collinear pair). The
//! cleanup pipeline operates in fixed-point integers to keep topology
//! deterministic; the angular comparison is integer cross-multiplied
//! ratios of (sin, cos) — see [`cmp_turn`].
//!
//! Plane-equality limitation: the grouping key is the exact `Plane3`
//! tuple `(n_x, n_y, n_z, d)`. Polygons coplanar in the Euclidean sense
//! but with proportional `Plane3` fields are not currently grouped.
//! BSP fragments inherit their parent triangle's plane field-for-field
//! so all fragments of one source share a key.
//!
//! Determinism: HashMap iteration order doesn't leak — bucket keys are
//! sorted before processing, and loop extraction walks edges in
//! deterministic order (sorted starts, sorted outgoing lists, VertexId
//! tiebreak in the angular pick).

use super::mesh::{IndexedMesh, IndexedPolygon, VertexId};
use crate::csg::plane::Plane3;
use crate::csg::point::Point3;
use std::collections::HashMap;

type PlaneKey = (i64, i64, i64, i128);
type BucketKey = (PlaneKey, u32);

fn plane_key(p: &Plane3) -> PlaneKey {
    (p.n_x, p.n_y, p.n_z, p.d)
}

fn bucket_key(p: &IndexedPolygon) -> BucketKey {
    (plane_key(&p.plane), p.color)
}

impl IndexedMesh {
    pub(super) fn merge_coplanar(self) -> Self {
        let IndexedMesh { vertices, polygons } = self;

        let buckets = group_by_bucket(&polygons);
        let mut sorted_keys: Vec<&BucketKey> = buckets.keys().collect();
        sorted_keys.sort();

        // Compute the global directed-edge multiset once. `process_bucket`
        // and `collapse_unbacked_boundary_runs` both need to ask "is this
        // edge backed by another bucket?" — i.e. is `(b, a)` present in
        // any non-coplanar polygon? A single global count plus the
        // bucket's own count answers that without rebuilding per bucket.
        let global_directed = build_global_directed(&polygons);

        let mut merged: Vec<IndexedPolygon> = Vec::with_capacity(polygons.len());
        for key in sorted_keys {
            let bucket = &buckets[key];
            process_bucket(&vertices, &polygons, bucket, &global_directed, &mut merged);
        }

        IndexedMesh {
            vertices,
            polygons: merged,
        }
    }
}

fn build_global_directed(polygons: &[IndexedPolygon]) -> HashMap<(VertexId, VertexId), u32> {
    let mut directed: HashMap<(VertexId, VertexId), u32> = HashMap::new();
    for poly in polygons {
        let m = poly.vertices.len();
        for i in 0..m {
            let a = poly.vertices[i];
            let b = poly.vertices[(i + 1) % m];
            *directed.entry((a, b)).or_insert(0) += 1;
        }
    }
    directed
}

fn group_by_bucket(polygons: &[IndexedPolygon]) -> HashMap<BucketKey, Vec<usize>> {
    let mut groups: HashMap<BucketKey, Vec<usize>> = HashMap::new();
    for (i, poly) in polygons.iter().enumerate() {
        groups.entry(bucket_key(poly)).or_default().push(i);
    }
    groups
}

/// Cancel twin directed edges across `bucket`, walk the surviving
/// boundary into closed loops, and emit one polygon per loop. The
/// emitted polygons share the bucket's plane and color.
fn process_bucket(
    vertices: &[Point3],
    polygons: &[IndexedPolygon],
    bucket: &[usize],
    global_directed: &HashMap<(VertexId, VertexId), u32>,
    out: &mut Vec<IndexedPolygon>,
) {
    if bucket.len() == 1 {
        out.push(polygons[bucket[0]].clone());
        return;
    }

    let mut directed: HashMap<(VertexId, VertexId), u32> = HashMap::new();
    for &pid in bucket {
        let poly = &polygons[pid];
        let m = poly.vertices.len();
        for i in 0..m {
            let a = poly.vertices[i];
            let b = poly.vertices[(i + 1) % m];
            *directed.entry((a, b)).or_insert(0) += 1;
        }
    }
    let boundary = boundary_edges_after_twin_cancellation(&directed);

    let plane = polygons[bucket[0]].plane;
    let loops = match extract_loops(&boundary, vertices, &plane) {
        Some(l) => l,
        // Pathological boundary topology — pass through originals.
        None => {
            for &pid in bucket {
                out.push(polygons[pid].clone());
            }
            return;
        }
    };

    let color = polygons[bucket[0]].color;
    for loop_verts in loops {
        let loop_verts = collapse_unbacked_boundary_runs(&loop_verts, global_directed, &directed);
        for normalized in normalize_loop(&loop_verts) {
            out.push(IndexedPolygon {
                vertices: normalized,
                plane,
                color,
            });
        }
    }
}

/// Survivors of bucket-wide twin cancellation. For each canonical pair
/// `(a, b)`, the multiset count of `(a, b)` and `(b, a)` differs by some
/// imbalance — that imbalance is what survives, in the dominant direction.
///
/// Multiplicity matters: BSP CSG can emit two copies of one directed
/// edge with one copy of its reverse (e.g. three coplanar fragments
/// meeting along a partition cut, two on one side and one on the other).
/// The naive boolean filter treats "both directions present" as
/// cancelled, which over-cancels by one and tears the boundary open.
/// Issue #350.
fn boundary_edges_after_twin_cancellation(
    directed: &HashMap<(VertexId, VertexId), u32>,
) -> Vec<(VertexId, VertexId)> {
    let mut edges = Vec::new();
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
                    edges.push((a, b));
                }
            }
            std::cmp::Ordering::Less => {
                for _ in 0..(reverse - forward) {
                    edges.push((b, a));
                }
            }
            std::cmp::Ordering::Equal => {}
        }
    }

    edges
}

/// Remove "spurious" interior vertices from a boundary loop that have no
/// external partner — i.e. vertices that BSP CSG introduced at a
/// 3-plane intersection (two cutter-mesh planes × this bucket's plane)
/// where no actual cutter-mesh vertex exists. Such vertices appear when
/// the cutter is a faceted approximation of a smooth surface (e.g. a
/// sphere or cone): the BSP partitioner-plane cuts extend past the
/// actual triangle's bounded extent, and where two such cuts meet on
/// this plane they produce a vertex that has no twin on the cutter
/// surface.
///
/// We recognize them by: an edge `(a, b)` whose reverse `(b, a)` exists
/// in the *global* directed-edge multiset but **not** within this
/// bucket. That reverse is the cross-bucket adjacency the actual
/// boundary edge needs to be 2-manifold. A run of consecutive loop
/// edges whose reverses are *not* externally backed is debris — they
/// connect the spurious vertex back to the real boundary. If a chord
/// from the run's start to its end *is* externally backed, we replace
/// the run with that chord.
///
/// ## Algorithm
///
/// Given a closed loop of `n` vertices and the predicate
/// `has_external_reverse(a, b)` ≡ `(b, a)` exists outside the bucket:
///
/// 1. **Rotate phase.** Find any collapsible chord that *wraps* the
///    loop's start/end (i.e. `j = (i + step) % n <= i`). If found,
///    rotate the loop so the chord no longer wraps. This avoids the
///    walk phase splitting a wrap-around run across the start.
///
/// 2. **Walk phase.** Walk the loop forward. At each vertex `i`, scan
///    `step` from largest to smallest and pick the longest collapsible
///    chord. Emit `verts[i]`, jump to `verts[(i + step) % n]`, repeat.
///    No collapsible chord at `i` ⇒ advance one vertex.
///
/// 3. **Degenerate fallback.** If the result has fewer than 3 vertices
///    we'd produce an invalid polygon — return the original loop
///    untouched. The original is still a valid loop topologically; it
///    just won't be 2-manifold along the spurious vertices.
///
/// `can_collapse(i, step)` requires the chord `verts[i]` →
/// `verts[(i + step) % n]` to be externally backed AND every step-1
/// intermediate edge to be *not* externally backed (otherwise we'd be
/// collapsing through a real boundary edge).
fn collapse_unbacked_boundary_runs(
    loop_verts: &[VertexId],
    global_directed: &HashMap<(VertexId, VertexId), u32>,
    bucket_directed: &HashMap<(VertexId, VertexId), u32>,
) -> Vec<VertexId> {
    if loop_verts.len() < 4 {
        return loop_verts.to_vec();
    }

    let has_external_reverse = |a: VertexId, b: VertexId| -> bool {
        let global = global_directed.get(&(b, a)).copied().unwrap_or(0);
        let bucket = bucket_directed.get(&(b, a)).copied().unwrap_or(0);
        global > bucket
    };

    let can_collapse = |verts: &[VertexId], i: usize, step: usize| -> bool {
        let n = verts.len();
        let a = verts[i];
        let b = verts[(i + step) % n];
        if !has_external_reverse(a, b) {
            return false;
        }
        for k in 0..step {
            let p = verts[(i + k) % n];
            let q = verts[(i + k + 1) % n];
            if has_external_reverse(p, q) {
                return false;
            }
        }
        true
    };

    let mut verts = loop_verts.to_vec();

    // Rotate phase: align any wrap-around collapse chord to the start
    // so the walk phase doesn't split a run across index 0.
    let n = verts.len();
    'rotate: for i in 0..n {
        for step in (2..n).rev() {
            let j = (i + step) % n;
            if j <= i && can_collapse(&verts, i, step) {
                verts.rotate_left(j);
                break 'rotate;
            }
        }
    }

    // Walk phase: emit vertices, greedily collapsing the longest run
    // available at each position.
    let n = verts.len();
    let mut out = Vec::with_capacity(n);
    let mut i = 0;
    while i < n {
        out.push(verts[i]);

        let mut next_i = None;
        for step in (2..n).rev() {
            if can_collapse(&verts, i, step) {
                next_i = Some((i + step) % n);
                break;
            }
        }

        match next_i {
            Some(j) if j > i => i = j,
            // Wrap-around collapse after rotation handled the head — stop.
            Some(_) => break,
            None => i += 1,
        }
    }

    out.dedup();
    if out.len() >= 2 && out.first() == out.last() {
        out.pop();
    }
    if out.len() < 3 { verts } else { out }
}

/// Normalize a possibly non-simple loop into a list of simple loops.
///
/// `extract_loops` walks an X-junction by smallest-turn continuation
/// and tracks visited *edges*, not vertices. With pinch-point topology
/// (figure-8) the walker can re-enter a vertex via a different edge,
/// emitting a single loop with a repeated vertex id. The post-merge
/// invariant (issue 337) catches these as `NonSimpleLoopViolation`.
///
/// Strategy: strip adjacent duplicates (including the wrap), then while
/// a non-adjacent repeat exists at indices `i < j` split at the pinch:
///
/// - outer = `verts[..=i] ++ verts[j+1..]`  (closes via the original
///   `(verts[n-1], verts[0])` edge)
/// - inner = `verts[i..j]`  (closes via the original `(verts[j-1], r)`
///   edge — same topology as the figure-8's inner cycle)
///
/// All edges of the original loop are preserved; only the grouping
/// changes. Branches with fewer than 3 vertices are dropped — they
/// represent antennae (e.g. `[a, b, a]`) whose contributing edges
/// twin-cancel and shouldn't have survived the bucket-wide pass.
///
/// Recurses to handle multiply-pinched loops. Pinch detection is the
/// HashMap-based first-repeat scan, which is O(n) per call; recursion
/// depth is bounded by the number of pinches (typically 1-2 in BSP
/// output).
fn normalize_loop(loop_verts: &[VertexId]) -> Vec<Vec<VertexId>> {
    let mut verts: Vec<VertexId> = Vec::with_capacity(loop_verts.len());
    for &v in loop_verts {
        if verts.last() == Some(&v) {
            continue;
        }
        verts.push(v);
    }
    while verts.len() > 1 && verts.first() == verts.last() {
        verts.pop();
    }
    if verts.len() < 3 {
        return Vec::new();
    }

    let mut first_seen: HashMap<VertexId, usize> = HashMap::with_capacity(verts.len());
    for (j, &v) in verts.iter().enumerate() {
        if let Some(&i) = first_seen.get(&v) {
            let mut outer: Vec<VertexId> = verts[..=i].to_vec();
            outer.extend_from_slice(&verts[j + 1..]);
            let inner: Vec<VertexId> = verts[i..j].to_vec();

            tracing::trace!(
                pinch_vertex = v,
                indices = ?(i, j),
                outer_len = outer.len(),
                inner_len = inner.len(),
                "normalize_loop split non-simple loop at pinch (issue 350)"
            );

            let mut result = normalize_loop(&outer);
            result.extend(normalize_loop(&inner));
            return result;
        }
        first_seen.insert(v, j);
    }

    vec![verts]
}

/// Walk directed boundary edges into closed loops. Returns `None` if
/// the boundary is not a disjoint union of closed loops (open chain,
/// dead-end branch, etc.).
///
/// At each vertex with multiple unvisited outgoing edges, the angular
/// continuation rule picks the candidate whose direction is closest to
/// the incoming direction (smallest absolute turn angle). VertexId
/// breaks turn-angle ties for determinism. The first edge of a loop
/// has no incoming direction; it's chosen by the sort order of
/// `boundary`. See module-level docs for why this is the right rule
/// for BSP-generated X-junctions.
fn extract_loops(
    boundary: &[(VertexId, VertexId)],
    vertices: &[Point3],
    plane: &Plane3,
) -> Option<Vec<Vec<VertexId>>> {
    let axes = drop_axis(plane);

    let mut outgoing: HashMap<VertexId, Vec<VertexId>> = HashMap::new();
    for &(a, b) in boundary {
        outgoing.entry(a).or_default().push(b);
    }
    for v in outgoing.values_mut() {
        v.sort();
    }

    let mut starts: Vec<(VertexId, VertexId)> = boundary.to_vec();
    starts.sort();

    let mut visited: std::collections::HashSet<(VertexId, VertexId)> =
        std::collections::HashSet::new();
    let mut loops = Vec::new();

    for &(start_a, start_b) in &starts {
        if visited.contains(&(start_a, start_b)) {
            continue;
        }
        visited.insert((start_a, start_b));
        let mut loop_verts = vec![start_a];
        let mut prev = start_a;
        let mut cur = start_b;
        loop {
            if cur == start_a {
                break;
            }
            loop_verts.push(cur);
            let next = pick_continuation(vertices, axes, &outgoing, &visited, prev, cur);
            match next {
                Some(n) => {
                    visited.insert((cur, n));
                    prev = cur;
                    cur = n;
                }
                None => return None,
            }
        }
        if loop_verts.len() < 3 {
            return None;
        }
        loops.push(loop_verts);
    }
    Some(loops)
}

/// Pick the next outgoing edge from `cur` that continues most directly
/// from the incoming direction `prev → cur`. With a single unvisited
/// candidate, returns it; with several, picks the smallest absolute
/// turn angle, VertexId tiebreak.
fn pick_continuation(
    vertices: &[Point3],
    axes: (usize, usize),
    outgoing: &HashMap<VertexId, Vec<VertexId>>,
    visited: &std::collections::HashSet<(VertexId, VertexId)>,
    prev: VertexId,
    cur: VertexId,
) -> Option<VertexId> {
    let nexts = outgoing.get(&cur)?;
    let unvisited: Vec<VertexId> = nexts
        .iter()
        .copied()
        .filter(|&n| !visited.contains(&(cur, n)))
        .collect();
    match unvisited.len() {
        0 => return None,
        1 => return Some(unvisited[0]),
        _ => {}
    }

    let prev_2d = project_2d(vertices[prev], axes);
    let cur_2d = project_2d(vertices[cur], axes);
    let in_dx = cur_2d.0 - prev_2d.0;
    let in_dy = cur_2d.1 - prev_2d.1;

    let mut best = unvisited[0];
    for &cand in &unvisited[1..] {
        match cmp_turn(in_dx, in_dy, cur_2d, vertices, axes, cand, best) {
            std::cmp::Ordering::Less => best = cand,
            std::cmp::Ordering::Equal => {
                if cand < best {
                    best = cand;
                }
            }
            std::cmp::Ordering::Greater => {}
        }
    }
    Some(best)
}

/// Compare the absolute turn angle from `(in_dx, in_dy)` to candidate
/// `a`'s outgoing direction vs candidate `b`'s. Returns `Less` when a
/// has the smaller turn (and so wins). Pure integer arithmetic — see
/// the module's "Drop-axis 2D and integer comparison" note.
///
/// The comparison reduces to:
/// - sign(dot) classifies the turn quadrant (+ → turn < π/2,
///   0 → turn = π/2, - → turn > π/2). Larger sign is smaller turn.
/// - same-quadrant ties resolve via tan(turn) = sin/cos. Cross-multiply
///   to avoid division: `|cross_a| · |dot_b|` vs `|cross_b| · |dot_a|`.
///   For positive cos (turn < π/2) smaller |tan| wins; for negative
///   cos (turn > π/2 → closer-to-π means smaller |cross|, larger |dot|)
///   the inequality flips.
///
/// On i128 multiplication overflow falls back to `Equal` so the
/// VertexId tiebreak in `pick_continuation` resolves the choice — that
/// only triggers for input coords near the i32 fixed-point limits,
/// and erring deterministic at the edge is fine.
fn cmp_turn(
    in_dx: i64,
    in_dy: i64,
    cur: (i64, i64),
    vertices: &[Point3],
    axes: (usize, usize),
    a: VertexId,
    b: VertexId,
) -> std::cmp::Ordering {
    let a_2d = project_2d(vertices[a], axes);
    let b_2d = project_2d(vertices[b], axes);
    let a_dx = a_2d.0 - cur.0;
    let a_dy = a_2d.1 - cur.1;
    let b_dx = b_2d.0 - cur.0;
    let b_dy = b_2d.1 - cur.1;

    let dot_a: i128 = (in_dx as i128) * (a_dx as i128) + (in_dy as i128) * (a_dy as i128);
    let dot_b: i128 = (in_dx as i128) * (b_dx as i128) + (in_dy as i128) * (b_dy as i128);
    let cross_a: i128 = (in_dx as i128) * (a_dy as i128) - (in_dy as i128) * (a_dx as i128);
    let cross_b: i128 = (in_dx as i128) * (b_dy as i128) - (in_dy as i128) * (b_dx as i128);

    let sign_a = dot_a.signum();
    let sign_b = dot_b.signum();
    if sign_a != sign_b {
        return sign_b.cmp(&sign_a);
    }

    let abs_cross_a = cross_a.unsigned_abs();
    let abs_cross_b = cross_b.unsigned_abs();
    let abs_dot_a = dot_a.unsigned_abs();
    let abs_dot_b = dot_b.unsigned_abs();

    let lhs = abs_cross_a.checked_mul(abs_dot_b);
    let rhs = abs_cross_b.checked_mul(abs_dot_a);
    match (lhs, rhs) {
        (Some(lhs), Some(rhs)) => {
            if sign_a >= 0 {
                lhs.cmp(&rhs)
            } else {
                rhs.cmp(&lhs)
            }
        }
        _ => std::cmp::Ordering::Equal,
    }
}

/// Pick the two world axes to project onto: drop the axis with the
/// largest absolute normal component. Exact for axis-aligned planes,
/// shears tilted ones (collinearity preserved).
fn drop_axis(plane: &Plane3) -> (usize, usize) {
    let abs_n = (
        plane.n_x.unsigned_abs(),
        plane.n_y.unsigned_abs(),
        plane.n_z.unsigned_abs(),
    );
    if abs_n.0 >= abs_n.1 && abs_n.0 >= abs_n.2 {
        (1, 2)
    } else if abs_n.1 >= abs_n.2 {
        (0, 2)
    } else {
        (0, 1)
    }
}

fn project_2d(p: Point3, axes: (usize, usize)) -> (i64, i64) {
    let coords = [p.x as i64, p.y as i64, p.z as i64];
    (coords[axes.0], coords[axes.1])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::csg::fixed::f32_to_fixed;
    use crate::csg::point::Point3;
    use crate::csg::polygon::Polygon;

    fn pt(x: f32, y: f32, z: f32) -> Point3 {
        Point3 {
            x: f32_to_fixed(x).unwrap(),
            y: f32_to_fixed(y).unwrap(),
            z: f32_to_fixed(z).unwrap(),
        }
    }

    fn weld_then_merge(polys: Vec<Polygon>) -> Vec<Polygon> {
        IndexedMesh::weld(polys).merge_coplanar().into_polygons()
    }

    #[test]
    fn singleton_passes_through() {
        let tri =
            Polygon::from_triangle(pt(0.0, 0.0, 0.0), pt(1.0, 0.0, 0.0), pt(0.0, 1.0, 0.0), 5)
                .unwrap();
        let out = weld_then_merge(vec![tri]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].vertices.len(), 3);
        assert_eq!(out[0].color, 5);
    }

    #[test]
    fn two_triangles_forming_a_quad_merge_to_one_quad_polygon() {
        // Quad split into two triangles by the (0,0)-(1,1) diagonal.
        // The diagonal is a twin pair and cancels; the outer 4 edges
        // form the boundary.
        let t1 = Polygon::from_triangle(pt(0.0, 0.0, 0.0), pt(1.0, 0.0, 0.0), pt(1.0, 1.0, 0.0), 0)
            .unwrap();
        let t2 = Polygon::from_triangle(pt(0.0, 0.0, 0.0), pt(1.0, 1.0, 0.0), pt(0.0, 1.0, 0.0), 0)
            .unwrap();
        let out = weld_then_merge(vec![t1, t2]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].vertices.len(), 4);
        let covered: std::collections::BTreeSet<Point3> = out[0].vertices.iter().copied().collect();
        let expect: std::collections::BTreeSet<Point3> = [
            pt(0.0, 0.0, 0.0),
            pt(1.0, 0.0, 0.0),
            pt(1.0, 1.0, 0.0),
            pt(0.0, 1.0, 0.0),
        ]
        .into_iter()
        .collect();
        assert_eq!(covered, expect);
    }

    #[test]
    fn two_coplanar_triangles_with_opposite_normals_dont_merge() {
        // Same triangle wound the other way; opposite plane normal →
        // different bucket key.
        let t1 = Polygon::from_triangle(pt(0.0, 0.0, 0.0), pt(1.0, 0.0, 0.0), pt(1.0, 1.0, 0.0), 0)
            .unwrap();
        let t2 = Polygon::from_triangle(pt(0.0, 0.0, 0.0), pt(1.0, 1.0, 0.0), pt(1.0, 0.0, 0.0), 1)
            .unwrap();
        let out = weld_then_merge(vec![t1, t2]);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn triangles_in_different_planes_are_unaffected() {
        let t_xy =
            Polygon::from_triangle(pt(0.0, 0.0, 0.0), pt(1.0, 0.0, 0.0), pt(0.0, 1.0, 0.0), 0)
                .unwrap();
        let t_yz =
            Polygon::from_triangle(pt(0.0, 0.0, 0.0), pt(0.0, 1.0, 0.0), pt(0.0, 0.0, 1.0), 0)
                .unwrap();
        let out = weld_then_merge(vec![t_xy, t_yz]);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn shattered_quad_collapses_to_one_quad_polygon() {
        // 4 fan triangles meeting at the centre. The 4 internal radial
        // edges all cancel as twin pairs; the outer 4 edges form one
        // boundary loop.
        let c = pt(1.0, 1.0, 0.0);
        let nw = pt(0.0, 2.0, 0.0);
        let ne = pt(2.0, 2.0, 0.0);
        let se = pt(2.0, 0.0, 0.0);
        let sw = pt(0.0, 0.0, 0.0);
        let polys = vec![
            Polygon::from_triangle(c, nw, ne, 0).unwrap(),
            Polygon::from_triangle(c, ne, se, 0).unwrap(),
            Polygon::from_triangle(c, se, sw, 0).unwrap(),
            Polygon::from_triangle(c, sw, nw, 0).unwrap(),
        ];
        let out = weld_then_merge(polys);
        assert_eq!(out.len(), 1, "expected 1 merged polygon, got {}", out.len());
        assert_eq!(out[0].vertices.len(), 4);
    }

    #[test]
    fn l_shaped_non_convex_loop_triangulates() {
        // L-shape via a fan from the bottom-left corner. Each fan
        // triangle has its own plane key (different cross-product
        // magnitude), so each goes into its own bucket; the middle
        // two share a plane key and form one bucket of size 2.
        let bl = pt(0.0, 0.0, 0.0);
        let br = pt(2.0, 0.0, 0.0);
        let inner = pt(2.0, 1.0, 0.0);
        let mid = pt(1.0, 1.0, 0.0);
        let top = pt(1.0, 2.0, 0.0);
        let tl = pt(0.0, 2.0, 0.0);
        let polys = vec![
            Polygon::from_triangle(bl, br, inner, 0).unwrap(),
            Polygon::from_triangle(bl, inner, mid, 0).unwrap(),
            Polygon::from_triangle(bl, mid, top, 0).unwrap(),
            Polygon::from_triangle(bl, top, tl, 0).unwrap(),
        ];
        let out = weld_then_merge(polys);
        assert_eq!(out.len(), 3);
        let lens: std::collections::BTreeSet<usize> =
            out.iter().map(|p| p.vertices.len()).collect();
        assert_eq!(lens, [3, 4].into_iter().collect());
    }

    #[test]
    fn merging_is_deterministic_across_runs() {
        let t1 = Polygon::from_triangle(pt(0.0, 0.0, 0.0), pt(1.0, 0.0, 0.0), pt(1.0, 1.0, 0.0), 0)
            .unwrap();
        let t2 = Polygon::from_triangle(pt(0.0, 0.0, 0.0), pt(1.0, 1.0, 0.0), pt(0.0, 1.0, 0.0), 0)
            .unwrap();
        let r1 = weld_then_merge(vec![t1.clone(), t2.clone()]);
        let r2 = weld_then_merge(vec![t1, t2]);
        assert_eq!(r1.len(), r2.len());
        for (a, b) in r1.iter().zip(r2.iter()) {
            assert_eq!(a.vertices, b.vertices);
            assert_eq!(a.color, b.color);
        }
    }

    /// Annular triangulation: 2x2 outer with a 1x1 hole, 8 CCW
    /// triangles all on z=0 with the same plane key.
    fn annular_indexed_mesh() -> IndexedMesh {
        let plane = Plane3 {
            n_x: 0,
            n_y: 0,
            n_z: 1,
            d: 0,
        };
        let color = 7;
        let vertices = vec![
            pt(0.0, 0.0, 0.0), // 0: A bottom-left
            pt(2.0, 0.0, 0.0), // 1: B bottom-right
            pt(2.0, 2.0, 0.0), // 2: C top-right
            pt(0.0, 2.0, 0.0), // 3: D top-left
            pt(0.5, 0.5, 0.0), // 4: E hole bottom-left
            pt(1.5, 0.5, 0.0), // 5: F hole bottom-right
            pt(1.5, 1.5, 0.0), // 6: G hole top-right
            pt(0.5, 1.5, 0.0), // 7: H hole top-left
        ];
        let polygons = [
            [0, 1, 4],
            [1, 5, 4],
            [1, 2, 5],
            [2, 6, 5],
            [2, 3, 6],
            [3, 7, 6],
            [3, 0, 7],
            [0, 4, 7],
        ]
        .into_iter()
        .map(|verts| IndexedPolygon {
            vertices: verts.to_vec(),
            plane,
            color,
        })
        .collect();
        IndexedMesh { vertices, polygons }
    }

    fn shoelace_2d(vertices: &[Point3], indices: &[VertexId]) -> i128 {
        let mut sum: i128 = 0;
        let n = indices.len();
        for i in 0..n {
            let j = (i + 1) % n;
            let a = vertices[indices[i]];
            let b = vertices[indices[j]];
            sum += (a.x as i128) * (b.y as i128) - (b.x as i128) * (a.y as i128);
        }
        sum
    }

    #[test]
    fn square_with_square_hole_emits_outer_and_hole_loops() {
        let vertices = annular_indexed_mesh().vertices.clone();
        let merged = annular_indexed_mesh().merge_coplanar();
        assert_eq!(
            merged.polygons.len(),
            2,
            "expected 2 boundary loops (outer + hole), got {}",
            merged.polygons.len()
        );
        for poly in &merged.polygons {
            assert_eq!(poly.vertices.len(), 4);
        }
        let signed_areas: Vec<i128> = merged
            .polygons
            .iter()
            .map(|p| shoelace_2d(&vertices, &p.vertices))
            .collect();
        let positive = signed_areas.iter().filter(|&&a| a > 0).count();
        let negative = signed_areas.iter().filter(|&&a| a < 0).count();
        assert_eq!(positive, 1, "expected one CCW outer loop");
        assert_eq!(negative, 1, "expected one CW hole loop");
        let total: i128 = signed_areas.iter().sum();
        let unit = 1_i128 << 16;
        assert_eq!(
            total,
            3 * 2 * unit * unit,
            "annular area mismatch — outer + hole signed sum should equal the annular region"
        );
    }

    #[test]
    fn multi_loop_merging_is_deterministic() {
        let r1 = annular_indexed_mesh().merge_coplanar();
        let r2 = annular_indexed_mesh().merge_coplanar();
        assert_eq!(r1.polygons.len(), r2.polygons.len());
        for (a, b) in r1.polygons.iter().zip(r2.polygons.iter()) {
            assert_eq!(a.vertices, b.vertices);
            assert_eq!(a.color, b.color);
        }
    }

    /// Coplanar polygons with proportional `Plane3` fields land in
    /// different buckets per the documented limitation.
    #[test]
    fn proportional_planes_do_not_merge() {
        let plane_small = Plane3 {
            n_x: 0,
            n_y: 0,
            n_z: 1 << 16,
            d: 0,
        };
        let plane_large = Plane3 {
            n_x: 0,
            n_y: 0,
            n_z: 4 << 16,
            d: 0,
        };
        let p1 = IndexedPolygon {
            vertices: vec![0, 1, 2],
            plane: plane_small,
            color: 0,
        };
        let p2 = IndexedPolygon {
            vertices: vec![0, 2, 3],
            plane: plane_large,
            color: 0,
        };
        let mesh = IndexedMesh {
            vertices: vec![
                pt(0.0, 0.0, 0.0),
                pt(1.0, 0.0, 0.0),
                pt(1.0, 1.0, 0.0),
                pt(0.0, 1.0, 0.0),
            ],
            polygons: vec![p1, p2],
        };
        let merged = mesh.merge_coplanar();
        assert_eq!(
            merged.polygons.len(),
            2,
            "proportional Plane3 fields must NOT merge — documented limitation"
        );
    }

    /// Polygons that share a plane but have different colors stay
    /// separate. Color is part of the bucket key — without it,
    /// `(composition red blue)` would steamroll the seam where two
    /// coplanar surfaces of different colors meet.
    #[test]
    fn polygons_of_different_colors_do_not_merge() {
        let plane = Plane3 {
            n_x: 0,
            n_y: 0,
            n_z: 1 << 16,
            d: 0,
        };
        let mesh = IndexedMesh {
            vertices: vec![
                pt(0.0, 0.0, 0.0),
                pt(1.0, 0.0, 0.0),
                pt(1.0, 1.0, 0.0),
                pt(0.0, 1.0, 0.0),
            ],
            polygons: vec![
                IndexedPolygon {
                    vertices: vec![0, 1, 2],
                    plane,
                    color: 11,
                },
                IndexedPolygon {
                    vertices: vec![0, 2, 3],
                    plane,
                    color: 22,
                },
            ],
        };
        let merged = mesh.merge_coplanar();
        assert_eq!(
            merged.polygons.len(),
            2,
            "different colors must stay in separate buckets and not merge"
        );
        let colors: std::collections::BTreeSet<u32> =
            merged.polygons.iter().map(|p| p.color).collect();
        assert_eq!(colors, [11, 22].into_iter().collect());
    }

    #[test]
    fn two_disjoint_quads_on_same_plane_emit_separately() {
        // Two completely disjoint quads — same plane, same color, no
        // shared edges. Bucket-wide cancellation leaves all 8 boundary
        // edges in place; extract_loops walks them as two loops.
        let plane = Plane3 {
            n_x: 0,
            n_y: 0,
            n_z: 1 << 16,
            d: 0,
        };
        let mesh = IndexedMesh {
            vertices: vec![
                pt(0.0, 0.0, 0.0),
                pt(1.0, 0.0, 0.0),
                pt(1.0, 1.0, 0.0),
                pt(0.0, 1.0, 0.0),
                pt(3.0, 0.0, 0.0),
                pt(4.0, 0.0, 0.0),
                pt(4.0, 1.0, 0.0),
                pt(3.0, 1.0, 0.0),
            ],
            polygons: vec![
                IndexedPolygon {
                    vertices: vec![0, 1, 2],
                    plane,
                    color: 0,
                },
                IndexedPolygon {
                    vertices: vec![0, 2, 3],
                    plane,
                    color: 0,
                },
                IndexedPolygon {
                    vertices: vec![4, 5, 6],
                    plane,
                    color: 0,
                },
                IndexedPolygon {
                    vertices: vec![4, 6, 7],
                    plane,
                    color: 0,
                },
            ],
        };
        let merged = mesh.merge_coplanar();
        assert_eq!(
            merged.polygons.len(),
            2,
            "two disjoint regions in one bucket must emit as 2 separate polygons"
        );
        for p in &merged.polygons {
            assert_eq!(p.vertices.len(), 4);
        }
    }

    /// Cross-plane shared edges must keep matching VertexIds. Different
    /// planes land in different buckets, but the manifold validator
    /// walks edges across all polygons regardless of plane.
    #[test]
    fn cross_plane_shared_edge_keeps_matching_vertex_ids() {
        let xy = Plane3 {
            n_x: 0,
            n_y: 0,
            n_z: 1 << 16,
            d: 0,
        };
        let xz = Plane3 {
            n_x: 0,
            n_y: -(1 << 16),
            n_z: 0,
            d: 0,
        };
        let mesh = IndexedMesh {
            vertices: vec![
                pt(0.0, 0.0, 0.0),
                pt(1.0, 0.0, 0.0),
                pt(0.0, 1.0, 0.0),
                pt(0.0, 0.0, 1.0),
            ],
            polygons: vec![
                IndexedPolygon {
                    vertices: vec![0, 1, 2],
                    plane: xy,
                    color: 0,
                },
                IndexedPolygon {
                    vertices: vec![0, 3, 1],
                    plane: xz,
                    color: 0,
                },
            ],
        };
        let merged = mesh.merge_coplanar();
        assert_eq!(merged.polygons.len(), 2);
        let xy_poly = merged.polygons.iter().find(|p| p.plane.n_z != 0).unwrap();
        let xz_poly = merged.polygons.iter().find(|p| p.plane.n_y != 0).unwrap();
        assert!(xy_poly.vertices.contains(&0));
        assert!(xy_poly.vertices.contains(&1));
        assert!(xz_poly.vertices.contains(&0));
        assert!(xz_poly.vertices.contains(&1));
    }

    /// X-junction where an outer rectangle's bottom edge passes
    /// through a vertex that is also on the boundary of an inner
    /// hole. The rim's two edges are exactly collinear (axis-aligned
    /// cube edge); the hole's two edges are exactly collinear. The
    /// "go straight" rule must pair the rim edges as one loop and the
    /// hole edges as another, not zigzag between them.
    ///
    /// Geometry: outer rectangle corners (-2,-2)..(2,2), with the
    /// bottom edge subdivided at J=(0,-2). A square hole with corners
    /// (-1,-1)..(1,1) has its edge passing through J? No — that hole
    /// can't reach J=(0,-2) without leaving the rectangle. Instead,
    /// route the hole as a degenerate loop that touches J: an
    /// approximation of the BSP X-junction where a slit runs from the
    /// rim (at J) into a hole.
    ///
    /// To get a true X-junction without twin cancellation, we need
    /// two CCW polygons that share J but no edge through J. The
    /// fan-of-triangles construction below builds such a topology
    /// with one collinear pair from each sub-region.
    #[test]
    fn x_junction_with_collinear_pairs_extracts_two_loops() {
        // Vertex pool laid out so two regions touch at J=index 0.
        //
        //   region A: triangles below the +X axis through J
        //     A1 = (J=0, A_left=1, A_below=2)
        //     A2 = (J=0, A_below=2, A_right=3)
        //   region B: triangles above the +X axis through J
        //     B1 = (J=0, B_right=4, B_above=5)
        //     B2 = (J=0, B_above=5, B_left=6)
        //
        // J at origin. A_left at (-1, 0), A_right at (+1, 0) — A's
        // edges through J are (J→A_left) and (A_right→J), both along
        // ±X. After cancellation, the boundary outgoing from J in
        // region A travels through A_below and back. Similarly for B
        // through B_above. The X-junction at J has the rim-style
        // collinear-pair structure (A_left↔A_right along ±X,
        // B_left↔B_right along ±X but vertically offset).
        //
        // For BSP-shape collinearity we need each region's IN/OUT
        // pair to be collinear *with each other through J*. That
        // means region A's incoming-to-J edge and outgoing-from-J
        // edge are collinear in 2D. Construct A as a triangle whose
        // boundary visits J on a straight stretch:
        //   A: a quadrilateral with J on the interior of its top
        //   edge, split: (-1,-1) → (1,-1) → (1,0) → J=(0,0) → (-1,0)
        //
        // Region B mirrored above:
        //   B: (-1,1) → J=(0,0) → (1,0) split: ...
        //
        // But (1,0)→J appears in A and J→(1,0) appears in B → twins,
        // cancel. Same for (-1,0)→J. So the X-junction edges cancel.
        //
        // Realistic BSP X-junction needs sliver topology: small
        // polygons whose edges TO J don't have twins. Easiest synthetic
        // construction: one large region with J on its rim (boundary
        // visits J once, with collinear in/out pair) and additional
        // sliver triangles touching J without sharing edges.
        //
        // Region A: rectangle with J on top edge, split at J.
        //   Verts: 0=(-2,-1), 1=(2,-1), 2=(2,0), 3=J=(0,0), 4=(-2,0)
        //   CCW: 0→1→2→3→4→0
        //   Edges incident to J: (2,3) inbound (from (2,0), direction
        //   -X), (3,4) outbound (to (-2,0), direction -X). Collinear ✓
        //
        // Sliver triangles above J that touch J at their apex but
        // don't share full edges with A:
        //   T1: J → (1,1) → (-1,1)  [single CCW triangle above J]
        //
        // T1's edges: (3,5), (5,6), (6,3) where 5=(1,1), 6=(-1,1).
        // None are twins of A's edges. T1 contributes (3,5) outbound
        // and (6,3) inbound at J. Direction (3,5) = (1,1)-(0,0) = +X+Y,
        // angle π/4. Direction (6,3) = (0,0)-(-1,1) = (1,-1), so the
        // inbound direction at J is +X-Y... no wait, (6,3) means edge
        // from 6 to 3, so it ARRIVES at J from direction (3-6) = (1,-1),
        // and at J the incoming direction (the d_in we use) is the
        // arrival direction, +X-Y, atan2 = -π/4.
        //
        // At J, four boundary edges:
        //   in1=(2,3): direction +X-stuff... d_in at J = (J - prev) =
        //     (0,0) - (2,0) = (-2,0). atan2(-2,0) = π. Hmm, that's the
        //     vector from prev to cur which is the "incoming direction"
        //     of motion at J. So d_in_motion = -X, angle π.
        //   out1=(3,4): cur=J going to (-2,0). direction (-2,0)-(0,0)
        //     = (-2,0). angle π. SAME as d_in_motion → straight (turn 0).
        //   in2=(6,3): d_in_motion at J = (0,0) - (-1,1) = (1,-1).
        //     atan2(-1,1) = -π/4.
        //   out2=(3,5): direction (1,1)-(0,0) = (1,1). atan2(1,1) = π/4.
        //     Turn from -π/4 to π/4: π/2. (Sliver makes a 90° corner at J.)
        //
        // At J's first visit (along A's rim), d_in = -X (angle π).
        //   to 4 (out1): turn = π - π = 0. abs = 0. ← go-straight picks this.
        //   to 5 (out2): turn from π to π/4 = -3π/4. abs = 3π/4.
        // Picks 4 (rim continuation). ✓
        //
        // At J's second visit (along sliver T1), d_in = +X-Y (angle -π/4).
        //   only out2 available (out1 already visited).
        //   picks 5 (sliver). ✓
        //
        // This exercises angular continuation at an X-junction with
        // one collinear pair (A's rim) and verifies the sliver loop
        // doesn't get tangled into the rim loop.
        let plane = Plane3 {
            n_x: 0,
            n_y: 0,
            n_z: 1 << 16,
            d: 0,
        };
        let mesh = IndexedMesh {
            vertices: vec![
                pt(-2.0, -1.0, 0.0), // 0
                pt(2.0, -1.0, 0.0),  // 1
                pt(2.0, 0.0, 0.0),   // 2
                pt(0.0, 0.0, 0.0),   // 3 = J
                pt(-2.0, 0.0, 0.0),  // 4
                pt(1.0, 1.0, 0.0),   // 5
                pt(-1.0, 1.0, 0.0),  // 6
            ],
            polygons: vec![
                // Region A: pentagon, J on its top edge.
                IndexedPolygon {
                    vertices: vec![0, 1, 2, 3, 4],
                    plane,
                    color: 0,
                },
                // Sliver T1: triangle touching J at its apex.
                IndexedPolygon {
                    vertices: vec![3, 5, 6],
                    plane,
                    color: 0,
                },
            ],
        };
        let merged = mesh.merge_coplanar();
        // Two separate loops: A's pentagon outline and T1's triangle.
        // No twin edges between them, so cancellation leaves both
        // boundaries intact; the angular rule at J keeps them on
        // their respective loops.
        assert_eq!(
            merged.polygons.len(),
            2,
            "X-junction at J must extract 2 loops, got {}",
            merged.polygons.len()
        );
        let lens: std::collections::BTreeSet<usize> =
            merged.polygons.iter().map(|p| p.vertices.len()).collect();
        assert_eq!(
            lens,
            [3, 5].into_iter().collect(),
            "expected one 5-gon (A) and one 3-gon (T1)"
        );
    }

    #[test]
    fn extract_loops_open_boundary_returns_none() {
        // Pass minimal vertex pool + dummy z=0 plane.
        let vertices = vec![pt(0.0, 0.0, 0.0), pt(1.0, 0.0, 0.0)];
        let plane = Plane3 {
            n_x: 0,
            n_y: 0,
            n_z: 1 << 16,
            d: 0,
        };
        let boundary = vec![(0_usize, 1_usize)];
        assert!(extract_loops(&boundary, &vertices, &plane).is_none());
    }

    #[test]
    fn extract_loops_branching_boundary_returns_none() {
        let vertices = vec![pt(0.0, 0.0, 0.0), pt(1.0, 0.0, 0.0), pt(0.0, 1.0, 0.0)];
        let plane = Plane3 {
            n_x: 0,
            n_y: 0,
            n_z: 1 << 16,
            d: 0,
        };
        let boundary = vec![(0_usize, 1_usize), (1, 0), (0, 2)];
        assert!(extract_loops(&boundary, &vertices, &plane).is_none());
    }

    #[test]
    fn extract_loops_two_disjoint_triangles() {
        let vertices = vec![
            pt(0.0, 0.0, 0.0),
            pt(1.0, 0.0, 0.0),
            pt(0.0, 1.0, 0.0),
            pt(3.0, 0.0, 0.0),
            pt(4.0, 0.0, 0.0),
            pt(3.0, 1.0, 0.0),
        ];
        let plane = Plane3 {
            n_x: 0,
            n_y: 0,
            n_z: 1 << 16,
            d: 0,
        };
        let boundary = vec![(0_usize, 1_usize), (1, 2), (2, 0), (3, 4), (4, 5), (5, 3)];
        let loops =
            extract_loops(&boundary, &vertices, &plane).expect("two disjoint loops should extract");
        assert_eq!(loops.len(), 2);
        assert_eq!(loops[0].len(), 3);
        assert_eq!(loops[1].len(), 3);
    }

    #[test]
    fn extract_loops_empty_boundary_returns_some_empty() {
        let vertices: Vec<Point3> = vec![];
        let plane = Plane3 {
            n_x: 0,
            n_y: 0,
            n_z: 1 << 16,
            d: 0,
        };
        let boundary: Vec<(VertexId, VertexId)> = vec![];
        let loops = extract_loops(&boundary, &vertices, &plane)
            .expect("empty boundary should be Some(empty)");
        assert!(loops.is_empty());
    }

    fn map(edges: &[(VertexId, VertexId)]) -> HashMap<(VertexId, VertexId), u32> {
        let mut m = HashMap::new();
        for &e in edges {
            *m.entry(e).or_insert(0) += 1;
        }
        m
    }

    /// All loop edges are externally backed (every reverse exists outside
    /// the bucket). No spurious vertices to collapse — return unchanged.
    #[test]
    fn collapse_noop_when_every_edge_externally_backed() {
        let loop_verts = vec![0_usize, 1, 2, 3];
        // Every reverse in global, none in this bucket.
        let global = map(&[(1, 0), (2, 1), (3, 2), (0, 3)]);
        let bucket = HashMap::new();
        let out = collapse_unbacked_boundary_runs(&loop_verts, &global, &bucket);
        assert_eq!(out, loop_verts);
    }

    /// Single spurious vertex between two real vertices. The chord that
    /// skips it is externally backed; its incident edges are not. Expect
    /// the spurious vertex collapsed out.
    #[test]
    fn collapse_drops_one_spurious_vertex() {
        // Loop: 0 -> 1 -> 2 -> 3 -> 4. Vertex 1 is spurious.
        let loop_verts = vec![0_usize, 1, 2, 3, 4];
        // Real boundary reverses: (2,0) chord + tail edges.
        let global = map(&[(2, 0), (3, 2), (4, 3), (0, 4)]);
        let bucket = HashMap::new();
        let out = collapse_unbacked_boundary_runs(&loop_verts, &global, &bucket);
        assert_eq!(out, vec![0, 2, 3, 4]);
    }

    /// Two consecutive spurious vertices. The chord that skips both is
    /// externally backed; both incident edges and the middle edge are
    /// not.
    #[test]
    fn collapse_drops_consecutive_spurious_run() {
        // Loop: 0 -> 1 -> 2 -> 3 -> 4 -> 5. Vertices 1 and 2 are spurious.
        let loop_verts = vec![0_usize, 1, 2, 3, 4, 5];
        let global = map(&[(3, 0), (4, 3), (5, 4), (0, 5)]);
        let bucket = HashMap::new();
        let out = collapse_unbacked_boundary_runs(&loop_verts, &global, &bucket);
        assert_eq!(out, vec![0, 3, 4, 5]);
    }

    /// Spurious run wraps the loop's start (verts at the last index +
    /// the first index are spurious). The rotate phase normalizes the
    /// loop so the walk phase sees the run as contiguous.
    #[test]
    fn collapse_handles_wrap_around_run() {
        // Loop indices 0..5 with values 10..14. Spurious indices are 4
        // (last) and 0 (first). The chord from index 3 to index 1 skips
        // them.
        let loop_verts = vec![10_usize, 11, 12, 13, 14];
        // (11, 13) is the chord's reverse: chord verts are 13 -> 11.
        let global = map(&[(11, 13), (12, 11), (13, 12)]);
        let bucket = HashMap::new();
        let out = collapse_unbacked_boundary_runs(&loop_verts, &global, &bucket);
        // After collapsing 14 and 10, only 11, 12, 13 survive (in some
        // rotation; rotate_left makes 11 the head).
        assert_eq!(out.len(), 3);
        let as_set: std::collections::BTreeSet<VertexId> = out.iter().copied().collect();
        assert_eq!(as_set, [11, 12, 13].into_iter().collect());
    }

    /// If the only externally-backed chords would collapse the loop to
    /// fewer than 3 vertices, fall back to the original loop. Better to
    /// emit a non-2-manifold polygon than a degenerate one.
    #[test]
    fn collapse_falls_back_when_result_would_degenerate() {
        // Loop: 0 -> 1 -> 2 -> 3. Chord (0, 3) is the only externally
        // backed edge — collapsing would leave [0, 3], which has <3 verts.
        let loop_verts = vec![0_usize, 1, 2, 3];
        let global = map(&[(3, 0)]);
        let bucket = HashMap::new();
        let out = collapse_unbacked_boundary_runs(&loop_verts, &global, &bucket);
        assert_eq!(out, loop_verts);
    }

    /// A reverse that exists only inside this bucket does NOT count as
    /// externally backed — bucket-internal twins were already cancelled
    /// before extract_loops ran. This is the "subtract bucket from
    /// global" check working as intended.
    #[test]
    fn collapse_ignores_intra_bucket_reverses() {
        let loop_verts = vec![0_usize, 1, 2, 3];
        // Pretend (3, 0) appears once globally and once in this bucket.
        // Net "external" count is zero — chord is not backed.
        let global = map(&[(3, 0), (1, 0), (2, 1), (3, 2), (0, 3)]);
        let bucket = map(&[(3, 0)]);
        let out = collapse_unbacked_boundary_runs(&loop_verts, &global, &bucket);
        // Same as the no-op test — no collapsible chord.
        assert_eq!(out, loop_verts);
    }

    /// Loops shorter than 4 vertices have no internal vertex to
    /// collapse. The function short-circuits.
    #[test]
    fn collapse_short_loop_short_circuits() {
        let loop_verts = vec![0_usize, 1, 2];
        let global = HashMap::new();
        let bucket = HashMap::new();
        let out = collapse_unbacked_boundary_runs(&loop_verts, &global, &bucket);
        assert_eq!(out, loop_verts);
    }

    /// Multiplicity-preserving cancellation: forward count 2, reverse
    /// count 1 must surface one boundary edge (the imbalance), not zero.
    /// The naive boolean filter that issue #350 replaced would drop both.
    #[test]
    fn cancellation_preserves_multiplicity() {
        let directed = map(&[(0, 1), (0, 1), (1, 0)]);
        let boundary = boundary_edges_after_twin_cancellation(&directed);
        assert_eq!(boundary, vec![(0, 1)]);
    }

    /// Symmetric multiplicity (2 + 2) cancels completely.
    #[test]
    fn cancellation_drops_symmetric_pairs() {
        let directed = map(&[(0, 1), (0, 1), (1, 0), (1, 0)]);
        let boundary = boundary_edges_after_twin_cancellation(&directed);
        assert!(boundary.is_empty());
    }

    /// Pathological topology where extract_loops returns None and the
    /// fallback emits the bucket's originals unchanged. Pinned because
    /// the fallback path otherwise has zero coverage.
    #[test]
    fn pathological_bucket_falls_back_to_originals() {
        let plane = Plane3 {
            n_x: 0,
            n_y: 0,
            n_z: 1 << 16,
            d: 0,
        };
        let mesh = IndexedMesh {
            vertices: vec![
                pt(0.0, 0.0, 0.0),
                pt(1.0, 0.0, 0.0),
                pt(0.0, 1.0, 0.0),
                pt(2.0, 0.0, 0.0),
                pt(2.0, 1.0, 0.0),
            ],
            polygons: vec![
                IndexedPolygon {
                    vertices: vec![0, 1, 2],
                    plane,
                    color: 0,
                },
                IndexedPolygon {
                    vertices: vec![1, 3, 4],
                    plane,
                    color: 0,
                },
                IndexedPolygon {
                    vertices: vec![0, 1, 3],
                    plane,
                    color: 0,
                },
            ],
        };
        let merged = mesh.merge_coplanar();
        assert!(
            !merged.polygons.is_empty(),
            "pathological bucket must not crash; fallback emits originals"
        );
    }

    #[test]
    fn normalize_simple_loop_passes_through() {
        let loops = normalize_loop(&[0, 1, 2, 3]);
        assert_eq!(loops, vec![vec![0, 1, 2, 3]]);
    }

    #[test]
    fn normalize_strips_adjacent_duplicates() {
        let loops = normalize_loop(&[0, 0, 1, 2, 2, 3]);
        assert_eq!(loops, vec![vec![0, 1, 2, 3]]);
    }

    #[test]
    fn normalize_strips_wrap_around_duplicate() {
        let loops = normalize_loop(&[0, 1, 2, 3, 0]);
        assert_eq!(loops, vec![vec![0, 1, 2, 3]]);
    }

    #[test]
    fn normalize_splits_pinch_into_two_simple_loops() {
        // [a, b, c, r, d, e, r, f, g] with r at indices 3, 6.
        // outer = verts[..=3] + verts[7..] = [a, b, c, r, f, g]
        // inner = verts[3..6]               = [r, d, e]
        let loops = normalize_loop(&[0, 1, 2, 99, 3, 4, 99, 5, 6]);
        assert_eq!(loops.len(), 2);
        assert_eq!(loops[0], vec![0, 1, 2, 99, 5, 6]);
        assert_eq!(loops[1], vec![99, 3, 4]);
    }

    #[test]
    fn normalize_drops_antenna_spike() {
        // [a, b, a, c, d] — split at a (0, 2): outer = [a, c, d], inner = [a, b].
        // Inner is len 2, dropped.
        let loops = normalize_loop(&[0, 1, 0, 2, 3]);
        assert_eq!(loops, vec![vec![0, 2, 3]]);
    }

    #[test]
    fn normalize_handles_recursive_pinch() {
        // [a, b, a, c, d, c, e] — two pinches.
        // First split at a (0, 2): outer = [a, c, d, c, e], inner = [a, b] (dropped).
        // outer recurses, splits at c (1, 3): outer-inner = [a, c, e], inner-inner = [c, d] (dropped).
        let loops = normalize_loop(&[0, 1, 0, 2, 3, 2, 4]);
        assert_eq!(loops, vec![vec![0, 2, 4]]);
    }

    #[test]
    fn normalize_drops_fully_degenerate_loop() {
        // [a, b, a, b] — two twin pairs. Split at a (0, 2): outer = [a, b],
        // inner = [a, b]. Both len 2, dropped.
        let loops = normalize_loop(&[0, 1, 0, 1]);
        assert!(loops.is_empty());
    }

    #[test]
    fn normalize_drops_too_short_inputs() {
        assert!(normalize_loop(&[]).is_empty());
        assert!(normalize_loop(&[0]).is_empty());
        assert!(normalize_loop(&[0, 1]).is_empty());
    }
}
