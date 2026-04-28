# ADR-0054: CSG operators for the DSL mesh

- **Status:** Superseded by ADR-0062
- **Date:** 2026-04-26
- **Amends:** ADR-0026, ADR-0051

## Context

ADR-0026 committed the engine to a primitive-composition DSL as the only mesh authoring path. It explicitly **deferred CSG boolean operators** to v2 vocabulary, with the reasoning that overlap-by-transform handles the common case for the chunky-low-poly aesthetic, CSG implementation cost is non-trivial, and demand wasn't established yet.

That demand has now shown up. Authoring a chess rook from a reference image surfaced a class of geometry the current vocabulary cannot express cleanly: a continuous solid with material removed in regions. The rook's crenellations are the canonical example — four notches cut into the rim of a tower. Without subtraction:

- Modeling them as four separate boxes glued onto the rim produces visible seams, mismatched geometry between the box faces and the cylindrical rim, and "stuck-on" reading rather than "carved-from-solid."
- Modeling them via lathe profile bumps merges the slot floors with the rim but cannot produce the angular variation (a lathe is rotationally symmetric).
- Modeling them via a partial-revolution lathe (an alternative "v3 vocabulary" addition) would work for this specific 4-fold-symmetric case but doesn't generalize. The next case — a window in a wall, a hole through a teapot spout, notches in a key — gets no help.

Boolean subtraction is the general operation. The rook is the forcing function the original ADR was waiting for.

## Decision

Add three CSG operators to the DSL grammar, implemented as a BSP-CSG mesher inside the existing `aether-dsl-mesh` crate.

### Grammar additions

- `(union child1 child2 ...)` — N-ary; result is the union of all children's solid regions. Geometric coincidence at boundaries is resolved without producing internal duplicate faces.
- `(intersection child1 child2 ...)` — N-ary; result is the intersection of all children's solid regions. Empty result is a valid empty mesh, not an error.
- `(difference base subtract1 subtract2 ...)` — first child minus the union of all subsequent children. At least two children required; one-child `(difference x)` is a parse error.

Each operator takes structural children (any DSL node — primitives or further structural nodes), so CSG composes with `translate`, `rotate`, `scale`, `mirror`, `array`, `composition`, and itself.

Color is inherited from the contributing input mesh: a triangle in the output came from one input mesh and carries that mesh's color. Faces produced from cuts adopt the color of the mesh whose face was clipped. (Per-operator color override is *not* part of v1 — composition with `:color`-bearing children is sufficient.)

### Algorithm: BSP CSG

The implementation uses the BSP-tree CSG algorithm (Thibault & Naylor, 1980; popularized by csg.js). Each input mesh's polygons are organized into a binary space partition tree where each node is a supporting plane. Boolean operations are expressed as tree-against-tree clipping:

- `union(A, B)`: `A.clip(B); B.clip(A); B.invert(); B.clip(A); B.invert(); A.merge(B)`
- `intersection(A, B) = invert(union(invert(A), invert(B)))`
- `difference(A, B) = invert(union(invert(A), B))`

The classical recursion for clipping a polygon against a tree splits it on each plane it crosses, retaining or discarding each fragment based on the operation. N-ary operators reduce left-to-right: `union(A, B, C, D) = union(union(union(A, B), C), D)`.

### Module layout

A new module `aether_dsl_mesh::csg` holds the BSP tree, polygon clipping, classification, and the three operation implementations. The `Node::Union`, `Node::Intersection`, `Node::Difference` AST nodes are added to `ast.rs`. The mesher in `mesh.rs` recursively meshes children to triangle lists, hands them to `csg::union` / `intersection` / `difference`, and writes the output to the `Vec<Triangle>`.

**No new crate.** The DSL is the only consumer in v1; if a future use case (e.g. a static-mesh component doing runtime booleans on imported geometry) needs the algorithm standalone, it extracts to `aether-csg` then. Splitting now is YAGNI.

### Numerical robustness

Robustness is normative, not best-effort. Three commitments:

