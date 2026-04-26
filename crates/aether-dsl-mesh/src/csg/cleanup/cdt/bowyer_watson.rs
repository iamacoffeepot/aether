//! Bowyer-Watson incremental Delaunay triangulation (ADR-0056 PR 2).
//!
//! Builds an unconstrained Delaunay triangulation of a set of 2D
//! integer points by:
//!
//! 1. Constructing a "super-triangle" guaranteed to contain every
//!    input point strictly in its interior.
//! 2. Inserting input vertices one at a time. For each insertion:
//!    - Walk to the triangle containing the new point (orient2d steps).
//!    - Expand the **cavity** = set of triangles whose circumcircle
//!      contains the new point (BFS via neighbor pointers + in_circle).
//!    - Delete cavity triangles, then re-triangulate the cavity by
//!      connecting the new vertex to each cavity-boundary edge.
//!    - Stitch neighbor pointers across the new fan.
//!
//! After insertion of all input vertices, the triangulation is
//! Delaunay over the union of input + super-triangle vertices.
//! Constraint enforcement and super-triangle removal are PR 3.
//!
//! ### Data layout
//!
//! Triangles are stored in a slot map (`Vec<Option<Triangle>>`). The
//! `None` slots are "deleted" — kept for index stability so neighbor
//! pointers stay valid. Each `Triangle` owns three CCW vertex indices
//! and three optional neighbor triangle indices, with the convention:
//! `neighbors[i]` is the triangle across the edge **opposite** vertex
//! `verts[i]`, i.e., the edge that runs from `verts[(i+1)%3]` to
//! `verts[(i+2)%3]`. The neighbor's edge runs in the opposite
//! direction (manifold mesh).
//!
//! ### Determinism
//!
//! Insertion order is the caller's responsibility — `build` consumes
//! `points` in `Vec` order. Internally, cavity expansion uses a stack
//! seeded in vertex-index order and the boundary-walk visits cavity
//! triangles in slot-map order, so identical inputs produce identical
//! triangulations across runs.
//!
//! ### Magnitude budget
//!
//! Per the ADR-0056 amendment, all predicates fit in i128 at the
//! ADR-0054 input cap (`|coord| ≤ 2^24`). The super-triangle pushes
//! coordinates out by `4 × bbox_extent` (so coord magnitudes ≤
//! `~2^27`), still well within the in-circle headroom (intermediates
//! ≤ `2^115`).
//!
//! ### Assumptions
//!
//! For PR 2, the algorithm assumes input points are in **general
//! position** — no three input points are collinear with a fourth, no
//! four are cocircular, and no input point is coincident with another
//! or with a super-triangle vertex. PR 3's constraint-enforcement
//! pass adds the boundary-edge handling that the cleanup pipeline
//! relies on; it is also where degenerate-input handling lands.

use super::predicates::{Point2, in_circle, orient2d};

pub(super) type VertId = usize;
pub(super) type TriId = usize;

#[derive(Debug, Clone)]
pub(super) struct Triangle {
    /// Vertex indices in CCW order.
    pub(super) verts: [VertId; 3],
    /// Triangle adjacent across the edge opposite each vertex. `None`
    /// means the edge is on the convex hull (only the super-triangle's
    /// outer edges hit this in PR 2's flow).
    pub(super) neighbors: [Option<TriId>; 3],
}

#[derive(Debug)]
pub(super) struct Mesh {
    pub(super) vertices: Vec<Point2>,
    /// Slot map: `triangles[id] = None` marks a deleted triangle whose
    /// id may not yet be reused. We don't reuse slots in PR 2 — kept
    /// simple at the cost of some memory.
    pub(super) triangles: Vec<Option<Triangle>>,
    /// First N vertex ids belong to the super-triangle. Useful for PR
    /// 3's classify-and-discard pass.
    pub(super) super_count: usize,
}

