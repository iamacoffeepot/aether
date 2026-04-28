# ADR-0061: Canonical Rim Vertex Identity for BSP CSG

- **Status:** Superseded by ADR-0062
- **Date:** 2026-04-28

## Context

ADR-0054 ships the BSP CSG core in fixed-point integer arithmetic. Plane–edge intersections in `csg::polygon::compute_intersection` already solve in exact `i128` (no float anywhere) using the standard parametric form `I_k = (s0·P1_k − s1·P0_k) / (s0 − s1)`, then snap-round each axis to the nearest integer grid unit via `round_div`. The rational result exists only as transient `i128` numerator/denominator inside the function — it is rounded and discarded before return. The snap-rounding bound is 0.5 fixed units per axis.

ADR-0055 / ADR-0057 layer a cleanup pipeline (weld → t-junctions → merge → slivers) on top, with welding tolerance and t-junction perpendicular tolerance both derived from that 0.5-unit bound.

The completeness matrix (`tests/csg_matrix.rs`, ADR-0054 follow-on) currently passes 231 of 243 cells. The 12 failures are all curved × sphere combinations — cylinder/sphere/lathe/torus on the LHS against a translated sphere on the RHS, across all three boolean ops. Each failure is a manifold violation: directed boundary edges that have no matching reverse partner.

Two diagnostic surfaces shipped in PRs 375 and 376 localized the cause:

1. **Cleanup-stage provenance** (`csg::cleanup::provenance::analyze_unmatched_boundaries`) re-runs cleanup with directed-edge snapshots at each stage. For every one of the 12 final unmatched edges it reports `reverse[w=0, t=0, m=0, s=0]` — the matching reverse never existed at *any* cleanup stage. Cleanup is not dropping it; it was never produced.

2. **Raw-BSP probe** (`tests/csg_bsp_probe.rs`) inlines `union_raw` step-by-step with imbalance counts. For cylinder × sphere: input solo polygons are imbalance 0 (closed); after `na.clip_to(nb)` imbalance jumps to 150 (141 cylinder-side); after `nb.clip_to(na)` imbalance is 187 (46 sphere-side added). Color-bucketed output confirms the asymmetric rim disagreement.

The mechanism has two related-but-distinct components:

- **Independent snap-rounding.** When two `clip_to` sites split against the same edge–plane construction (same numerator and denominator before `round_div`), they produce the same rounded integer today. But rim-mismatch cases involve *different* constructions on the two sides — A's edge against B's face plane on one side, vs the corresponding rim insert into B's face boundary on the other. The rationals are related by the surface arrangement but are not literally equal, so today's per-call `round_div` can land them on slightly different integers — and once a vertex is integer-only, downstream stages cannot tell which integer pair "should have" been the same point.

- **Independent rim construction.** A symmetric-looking pair of `clip_to` calls can compute the rim point on one side and fail to emit a matching rim insert on the other — the *topology* asymmetry remains even if both sides' geometry would round identically. The raw-BSP probe shows asymmetric color-bucket imbalance (141 cylinder-side vs 46 sphere-side after the second `clip_to`), which is a topology disagreement: side A is producing roughly three times as many rim fragments as side B absorbs.

Cleanup's weld step uses Chebyshev tolerance ≤ `WELD_TOLERANCE_FIXED_UNITS = 4` — not exact equality. The reason 1-to-4-unit clusters don't merge into shared identity here is that the partner vertex on the other side is, in many cases, *not present at all* (the topology-asymmetry component above); welding tolerance has no near-coincident pair to merge. T-junction repair only handles within-edge collinear inserts, not cross-bucket missing partners. Welding tolerance bumps from 4 → 8 → 16 were proven neutral on the matrix during the issue 350 investigation (see `project_csg_350_rim_vertex_mismatch.md`) — confirming partners are missing, not just out of tolerance.