- **Internal fixed-point coordinates with exact integer predicates.** On entry to the CSG core, all input vertex coordinates are snapped to a 16:16 binary fixed-point grid (multiply by `2^16`, round to nearest, store as `i32`). All predicates inside the CSG core ("which side of plane Q is point P on?", "are vertices A and B identical?", "do edges AB and CD intersect?") run as exact integer determinants — `i32 × i32` multiplications fit in `i64` intermediates, no floating-point arithmetic is involved in any classification or topology decision. This eliminates the predicate-ambiguity failure mode at its root rather than papering over it with epsilon thresholds or adaptive-precision arithmetic. On exit from CSG, vertices convert back to `f32` for the wire (divide by `2^16` in `f64`, cast to `f32`). **The wire format `Vertex` is unchanged** — fixed-point is an internal CSG implementation detail, not an architectural change to the engine's coordinate system.
- **Deterministic execution.** The CSG core MUST be single-threaded and produce identical output for identical inputs across platforms, wasm runtimes, and threads. Polygon ordering inside the BSP build sorts by a stable derived ID (e.g., FNV1a of the polygon's plane equation + first vertex), not by `Vec` insertion order. No platform-dependent float intrinsics. Combined with the integer arithmetic above, the CSG output is bit-exactly reproducible across platforms — golden-file snapshots become structurally meaningful, not best-effort.
- **SoS-style tie-breaking.** When a vertex's integer classification against a plane is exactly zero (unavoidable in cases like a vertex shared between two coplanar input meshes), it MUST be assigned to a side via lexicographic tie-breaking on a stable vertex identifier, consistently across the entire computation. This eliminates the inconsistent-topology failure mode where the same vertex goes to different sides at different points in the algorithm.

**CSG input range.** Coordinates entering CSG must satisfy `|coord| ≤ 256` units, so the snap-then-cast round trip (`i32` → `f32` → `i32`) preserves the integer exactly through `f32`'s 24-bit mantissa. Coordinates outside that range produce a mesh-time error, not a silent precision degradation. The ±256 unit cap is generous for chunky-low-poly asset-local geometry (a teapot is ±2 units, a castle is ±50); world-scale composition places asset-local geometry via per-instance transforms outside the CSG pipeline, not via vertex coordinates exceeding the cap.

The DSL grammar already guarantees inputs are well-behaved (primitive shapes composed via clean transforms — no random user-supplied meshes), so the worst-case degeneracies of arbitrary mesh boolean input are out of scope. Robustness for arbitrary mesh input (a hypothetical "import OBJ then subtract" path) is a separate problem deferred to that ADR.

### Triangle budget

CSG operations produce additional triangles around cut edges (the polygon clipping creates fan triangulation of split fragments). The DSL's vertex buffer cap is currently `64 * 1024` bytes (~910 triangles); a CSG-heavy composition can hit it faster than a transform-only one. Bumping the cap is a separate change tracked outside this ADR.

## Consequences

### Positive

- **Rook-class geometry is now expressible.** Continuous-solid-with-material-removed becomes a single composition: `(difference (cylinder ...) (box ...) (box ...) ...)`.
- **Generalizes beyond the rook.** Doors in walls, holes through pipes, notches in keys, slots in plates, mortise-and-tenon joints, gear teeth all use the same operator. No per-shape ad-hoc tricks.
- **LLM emission is natural.** "Cylinder minus four boxes" is how you'd describe a rook in English. Despite ADR-0026's original conservatism on this point, the actual experience of LLMs (including this author) reasoning about CSG terms is clean — they are a natural mental model.
- **Composes with existing structural operators.** A `(rotate ... (difference ...))` works; an `(array N ... (difference ...))` works; `(difference (composition ...) ...)` works. No special cases.

### Negative

- **Implementation cost is real.** Roughly 800–1200 lines of Rust including tests. BSP CSG is well-understood but numerical robustness is the perennial gotcha — even with epsilon-based classification, edge cases (face-face exactly coplanar, vertex landing within epsilon of multiple planes) need careful handling.
- **Triangle count grows around cuts.** A `(difference cylinder box)` produces fan triangulation along the box's cut edges. Within reason for the chunky aesthetic but eats the vertex buffer cap faster than a transform-only composition.
- **CSG output doesn't preserve LLM-readable structure.** A `(difference cylinder box)` produces triangles whose origin is opaque from the wire — there is no "this triangle came from the cylinder" annotation past the color-inheritance rule. For LLMs reading rendered output back, this is a downgrade from pure composition where every triangle is provenance-tagged by its source primitive.

### Neutral

- **Partial-revolution lathe is no longer needed for the rook.** It was the alternative considered. CSG subsumes its use case (rook crenellations) and many others. Partial-lathe remains a candidate v3 addition if a non-CSG-fixable case appears (e.g. a fan of arc segments where each wedge needs a different profile).
- **Bezier/spline paths remain unspecified** (per ADR-0051). Polyline paths still suffice.
- **The DSL stays Lisp-syntactic data**, not programmable Lisp. CSG operators are pure data — `(difference a b c)` is just an AST node, not a function call evaluated at parse time.

## Alternatives considered

- **Partial-revolution lathe instead of CSG.** A `lathe-arc` primitive that revolves a profile from angle α to angle β rather than full 2π. Composes naturally with `composition`. Solves the rook (4 tall arc-lathes for battlements + 4 short ones for slot floors) and similar rotationally-discrete cases. Rejected as the *only* fix because it doesn't generalize: doors, holes, notches, slots in non-rotational geometry get no help. May still ship later if an arc-of-revolution case appears that isn't naturally a CSG case.
- **A new `aether-csg` crate.** Self-contained algorithm with mesh-in-mesh-out interface. Rejected for v1: the DSL is the only consumer; splitting now is structural speculation. If a non-DSL consumer materializes (runtime booleans on imported meshes), extract then.
- **Vendor a Rust CSG library** (`csgrs`, `mesh-boolean`, etc). Rejected: the available options are either toy-sized or wrappers around C++ libraries (CGAL, libigl) that complicate the WASM-friendly build story. Hand-rolling matches the codebase's pattern (`aether-math` over `glam`, mail runtime over a vendored actor framework) and lets the implementation tune to our triangle layout, our `Vertex` type with embedded color, and our ADR-0030 schema constraints.
- **SDF-based CSG (sample volumes, marching cubes the result).** Robust against arbitrary input. Rejected: the output is a tessellated isosurface that loses the input topology, shifting the aesthetic toward smooth/voxel rather than the chunky-low-poly target of ADR-0025/0026. Also expensive at runtime.
- **Defer CSG further; ship a `lathe-arc` primitive only.** Rejected: punting again on the general operation produces another rook-shaped surprise the next time a "carved from solid" subject comes up. The cost was deferred at ADR-0026 because demand was unproven; demand is now proven.

## Follow-up work

- **Implementation.** `aether-dsl-mesh::csg` module: BSP tree, polygon clipping, three operation impls. New AST nodes `Node::Union`, `Node::Intersection`, `Node::Difference`. Parser support for the three operators with arity checks. Round-trip serialization. Tests: idempotence (`A ∪ A = A`, `A ∩ A = A`, `A − A = empty`), known geometric cases (cube minus inset cube = box-with-hole, cylinder minus 4 boxes = rook crenellations), regression set against a worked-example DSL. Estimate: 4–6 days for a working subset; numerical robustness eats additional time on edge cases.
- **Re-mesh the chess rook example** as `(difference cylinder box × 4)` and add it to `crates/aether-dsl-mesh/examples/` as the canonical CSG example.
- **Vertex buffer cap bump.** Tracked as a separate concern; not coupled to this ADR's acceptance.

## Amendments

### 2026-04-26: Snap-drift tolerance and iterative implementation

After PR 272 landed the BSP CSG core, a live demo on `(difference cylinder box₁ box₂)` immediately stack-overflowed. Two corrections to the original ADR:

**Predicate purity was overstated.** The "Internal fixed-point coordinates with exact integer predicates" bullet claimed that "no floating-point arithmetic is involved in any classification or topology decision" was sufficient to "eliminate the predicate-ambiguity failure mode at its root." That is not true. Even with exact i128 arithmetic, intersection points must be snapped to the integer grid (they are rational numbers that don't generally land on grid points). The snapped vertex is up to half a grid unit off the partitioner plane. On a subsequent BSP pass, when that fragment is tested against the same plane it nominally lies on, the integer side test returns non-zero — the fragment is classified as SPANNING and split again, with each split producing more drifted vertices. The cascade is unbounded.

This bug fires only when plane normals have multiple non-zero components — i.e., cylinder facets, swept profiles, rotated boxes. Axis-aligned plane normals (boxes in the standard orientation) zero out drift in the unrelated axes, so box-only CSG is naturally immune. Every test in the original PR used axis-aligned boxes and missed it.

**Fix.** A snap-drift tolerance in `Plane3::coplanar_threshold()` returns `|n_x| + |n_y| + |n_z|` — the worst-case side-test value for a vertex one grid unit off the plane. Vertex classifications use `if s > threshold then FRONT else if s < -threshold then BACK else COPLANAR`. This is the integer-arithmetic equivalent of csg.js's `EPSILON` constant, but the threshold is *derived from the plane's own normal magnitude* rather than a global guess, so it scales correctly across very small and very large meshes. The original ADR's "epsilon thresholds" rejection language was about *guessed* epsilons; a *derivable* tolerance is a different kind of fix.

The corrected framing: integer arithmetic gives us cross-platform determinism for free and makes the tolerance principled, but it does not eliminate predicate ambiguity entirely. Some drift handling is intrinsic to any BSP CSG implementation that stores vertices at finite precision (which is to say, every implementation that doesn't keep coordinates as unbounded-precision rationals). What we have is "deterministic predicates with derivable threshold," which is meaningfully better than "deterministic predicates with magic-number epsilon," but it isn't ambiguity-free.

**Iterative traversal.** Even with the tolerance fix, the same demo session prompted a follow-up question: "what if a future input still triggers a cascade we don't anticipate?" To eliminate the failure mode entirely (not just the known case), every recursive site in `aether-dsl-mesh::csg::bsp` was rewritten iteratively. The tree now lives in a flat `Vec<NodeData>` arena with `Option<usize>` child indices; `build`, `invert`, `clip_polygons`, `clip_to`, and `all_polygons` all walk via explicit work-stack rather than the call stack. The `MAX_RECURSION_DEPTH` bound becomes `MAX_WORK_QUEUE`, returning `CsgError::RecursionLimit` on overrun rather than overflowing the stack. This generalizes to a project-wide guideline added to `CLAUDE.md`: prefer iterative implementations for any algorithm whose depth could exceed a few hundred frames in practice; bounded recursion on small ASTs (DSL parse, mesh AST walk) is fine if depth is structurally limited; either way, recursive code on user-controlled or geometrically-derived data must enforce a depth/budget cap.

**Test coverage gap.** Both the cascade and the recursion-vs-iteration question would have been caught earlier if the test suite had included any non-axis-aligned geometry. The amendment adds the cylinder-plus-two-cuts case as a regression test in `csg_integration.rs`. Future CSG work in this crate must include at least one test case with a non-axis-aligned plane normal.