impl Mesh {
    /// Build the Delaunay triangulation of the given points. Returns
    /// an empty mesh if `points` is empty.
    pub(super) fn build(points: Vec<Point2>) -> Self {
        let mut mesh = Mesh {
            vertices: Vec::new(),
            triangles: Vec::new(),
            super_count: 0,
        };
        if points.is_empty() {
            return mesh;
        }
        mesh.add_super_triangle(&points);
        let super_count = mesh.vertices.len();
        mesh.super_count = super_count;
        for p in points {
            mesh.vertices.push(p);
        }
        let total = mesh.vertices.len();
        for vid in super_count..total {
            mesh.insert_vertex(vid);
        }
        mesh
    }

    /// Iterator over alive (non-deleted) triangles, yielding
    /// `(TriId, &Triangle)` pairs.
    pub(super) fn alive_triangles(&self) -> impl Iterator<Item = (TriId, &Triangle)> {
        self.triangles
            .iter()
            .enumerate()
            .filter_map(|(i, t)| t.as_ref().map(|t| (i, t)))
    }

    /// True iff edge `(u, v)` exists in some alive triangle.
    pub(super) fn has_edge(&self, u: VertId, v: VertId) -> bool {
        self.alive_triangles().any(|(_, tri)| {
            (0..3).any(|i| {
                let a = tri.verts[i];
                let b = tri.verts[(i + 1) % 3];
                (a == u && b == v) || (a == v && b == u)
            })
        })
    }

    /// Find any alive edge that strictly crosses segment `(u, v)` — used
    /// during constraint enforcement to locate a diagonal worth flipping.
    /// Returns `(triangle id, edge index)` for the first crossing found.
    /// Iteration is in slot-map order so the choice is deterministic.
    pub(super) fn find_crossing_edge(&self, u: VertId, v: VertId) -> Option<(TriId, usize)> {
        let pu = self.vertices[u];
        let pv = self.vertices[v];
        for (tid, tri) in self.alive_triangles() {
            for i in 0..3 {
                let a = tri.verts[(i + 1) % 3];
                let b = tri.verts[(i + 2) % 3];
                // Skip edges that share an endpoint with the constraint —
                // we want strictly-crossing diagonals only.
                if a == u || a == v || b == u || b == v {
                    continue;
                }
                if segments_strictly_cross(pu, pv, self.vertices[a], self.vertices[b]) {
                    return Some((tid, i));
                }
            }
        }
        None
    }

