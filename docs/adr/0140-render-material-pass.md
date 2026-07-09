# ADR-0140: Render material pass

- **Status:** Accepted (shipped — world-space material pass; crates/aether-substrate/src/render/material.rs)
- **Date:** 2026-07-08

## Context

The renderer exposes two ways to put pixels on screen. The main pass draws depth-tested `(pos, color)` triangles in world space (`crates/aether-substrate/src/render/pipeline.rs`, `shader.wgsl`); it binds no textures. The quad overlay pass draws alpha-blended textured quads over the finished world image (`quad.rs`, `quad.wgsl`); it deliberately binds no depth. Nothing is both textured and depth-tested, so content that wants to sit *in* the world as an image or a field-driven surface — terrain coverage bands, decals, world-anchored sprites — must be CPU-triangulated into colored geometry and re-uploaded whole on every change. Issue #2781 hit this directly: a chunk's prepared `u8` coverage plane should upload once as an R8 texture and have its iso boundary reconstructed by fragment sampling, with a paint or cellular-automaton step becoming a texture sub-rect update instead of a chunk remesh; today no pass can host that shader.

ADR-0025 sets the renderer's scope: a hand-manageable pass set (5–8 passes), no render graph, expression through palette-driven low-poly form rather than fidelity. ADR-0071 gives each chassis one driver that owns the encoder and records passes against capability-owned accumulator state (`RenderHandles`). ADR-0105 added the quad overlay as a sibling pass with its own vertex layout, shader, and texture registry — RGBA8-only, with the registry's staged-pixel lifecycle (CPU staging as source of truth, lazy GPU realization at record time, dirty-flag re-upload, sub-rect staging updates).

The forces to balance: components need a wider range of expression than raw triangles; the substrate must keep sole ownership of WGSL and pass structure (a wasm guest supplying shader source would drag in validation, binding reflection, pipeline caching, and a versioned shader ABI — none of which anything on the roadmap needs); and whatever is added must not disturb the main pass, which every existing component depends on.

## Decision

Add one **material pass**: a depth-tested, texture-sampling, alpha-blended world-space pass recorded between the main pass and the quad overlay pass. The commitment components get is a **wire vocabulary** — texture ids plus typed per-material draw kinds — while the shader set behind it stays **closed and substrate-authored**. Pipelines remain swappable implementation; the kinds are the durable contract.

### Texture formats and the shared bind group layout

`aether.render.create_texture` gains a `format` field — a wire enum with `Rgba8` and `R8` variants. The texture registry threads the format through staging (`expected_pixel_bytes` becomes format-aware: 4 bytes per pixel for `Rgba8`, 1 for `R8`), realization (`wgpu::TextureFormat::Rgba8Unorm` / `R8Unorm`), and sub-rect updates. Both formats are filterable, so the existing shared linear clamp-to-edge sampler serves both.

One texture + sampler `BindGroupLayout` (texture view at binding 0, sampler at binding 1 — the shape the quad pipeline already uses at group 1) is built once at GPU install and handed to every pipeline that samples textures. `RealizedTexture` builds its bind group against that shared layout instead of the quad pipeline's private one, so any registered texture is bindable by the quad pass and every material pipeline alike. This half of the decision is implemented by #2815 ahead of the pass itself.

### The pass

