# ADR-0025: Art direction and renderer scope

- **Status:** Proposed
- **Date:** 2026-04-19

## Context

Aether's primary product is not graphics fidelity — it is the speed and coherence of LLM-driven content generation. The renderer exists to make generated content feel alive enough that the generation loop is satisfying. That is a radically cheaper bar than "modern 3D engine," and it is also a better-specified bar: we know when we have hit it.

Without an explicit art direction, every future renderer decision becomes a style debate. "Should we add PBR?" "Should we support normal maps?" "Do we want deferred rendering?" — each of these has a principled answer only if there is a north star to check against. Absent one, scope creep is close to guaranteed, especially given how much interesting graphics-programming work exists in the direction of fidelity. This ADR writes the north star down so that future decisions are gated by a concrete "does this violate the scope?" check rather than case-by-case aesthetic discussion.

The direction emerged from exploration in chat (2026-04-19). The target aesthetic is chunky low-poly flat-shaded forms with palette-indexed per-vertex color, combined with a small set of modern rendering features that enhance the silhouette without violating it — antialiasing, screen-space outlines, soft directional shadows, and tonemapped HDR. The phrase that captures the intent is **"old, but improved"**: keep the chunky-form, palette-driven, flat-shaded silhouette; add the modern rendering features that sharpen it without pushing toward photorealism.

This aesthetic choice is not incidental. It is load-bearing for the generation-first product positioning:

- Low-poly flat-shaded meshes are **primitive-composable**: a character is a tree of boxes, cylinders, and wedges with palette colors. LLMs can describe this reliably in a small DSL. The representation commitment is the subject of a sibling ADR.
- Palette-indexed color enforces **structural style consistency**: generated content cannot produce off-palette colors because colors are indices, not RGB values.
- Low triangle counts (<500k per scene) keep the renderer cheap enough that iteration speed — the actual performance metric — stays high.
- Pass count stays in the **hand-manageable range (5–8 passes)**, which means no render graph is required and likely never will be for this aesthetic. The architectural questions deferred by prior ADRs (render beyond mail, slot trees, two-tier intents) remain deferred and are likely to stay that way for this scope.

## Decision

### Art direction

**Low-poly flat-shaded stylized 3D, with screen-space outlines, palette-indexed per-vertex color, and selectively-applied modern rendering features that enhance the chunky-form aesthetic without violating it.**

Concretely the visual target is:

- Chunky silhouettes built from a small vocabulary of 3D primitives (boxes, cylinders, cones, wedges, spheres, plus profile-based lathe/extrude operations).
- Per-vertex palette-indexed color as the primary surface appearance. Textures are not forbidden, but they are not the default path and should not be assumed by any pipeline decision.
- Flat or near-flat shading (`@interpolate(flat)` on surface attributes where it reads well).
- A restricted palette declared at the scene (or model) level. All colors are indices into that palette.
- Screen-space outlines as the signature stylistic feature — this is what pushes the aesthetic from raw faceted geometry toward an illustrated-world feel.

### Renderer scope (the "improved" part)

The renderer is explicitly bounded at **tier 2** from the render-graph taxonomy: forward rendering with a small, hand-authored set of passes. The maximum pass inventory supported by this ADR is:

1. **Directional shadow pass** — single shadow map with PCF softening. One cascade.
2. **Main forward pass** — writes HDR color, scene depth, and a gbuffer-lite normal texture (for outline edge detection). Uses early-Z from an optional depth prepass if profiling shows the win.
3. **Screen-space outline pass** — edge detection on depth + normals, writes an outline mask.
4. **SSAO (optional)** — dialed low, subtle crevice darkening only. May be deferred indefinitely.
5. **Tonemap + composite** — applies tonemap, fog (done in-shader), outlines, optional AO, optional vignette, and color grading in a single fullscreen pass.
6. **UI pass** — ImGui-equivalent or game HUD over LDR color.
7. **Present** — copy to swapchain.

Target: **5–8 passes total**, hand-scheduled in substrate code. No render-graph compiler. Ever, for this aesthetic.

### Explicit non-goals

The following are **foreclosed** by this ADR. Revisiting any of them requires superseding this ADR, not extending it:

- **Deferred rendering.** Wrong tradeoff at this pass count and material simplicity.
- **PBR (physically-based rendering).** Violates the per-vertex palette model and introduces a material authoring burden that contradicts the LLM-generation-first product positioning.
- **Normal maps, detail maps, tiling textures.** Not compatible with the per-vertex-color default path.
- **Ray tracing (hardware or software).** Wrong aesthetic; wrong cost.
- **Bindless rendering, GPU-driven rendering.** Overkill at sub-500k triangle scenes.
- **Mesh shaders, visibility-buffer pipelines.** Irrelevant at this triangle count.
- **TAA.** MSAA is cleaner for flat-shaded geometry; TAA's temporal smearing fights the illustrated-world feel.
- **DLSS-class upscaling.** Not applicable; resolution is not the bottleneck.
- **Motion blur, depth of field, chromatic aberration.** All violate the illustrated-world silhouette.
- **Volumetric fog, volumetric lighting.** The distance-attenuated in-shader fog is sufficient and in-style.

### Target budgets

- **Pass count:** 5–8 per frame.
- **Triangle count:** <500k per scene; typical scene <200k.
- **Unique materials / pipelines:** <10. Most geometry runs through one core shader.
- **Resolution:** configurable; optional render-at-lower-resolution mode for the pixel-art / RS horizontal-resolution flavor is a non-goal for v1 but a candidate for later.

