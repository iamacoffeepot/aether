# ADR-0055: Post-CSG mesh cleanup pipeline

- **Status:** Proposed
- **Date:** 2026-04-26
- **Amends:** ADR-0054

## Context

ADR-0054 shipped BSP CSG (`union` / `intersection` / `difference`) as the v2 vocabulary addition that unlocked rook-class geometry — continuous solids with material removed. The amendment in PR 273 made it numerically robust against snap-drift. The geometry coming out is *correct*. The geometry coming out is also *shattered*: every CSG-touched surface is fragmented into many small triangles, because polygon clipping fan-triangulates each split fragment and never re-merges what was originally a single planar face.

Concretely, on the in-flight anvil:

- The slab top — geometrically a single rectangle minus two small holes — comes out as ~40 fan triangles spilling out from the cuts across an otherwise flat face.
- The body lathe meeting the slab produces a clean union but each contributing facet of the lathe gets split into 3–8 sub-fragments along the slab's footprint.
- A box-minus-box (rook crenellation cut) leaves 6–12 triangles on each face that started life as a single quad.

The wire format is `Vec<Triangle>`, so the renderer can't tell what was conceptually one face; the wireframe overlay (currently the only way to read geometry — we have no lighting yet) shows every internal split as a visible edge. Wireframes-always is the visualization regime for the foreseeable future, so "fewer noisy edges in the wireframe" is the actual visual quality goal — not the smooth-shaded-face goal that polygon merging would also unlock once lighting lands.

A second class of artifact also stems from CSG: T-junctions. After two solids are unioned, an edge of one piece's polygon can pass through an interior vertex of an adjacent piece's polygon, leaving a "T" in the topology. Without lighting these are mostly invisible in the wireframe (they look like extra edges, indistinguishable from the merge artifacts above), but they will produce hairline rendering cracks the moment shading is introduced. Better to fix them now while the cleanup pipeline is being built than to retrofit later.

The integer-coordinate / integer-plane infrastructure built for ADR-0054 makes this cleanup tractable in a way it would not be if we'd built CSG against floats. Two vertices are equal iff their `Point3` integer triples are equal — no epsilon. Two polygons are coplanar iff their `Plane3` integer coefficients are equal up to sign — no tolerance. The cleanup passes can be exact for the same reason the CSG predicates can.

## Decision

Add a three-pass cleanup pipeline that runs on the polygon stream produced by CSG, before triangulation back to the `Vec<Triangle>` wire format. The passes are pure functions operating in the same fixed-point domain as the CSG core; they live in a new module `aether_dsl_mesh::csg::cleanup` alongside the existing CSG passes. They run unconditionally — there is no "skip cleanup" mode for v1, the cleanup output is always what the wire sees.

### Pass 1: Vertex welding (indexed-mesh conversion)

Input: `Vec<Polygon>` from CSG, each polygon owning its own `Vec<Point3>` vertex list.

Output: an indexed representation — `IndexedMesh { vertices: Vec<Point3>, polygons: Vec<IndexedPolygon> }`, where each `IndexedPolygon` carries `Vec<VertexId>` (indices into the global vertex pool) and the polygon's `Plane3`.

Algorithm: hash-map fold over input polygons, canonicalizing each vertex by exact `Point3` equality. Polygons that collapse to fewer than 3 distinct vertices after welding are dropped as degenerate (they were slivers from a CSG split that produced a near-coincident edge — the snap-to-grid round-trip occasionally produces these even when the source geometry was non-degenerate).

This pass exists primarily as the foundation for the other two — coplanar merging needs canonical vertex identity to detect shared edges, and T-junction repair needs canonical identity to detect collinear-vertex-on-edge. As a side benefit it shrinks the working representation by ~3× (each interior vertex was previously duplicated by every incident polygon).

Cost: O(V) where V is the input vertex count; the hashmap dominates.

### Pass 2: Coplanar polygon merging

Input: `IndexedMesh` from Pass 1.

Output: `IndexedMesh` with fewer, larger polygons.

Algorithm:

1. **Group by plane.** Build `HashMap<CanonicalPlane, Vec<PolygonId>>`. Two `Plane3`s are canonically equal iff they have the same `(n_x, n_y, n_z, d)` after normalizing the sign convention (e.g. flip if `n_x < 0`, or if `n_x == 0 && n_y < 0`, etc.). Polygons with opposite-facing planes are *not* merged — they are different surfaces of the same plane (think a double-sided card).

2. **Within each plane group, find connected components by shared edges.** Two indexed polygons share an edge iff they share two vertex IDs that are adjacent in both polygons' vertex lists (in opposite directions, since outward-facing polygons have opposite winding on a shared edge). Union-find over polygons keyed by shared-edge incidence.

3. **For each connected component, extract the boundary loop(s).** An edge appears in the boundary iff it appears in exactly one polygon of the component. Walk boundary edges to assemble closed loops. A component may produce multiple loops — one outer loop and zero or more inner loops representing holes.

