# ADR-0057: N-gon polygons are the canonical mesh form

- **Status:** Proposed
- **Date:** 2026-04-26
- **Amends:** ADR-0055, ADR-0056

## Context

ADR-0055 shipped the post-CSG cleanup pipeline (vertex welding → coplanar polygon merging → triangulation → T-junction removal) and ADR-0056 replaced the triangulation step with constrained Delaunay triangulation. The triangulation it produces is correct, deterministic, sliver-free in the bulk, and approaches the topological minimum triangle count for any individual face. By the standards of "is this a valid triangle mesh of the CSG output," the pipeline is solved.

By the standards of "does this look like the output of a DCC tool," it doesn't. Two visible classes of artifact remain after the live smoke tests on cube + sphere CSG:

- **Cocircular fans.** A polygonal cylinder or sphere cut creates many cocircular vertices on its boundary. The CDT picks Delaunay-optimal diagonals, but several adjacent triangles are coplanar and share a common vertex — visually a fan of triangles where one polygon would suffice. The CDT can't merge them because its output type is triangles.
- **Visible interior diagonals.** A merged coplanar region (the top of a cube with a hole bored through it, the side wall of a multi-cut box) is a single logical face, but the wireframe shows every triangle diagonal inside it. The triangulator picked those diagonals; they have no semantic meaning, but the user sees them.

Both artifacts are downstream symptoms of the same decision: the cleanup pipeline triangulates at its output stage, so the "logical face" concept dies the moment cleanup ends. Every layer downstream — T-junction repair, the renderer, the wireframe — works with triangles and has no record that those diagonals are *interior* to one face vs *sharp* between two.

The forcing function is the engine's stated direction as a DCC-class authoring environment. Every CAD kernel (Parasolid, ACIS, OpenCASCADE) and every modeling DCC (Blender, Maya, 3ds Max, Houdini) stores n-gon polygonal faces as the canonical form: one face per maximal planar region (and in CAD, per smooth surface patch), with edges classified as feature (between two faces) or interior. Triangulation is a viewport / export concern, regenerated on demand and never persisted. The hexagonal cap of a cylinder cut is *one hexagon*, not a fan of four triangles. We are the odd one out in storing triangles as the source of truth.

The deeper consequence is that polygon-domain operations — bevels, smooth/sharp edge separation, lighting normal seams, subdivision surfaces, quad remeshing — aren't expressible in the current pipeline at all. Triangle-domain meshes can simulate them with face-group annotations, but the bookkeeping is invasive and gets every operation wrong by default. Moving to polygons up front avoids the simulation entirely.

## Decision

The canonical mesh representation in `aether-dsl-mesh` becomes a list of polygonal faces. CSG and the cleanup pipeline operate end-to-end in the polygon domain. Triangulation moves from "step in the cleanup pipeline" to "operation performed at GPU upload time" inside the consumer (today: the mesh editor component).

### Polygon as the canonical face

```rust
pub struct Polygon {
    pub vertices: Vec<VertexId>, // CCW around the plane normal, ≥ 3 verts
    pub plane: Plane3,
    pub color: u32,
}

pub struct Mesh {
    pub vertices: Vec<Point3>,
    pub polygons: Vec<Polygon>,
}
```

A polygon may be convex or non-convex, and may be self-bounding or accompanied by hole loops. The hole representation is a follow-on detail (see "Open: holes representation" below); the v1 carrier is "polygon with no holes" — coplanar merging that produces an annular region emits the outer boundary plus the hole boundaries as separate `Polygon` records sharing the same plane and color, and the display tessellator groups them by plane+color before triangulating.

### Pipeline shape after this ADR

```
DSL primitives → CSG → cleanup (weld → coplanar merge → tjunction) → Vec<Polygon>
                                                                          ↓
                                                       editor component caches
                                                                          ↓
                                                       cdt::triangulate at emit time
                                                                          ↓
                                                                Vec<DrawTriangle> mail
```

The CDT module shipped in ADR-0056 keeps doing what it does. Its consumer changes from `cleanup::process_component` to the editor component's emit path. Coplanar merging produces non-convex polygons-with-holes; CDT is the right algorithm for tessellating those for display, regardless of whether it runs in the cleanup pipeline or at the GPU upload boundary.

### Wireframe is polygon-edge

The wireframe view shows polygon edges only — never display-tessellation diagonals. This is the visible promise of this ADR; without it, the cocircular fans and interior diagonals stay on screen even though the canonical form is clean.

Concretely: the editor component (which today emits one `aether.draw_triangle` per triangle in the cached mesh) gains a parallel emit path for line geometry — one line segment per polygon edge — drawn over the filled triangles. The substrate-side `render` sink learns a `aether.draw_line { v0, v1, color }` mail kind to consume them. The triangulation diagonals from the display tessellator never get emitted as lines, so they never reach the wireframe.

This is the "DCC mode" wireframe and the only wireframe we ship. A debug "tessellator wireframe" mode that visualizes diagonals is a separate concern, deferable until needed.

### CSG operates on polygons

