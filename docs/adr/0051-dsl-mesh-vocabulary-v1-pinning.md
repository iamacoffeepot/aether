# ADR-0051: DSL mesh vocabulary v1 — pin syntax, promote torus + sweep

- **Status:** Accepted
- **Date:** 2026-04-26
- **Amends:** ADR-0026

## Context

ADR-0026 committed the engine to a Lisp-syntactic primitive-composition DSL as the only mesh authoring path and listed a v1 vocabulary plus structural operators. That ADR was deliberately light on detail in two places:

- The **exact syntax** of `translate` / `rotate` / `scale` was left to "sub-grammar decisions inside the spike." Only `translate` appeared by example; rotate and scale were implied but not pinned.
- **Torus** and **sweep-along-path** were called out as parked v2 vocabulary extensions, deferred until a demo needed them.

The dsl-mesh spike (PR 256, PR 257) implemented the v1 vocabulary plus torus and sweep, validated end-to-end through the substrate's render path, and produced a recognizable teapot from a single composition node tree. The spike's work surfaced two facts:

1. The syntax for `translate` / `rotate` / `scale` / `mirror` / `array` is now load-bearing — the spike wrote it, the example models depend on it, and any second implementation needs to match. It belongs in the ADR.
2. **A teapot needs torus and sweep.** Both are widely useful beyond the teapot (rings, tubes, pipes, decorative bands, character fingers, vegetation stems). Keeping them parked while we ship the DSL implementation creates a vocabulary cliff: v1 is technically usable but visibly lacks the primitives most LLM-emitted designs would reach for.

A second forcing function: the spike's sweep mesher needed **parallel-transport framing** to avoid visible cross-section twist at sharp tangent turns. The naive "pick a perpendicular each time off world up" approach produced visibly varying tube diameter on the teapot handle. Without naming this requirement in the ADR, a future re-implementation could re-introduce the bug.

## Decision

Amend ADR-0026's primitive vocabulary and structural-operator section with the following pins.

### Structural operator syntax (v1, pinned)

- `(translate (x y z) child)` — translate child by `(x, y, z)`. Already pinned by ADR-0026's example.
- `(rotate (ax ay az) angle child)` — axis-angle rotation in radians. The axis is normalized internally; the angle uses the right-hand rule.
- `(scale (sx sy sz) child)` — per-axis scale. **Not** uniform; uniform scale is composable as `(scale (s s s) ...)`.
- `(mirror axis child)` where `axis` is the symbol `x`, `y`, or `z` — reflect across the named axis-plane (e.g. `(mirror x ...)` reflects across the YZ plane). Faces in the mirrored copy MUST be re-wound so their normals continue to face outward.
- `(array n (sx sy sz) child)` — `n` copies of child, each translated by `i * (sx, sy, sz)` for `i ∈ [0, n)`. `n` is `u32`.
- `(composition child1 child2 ...)` — group node, each child rendered in the group's local frame (which is the parent's frame for the group itself).

### Primitive vocabulary additions (v1, promoted from v2 parked)

- `(torus major_radius minor_radius major_segments minor_segments :color N)` — donut around the Y axis. `major_radius` is the distance from the torus center to the center of the tube; `minor_radius` is the tube's radius. Default axis is `+Y` — rotate around `+X` by `π/2` to make a vertical donut whose hole faces `±Z`.
- `(sweep profile path :scales? :color N)` — sweep a closed 2D `profile` polygon along a 3D `path` polyline. `:scales` is an optional list of per-waypoint scalar multipliers (length must equal `path` length); when present, the profile at waypoint `i` is multiplied by `scales[i]`. When absent, the tube has uniform diameter.

### Sweep framing requirement (v1, normative)