A cleanup-side closure pass that synthesizes the missing reverse edges was considered and rejected: the BSP kernel is making asymmetric keep/drop decisions on these polygons, so the visible boundary mismatch is a symptom of an invalid solid, not just an indexing artefact. Closing the boundary after the fact tracks symptoms.

The forcing function flagged in `project_csg_vertex_identity_design.md` ("drift accumulation outpaces what tolerance can absorb — likely arises with deep CSG composition or higher subdivisions") fired on this matrix class.

## Decision

Use **exact rational construction** for BSP split vertices, then **canonicalize equal rational points before rounding** at the BSP-to-cleanup boundary. The fixed-point snap-rounding step moves out of the inner solver and into a single global pass.

Concretely:

- **Internal-only types.** Introduce `csg::bsp::BspPoint3` carrying per-axis rationals (`{ num: i128, den: i128 }` per axis, or shared denominator), and `csg::bsp::BspPolygon` analogous to today's `csg::polygon::Polygon` but vertex-typed `BspPoint3`. The existing public `Polygon` and integer `Point3` are unchanged. Conversion happens at the BSP-to-cleanup boundary; nothing rational escapes the BSP module.
- **`compute_intersection` returns rational.** Replace the integer-returning `compute_intersection` with a `BspPoint3`-returning variant used by `BspPolygon::split` and `clip_to`. Input-mesh vertices entering the BSP run lift trivially to rational (denominator 1).
- **Global canonicalization pass.** After `union_raw` / `intersection_raw` / `difference_raw` finish composing, a single pass walks every `BspPolygon`, reduces each axis rational to a unique normal form, interns identical rational triples to a shared id, then snap-rounds each canonical rational to a fixed-grid integer exactly once. Two vertices with the same rational triple share both their canonical id and their rounded integer. **Normalization rule** for each axis rational `(num, den)`:
  - `den > 0` (sign carried entirely by `num`),
  - `gcd(|num|, den) == 1`,
  - zero represented as `0/1`.

  This makes hash and equality on rational triples behave correctly without geometric tolerance — equal rationals are bit-identical after normalization.
- **Cleanup runs on integer polygons** exactly as it does today. Weld and t-junction tolerances stay in place; their derivations need re-auditing under the new rounding model (see Consequences) but not reducing.
- **Public wire boundary unchanged.** `aether_dsl_mesh::Polygon`, the GPU vertex stream, and the OBJ exporter stay `f32`, fed from integer polygons via the existing fixed-to-float conversion.
- **Overflow policy.** Rational arithmetic in `compute_intersection`, gcd reduction, and canonicalization use **checked** `i128` operations. Overflow returns `CsgError::NumericOverflow { stage, context }` rather than wrapping or panicking. The matrix and the eventual seeded-fuzz layer (Tier B) must run without overflow at ADR-0054 coordinate bounds; any overflow under those bounds is a bug to fix, not absorb.
- **Rim provenance is left as a future hook, not shipped here.** This ADR addresses the coordinate-drift class. If the matrix's 12 cells reveal residual topology asymmetry — a partner-edge that's missing despite the rationals matching — the next move is per-vertex provenance (plane-A ∩ plane-B line, source edge, owning side) so the kernel can ensure the partner surface receives the corresponding rim insert. The rational types should leave room for a provenance field; we won't populate it in this ADR.

The two diagnostic probes (provenance analyzer and raw-BSP probe) stay in-tree as regression gates for this class of bug.

## Consequences

**Positive.** Removes independent snap-rounding as a source of rim mismatch. The class "two BSP sides round to different integers for what was the same rational point" is eliminated. The matrix's 12 remaining cells are *expected* to go to manifold-clean — validation is a deliverable of the implementation, not a guarantee of this ADR. The completeness path (`project_csg_completeness_path.md`) is unblocked at the design layer: if rationals + canonicalization close the matrix, Tier B (seeded fuzz) becomes the next move; if some cells survive, they're isolated to topology asymmetry (rim emission, classification) and the rim-provenance hook is the next ADR.