    /// Flip the diagonal of the quad formed by triangle `tid` and its
    /// neighbor across edge `edge_idx`. Returns the new triangle ids
    /// `(t1', t2')` on success, or `None` if the quad is non-convex /
    /// the requested edge is on the convex hull.
    ///
    /// Stitches all neighbor pointers (the four outside neighbors get
    /// their back-pointers redirected). The two original slots are
    /// reused for the new triangles to keep `TriId`s compact in the
    /// common flip-heavy case.
    pub(super) fn flip_edge(&mut self, tid: TriId, edge_idx: usize) -> Option<(TriId, TriId)> {
        let t1 = self.triangles[tid].clone()?;
        let n_tid = t1.neighbors[edge_idx]?;
        let t2 = self.triangles[n_tid].clone()?;
        // Find which edge of T2 points back to T1 — that's the shared edge.
        let n_edge_idx = (0..3).find(|&j| t2.neighbors[j] == Some(tid))?;

        // Label vertices around the shared edge:
        //   v1 = T1's vertex opposite the shared edge
        //   v2 = T2's vertex opposite the shared edge
        //   a, b = endpoints of the shared edge (T1 walks a→b, T2 walks b→a)
        let v1 = t1.verts[edge_idx];
        let v2 = t2.verts[n_edge_idx];
        let a = t1.verts[(edge_idx + 1) % 3];
        let b = t1.verts[(edge_idx + 2) % 3];

        // Convexity test: v1 and v2 must lie on opposite sides of the
        // line through (a, b), AND a and b must lie on opposite sides of
        // the line through (v1, v2). The latter is implied by the former
        // for a planar quad; checking both guards against degenerate
        // cocircular configurations.
        let pv1 = self.vertices[v1];
        let pv2 = self.vertices[v2];
        let pa = self.vertices[a];
        let pb = self.vertices[b];
        let s_ab_v1 = orient2d(pa, pb, pv1);
        let s_ab_v2 = orient2d(pa, pb, pv2);
        if s_ab_v1 == 0 || s_ab_v2 == 0 || s_ab_v1 == s_ab_v2 {
            return None;
        }
        let s_v1v2_a = orient2d(pv1, pv2, pa);
        let s_v1v2_b = orient2d(pv1, pv2, pb);
        if s_v1v2_a == 0 || s_v1v2_b == 0 || s_v1v2_a == s_v1v2_b {
            return None;
        }

        // Capture T1's and T2's outside neighbors (the four NOT each other).
        // Naming follows the convention "n_after_<vertex>" = the neighbor
        // across the edge starting at <vertex> going CCW.
        let n_after_a_in_t1 = t1.neighbors[(edge_idx + 2) % 3]; // edge from v1 to a
        let n_after_b_in_t1 = t1.neighbors[(edge_idx + 1) % 3]; // edge from b to v1
        let n_after_a_in_t2 = t2.neighbors[(edge_idx_a_in_t2(&t2, a))?]; // edge from a to v2
        let n_after_b_in_t2 = t2.neighbors[(edge_idx_b_in_t2(&t2, b))?]; // edge from v2 to b

        // New triangles share the new diagonal (v1, v2):
        //   T1' = [a, v2, v1]  edges: a→v2, v2→v1 (new diag), v1→a
        //   T2' = [b, v1, v2]  edges: b→v1, v1→v2 (new diag), v2→b
        let new_t1_id = tid;
        let new_t2_id = n_tid;
        let new_t1 = Triangle {
            verts: [a, v2, v1],
            neighbors: [
                Some(new_t2_id), // opposite a (new diagonal v2→v1)
                n_after_a_in_t1, // opposite v2 (edge v1→a) — was T1's neighbor across v1→a
                n_after_a_in_t2, // opposite v1 (edge a→v2) — was T2's neighbor across a→v2
            ],
        };
        let new_t2 = Triangle {
            verts: [b, v1, v2],
            neighbors: [
                Some(new_t1_id), // opposite b (new diagonal v1→v2)
                n_after_b_in_t2, // opposite v1 (edge v2→b) — was T2's neighbor across v2→b
                n_after_b_in_t1, // opposite v2 (edge b→v1) — was T1's neighbor across b→v1
            ],
        };

        // Redirect the four outside neighbors' back-pointers.
        // Each Ni used to point to T1 or T2 across some edge; that edge
        // still exists, but it now belongs to T1' or T2'.
        retarget_back_pointer(self, n_after_a_in_t1, tid, new_t1_id);
        retarget_back_pointer(self, n_after_b_in_t1, tid, new_t2_id);
        retarget_back_pointer(self, n_after_a_in_t2, n_tid, new_t1_id);
        retarget_back_pointer(self, n_after_b_in_t2, n_tid, new_t2_id);

        self.triangles[new_t1_id] = Some(new_t1);
        self.triangles[new_t2_id] = Some(new_t2);
        Some((new_t1_id, new_t2_id))
    }

