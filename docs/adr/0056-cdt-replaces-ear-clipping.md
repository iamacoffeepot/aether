# ADR-0056: Constrained Delaunay triangulation replaces ear-clipping

- **Status:** Proposed
- **Date:** 2026-04-26
- **Amends:** ADR-0055

## Context

ADR-0055 shipped a three-pass cleanup pipeline (vertex welding → coplanar polygon merging → T-junction removal). The middle pass uses ear-clipping with hole-bridging: for a face-with-holes, the outer loop and each inner hole are spliced into a single non-simple "slit" polygon by duplicating the bridge endpoints, then ear-clipping triangulates the slit polygon.

A live smoke test on `(difference box box)` (a box with a rectangular hole) made the limitation obvious. The pierced top face emits roughly the topological minimum number of triangles (~8 for a rectangle-with-rectangular-hole), but a few of them are visible **slivers** clustered around the bridge endpoint — long thin triangles with one nearly-zero edge. They are not topology errors (they cover the right area, with the right winding) but they are degenerate-by-construction:

- The bridge duplicates a vertex, producing two coincident edges in the spliced loop.
- Ear-clipping cannot pick the "ear" containing both duplicates because its cross product is zero (degenerate triangle, fails the convexity check).
- The slivers fall out of the algorithm needing to walk *around* the duplicate.

A best-ear heuristic (pick the most-equilateral ear among the valid candidates) and a Lawson edge-flipping post-pass were prototyped on a side branch. They cleaned up the long-fan slivers that appeared elsewhere in the merged region, but the slit slivers themselves stayed — the convexity check that gates an edge flip fails on the quad containing the duplicated bridge endpoints, so Lawson cannot reach the slit. The prototype confirmed: as long as the algorithm goes *through* a slit, the slit slivers are intrinsic.

The forcing function for moving past this is the future of mesh editing in the engine. Any operation that subdivides, smooths, deforms, or applies offsets to mesh vertices will amplify slivers — a near-zero-edge triangle becomes degenerate or self-intersecting under almost any vertex perturbation. We are accumulating fragile geometry. Even before mesh editing, smooth shading would make slivers visibly bad (lighting interpolation across a thin triangle produces shading discontinuities).

The canonical algorithm class that triangulates a polygon-with-holes *without* a slit is **Constrained Delaunay Triangulation** (CDT). Rather than splicing the holes into the outer loop, CDT computes the Delaunay triangulation of the union of all loop vertices and then *enforces* the boundary edges as constraints — never producing a triangle that crosses a constraint. Triangles outside the outer loop (or inside a hole) are then discarded. The output is sliver-free in the sense that it locally maximizes the minimum angle subject to the boundary constraints.

## Decision

Replace the ear-clipping path in `aether_dsl_mesh::csg::cleanup::merge` with an incremental Constrained Delaunay Triangulation. The cleanup pipeline's *shape* doesn't change — vertex welding still feeds in, coplanar grouping still produces components, T-junction repair still runs after. Only the triangulation step inside `process_component` swaps.

### Algorithm: incremental Bowyer-Watson + constraint enforcement

Three sub-steps inside the new `cdt::triangulate(loops, plane)`:

1. **Project all loop vertices to 2D** using the existing `projection_axes` helper.

2. **Insert vertices one at a time, Bowyer-Watson**:
   - Start with a "super-triangle" enclosing all loop vertices (computed from the bounding box of the projected coords, expanded enough that no real vertex lies on or near its edges).
   - For each loop vertex, find the triangles whose circumcircle contains it. The union of these triangles forms a "cavity"; remove them and re-triangulate the cavity by connecting the inserted vertex to each cavity boundary edge.
   - The in-circle test for "is point P inside the circumcircle of triangle ABC?" is the central predicate — it must be *exact*, not float, to guarantee determinism and avoid topology bugs. See "Numerical robustness" below.

3. **Enforce constraint edges**: for each boundary edge in the input loops that is not already in the Delaunay triangulation, find the chain of triangles the constraint edge crosses and flip diagonals along the chain until the constraint edge appears. This is the standard "edge insertion" algorithm (Anglada 1997).