The sweep mesher MUST use a **parallel-transport frame**: the first frame is seeded from world up (with a fallback to world `+x` when the first tangent is nearly vertical), and each subsequent frame is the previous frame rotated by the smallest angle that aligns the previous tangent with the current one. The naive "pick a perpendicular per waypoint off a fixed up reference" approach is **not** acceptable because it visibly twists the cross-section at sharp tangent changes and reads as varying tube diameter.

This requirement is normative because a sweep is a load-bearing primitive (teapot spouts, lamp pipes, character limbs) and the wrong framing produces visibly broken geometry that a downstream consumer cannot fix without re-meshing.

### Path representation choice (v1)

`sweep`'s `path` is a **polyline** — a list of `(x, y, z)` waypoints with linear interpolation between them. Bezier and Catmull-Rom interpolation are **not** committed by this ADR; if a future use case needs smooth-curve paths, it lands as a separate ADR amendment with a distinct primitive (e.g. `(sweep-bezier ...)`) rather than modifying `sweep`'s semantics.

## Consequences

### Positive

- **Two implementations of the DSL produce byte-identical meshes** for the same input. Without pinned syntax for `rotate`/`scale`/`mirror`/`array`, two implementers would diverge.
- **Teapot-class objects are expressible in v1.** Bodies, lids, knobs (lathe), handles (torus or sweep), spouts (sweep with `:scales`). The vocabulary cliff that would have surrounded "you can author a cup but not a teapot" is removed before it bites.
- **Parallel-transport framing is documented as load-bearing.** A re-implementation can't accidentally re-introduce the twist bug.
- **`:scales` is a small, additive extension to sweep** that unlocks all tapered-tube use cases (teapot spouts, character limbs that thicken at the joint, decorative banister spindles) without committing to a more complex per-waypoint API.

### Negative

- **The v1 vocabulary is one node larger** (torus added). Implementations have one more primitive to mesh. Cost is small — torus meshing is well-understood and the spike's implementation is ~40 lines.
- **`array` does not support per-copy rotation.** A spiral staircase or a row of progressively-rotated petals needs a `(composition (rotate ... (translate ...)) ...)` unrolling rather than a single `array` node. This is intentional — array is for the cheap, common case; arbitrary per-copy transforms compose with existing structural operators.

### Neutral

- **CSG boolean operators remain parked.** Overlap-by-transform is still the v1 fusion pattern; CSG enters when a forcing function appears (per the parked-design memory).
- **Bezier/spline paths remain unspecified.** Polyline paths are sufficient for the teapot use case and the validation set.

## Alternatives considered

- **Defer torus + sweep to a v2 ADR.** Rejected: the spike already implements them, the teapot already uses them, and shipping v1 without them creates a vocabulary cliff that contradicts ADR-0026's "LLM-friendly" thesis. The ADR cluster shipping today should include them.
- **Add a `(sweep-bezier ...)` primitive instead of polyline-only `sweep`.** Rejected for v1: bezier tessellation is a separate problem with its own design surface (control-point density, parameterization, C¹ continuity at joins). Polyline is the simplest correct choice. Bezier lands as its own primitive when needed.
- **Make `scale` accept a single uniform factor.** Rejected: `(scale (s s s) ...)` is one extra symbol per call and avoids a syntactic special case. The DSL prefers uniform shapes over convenient shorthands.
- **Make sweep's framing the implementer's choice (don't require parallel transport).** Rejected: the wrong choice produces visibly broken meshes, and the spike already paid the cost of discovering this. Naming it in the ADR saves the next implementer.
- **Add per-copy rotation to `array`.** Rejected for v1: composition + rotate handles the case at the cost of slightly more verbose DSL. v1 stays small.

## Follow-up work

- Implement remaining v1 meshers in the spike: cylinder, cone, wedge, sphere, extrude, mirror, array. (PR follow-up.)
- Promote `spikes/dsl-mesh-spike/` to a real workspace crate. (See ADR-0053.)
- Replace the existing vertex/face mesh editor with a DSL hot-loader. (See ADR-0052.)
