# ADR-0062: Retire CSG from v1 Mesh DSL

- **Status:** Accepted
- **Date:** 2026-04-28

## Context

ADR-0054 introduced a BSP-tree CSG kernel into `aether-dsl-mesh` so the
authoring DSL could express boolean composition (`union`, `intersection`,
`difference`) over its primitives. ADRs 0055–0057 + 0061 layered post-CSG
cleanup, CDT tessellation, n-gon canonical form, and exact-rational
vertex identity on top.

The implementation works for many cases and produced real engine value
along the way (CDT pipeline, n-gon canonical form, fixed-point geometry
core). It is not production-ready as a generally-correct boolean solver:

- The 9×9×3 primitive-pair matrix passes 236/243 cells on native after
  ADR-0061 phase 3. The 7 surviving cells are topology-asymmetry residue
  on curved-pair compositions (sphere×sphere, torus×sphere,
  difference(lathe×sphere)) — failures the BigInt-rational rewrite cannot
  reach by construction. The drafted-but-unwritten ADR-0062-rim-provenance
  was meant to address them with arrangement-level reconciliation.
- Even with that planned work, the matrix is a pinned-position sample
  along one axis of a much larger correctness surface (translation,
  rotation, scale, primitive-segment-count fuzz). Tier B seeded fuzz was
  parked because the residue made it noise-on-noise.
- The wasm32 target additionally hits `alloc::raw_vec::capacity_overflow`
  inside `BspTree::build` on cells that pass on native. The 2 GB wasm32
  `isize::MAX` ceiling exposes a Vec-growth issue native's effectively-
  unlimited address space hides. Production runs the mesher inside the
  wasm mesh-editor component, so this gap matters more than the residue
  count suggests.
- General-purpose mesh boolean correctness is an active research area.
  Industrial CAD packages have decades of work on it; mature open-source
  engines (Blender, OpenSCAD) have known long-tail correctness bugs. It
  is not a feature one engineer ships in a quarter while building the
  rest of an engine.

The realistic alternatives — more iteration on the BSP path, switching
to a half-edge boundary-rep solver, switching to plane-based
arrangements, integrating an external solver — each represent a multi-
quarter focused effort that would block other engine work. None of the
remaining engine surface (mail substrate, GPU sink, audio, I/O,
networking, agent harness) needs CSG to function.

## Decision

Retire boolean CSG from v1 of the mesh authoring DSL. The DSL keeps its
non-boolean surface — primitives (`box`, `cylinder`, `cone`, `wedge`,
`sphere`, `lathe`, `extrude`, `torus`, `sweep`) and structural ops
(`composition`, `translate`, `rotate`, `scale`, `mirror`, `array`).

Concretely:

- Mark ADR-0054 (CSG operators), ADR-0055 (post-CSG cleanup pipeline),
  and ADR-0061 (canonical rim vertex identity) **Superseded by
  ADR-0062**. Their rationale stands historically; their implementations
  do not ship in v1.
- The drafted-but-unwritten ADR-0062-rim-provenance is **rejected**;
  this ADR takes its number.
- `archive/csg-bsp` (cut from main at e07752c, 2026-04-28) preserves
  the full implementation: BSP kernel, BigInt rationals, matrix test,
  cleanup pipeline, CDT pipeline, provenance probes, and all related
  ADR text. A future revisit picks up from that branch — not a green
  field.
- Adjacent infrastructure that was built for CSG but is also used by
  non-CSG mesh paths is **kept** under the renamed crate:
  - **CDT tessellation (ADR-0056)** — every n-gon-emitting primitive
    (`extrude` with concave outline, `sweep`, `lathe`) needs it. ADR-0056
    stays Accepted.
  - **Cleanup pipeline (ADR-0055)** — runs on every mesh, not just CSG
    output. The current `mesh()` entry flows plain `(box 1 1 1)` through
    `cleanup::run_to_loops` then `tessellate::run`. The cleanup *types*
    and merge/T-junction passes stay; the post-CSG-specific provenance
    diagnostics may be deleted or kept opportunistically — the code-
    removal PR makes that call.
  - **Fixed-point geometry core** (`fixed.rs`, `plane.rs`, `point.rs`,
    polygon types) — load-bearing for the entire mesh pipeline. Stays.
- The `aether-dsl-mesh` crate is renamed to `aether-mesh` to reflect
  its narrower scope: mesh authoring DSL plus the n-gon → triangle
  pipeline, no boolean composition. Mail kinds rename
  `aether.dsl_mesh.*` → `aether.mesh.*` for consistency. The mesh-
  editor component and CLAUDE.md are updated in the same change.

## Consequences

- v1 mesh authoring is strictly compositional + transformative. Authors
  who would have used CSG to drill holes or unify shapes either model
  the result directly with primitives + transforms, or accept that some
  shapes (a cube with a precise circular bore) are out of scope until
  the engine returns to the problem.
- The 100s of lines of CSG-specific code (BSP, rational arithmetic,
  matrix harness, provenance analysis) leave the crate, taking the
  num-bigint / num-integer / num-traits dependency tree with them.
  Compile time and binary size drop materially.
- The three failing unit-test classes documented in
  `project_csg_350_rim_vertex_mismatch.md`,
  `project_csg_n_ary_difference_bug.md`, and
  `project_issue_370_diagnostic_chain.md` no longer pin engine work;
  the underlying issues live with the archived branch.
- When the engine returns to mesh booleans, the prior implementation,
  ADRs, matrix, and decision history are all on `archive/csg-bsp`.
  That branch is the starting point for whatever approach is chosen
  then (BSP iteration, half-edge solver, external library), not a
  rewrite-from-scratch.
- The `aether-mesh` rename invalidates external imports of the prior
  crate name. There are no external consumers — every workspace
  reference is updated in the same PR.

## Alternatives considered

- **Keep CSG, narrow the supported set to "axis-aligned, non-curved-pair
  cells only" and gate the rest behind a known-bad list.** Rejected:
  hides the correctness gap rather than removing it; agents driving the
  DSL would still hit failures on natural compositions, just less often.
- **Switch from BSP to a half-edge / boundary-rep solver and ship that
  for v1.** Rejected as still a multi-quarter project gating other
  engine work, with no guarantee it lands closer to correct than the
  current BSP path.
- **Integrate an external CSG library (`csg.js` port, `manifold`, etc.)
  behind the DSL.** Reasonable revisit option when CSG returns; not
  pursued now because it's still a non-trivial integration and the
  engine doesn't yet need the feature.
- **Defer the retirement and keep iterating on ADR-0062 rim provenance.**
  Rejected: even that ADR's success closes only the topology-asymmetry
  class; the wasm32 capacity-overflow class and the fuzz-coverage gap
  remain. The argument that "one more ADR fixes it" has fired three
  times now.
