# ADR-0172: GPU stroke visibility through a stroke-parameter field

- **Status:** Proposed
- **Date:** 2026-08-04

## Context

The puppet's pen solves stroke visibility on the CPU. Every time the eye moves, the re-split walks each extracted curve and asks the mesh's occlusion index whether each point can see the camera (`visibility::runs`, sampled every third point and refined at verdict flips per #4409), splits the curve into visible runs, and hands each run to `ribbon`, which anchors the pressure taper at the run's ends. Authored face marks pass through a whole-or-nothing coverage rule (`CHART_COVERAGE`, 0.35) so a mostly-hidden eye vanishes rather than shattering into crumbs.

That path is now the frame budget's binding constraint. After the #4416 algorithmic pass (surface-area BVH splits, a packed weld index, a rebuilt visibility loop), the eye-moved re-split measures 19.25 ms in release wasm — 10.94 ms native, a 1.65× wasm tax that resisted simd128 (measured as a regression: the traversal is branchy and the dot products three-wide, so there is nothing for the vectorizer). The whole-frame target is 16.7 ms with headroom, shared with the per-frame wash that ADR-0171's landing slice (#4387) is bringing in at roughly 5–6 ms. Orbiting today runs ~21–22 ms; with the wash live it lands near 25–28 ms. The occlusion rays are the largest single share of the re-split, and they are also the piece that scales worst into the animation arc: a deforming mesh makes every frame a re-split frame and would additionally force per-frame rebuilds of the BVH those rays traverse. The direction set for this codebase is that per-frame computation prefers the GPU.

What the program surface already offers, and what it deliberately does not: draw passes carry program-owned depth transients (`DrawPass.depth` into `ProgramRegister.depth_transients`), fragment passes chain through `InputSlot::PassOutput` and can run at reduced extents (`SlotExtent::Divided`) for pyramid work, and geometry is resident through the ADR-0171 geometry resource — the ink ribbons already ride one (#4415). There is no readback, no compute stage, and no MRT, and this record changes none of that.

One structural fact does most of the work here: **the subject mesh is never drawn**. The picture is ink ribbons over the wash sheet; the mesh exists only as the thing the ink and wash are *about*. Stroke occlusion against the subject is therefore not a frame-depth question at all — it is a question the program can answer entirely inside its own pass graph, against a depth image of a mesh the viewer never sees.

## Decision

Stroke visibility moves onto the GPU as a field over each stroke's own parameterization, and the ink becomes a program-rendered image composited into the scene the same way the wash sheet already is.

**The depth prepass.** One draw pass renders the resident subject geometry into a program depth transient at canvas resolution — no color output, no lighting, just the surface the rays used to march against, rasterized once per frame. Under animation this is automatically correct: whatever pose the vertex stage produces is the pose occlusion is judged against, with no index to rebuild.

**The stroke-parameter visibility field.** Strokes upload with their arc parameterization: each curve owns a row of a small field texture, each point a texel along that row (a curve id / point index layout; the current drawing is ~3,800 curves and ~170k points, a texture in the single-digit megabytes). A pass projects each point, samples the depth prepass with the existing surface bias, and writes the point's visibility. Two short chains of fragment passes then derive, per point and per curve, everything the CPU run-splitter produced:

- a log-step distance transform along each row turns raw visibility into *arc distance to the nearest hidden point or curve end* — exactly the quantity `pressure` tapers by, so stroke ends thin into an occluder instead of cutting hard;
- a row-reduction pyramid (`SlotExtent::Divided` hops) produces each curve's visible fraction, and the whole-or-nothing rule becomes a comparison against `CHART_COVERAGE` wherever the curve's row says so — an authored mark still arrives whole or not at all.

**The ink pass.** The ribbon draw pass renders every curve unclipped; its vertex stage reads the visibility and distance fields at the vertex's own parameter coordinate, zeroes the width of hidden vertices, and applies taper from the field's distance rather than from CPU run ends. The pass writes an ink RGBA transient, depth-tested against the prepass so overlapping strokes keep their own order. That transient composites into the frame as a screen-space textured rect in front of the wash sheet — the mechanism the sheet has used since #4350/#4351. To keep #4407's edge quality, the ink transient renders supersampled (a `Divided`-style extent in reverse: twice the canvas edge) and the bilinear composite resolves it; ribbons are sparse, so the fill cost is per-covered-texel and small.

**What stays on the CPU.** Extraction (the level set per eye, the weld, the wobble seeds and per-point weights packed as vertex attributes) remains CPU work at re-split cadence — measured at ~10–11 ms wasm after #4416, and no longer carrying the rays. Tone gating stays where it is (load cadence). The whole `visibility::runs` / per-run `ribbon` path and the streamed per-frame `DrawTriangle` ink retire together once parity gates pass.

## Consequences

- Occlusion cost leaves the re-split: the CPU path drops to extraction and packing, and the GPU adds a depth rasterization plus small field passes. The expected orbit frame lands near the extraction floor (~10–11 ms wasm today) plus the wash's per-frame cost — inside budget with real headroom for the first time, and the remaining CPU share is the natural next target when the animation arc arrives.
- Visibility is per-frame by construction, so a deforming subject occludes its own strokes correctly with no per-frame BVH rebuild — the animation arc inherits a solved problem.
- The pen's semantics survive as textures rather than control flow: taper anchors to the distance field, whole-or-nothing to the coverage reduction. Both are gated by the existing fixed-framing parity instrument before the CPU path retires; the look, not the mechanism, is the acceptance bar.
- The ink's anti-aliasing changes mechanism (supersampled transient + bilinear composite instead of 4× MSAA in the world pass); the crispness gate re-runs at the switch.
- The world pass loses its last per-frame streaming producer in this scene; `aether.draw_triangle` remains for other clients, but the puppet stops using it.
- Stroke identity must be stable across re-splits for the field layout (curve id → row); the weld already assigns stable seeds, which the layout reuses.

## Alternatives considered

- **Frame-targeting authored passes** (draw passes writing the frame's MSAA color+depth after the world pass). Solves the same composition problem, but inverts the recording order ADR-0170 fixed and adds surface where the sheet's compositing mechanism already suffices; the supersampled transient costs less than the invariant.
- **Native hosting of the re-split** (move the solve across the wasm boundary). Kills the 1.65× tax and nothing else — lands at the budget's edge with no headroom, buys no animation story, and moves actor-owned logic into the substrate against the grain of the actor model.
- **Amortizing the re-split during motion** (ink at half cadence while orbiting). No surface change, but the ink visibly lags the camera and animation would live under the same compromise permanently; rejected against the stated everything-at-60 metric.
- **Image-space silhouettes** (extract the outline from a facing/depth image instead of curves). Removes extraction too, but the pen's identity — seeded wobble, welded curves, per-stroke taper — needs stable curves; image-space lines re-derive per frame and boil.