4. **Re-triangulate each boundary as a polygon-with-holes.** For convex single-loop boundaries (the common case for most surfaces — cube faces, lathe caps), fan triangulation from any vertex suffices. For non-convex single loops, use ear clipping. For polygons with holes, bridge each hole to the outer loop via a shortest-segment edge, producing a single non-convex loop that ear clipping then handles. Ear clipping in 2D requires projecting the loop into the plane's dominant axis; the projection is exact because the plane's normal is integer.

The output replaces the input polygons of each component with the re-triangulated set, which is also expressed as `IndexedPolygon`s referencing the same vertex pool. Triangle count typically drops by 5–10× on CSG-heavy meshes.

Edge cases handled:

- **Multiple disconnected components on one plane.** Two separate slabs both having a top face at `z = 1.0` — they share a plane but not an edge, so they form two components and merge independently. Correct: a single top face would be a topology error.
- **Components with holes.** A square face minus a circular cut: the outer rectangle and the inner circle form a single connected component (connected via the cut's side faces? — no, those are on different planes; the cut leaves the top face with a *hole* in it, not a disconnected component). This is handled by the bridging step.
- **Genuinely non-planar adjacent regions.** Out of scope by definition — coplanar grouping only acts within a plane group, so a slightly-different-plane neighbor is left alone.

Edge cases NOT handled in v1:

- **Coplanar polygons with different colors.** v1 merges across color boundaries, picking the color of the first polygon in the component as the merged color. This means a CSG-cut where the cutter and the base share a face will have the base's color across the entire merged surface (the cutter's contribution gets absorbed). For most geometry this is invisible (the cut surface is one color); for the case where cut surfaces should keep distinct color, either author the colors to match or partition the input differently. A future v2 could refuse to merge across color boundaries — the connected-components step would split on color in addition to edge adjacency.

Cost: O(P log P) for the hashmap pass + O(P · k) for connected components where k is the average component size + O(B²) per component for ear clipping where B is the boundary loop length. For typical CSG output (P ~ 10²–10³, B ~ 10–50 per component) this is well within frame budget for a one-shot mesh authoring step.

### Pass 3: T-junction removal

Input: `IndexedMesh` from Pass 2.

Output: `IndexedMesh` with no T-junctions.

Algorithm: for each edge (V1, V2) appearing in any polygon, check whether any vertex V3 in the global vertex pool lies *strictly between* V1 and V2 on the edge's line segment. If so, subdivide every polygon containing the edge V1→V2: insert V3 between V1 and V2 in the polygon's vertex list, splitting the polygon into a (V1, V3) edge and a (V3, V2) edge.

The "lies strictly between" test is exact in fixed-point integers:

- Collinearity: `(V3 - V1) × (V2 - V1) == (0, 0, 0)` — the cross product of i64 differences fits in i128, comparison is exact.
- Between-ness: `0 < (V3 - V1) · (V2 - V1) < |V2 - V1|²`, all i128 arithmetic.

Detection is the expensive part. The naive algorithm is O(E · V) where E is the edge count and V is the vertex count. A spatial bucketing pass (group vertices by cell on a coarse grid, only test edges against vertices in cells the edge intersects) brings it down to roughly O(E · v) where v is the average vertices-per-cell, which for our mesh sizes is small. v1 ships the naive algorithm and accepts the cost; bucketing is a Phase 2 optimization if profiling shows cleanup time dominating mesh authoring.

Subdivisions can introduce new T-junctions (the inserted V3 may itself create a junction against another edge), so the pass is run to fixed point — repeat until no junctions are detected. Termination is guaranteed because each iteration strictly decreases the count of `(edge, intermediate vertex)` violation pairs.

### Triangulation to wire format

After the three cleanup passes, each merged polygon is triangulated into the `Vec<Triangle>` wire format using the same ear-clipping path that Pass 2 used internally. Triangles inherit the polygon's color; vertex `f32` coordinates come from the standard fixed-to-float conversion (`coord_i32 / 2^16`).

### Module layout

Three sibling modules under `aether_dsl_mesh::csg`:

- `cleanup::weld` — the indexed-mesh conversion (Pass 1)
- `cleanup::merge` — coplanar merging (Pass 2)
- `cleanup::tjunctions` — T-junction repair (Pass 3)

A single entry point `cleanup::run(polygons: Vec<Polygon>) -> Vec<Polygon>` chains them. The `csg::ops::{union, intersection, difference}` functions call `cleanup::run` on their output before returning, so callers see cleaned polygons unconditionally.

The existing `IndexedMesh` shape lives in `cleanup::mesh`; it is not exposed outside `csg::cleanup` because callers should not need to know about the intermediate representation.

## Consequences

### Positive