**Negative.** Rationals run through the BSP hot path. We do not yet know the perf cost; matrix runtime is the first benchmark. Checked `i128` operations may overflow on adversarial inputs (deep recursion × large plane coefficients) — overflow surfaces as `CsgError::NumericOverflow` rather than wrapping, which is the right shape but does need handling at every call site that previously assumed an infallible intersection. The BSP module grows new internal types (`BspPoint3`, `BspPolygon`); migration is structural but contained to `crates/aether-dsl-mesh/src/csg/bsp/`.

**Tolerance rationale needs re-audit.** Today's weld tolerance (`WELD_TOLERANCE_FIXED_UNITS = 4`) and t-junction perpendicular tolerance (`COLLINEAR_TOLERANCE_FIXED_UNITS = 4`) derive from "0.5 fixed units per axis per `compute_intersection` call, accumulated over BSP recursion depth." With rational construction and a single global rounding step, the per-call accumulation goes away and the bound on integer drift from BSP composition is at most 0.5 fixed units per axis *across the whole composition*. Tolerances can stay conservative initially (we don't want a regression in unrelated drift sources like mesh-authored near-duplicates), but the derivation comments at each tolerance site need updating, and a follow-on can audit whether tolerances can shrink.

**Neutral.** Per-line vertex pools (option C from `project_csg_vertex_identity_design.md`) are not part of this ADR. Interning rational triples in the canonicalization pass covers the coordinate-identity half of that design; topological rim identity remains a separate concern that a future provenance hook would address. The two diagnostic probes are kept; they remain the regression gate for any future class of rim-mismatch bug.

**Follow-on work.** The rational primitive type lives in `csg::bsp::` for now; if a second caller (fillet/chamfer authoring, ADR-0026 follow-ons) needs it, it graduates. Issue 370 closes when the implementation lands and the matrix passes 243/243 (or, if some cells survive, when the residue is reclassified into a topology-asymmetry follow-up). The `forcing functions to revisit a vertex pool` list in the vertex-identity memory shrinks by one — that bullet is now historical.

**Forecloses.** Cleanup-side global boundary closure (issue 370 path 1) is off the table for this class. Tolerance bumps in cleanup as a fix for snap-rounding asymmetry are foreclosed. Float-epsilon comparisons in BSP predicates remain foreclosed.

## Alternatives considered

- **Shared compute at the splitter** — cache keyed on `(splitter_plane_id, edge_endpoint_pair)` so the second `clip_to` site reuses the first site's rounded coord. Works for cases where both sides really are computing the same edge–plane intersection, but the cache invariant is delicate through recursion, and the approach doesn't address related-but-different constructions (A's edge ∩ B's plane vs the corresponding insert into B's face boundary). Rationals + interning generalize cleanly.

- **Per-line vertex pool (option C)** — earlier candidate from `project_csg_vertex_identity_design.md`. Empirically proven redundant for that era's bugs and deleted; the forcing function (drift outpacing tolerance) just fired. Rational triples interned in the canonicalization pass cover the coordinate-identity half cleanly; the topological-identity half a pool could also encode is what the future provenance hook is for, not a pool retrofit.

- **Cleanup-side global boundary closure** — synthesize missing reverse edges across buckets after BSP composition. Patches the visible manifold violation but leaves the kernel producing asymmetric keep/drop decisions. Tracks symptoms.

- **Welding tolerance bumps** — already evaluated on the matrix during the issue 350 investigation. Topology shape changes but violation count holds steady at 1524, because the rim disagreement survives any tolerance small enough to preserve legitimate features. Documented neutral.

- **Skip rationals; ship rim provenance directly** — track plane-A ∩ plane-B line + source edge + owning side at each split, and use it to ensure the partner surface emits the matching rim insert. Solves the topology-asymmetry component but leaves coordinate-drift untouched. We expect rationals + canonicalization alone to close most or all of the matrix; if topology asymmetry survives, provenance is the immediate next step (and the rational types should leave room for a provenance field per the Decision).