## Consequences

### Positive

- **Scope is bounded in writing.** Every future renderer decision gets a concrete check: "does this violate ADR-0025?" The debate moves from "is this a good idea" (unbounded) to "is this in scope" (bounded).
- **No render graph.** The deferred render-architecture question stays deferred, likely permanently for this aesthetic. Substrate owns a small, hand-scheduled pass list; that is the architecture.
- **Low implementation complexity across the stack.** One core shader, palette-indexed color, no PBR material system, no texture-binding complexity, no material asset pipeline.
- **Tight composition with DSL-authored content.** Primitive-composed meshes with palette colors are exactly what this aesthetic already expects. The representation and the rendering reinforce each other.
- **Fast iteration.** Small renderer surface means demos land in days, not weeks. Keeps the generation-loop timeline short.
- **Platform-friendly.** Low pass count and low triangle budgets make the engine well-behaved on tile-based mobile GPUs if that ever becomes relevant.

### Negative

- **Forecloses graphical ambition.** If the project later decides fidelity matters more than generation speed, this ADR must be superseded. That is an explicit tradeoff, not an accidental constraint.
- **Limits the asset space.** Organic, soft, or smoothly-deformable content (cloth, faces with vertex-blended expressions, volumetric effects) is awkward in this aesthetic. These are not blocked, but they are not native.
- **"Palette-first" is opinionated.** Content that wants a broader color space — photographic references, HDR imagery, realistic skin tones — does not fit. This is intentional and ties to the LLM-generation product positioning, but it is a real limitation.
- **Outlines are load-bearing stylistically.** Losing the outline pass for any reason visibly degrades the aesthetic. This couples the renderer to one specific post-process in a way that more conventional engines avoid.

### Neutral

- **Shader hot-reload becomes important.** Not a cost specific to this ADR, but the aesthetic's iteration flow makes it valuable earlier than it otherwise would be. Tracked as separate follow-up.
- **Screenshot capture / feedback loop becomes important.** Similarly, the generation-first product positioning makes this valuable earlier. Tracked as separate follow-up (and likely a future ADR if the scope grows).
- **The aesthetic is not unique.** A growing cohort of stylized low-poly titles occupies adjacent territory. The project's differentiation is the LLM-generation loop, not the visual style — and this ADR is fine with that.

## Alternatives considered

- **No art direction constraint; let the aesthetic emerge.** Rejected: without an explicit constraint, renderer decisions become endless style debates and scope creeps toward fidelity. The whole value of this ADR is writing the constraint down.
- **AAA-tier fidelity ceiling (deferred + PBR + RT + modern GI).** Rejected: wrong product positioning. Aether's differentiator is content-generation speed, not visual fidelity. Investing in fidelity spends budget on the wrong axis and makes the LLM-generation story harder (PBR materials are hostile to DSL-first asset representation).
- **Voxel aesthetic (regular grid of colored cells).** Rejected: different representation demands (grids vs. primitive trees), different rendering demands (typically needs custom voxel rasterizer or ray-marcher), and loses the continuous-diagonal silhouette character the target aesthetic depends on. Voxels are a good LLM-generation target in their own right, but not *this* project's target.
- **Pure 2D (sprite-based, pixel art, or vector).** Rejected: limits the world-generation ambition and forecloses 3D spatial reasoning that the project wants to support. The DSL + primitive-composition story is fundamentally about 3D shape description.
- **Full PBR low-poly** (low triangle counts with PBR materials, as some modern stylized games do). Rejected: contradicts the "old but improved" framing, pushes toward texture authoring as a default path, and makes the LLM-generation story harder. The selective modern polish (MSAA, shadows, outlines, tonemap) achieves 80% of the "feels modern" benefit at 10% of the material-system cost.
- **Defer the ADR; let demos reveal the aesthetic.** Rejected for a specific reason: the point of this ADR is to bound the renderer *before* demos start, so demos can be built inside the bound. Demos are the execution of this decision, not the input to it.

## Follow-up work

This ADR does not commit any implementation. It commits scope. Implementation lands as separate PRs:

- **Shader hot-reload** in `aether-substrate`. Prerequisite for productive iteration on renderer demos.
- **Screenshot capture + MCP tool** (`capture_frame`). Prerequisite for the generation-feedback loop. Builds as a spike; may warrant its own ADR retroactively if the surface grows.
- **MSAA setup** in the substrate's swapchain / offscreen target.
- **Depth prepass** as the first renderer demo — validates pass-structure plumbing and teaches early-Z.
- **Directional shadow pass (one cascade, PCF)** as the second renderer demo.
- **Screen-space outline pass** as the third renderer demo. This is where the aesthetic identity first reads on screen.
- **HDR target + tonemap + composite pass** consolidating fog, outlines, AO, vignette, color grading.
- **Subtle SSAO (optional).** Defer until everything else is in; may never ship if it is not missed.

Each becomes its own issue / PR. This ADR is the umbrella.

Parked, not committed:

- Resolution-downsample-and-upscale mode (the "pixelated RS" flavor). Candidate for later; easy to add once HDR + tonemap are in.
- Subsurface scattering approximation. Unlikely to fit, but not actively rejected.
- Per-object outline style control (thickness, color per mesh). Screen-space outlines handle the general case; per-object control is a later refinement if needed.
- Any compute-shader-based effects beyond AO. Not foreclosed, but not in the baseline pass inventory.