- **Wireframe readability dramatically improves.** The slab top of the anvil goes from ~40 visible triangles to ~4 (one quad with two cut-corners triangulated). The body of a CSG-merged anvil drops from ~300 wireframe edges to ~40. This is the dominant visual quality win for the current era (wireframes-always, no lighting).
- **Triangle count drops, vertex buffer cap pressure relaxes.** Typical reduction is 5–10× on CSG-heavy meshes. The 64KB vertex buffer cap that PR 269 bumped becomes substantially less pressing.
- **T-junctions are gone before lighting arrives.** When shading lands, we don't need a retrofit pass to remove hairline cracks — the cleanup pipeline already produces watertight, junction-free meshes.
- **Foundation for future mesh tools.** Indexed-mesh form, coplanar grouping, and T-junction detection are also load-bearing for chamfer / fillet / decimation operators (the ADR-0055-follow-on tools). Building them now amortizes that infrastructure cost.
- **Exact arithmetic throughout.** The cleanup passes inherit the same determinism guarantees as the CSG core — bit-exact reproducible output across platforms and runs. No new sources of float ambiguity.

### Negative

- **Implementation cost.** Estimated 600–900 lines including tests across the three passes. Coplanar merging is the largest chunk (boundary extraction + ear clipping with hole bridging). T-junction repair is the most subtle (the fixed-point predicates are easy; the iteration-to-fixed-point and the spatial-bucketing escape hatch take care).
- **Cleanup adds a per-mesh-authoring cost.** For the chunky-low-poly target this is one-shot at DSL evaluation time, not per-frame, so the absolute cost is fine — but mesh editor hot-reload latency goes up. Estimated impact: <50ms for anvil-class meshes.
- **Color information is lossy across merge boundaries.** The "first polygon wins" rule is a real downgrade from CSG's per-triangle color provenance. v1 tolerates this; if it bites, the v2 fix is to refuse to merge across color boundaries.
- **Coplanar-but-different-plane-orientation polygons stay separate.** Two polygons facing opposite directions in the same plane don't merge. This is correct (they're different surfaces) but means a thin double-sided shell will show twice the polygons of a one-sided cut. No real geometry hits this case in current usage.

### Neutral

- **The intermediate `IndexedMesh` representation is private.** No public API churn — callers see `Vec<Polygon>` in and `Vec<Polygon>` out.
- **Cleanup is not optional.** A user who wants the raw fragmented CSG output for debugging won't get it via the DSL. Debugging visualizations of intermediate stages can be added if needed.
- **The pipeline is still pure CPU, single-threaded.** Same execution model as CSG — fits the same actor-per-component thread.

## Alternatives considered

- **Skip merging, only fix T-junctions.** Rejected because wireframe noise is the immediate visual problem; T-junctions don't even render visibly without lighting. Doing the easier pass first and skipping the higher-impact one inverts the priority.
- **Skip T-junctions, only do welding + merging.** Tempting (would ship faster, T-junctions don't hurt without lighting). Rejected because the indexed-mesh + coplanar-grouping infrastructure is most of the cost; T-junction detection adds <150 lines on top, and shipping it now means the cleanup pipeline produces lighting-ready meshes the day lighting lands.
- **Quadric error decimation (Garland-Heckbert).** General-purpose triangle reduction — minimizes vertex count subject to a quadric error metric. Rejected: it's lossy by design (approximates the surface), would erode the exact-CSG topology we just paid for, and is overkill for our use case where we want to *recover* the original planar structure not approximate it.
- **Half-edge mesh data structure.** A "proper" mesh representation with O(1) traversal of edge neighbors. Rejected for v1: indexed-polygon form is sufficient for the three passes, half-edge would add ~400 lines of mesh-management code we don't need yet. Half-edge is a candidate refactor if a future operator (specifically fillet) needs more than O(E·k) neighbor traversal.
- **Run cleanup as a separate `(cleanup ...)` DSL operator.** Make it an explicit authoring step. Rejected: there is no use case for *not* cleaning up CSG output; making it explicit just gives users a footgun for forgetting it. Cleanup is a property of the CSG output, not a separate composable operation.
- **Cleanup at triangulation time only (no intermediate polygon merging).** Triangulate each polygon group into a single fan, skipping the connected-components / boundary-extraction steps. Rejected: two adjacent rectangles on the same plane would still produce two fans with a shared edge between them, defeating the point. The merging matters — the triangulation is just the output format.

## Follow-up work

- **Chamfer operator (ADR-0056 candidate).** Now that we have indexed-mesh / shared-edge infrastructure, an edge-chamfer operator (offset adjacent face normals along an edge by a small amount) becomes tractable. Order-of-magnitude effort: ~500 lines on top of cleanup.
- **Fillet operator (deferred).** Rolling-ball or offset-surface fillets are research-grade and not gated on this ADR. Mentioned only because cleanup is on the fillet implementation path.
- **Spatial bucketing for T-junction detection.** Phase 2 optimization if mesh authoring latency profiles hot here.
- **Color-aware merging.** Phase 2 if color loss across merge boundaries becomes a visible problem in practice.
- **Cleanup metrics in `engine_logs`.** Triangle-count-before / triangle-count-after per CSG operation, so `engine_logs --level debug` shows the cleanup ratio and surfaces regressions if a future change breaks merging.