    /// Force the constraint edge `(u, v)` to appear in the triangulation
    /// by repeatedly flipping diagonals that cross it. Returns `Ok(())`
    /// once `(u, v)` is present (or was already), or `Err(())` if no
    /// flippable crossing edge could be found before progress stalled.
    ///
    /// The algorithm is the standard Anglada-style approach reduced to
    /// our small input sizes: while `(u, v)` is missing, find any edge
    /// that strictly crosses the segment, flip it if its quad is convex,
    /// and try again. Bounded by `MAX_ENFORCE_ITERATIONS` to guarantee
    /// termination on pathological input (returning `Err` rather than
    /// hanging).
    pub(super) fn enforce_constraint(&mut self, u: VertId, v: VertId) -> Result<(), ()> {
        const MAX_ENFORCE_ITERATIONS: usize = 4096;
        for _ in 0..MAX_ENFORCE_ITERATIONS {
            if self.has_edge(u, v) {
                return Ok(());
            }
            let (tid, edge_idx) = match self.find_crossing_edge(u, v) {
                Some(x) => x,
                None => return Err(()), // no crossing but edge missing — topology bug
            };
            // Try to flip. If non-convex, scan for any other crossing
            // edge that IS flippable.
            if self.flip_edge(tid, edge_idx).is_some() {
                continue;
            }
            // Walk all alive triangles, edge by edge, looking for any
            // crossing diagonal whose quad is convex.
            let mut flipped = false;
            'outer: for (tid2, tri) in self
                .alive_triangles()
                .map(|(i, t)| (i, t.clone()))
                .collect::<Vec<_>>()
            {
                for i in 0..3 {
                    let a = tri.verts[(i + 1) % 3];
                    let b = tri.verts[(i + 2) % 3];
                    if a == u || a == v || b == u || b == v {
                        continue;
                    }
                    let pu = self.vertices[u];
                    let pv = self.vertices[v];
                    if !segments_strictly_cross(pu, pv, self.vertices[a], self.vertices[b]) {
                        continue;
                    }
                    if self.flip_edge(tid2, i).is_some() {
                        flipped = true;
                        break 'outer;
                    }
                }
            }
            if !flipped {
                return Err(());
            }
        }
        Err(())
    }
}

/// True iff segments `p–q` and `r–s` strictly cross at a single interior
/// point (no endpoint touches another segment, no collinearity).
fn segments_strictly_cross(p: Point2, q: Point2, r: Point2, s: Point2) -> bool {
    let o1 = orient2d(r, s, p);
    let o2 = orient2d(r, s, q);
    if o1 == 0 || o2 == 0 || o1 == o2 {
        return false;
    }
    let o3 = orient2d(p, q, r);
    let o4 = orient2d(p, q, s);
    if o3 == 0 || o4 == 0 || o3 == o4 {
        return false;
    }
    true
}

/// Find the index in `t2.neighbors` whose edge starts at vertex `a` —
/// helper for unpacking T2's outside neighbors during a flip.
fn edge_idx_a_in_t2(t2: &Triangle, a: VertId) -> Option<usize> {
    // T2 = [v2, b, a] where shared edge is opposite v2 (idx 0). We want
    // the edge index opposite vertex `a` (verts[2]) — that's the edge
    // from verts[0]=v2 to verts[1]=b... wait, that's the edge opposite a,
    // which runs from v2 to b — not what we want. Let's find by vertex.
    // The "edge starting at a going CCW" is the edge from verts[i]=a to
    // verts[i+1]; its opposite vertex is verts[i+2]; so the neighbor
    // index is (i+2) mod 3.
    let i = (0..3).find(|&i| t2.verts[i] == a)?;
    Some((i + 2) % 3)
}

/// Same idea, for vertex `b` in T2.
fn edge_idx_b_in_t2(t2: &Triangle, b: VertId) -> Option<usize> {
    let i = (0..3).find(|&i| t2.verts[i] == b)?;
    Some((i + 2) % 3)
}

/// Retarget `neighbor`'s back-pointer from `old` to `new` if it currently
/// points to `old`. No-op if `neighbor` is `None` or no slot matches.
fn retarget_back_pointer(mesh: &mut Mesh, neighbor: Option<TriId>, old: TriId, new: TriId) {
    let Some(n) = neighbor else { return };
    if let Some(tri) = mesh.triangles[n].as_mut() {
        for i in 0..3 {
            if tri.neighbors[i] == Some(old) {
                tri.neighbors[i] = Some(new);
                return;
            }
        }
    }
}