BSP CSG (ADR-0054) splits triangles against planes. After this ADR it splits polygons against planes — Sutherland-Hodgman polygon clipping is the canonical routine and has the same per-vertex sign test the current triangle splitter uses. Polygon-vs-plane split is *easier* than triangle-vs-plane on the algorithmic side: a triangle straddling a plane can produce one or two output triangles depending on which vertices lie on which side, with three special cases; a polygon straddling a plane produces exactly two output polygons (front and back) with no case analysis on the input vertex count.

The BSP tree's invariants don't change. The exact-predicate plane test (sign of plane equation evaluated at a vertex, integer arithmetic per ADR-0054) is reused unchanged. Determinism, robustness, and the fixed-point coordinate budget all carry over.

### DSL primitives emit polygons natively where it's natural

- `cube` → 6 quads (one per face).
- `cylinder` → 2 n-gon caps + n quads on the side wall.
- `cone` → 1 n-gon base + n triangles on the side.
- `wedge` → 2 triangles + 3 quads.
- `extrude`, `mirror`, `array` → composition of the above.
- `sweep` → segment-by-segment quads where the cross-section is a polygon, triangle-strips where it's a curve.

`sphere` and `lathe` with curved profiles are intrinsically triangulated (the lat/lon segments don't decompose cleanly into quads at the poles, and arbitrary lathe profiles likewise). They emit polygons-as-triangles — `Polygon { vertices: [v0, v1, v2], ... }` records — and the cleanup pipeline's coplanar merge collapses them into larger polygons whenever vertices happen to be coplanar (which they generally are not for sphere/lathe surfaces). This is fine: sphere and lathe surfaces are *meant* to look like a triangle approximation of a curve, and the resulting triangle count matches what a DCC would store for a UV sphere.

### Per-polygon color

Color moves from a triangle attribute to a polygon attribute. This matches DCC and CAD conventions and follows naturally from coplanar merge (a merged region is one color by construction — adjacent triangles with different colors don't get merged into the same polygon).

The `aether.draw_triangle` mail kind keeps its per-triangle color field — the wire format is unchanged — but the editor component fills it from the source polygon's color when it tessellates.

### Module layout impact

- `aether_dsl_mesh::mesh::Mesh` switches its source-of-truth field from `triangles: Vec<Triangle>` to `polygons: Vec<Polygon>`.
- `aether_dsl_mesh::csg` operates on polygons throughout. Public API surface returns polygons.
- `aether_dsl_mesh::cleanup::process_component` returns `Vec<Polygon>` instead of `Vec<Triangle>`. The CDT call inside it is removed.
- `aether_dsl_mesh::cleanup::cdt` (the module we just shipped) is unchanged in implementation but re-exports `triangulate_loops` at a public-from-the-crate visibility so the editor component can call it.
- `aether-mesh-editor-component` caches `Vec<Polygon>` and tessellates at emit time, sending `DrawTriangle` for filled geometry and `DrawLine` for wireframe edges.

### What does not change

- The substrate-side `render` sink keeps consuming `DrawTriangle`. It gains a parallel path for `DrawLine` but the triangle path is byte-identical.
- Mail wire formats (`aether.draw_triangle`) unchanged.
- BSP CSG's exact-predicate plane test, the integer fixed-point coordinate system (ADR-0054), and the cleanup pipeline's vertex welding and T-junction repair logic all unchanged in algorithm — they get ported from operating on triangles to operating on polygons, but the math is the same.
- The DSL surface (s-expression vocabulary from ADR-0026 / ADR-0051) is unchanged. Authors don't see this distinction.
- The CDT module's exact-predicate machinery stays. Overkill for display tessellation but harmless, and removing it later is a clean follow-on if proven unnecessary.

## Consequences

### Positive

- **Visible "DCC-class" output.** Wireframe shows polygon edges only — the cocircular fans and interior diagonals that motivated this ADR vanish from the user's view. The hexagonal sphere cut is one hexagon outline, not four.
- **Foundation for future DCC features.** Bevels, edge attribute classification (sharp/smooth), lighting normal seams, subdivision surfaces, quad-dominant remeshing — all of these are polygon-domain operations that aren't expressible without polygon-domain storage. This ADR unblocks the entire class.
- **CSG codepath is simpler.** Sutherland-Hodgman polygon-vs-plane clipping has fewer cases than the current triangle-vs-plane splitter. The BSP tree shape and invariants are unchanged but the per-node split logic is cleaner.
- **T-junction repair is cleaner in the polygon domain.** Polygon edges are exactly the boundary set; no diagonal interleaved that has to be reasoned about separately.
- **Per-polygon color naturally matches the storage shape.** Merged regions are one color; we stop carrying the same color around on every triangle of a face.

### Negative

- **Significant rewrite touching every layer of the mesh pipeline.** Five PRs after this ADR: polygon type + cleanup output, BSP polygon clipping, DSL primitives emit polygons, polygon-edge wireframe (mail kind + renderer + editor), and the migration cleanup. Each one is small to medium individually; the aggregate is on the order of 1500-2500 LOC across the crate.
- **Triangulating at emit time per-frame is wasteful if the mesh hasn't changed.** Mitigation: the editor component caches the tessellation output alongside the polygon source, invalidating the cache only when the polygon list changes. The current editor component already caches the triangle list; this just changes what's cached and when it's regenerated.
- **CDT's exact-predicate machinery is overkill for display tessellation.** Display diagonals don't affect mesh identity, so float arithmetic would be fine. Not addressed in this ADR; the existing exact-predicate path is reused as-is. Replacing it with a simpler tessellator is a clean follow-on if it ever shows up in profiles.
- **Polygon-edge wireframe needs new mail wire (`aether.draw_line`).** Not a backward-incompatible change, but it's a new kind to define and a new render path to wire up. Worth its own PR.

### Neutral

- **DSL surface unchanged.** The s-expression vocabulary, the primitive set, and the operator semantics are all unchanged. Authors don't see this transition.
- **Substrate-side `render` sink is additive only.** It gains `DrawLine` consumption; existing `DrawTriangle` flow is unchanged.
- **CDT we just shipped is repurposed, not thrown away.** The exact predicates and the constraint enforcement logic become the display tessellator. The investment carries forward.
- **Determinism guarantees carry over unchanged.** Polygon order is deterministic (CSG and cleanup walk vertices in deterministic order); display tessellation is deterministic (CDT is). Bit-exact reproducibility across platforms holds.

## Alternatives considered

- **Cocircular-fan merge as a triangle-domain post-pass.** Treats one symptom (the fan artifact) without addressing the underlying triangulate-too-early problem. Doesn't enable any of the future DCC features listed under Positive. Rejected as a band-aid that defers the real move.
- **Half-edge / DCEL representation.** The standard topology data structure in DCC kernels — supports queries like "which polygon shares this edge?" and "walk the loop of edges around this vertex" in constant time. Rejected for v1 because the simple `Vec<Polygon>` is sufficient for CSG, cleanup, and display tessellation; topology queries aren't hot in the current pipeline. Easy to migrate to half-edge later if a concrete operation needs it (e.g., interactive bevels).
- **Keep triangles as canonical, add face-group metadata.** Each triangle carries a "face id" annotation; the wireframe renderer suppresses interior edges. Rejected because the bookkeeping pollutes every operation that touches a triangle, and polygon-domain operations like Sutherland-Hodgman or future bevels aren't expressible — you'd be reconstructing the polygon from the triangle group on every operation, which is a worse version of just storing the polygon.
- **Defer until a forcing function shows up.** The forcing function is the visible artifact (cocircular fans, interior diagonals) plus the engine's stated direction toward DCC-class authoring. Both are present. Deferring would mean shipping more triangle-domain code that has to be migrated later.
- **Use floating-point CDT for display tessellation, keep integer CDT for cleanup.** Would let us drop the exact predicates from the display path. Rejected as out of scope for this ADR — moving the consumer is the load-bearing change; replacing CDT internals is a separable optimization. The exact-predicate display tessellator runs sub-millisecond on the meshes we've been testing; no measured pressure to swap it.

## Open questions

These are deliberately not decided in this ADR. They are settled by the implementation PRs or by follow-on ADRs as they come up.

- **Holes representation.** v1 emits annular faces as separate `Polygon` records (outer + hole) sharing plane+color, and the display tessellator groups them. A future representation could give `Polygon` an explicit `holes: Vec<Vec<VertexId>>` field. The latter is closer to what DCC tools store and avoids the group-by-plane-color reconstruction at tessellation time, but the former is enough for v1 and doesn't change the tessellator's input shape.
- **Sphere primitive in n-gon form.** Currently emits triangles; could emit quad-body + triangle-pole-strip. Defers to a future ADR if/when sphere appearance becomes a forcing function.
- **Half-edge migration.** If/when interactive editing operations need fast topology queries.
- **Replacing the CDT display tessellator with floats.** If/when display tessellation shows up in profiles.

## Follow-up work

Implementation in five PRs against this ADR:

1. **`Polygon` type + cleanup pipeline outputs `Vec<Polygon>`.** Editor component triangulates at emit time using the existing CDT module. CSG and DSL still emit triangles, converted to polygons at the coplanar merge boundary (which already produces them internally). The smallest change that lets the polygon-domain output be visible end-to-end.
2. **BSP CSG operates on polygons.** Sutherland-Hodgman polygon-vs-plane clipping replaces the triangle-vs-plane splitter. BSP tree shape unchanged.
3. **DSL primitives emit polygons natively.** Cube, cylinder, cone, wedge get n-gon emission. Sphere and lathe-with-curved-profiles keep emitting triangles (as `Polygon { vertices: [v0, v1, v2] }`).
4. **Polygon-edge wireframe.** New `aether.draw_line` mail kind, substrate-side render path, editor component emits one line per polygon edge in addition to the tessellated triangles.
5. **Cleanup pipeline migration.** T-junction repair operates on polygon edges; coplanar merge stops triangulating internally; remove dead triangle-domain helpers.