- **Placement**: recorded after the main pass, before the quad overlay. World geometry first, draped/embedded material surfaces second, screen-space UI last.
- **Depth**: the main pass's depth attachment flips from `StoreOp::Discard` to `StoreOp::Store`; the material pass attaches the same depth view with `LessEqual` testing enabled and depth writes disabled. Material surfaces are therefore occluded correctly by world geometry, and ordering among overlapping material draws is the accumulator's submission order (painter's order), which is sufficient for draped planar content.
- **Blending**: standard alpha blending — coverage-band antialiased edges and sprite transparency both require it.
- **Vertex layout**: `(pos: vec3<f32>, uv: vec2<f32>)`, 20 bytes per vertex, expanded substrate-side. Components never author material vertices; they send rect-based draw kinds and the record path expands each rect into six vertices, batched per texture the same way the quad pass batches.
- **Accumulation**: a per-frame accumulator with a last-submitted cache, mirroring the triangle and quad paths' `commit_or_replay` semantics — immediate-mode, resend every frame, idle capture replays the cache.
- **Buffer bound**: the material vertex buffer is fixed-size like the quad buffer, sized for rect counts in the thousands (a rect is 120 bytes of vertex data; terrain-chunk and decal workloads are hundreds of rects per frame). Overflow warn-drops the pass for the frame, matching the quad path's degradation.

### The material set

A material is a substrate-authored WGSL fragment behavior plus one typed draw kind under the `aether.render.material.*` name family. The set is closed: adding a material is a deliberate substrate PR that adds a shader (or shader branch), a pipeline entry, and a kind — the same posture ADR-0025 takes for passes. The initial set is two:

- **`aether.render.material.textured`** — `{ texture_id, rects }`; each rect `{ x, y, width, height, z, u0, v0, u1, v1, tint }` in world units on the world plane at layer `z`. Samples the texture (any format), multiplies by `tint`. This is the general image-in-world surface: sprites, decals, splats.
- **`aether.render.material.coverage`** — `{ texture_id, rects }`; each rect `{ x, y, width, height, z, body_color, rim_color, rim_width }`. Requires an `R8` texture. The fragment shader samples the coverage plane bilinearly, thresholds at iso 127.5 (fixed — the same iso the CPU march uses, so GPU and CPU boundaries agree by construction, #2781), renders a `rim_color` band of `rim_width` (in coverage-fraction units) inside the boundary and `body_color` within, with `fwidth`-based antialiasing at the iso edge. This is #2781's runtime path; the CPU mesher remains the TestBench-assertable reference.

Both kinds are fire-and-forget with per-frame accumulation. An unknown `texture_id`, or a `coverage` draw against a non-`R8` texture, warn-drops the batch at record time.

### Chassis behavior

Desktop and test-bench record the material pass in their existing frame paths; the capture readback already images the shared offscreen target, so material output is TestBench-assertable with no new capture machinery. Headless absorbs the new draw kinds as no-ops (they are fire-and-forget, matching its handling of `draw_textured_quads`); `create_texture` on headless keeps its ADR-0105 fail-fast `Err` reply.

## Consequences

- Components gain a depth-tested textured surface — image-in-world and field-driven materials — without the substrate giving up WGSL or pass-structure ownership. The pass count grows to three (main, material, overlay), inside ADR-0025's hand-managed budget.
- #2781's coverage rendering becomes a material rather than a bespoke pass, and a paint/cellular-automaton update becomes an `update_texture` sub-rect on an R8 plane.
- The main pass stores its depth buffer instead of discarding it — a bandwidth cost that is negligible at ADR-0025's target scale, paid on every frame whether or not material draws are present.
- `CreateTexture` changes wire shape (new `format` field); its in-repo senders (the text capability's atlas, widget actors) update in the same change. Pre-1.0, no external compatibility is owed.
- Every future material is a substrate PR. That is deliberate friction: the material set stays curated, in-scope for the art direction, and discoverable via `describe_kinds` — but it means components cannot ship novel shader behavior without a substrate change.
- Follow-on work: #2815 (format-aware registry + shared bind group layout), #2816 (the pass and the two materials), then #2781 (aether-kit switches overlay rendering to coverage materials and retires marched overlay triangles from the runtime path). `docs/guide/systems/rendering.md` documents materials alongside triangles and textured quads once the implementation lands.
- The chunk-boundary bilinear-continuity contract (a one-texel gutter duplicating neighbor edge samples in each chunk's prepared plane, or a shared atlas) is a data-preparation concern owned by the producer (`aether-kit`), decided in #2781's plan — the material shader samples whatever plane it is handed.

## Alternatives considered

- **A bespoke coverage-only pass** — solves #2781 alone; the next textured-in-world need (decals, sprites) re-litigates the same design. Rejected for generality-at-equal-cost: the coverage shader needs the pass, formats, and bind-layout work anyway.
- **User-supplied WGSL (open material system)** — maximal expression, but pulls in shader validation, bind-group reflection or a rigid binding ABI, pipeline caching, and a versioned shader interface. Nothing on the roadmap needs arbitrary shaders; the closed set leaves this door open without paying for it now.
- **Widening the main pass's vertex format** (`pos, color, uv, texture_id` in one buffer) — touches the one path every existing component depends on and forces per-texture draw splitting inside the main pass, rebuilding the batching the sibling-pass shape already provides.
- **Reusing the quad overlay for coverage** — the overlay has no depth interaction and no world fragment context; a coverage band drawn there floats over movers instead of sitting on the terrain. Rejected in #2781's design notes for the same reason.
- **A render graph / material sub-capability hierarchy** — forecloses ADR-0025's hand-managed pass posture for no present need; three passes do not need a graph.