impl Mesh {
    fn add_super_triangle(&mut self, points: &[Point2]) {
        let mut min_x = i64::MAX;
        let mut max_x = i64::MIN;
        let mut min_y = i64::MAX;
        let mut max_y = i64::MIN;
        for &(x, y) in points {
            min_x = min_x.min(x);
            max_x = max_x.max(x);
            min_y = min_y.min(y);
            max_y = max_y.max(y);
        }
        let dx = max_x - min_x;
        let dy = max_y - min_y;
        let cx = (min_x + max_x) / 2;
        let cy = (min_y + max_y) / 2;
        // `+ 1` ensures non-zero scale even for a single point or all
        // collinear points; `* 4` gives generous margin so input points
        // sit comfortably inside the super-triangle.
        let scale = (dx.max(dy) + 1) * 4;
        let v0 = (cx - scale, cy - scale);
        let v1 = (cx + scale, cy - scale);
        let v2 = (cx, cy + scale);
        debug_assert!(orient2d(v0, v1, v2) > 0, "super-triangle must be CCW");
        self.vertices.push(v0);
        self.vertices.push(v1);
        self.vertices.push(v2);
        self.triangles.push(Some(Triangle {
            verts: [0, 1, 2],
            neighbors: [None, None, None],
        }));
    }

    /// Walk through the mesh from any alive triangle to the one that
    /// contains the query vertex. Uses orient2d at each edge: if the
    /// query is on the negative side of an edge, step across to that
    /// edge's neighbor.
    fn find_containing_triangle(&self, vid: VertId) -> TriId {
        let p = self.vertices[vid];
        let mut current = self.first_alive();
        loop {
            let tri = self.triangles[current]
                .as_ref()
                .expect("walked into a deleted triangle");
            let v = tri.verts;
            let mut moved = false;
            for i in 0..3 {
                let a = self.vertices[v[(i + 1) % 3]];
                let b = self.vertices[v[(i + 2) % 3]];
                // Edge a→b is opposite vertex `i`. Triangle interior is on
                // the positive (CCW) side of every edge. If `p` is on the
                // negative side, cross over.
                if orient2d(a, b, p) < 0 {
                    let next = tri.neighbors[i].expect(
                        "find_containing_triangle: query lies outside super-triangle — \
                         either input violated the ADR-0054 coord cap or super-triangle \
                         scale was set too tight",
                    );
                    current = next;
                    moved = true;
                    break;
                }
            }
            if !moved {
                return current;
            }
        }
    }

    fn first_alive(&self) -> TriId {
        for (i, slot) in self.triangles.iter().enumerate() {
            if slot.is_some() {
                return i;
            }
        }
        unreachable!("first_alive called on empty mesh");
    }

    /// Expand the cavity around vertex `vid` outward via in_circle.
    /// The starting triangle is always in the cavity (its circumcircle
    /// contains `vid` by construction — `vid` is in its interior).
    fn expand_cavity(&self, start: TriId, vid: VertId) -> Vec<TriId> {
        let p = self.vertices[vid];
        let mut cavity = vec![start];
        let mut visited = vec![false; self.triangles.len()];
        visited[start] = true;
        let mut stack = vec![start];
        while let Some(tid) = stack.pop() {
            let tri = self.triangles[tid]
                .as_ref()
                .expect("expand_cavity: visited a deleted triangle");
            // Iterate neighbor slots in fixed order for determinism.
            let neighbors: [Option<TriId>; 3] = tri.neighbors;
            for &maybe_n in &neighbors {
                let Some(n) = maybe_n else { continue };
                if visited[n] {
                    continue;
                }
                visited[n] = true;
                let n_tri = self.triangles[n]
                    .as_ref()
                    .expect("expand_cavity: neighbor was deleted");
                let nv = n_tri.verts;
                if in_circle(
                    self.vertices[nv[0]],
                    self.vertices[nv[1]],
                    self.vertices[nv[2]],
                    p,
                ) > 0
                {
                    cavity.push(n);
                    stack.push(n);
                }
            }
        }
        // Sort for determinism — the BFS pop order depends on stack
        // mechanics and adjacency, but the final cavity set is
        // determined by geometry. Sorting normalizes downstream walks.
        cavity.sort();
        cavity
    }

