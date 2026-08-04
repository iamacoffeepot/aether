# ADR-0171: Authored Draw Passes

- **Status:** Proposed
- **Date:** 2026-08-04

## Context

ADR-0170 shipped authored render programs as fragment-only: fullscreen passes over planes that arrive as uploaded textures. The wash runs on it live, and the develop-stage measurement recorded in #4380 shows where that architecture tops out: a repaint at 1280×960 spends ~160 milliseconds CPU-rasterizing the region planes, ~1.8 seconds shipping 59 megabytes of them through mail, 30 milliseconds encoding uniforms, and ~60 milliseconds executing the graph. The plane traffic dominates eleven to one, and even with the #4381 codec fast path it remains a per-repaint pixel shipment that scales with canvas area.

The target is a wash that develops every frame at 60 frames per second. The frame budget is 16.6 milliseconds; no amount of upload optimization fits a per-frame plane shipment inside it. The planes must stop existing as traffic: everything the wash reads that depends on the view — region classes, key-light tone, facing, the ink coverage the flow rides — must be rasterized on the GPU, inside the program, from geometry that is already resident. ADR-0170 named this growth direction (draw passes) without designing it; this record designs it.

What the surface has today: passes are `PassStage::Fragment` only; inputs are dispatch-bound registry textures, prior-pass outputs, or pooled transients; there is no geometry resource an actor can own on the GPU, and no depth machinery inside a program (the world pass's depth target belongs to the frame, not to programs). The offline bake in the tessera research tree already packs class, tone, and facing as the channels of one image through the drawing's camera — precedent that multi-plane data rides one color target's channels, needing no multiple-render-target machinery.

## Decision

The program surface grows a second pass class — the draw pass — and the geometry resource it consumes. The substrate stays ignorant of what the geometry means; it executes what was registered, exactly as ADR-0170 drew the line.

**Geometry resource** (registry-style, mirroring textures):

- `aether.render.create_geometry { layout, vertices, indices }` → `create_geometry_result` (`Ok { geometry_id }` / `Err { reason }`). `layout` declares the vertex attributes as an enumerated list (position is three 32-bit floats; additional attributes from a closed set of scalar and small-vector formats, each named by a location index the WGSL vertex stage binds). The closed set includes the small integer and normalized forms skinning needs — four 8-bit unsigned indices and four normalized 8-bit weights — so a rigged mesh's layout is expressible without reopening the enum. `vertices` and `indices` are `Bytes` (the #4381 fast path is what makes a large mesh upload reasonable); indices are 32-bit. Session-scoped ids, `update_geometry` for in-place replacement, `destroy_geometry` for release — the texture registry's lifecycle conventions verbatim.
- Geometry uploads happen at subject-load cadence, not per frame. A repaint touches no geometry mail.
- Deformation is program content, never surface: when the subject animates, the base mesh stays resident, the pose rides the uniform blob as matrices, and the authored vertex stage applies the skin. The substrate carries no skinning, deformer, or mesh-manipulation vocabulary — per-frame `update_geometry` of a deforming mesh re-creates the plane-traffic wall this record exists to remove, and is the anti-pattern, not the path. View-dependent geometry that already crosses mail per frame at small scale (the ink ribbons) may ride per-frame `update_geometry` at that same scale.

**Draw pass** (a `PassStage::Draw` arm on the existing pass entry):

- Declares a vertex entry point and a fragment entry point from the program's WGSL module, a geometry binding (resolved at dispatch like texture bindings, by id), and an output color slot — a transient or writable binding, exactly as fragment passes output today. Multi-plane data packs into the output's channels, as the offline bake already does; multiple-render-target machinery is deliberately not part of this design.
- Declares an optional depth transient — a `Depth32Float` pooled slot the pass clears and tests against, so a z-buffered plane bake is expressible inside a program. Depth transients pool by extent alongside color transients and may be shared by consecutive draw passes (an ink pass over the same depth the plane pass wrote, so occlusion agrees between them).
- The vertex stage receives the declared attributes plus the pass's uniform window (the view-projection and anything else per-frame rides there). Load semantics on the output slot are declared per pass (clear-then-draw, or load-and-draw-over for layered bakes).
- Validation extends the ADR-0170 register-time classes: the vertex entry must exist and consume exactly the declared layout (naga's interface reflection checks locations and formats), the depth declaration must be present when the fragment stage writes depth-tested output, and a draw pass's geometry binding must name a geometry slot the dispatch will fill. Runtime mismatches (unknown geometry id, layout disagreement) warn-drop naming program, pass, and binding — the established taxonomy.

**Per-frame execution semantics.** Dispatch is already per-frame capable; what changes is the caller's shape. A program whose inputs are geometry and uniforms alone — no plane bindings — makes a repaint a uniforms-only dispatch: the puppet re-dispatches every frame with a fresh view-projection and the wash re-develops in the same frame's encoder, before the passes that sample its output. The settle gate stops being load-bearing and its removal is caller policy, not a surface change.

**What stays CPU-side, by design.** Accident pre-roll (seeded, tiny), chart anchor solving, and palette policy — all uniforms. The care field's chamfer distance does not port as-is (a sequential two-sweep transform); the caller either approximates care analytically from anchor distances in the shader or bakes it as a jump-flood follow-up if the approximation reads wrong. This record does not decide that call — it is easel policy on top of the surface, and the parity scenario is the judge.

**Budget accounting** (the design's own arithmetic, to be re-measured in the landing slice): plane rasterize moves from ~160 milliseconds of CPU to a sub-millisecond draw of a few hundred thousand faces; plane traffic moves from ~1.8 seconds to zero; uniform encode shrinks to the view-dependent slice of the blob (the accident table is static per seed); the wash graph's ~60 milliseconds of execution comes down through the resolution notch ADR-0170 already argued (the wash carries low-frequency form — a half-resolution sheet under full-resolution ink is aligned with the medium, and `SlotExtent::Divided` already expresses it) plus pass-count trimming. The 16.6-millisecond frame is the acceptance bar of the landing slice, not a hope.

## Consequences

- The easel's repaint becomes dispatch-plus-uniforms: subject load uploads geometry once (mesh with class labels as a vertex attribute, ribbon geometry for the ink plane), and each frame's develop is one dispatch. The wash follows the camera live; the two-beat delay the current build shows disappears structurally rather than being tuned away.
- The program surface's vocabulary grows a stage arm and a resource class but no new concepts: geometry ids resolve like texture ids, depth transients pool like color transients, validation extends the same taxonomy. A fragment-only consumer is untouched.
- The ink ribbons render twice conceptually — once by the world pass for the screen, once into the program's ink plane for flow — until a later slice decides whether the world pass itself can feed programs. That sharing question is deliberately out of scope here.
- Wire growth: three new kinds and a `PassStage` variant; existing programs re-register unchanged.
- Implementation lands in slices: the geometry registry; the draw-pass stage in the executor; the puppet's plane/ink bake programs replacing the CPU rasterize and the upload path; the real-time easel loop (settle-gate removal, uniform patching, the resolution notch) with the 60 frames-per-second acceptance measurement; the care-field decision inside that slice.

## Alternatives considered

- **Optimize the upload path only (#4381) and keep CPU rasterize.** Roughly 400 milliseconds per repaint at best — an order short of the frame budget, and still scaling with canvas area. Landed anyway for every other large mail, but rejected as the answer here.
- **Let programs sample the frame's own world-pass outputs** (depth, color) instead of re-rasterizing. Attractive sharing, but it inverts the recording order ADR-0170 fixed (programs record before the world pass) and couples program inputs to frame internals the surface deliberately does not expose. Revisit only if the double-render of ink proves costly.
- **Multiple render targets for the plane bake.** One draw pass filling one target's channels covers every named consumer, matches the offline bake's proven packing, and keeps the pass entry's one-output shape. MRT adds surface for no named need.
- **Compute-stage rasterization.** Software rasterizing in compute re-implements what the hardware vertex pipeline does natively; the draw pass is the smaller and faster design.