4. **Mark and discard**: use a flood-fill from the super-triangle outward to mark triangles outside the polygon (outside the outer loop, or inside any hole). Discard the marked triangles. Discard the super-triangle vertices and the triangles that referenced them.

The output is `Vec<[VertexId; 3]>` — same wire shape as ear-clipping produced. `process_component` then wraps each into an `IndexedPolygon` with the shared plane and color, exactly as before.

### Numerical robustness

The in-circle test is the determinant of a 4×4 matrix (lifted to 3D via the parabolic lifting trick), or equivalently a 3×3 determinant after subtracting D from the others:

```
| Ax-Dx  Ay-Dy  (Ax-Dx)²+(Ay-Dy)² |
| Bx-Dx  By-Dy  (Bx-Dx)²+(By-Dy)² |  >  0  iff D is inside circumcircle of (A, B, C)
| Cx-Dx  Cy-Dy  (Cx-Dx)²+(Cy-Dy)² |
```

Magnitude budget at our coord cap (±2^24 fixed-point units, per ADR-0054):

- Linear differences: i32, ≤ 2^25.
- Squared sum entries (third column): ≤ 2^51.
- 2×2 sub-determinants: ≤ 2^25 · 2^51 + 2^25 · 2^51 = 2^77.
- Outer product (third-column entry × 2×2 det): ≤ 2^51 · 2^77 = 2^128.

That's exactly i128's signed range (`±2^127`). The worst case overflows by one bit. We therefore need **i256-equivalent arithmetic** for the in-circle predicate.

We hand-roll a minimal i256 helper (`csg::cleanup::cdt::wide`) — two `i128` limbs (high + low), with `mul_i128_i128 -> i256`, `add_i256_i256 -> i256`, and a `signum() -> i32`. Roughly 80–120 lines including tests. The only consumer is the in-circle predicate; we don't need a general-purpose i256 type. This matches the pattern of `aether-math` (hand-rolled), the BSP CSG fixed-point core (hand-rolled), and avoids pulling in `ethnum`, `primitive-types`, or similar wide-int crates for one predicate.

The orientation predicate (sign of `(B-A)×(C-A)` in 2D) stays as i128 — it doesn't approach the overflow boundary at our coord range.

### Module layout

A new module under `csg::cleanup`:

- `cleanup::cdt::wide` — hand-rolled i256 add / mul / signum.
- `cleanup::cdt::predicates` — `in_circle(a, b, c, d) -> Sign` and `orient2d(a, b, c) -> Sign`.
- `cleanup::cdt::triangulate` — the public entry point, taking projected loops + a plane and returning `Vec<[VertexId; 3]>`.

The bridging code (`splice_hole_into_outer`), the two ear-clipping entry points (`ear_clip_loop`, `ear_clip_with_holes`, `ear_clip_2d`), and the supporting `point_in_triangle` are removed from `merge.rs`. `projection_axes`, `project`, `signed_area2_2d`, `cross2d`, and `squared_dist` stay — they're shared infrastructure CDT also uses.

### Determinism

Same guarantee as ADR-0054: bit-exact reproducibility across platforms, runs, and threads. Concretely:

- Vertex insertion order is deterministic (sorted by `VertexId` before insertion).
- Bowyer-Watson cavity construction picks triangles in a deterministic order (sorted by triangle id within the cavity-finding flood fill).
- Edge-flip choice during constraint enforcement walks the constraint-crossing chain in deterministic order (sorted by the entry edge's canonical form).
- The in-circle and orientation predicates are exact integer arithmetic; no float anywhere in the predicate path.

## Consequences

### Positive

- **Sliver-free output.** The slit-induced slivers go away because there is no slit. Triangles approach Delaunay-quality (locally max-min-angle) subject to the boundary constraints. Visible immediately in wireframe; load-bearing for any future mesh editing.
- **Clean handling of single-loop and multi-loop alike.** CDT doesn't care whether the polygon has holes — it's the same algorithm, just with more constraint edges. The `loops.len() == 1` vs `loops.len() > 1` branch in `process_component` collapses.
- **Sets up smooth shading.** When per-vertex normals or interpolated lighting eventually land, the triangulation is already lighting-quality; no retrofit needed.
- **Fewer triangles in degenerate cases.** Some cases ear-clipping handled with extra triangles (non-convex outer forced fan-shaped output) collapse cleanly under CDT.

### Negative

- **Implementation cost.** ~800–1200 lines of Rust including tests and the i256 helper. CDT is a well-known algorithm and the Bowyer-Watson + constraint enforcement formulation is textbook, but it is meaningfully more code than ear-clipping.
- **More complex code path to debug.** Bowyer-Watson cavity construction has well-known subtleties (degenerate cocircular cases, robustness near the super-triangle boundary). Mitigation: the exact i256 in-circle predicate eliminates the float-precision class of failures; we still have to test the topology code carefully.
- **Slightly higher per-merge cost.** CDT is `O(n log n)` for incremental Delaunay, vs ear-clipping's `O(n²)` per ear (best case for both). For our typical mesh sizes (n ≤ 50 per merged region), the wall-clock difference is negligible — sub-millisecond either way. No expected impact on mesh authoring latency.
- **i256 helper is new code with new failure modes.** Mitigation: tested in isolation against direct i128 reference for cases that fit, and against known signed determinant signs for cases that don't.

### Neutral

- **Wire format unchanged.** `Vec<Triangle>` in, `Vec<Triangle>` out — same as before.
- **Cleanup pipeline shape unchanged.** Welding → coplanar grouping → CDT → T-junction repair, in that order. The other two passes don't change.
- **Best-ear heuristic and Lawson flipping aren't shipped.** Both were prototyped and proved insufficient against the slit problem; CDT obviates them entirely.

## Alternatives considered

- **Ship best-ear + Lawson flipping (the prototype).** Real improvement on the long-fan slivers, but doesn't reach the slit slivers (the convexity check on flips fails at the duplicated bridge endpoints). Rejected as a half-measure: leaves slivers in the output, which compound under any future mesh editing operation. The prototype is documented in this ADR and can be re-derived if CDT is ever rolled back; we don't ship interim improvements that get replaced wholesale.
- **Sweep-line CDT (Domiter & Žalik 2008).** Single-pass algorithm, mathematically elegant. Rejected because it is harder to implement correctly than incremental Bowyer-Watson and the per-merge cost difference is irrelevant at our mesh sizes.
- **Vendor a CDT crate (`spade`, `delaunator`).** `delaunator` is unconstrained Delaunay only; `spade` is constrained Delaunay but heavy and depends on `num-traits` family. Rejected for the same reasons we hand-rolled BSP CSG and `aether-math`: vendoring a triangulation library to fix one predicate is out of proportion, and the algorithm fits our integer-exact predicate convention better when written in-tree.
- **Use floating-point in-circle predicates with adaptive precision (Shewchuk).** Standard solution in computational geometry — try `f64`, fall back to extended precision when the result is close to zero. Rejected because the i256 alternative is simpler in our codebase: we already have an integer-exact predicate culture from BSP CSG, the implementation is mechanical, and we avoid introducing float into a predicate path that decides topology.
- **Coord-scale-down to fit i128.** Halve the fixed-point precision (16:16 → 16:15, or cap coords at 2^23 instead of 2^24). Rejected because ADR-0054's ±256 unit cap is already a tight budget and halving it would make multi-asset compositions (e.g., a castle made of 50-unit chunks) bump against the limit. Better to pay the i256 implementation cost once.

## Follow-up work

Implementation in three PRs against this ADR, mirroring the ADR-0055 cascade:

1. **i256 helper + exact predicates.** `cleanup::cdt::wide` with `mul_i128_i128`, `add_i256_i256`, `signum`, plus tests. `cleanup::cdt::predicates::{in_circle, orient2d}`. No integration yet — verifies the math in isolation.
2. **Bowyer-Watson incremental Delaunay.** Build the unconstrained triangulation of all loop vertices including the super-triangle setup. Tested against simple known cases (regular polygons, random points).
3. **Constraint enforcement + inside/outside marking + wire into `process_component`.** This is where CDT actually replaces ear-clipping; old ear-clipping and bridging code is removed in this PR. Tests cover the previously-bridged cases (annular, anvil-class slab-with-holes) and confirm the sliver-free property.