    /// Walk the cavity boundary, producing for each boundary edge the
    /// pair `(a, b, outside_neighbor)` where `a → b` is the boundary
    /// edge in CCW order around the cavity (i.e., as it appears in
    /// some cavity triangle's vertex list), and `outside_neighbor` is
    /// the triangle on the far side of the edge (or `None` if the edge
    /// is on the convex hull — only the super-triangle's outer edges).
    fn cavity_boundary(&self, cavity: &[TriId]) -> Vec<(VertId, VertId, Option<TriId>)> {
        let in_cavity: Vec<bool> = {
            let mut v = vec![false; self.triangles.len()];
            for &t in cavity {
                v[t] = true;
            }
            v
        };
        let mut boundary = Vec::new();
        for &tid in cavity {
            let tri = self.triangles[tid]
                .as_ref()
                .expect("cavity_boundary: cavity contains deleted triangle");
            for i in 0..3 {
                let n = tri.neighbors[i];
                let in_cav = matches!(n, Some(n) if in_cavity[n]);
                if in_cav {
                    continue;
                }
                let a = tri.verts[(i + 1) % 3];
                let b = tri.verts[(i + 2) % 3];
                boundary.push((a, b, n));
            }
        }
        boundary
    }

    fn insert_vertex(&mut self, vid: VertId) {
        let containing = self.find_containing_triangle(vid);
        let cavity = self.expand_cavity(containing, vid);
        let boundary = self.cavity_boundary(&cavity);

        // Delete cavity triangles (slot stays, becomes None).
        for &t in &cavity {
            self.triangles[t] = None;
        }

        // Allocate new triangle ids first so we can stitch neighbors
        // across the fan in one pass.
        let n_new = boundary.len();
        let first_new_tid = self.triangles.len();
        for _ in 0..n_new {
            self.triangles.push(None);
        }

        // Build new triangles: each fills a boundary edge with vid.
        // For boundary edge (a, b), the new triangle is (vid, a, b)
        // when written CCW — `cavity_boundary` returns edges in CCW
        // order around the cavity, which is the orientation we want.
        for (k, &(a, b, outside)) in boundary.iter().enumerate() {
            let tid = first_new_tid + k;
            // Verify the new triangle's CCW orientation. If the cavity
            // expansion respected the "vid in start's interior" + Delaunay
            // invariants, this always holds.
            debug_assert!(
                orient2d(self.vertices[vid], self.vertices[a], self.vertices[b]) > 0,
                "new triangle is not CCW — cavity expansion broke an invariant"
            );
            self.triangles[tid] = Some(Triangle {
                verts: [vid, a, b],
                // neighbors[0] is across edge opposite vid: the cavity
                // boundary edge from a to b. That neighbor is the
                // outside triangle (or None on the convex hull).
                neighbors: [outside, None, None],
            });
            // If we have an outside neighbor, update its back-pointer.
            if let Some(o) = outside {
                let o_tri = self.triangles[o]
                    .as_mut()
                    .expect("outside neighbor unexpectedly deleted");
                let edge_idx = (0..3)
                    .find(|&j| {
                        let na = o_tri.verts[(j + 1) % 3];
                        let nb = o_tri.verts[(j + 2) % 3];
                        // The outside triangle's edge opposite this vertex
                        // runs from na to nb; the matching cavity-side
                        // edge runs the other way (b to a).
                        na == b && nb == a
                    })
                    .expect("outside neighbor doesn't share the boundary edge");
                o_tri.neighbors[edge_idx] = Some(tid);
            }
        }

        // Stitch the new fan together: each new triangle (vid, a, b)
        // has a neighbor "across edge opposite a" = the new triangle
        // whose b is this triangle's a, and "across edge opposite b" =
        // the new triangle whose a is this triangle's b.
        // Build a vertex → (new_tid, "is this vert the `a` slot"?) map.
        let mut by_a: std::collections::HashMap<VertId, TriId> =
            std::collections::HashMap::with_capacity(n_new);
        let mut by_b: std::collections::HashMap<VertId, TriId> =
            std::collections::HashMap::with_capacity(n_new);
        for (k, &(a, b, _)) in boundary.iter().enumerate() {
            let tid = first_new_tid + k;
            by_a.insert(a, tid);
            by_b.insert(b, tid);
        }
        for (k, &(a, b, _)) in boundary.iter().enumerate() {
            let tid = first_new_tid + k;
            // Neighbor across edge opposite `a` (verts[1]): runs from
            // b (verts[2]) to vid (verts[0]). The fan triangle adjacent
            // here has `b` in its `a` slot.
            let nbr_opp_a = by_a.get(&b).copied();
            // Neighbor across edge opposite `b` (verts[2]): runs from
            // vid (verts[0]) to a (verts[1]). The fan triangle adjacent
            // here has `a` in its `b` slot.
            let nbr_opp_b = by_b.get(&a).copied();
            let t = self.triangles[tid].as_mut().unwrap();
            t.neighbors[1] = nbr_opp_a;
            t.neighbors[2] = nbr_opp_b;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn count_alive(mesh: &Mesh) -> usize {
        mesh.alive_triangles().count()
    }

    /// Every alive triangle is wound CCW.
    fn assert_all_ccw(mesh: &Mesh) {
        for (tid, tri) in mesh.alive_triangles() {
            let p0 = mesh.vertices[tri.verts[0]];
            let p1 = mesh.vertices[tri.verts[1]];
            let p2 = mesh.vertices[tri.verts[2]];
            assert!(
                orient2d(p0, p1, p2) > 0,
                "triangle {tid} is not CCW: verts={:?}",
                tri.verts
            );
        }
    }

    /// Every neighbor pointer is symmetric: if A→B then B→A across the
    /// same shared edge.
    fn assert_neighbors_symmetric(mesh: &Mesh) {
        for (tid, tri) in mesh.alive_triangles() {
            for i in 0..3 {
                let Some(n) = tri.neighbors[i] else {
                    continue;
                };
                let n_tri = mesh.triangles[n]
                    .as_ref()
                    .expect("neighbor pointer to deleted triangle");
                let back = (0..3).any(|j| n_tri.neighbors[j] == Some(tid));
                assert!(
                    back,
                    "triangle {tid} → {n} but {n} has no back-pointer to {tid}"
                );
            }
        }
    }

    /// Delaunay property over the full mesh (super + real vertices).
    /// No alive triangle's circumcircle should strictly contain another
    /// alive vertex.
    fn assert_delaunay(mesh: &Mesh) {
        let alive: Vec<_> = mesh.alive_triangles().collect();
        for (tid, tri) in &alive {
            let v = tri.verts;
            for vid in 0..mesh.vertices.len() {
                if vid == v[0] || vid == v[1] || vid == v[2] {
                    continue;
                }
                let sign = in_circle(
                    mesh.vertices[v[0]],
                    mesh.vertices[v[1]],
                    mesh.vertices[v[2]],
                    mesh.vertices[vid],
                );
                assert!(
                    sign <= 0,
                    "Delaunay violated: triangle {tid} {:?} contains vertex {vid} {:?}",
                    v,
                    mesh.vertices[vid]
                );
            }
        }
    }

    #[test]
    fn empty_input_yields_empty_mesh() {
        let mesh = Mesh::build(vec![]);
        assert_eq!(mesh.vertices.len(), 0);
        assert_eq!(count_alive(&mesh), 0);
    }

    #[test]
    fn single_point_yields_three_triangles_around_super_corners() {
        let mesh = Mesh::build(vec![(100, 100)]);
        // 1 input + 3 super = 4 vertices; inserting 1 point splits the
        // single super-triangle into 3.
        assert_eq!(mesh.vertices.len(), 4);
        assert_eq!(count_alive(&mesh), 3);
        assert_all_ccw(&mesh);
        assert_neighbors_symmetric(&mesh);
        assert_delaunay(&mesh);
    }

    #[test]
    fn four_corners_of_square_triangulate_validly() {
        let pts = vec![(0, 0), (1000, 0), (1000, 1000), (0, 1000)];
        let mesh = Mesh::build(pts);
        // 4 input + 3 super = 7 vertices. After Delaunay: depends on
        // convex hull, but each insertion adds 2 triangles to the count
        // (or more for cavity-expanding inserts).
        assert!(count_alive(&mesh) >= 4);
        assert_all_ccw(&mesh);
        assert_neighbors_symmetric(&mesh);
        assert_delaunay(&mesh);
    }

    #[test]
    fn regular_hexagon_triangulates_validly() {
        // Six points on a circle of radius 1000 (integer-snapped).
        // After the build, every triangle is Delaunay; the property
        // checker exercises both `assert_all_ccw` and `assert_delaunay`.
        let pts: Vec<(i64, i64)> = (0..6)
            .map(|i| {
                let theta = i as f64 * std::f64::consts::PI / 3.0;
                ((1000.0 * theta.cos()) as i64, (1000.0 * theta.sin()) as i64)
            })
            .collect();
        let mesh = Mesh::build(pts);
        assert_all_ccw(&mesh);
        assert_neighbors_symmetric(&mesh);
        assert_delaunay(&mesh);
    }

    #[test]
    fn random_general_position_points_triangulate_validly() {
        // Scatter 12 points across a wide range, deterministic
        // (no rng) so the test is reproducible.
        let pts: Vec<(i64, i64)> = vec![
            (10, 20),
            (450, 30),
            (200, 800),
            (700, 600),
            (100, 500),
            (900, 100),
            (300, 300),
            (600, 900),
            (50, 750),
            (850, 450),
            (400, 100),
            (250, 650),
        ];
        let mesh = Mesh::build(pts);
        assert_all_ccw(&mesh);
        assert_neighbors_symmetric(&mesh);
        assert_delaunay(&mesh);
    }

    #[test]
    fn build_is_deterministic_across_runs() {
        let pts = vec![(0, 0), (100, 0), (100, 100), (0, 100), (50, 50)];
        let m1 = Mesh::build(pts.clone());
        let m2 = Mesh::build(pts);
        assert_eq!(m1.vertices, m2.vertices);
        assert_eq!(m1.triangles.len(), m2.triangles.len());
        for (a, b) in m1.triangles.iter().zip(m2.triangles.iter()) {
            match (a, b) {
                (Some(ta), Some(tb)) => {
                    assert_eq!(ta.verts, tb.verts);
                    assert_eq!(ta.neighbors, tb.neighbors);
                }
                (None, None) => {}
                _ => panic!("non-deterministic triangle slot occupancy"),
            }
        }
    }

    #[test]
    fn build_handles_extreme_coordinates_at_adr_0054_cap() {
        // Inputs at the contract cap. Super-triangle scale of `4 * extent`
        // keeps every intermediate well within the i128 in-circle headroom.
        let cap: i64 = 1 << 24;
        let pts = vec![(cap, cap), (-cap, cap), (-cap, -cap), (cap, -cap), (0, 0)];
        let mesh = Mesh::build(pts);
        assert_all_ccw(&mesh);
        assert_neighbors_symmetric(&mesh);
        assert_delaunay(&mesh);
    }
}
