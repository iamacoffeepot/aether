# ADR-0170: Authored Render Programs

- **Status:** Accepted
- **Date:** 2026-08-03

## Context

The watercolour easel (`crates/aether-puppet/src/easel/`) develops its sheet on the CPU: `field::Sheet::coats` runs roughly three hundred full-canvas operations — separable box blurs, thresholds, rim subtractions, granulation, flow smears — over `f32` planes, inside the render handler, taking a few hundred milliseconds per develop at window resolution. The settle gate (`SETTLE_FRAMES`) exists to hide that cost: the wash repaints only after the eye has rested. A moving subject repainting on 2s has an 83-millisecond budget the CPU develop cannot meet at full resolution, and a stalled frame either way.

Every hot operation in that pipeline is pointwise arithmetic or a separable blur — fragment-shader material. The question is where the GPU version lives. The render capability could grow a built-in wash pass, but the wash's vocabulary (pours, rims, granulation) is policy owned by the painter, and its parameter space (`palette::Material`, `WashParams`) is already pure data authored in `aether-puppet`. Baking one medium into the substrate would push look-adjacent vocabulary below the mail boundary and grow the widest kind in the engine to parameterize it — against the thin-substrate line (ADR-0074).

The wash is also not the only consumer on the roadmap. A post-stack style pass over the finished frame, cellular-automata media (a grid state is already a `field::Planes` class plane), and eventually GPU rasterization of the easel's region planes all want authored per-pixel work on the GPU. Each would otherwise be its own substrate change.

What the render capability has today: a `TextureRegistry` of sampled textures (`aether.render.create_texture` / `update_texture` / `destroy_texture`, `Rgba8` / `R8`, `TEXTURE_BINDING | COPY_DST`), offscreen `Targets` with depth for the world pass, the ADR-0140 material pass that draws the easel's sheet, and the ADR-0105 overlay pass. There is no path for an actor to put its own code on the GPU.

## Decision

The render capability gains a generic authored-pass surface: an actor registers a WGSL module plus a declared pass graph, and the substrate compiles, validates, and executes it without knowing what it paints. The medium lives with the actor that authors the look; the substrate executes programs.

**Kinds** (the `aether.render.program` family, on the `aether.render` mailbox):

- `aether.render.program.register { wgsl, passes }` → `register_result` (`Ok { program_id }` / `Err { reason }`). Validation happens here, once: the WGSL through naga, then the graph — every entry point exists, every input slot is written before it is read, every pass's declared uniform window fits the blob layout its shader expects. `program_id` is session-scoped, assigned like texture and instrument identifiers.
- `aether.render.program.dispatch { program_id, bindings, uniforms }` — fire-and-forget, immediate-mode like every draw kind: register once, dispatch per repaint or per frame with fresh uniforms. `bindings` names the registry textures this run reads and the writable registry texture it targets; `uniforms` is one byte blob the passes window into. Runtime mismatches (a binding whose size or format disagrees with the graph) warn-drop and name the program, pass, and binding in the render actor's log ring — the same convention as an unknown texture id in `draw_textured_quads`.
- `aether.render.program.destroy { program_id }` — fire-and-forget release, mirroring `destroy_texture`.

**Pass graph.** Each pass declares a fragment entry point, its input slots, its output slot, and its uniform window (offset and length into the dispatch blob). A slot is a dispatch binding, a prior pass's output, or a transient intermediate; transients carry an extent (full target size, or an integer divisor for pyramid work) and a format, and are pooled by extent and format so a ping-pong chain reuses two allocations rather than three hundred. The graph is a sequence — a pass may read only slots already written — which makes the DAG check a single index comparison at register time. A pass may declare a repeat count with a per-iteration uniform stride, so a chain of pours is one entry rather than many. Program structure is fixed at register; everything that varies per run rides the uniform blob, and a structurally present but unneeded pass is neutralized through its uniforms (a zeroed contribution costs one cheap pass, far less than re-registering).

**Stages.** Version one executes fragment passes only — fullscreen-triangle pipelines over render attachments, reusing the pipeline plumbing the capability already has. Every named consumer's operations are gathers or pointwise math; compute earns a `PassStage::Compute` arm when a consumer needs shared-memory tiles, reductions, or scatter writes, and the graph vocabulary — slots, extents, uniform windows — is stage-agnostic, so that arrival is an addition rather than a redesign. Draw passes (a vertex stage over actor-owned geometry, rendering into graph slots) are the named growth direction for GPU-side plane rasterization; version one does not build them.

**Registry growth.** `create_texture` gains a writable variant — `RENDER_ATTACHMENT | TEXTURE_BINDING`, created without staged pixels — and the format set gains a single-channel float for data planes, with nearest sampling for label planes whose values are identities rather than colours. A program's final output is a writable registry texture, so the material and overlay passes sample a program's result exactly as they sample an uploaded one; there is no readback anywhere in the loop.

**Determinism.** Nothing on the GPU rolls dice. Accidents — pour jitters, noise windows, spatter positions — are pre-rolled by the authoring actor into the uniform blob, and shared noise fields (tooth, mottle, edge) upload once per canvas size as ordinary textures. The CPU implementation in `field.rs` remains the oracle: parity is a `SubstrateHarness` similarity scenario over the developed sheet, thresholded rather than bit-exact, since an iterated-tap GPU blur legitimately differs from the CPU running sum in the last bits.

**Chassis behaviour.** Headless replies `Err` to `register` (fail-fast, exactly as it does for `create_texture`) and absorbs `dispatch` / `destroy`. `SubstrateHarness` executes programs for real — it has an adapter — which is what makes the parity scenario a `cargo test`.

## Consequences

- The wash medium migrates from `easel/field.rs` into authored WGSL living in `aether-puppet`, registered as the first program on the surface. The puppet keeps region rasterization, flow, accents, palette, and cadence — the look never crosses into the substrate. The develop drops from a few hundred milliseconds of blocked handler to one dispatch encoding a few milliseconds of GPU work.
- Repaint cadence stays actor law: the puppet dispatches under its own settle gate today and on 2s when the subject animates. While plane rasterization remains on the CPU, each repaint uploads plane pixels through mail (megabytes per develop) — acceptable at settle cadence, and the named ceiling that draw passes remove by making a repaint uniforms-only.
- Later GPU consumers — the post-stack style pass, cellular-automata media — become actor-side WGSL plus a register call, with no substrate change. That is the point of paying for the generic surface now.
- The pass-graph vocabulary (slots, extents, uniform windows, repeats) becomes a public contract; a poor convention here taxes every consumer, which is why it is decided in this ADR rather than grown ad hoc.
- Shader code is harder to unit-test than Rust. The oracle-plus-similarity pattern covers whole-program behaviour; per-operation confidence comes from small single-pass scenarios rather than in-shader assertions.
- Implementation lands in slices: writable and float registry textures; the program surface itself; the wash port with its parity scenario; the puppet consuming it (and measuring the rasterize-versus-upload split that decides how urgent draw passes are); draw passes as their own follow-up.

## Alternatives considered

- **A built-in wash pass, parameterized by mail.** Rejected: bakes one medium's vocabulary into the substrate, requires the engine's widest kind to steer it, and every future effect starts the same argument over again.
- **Compute-first surface.** Rejected for version one: no named consumer needs scatter, reductions, or shared memory; fragment passes reuse existing pipeline machinery, and the stage enum leaves the door open at the cost of one variant.
- **CPU optimization only (develop at reduced resolution).** Meets the on-2s budget at roughly quarter resolution and remains a useful fallback knob, but it is a ceiling rather than a path: full-resolution animation and any second GPU consumer are out of reach. Rejected as the destination.
- **Bespoke substrate passes per consumer.** Each future effect (style pass, cellular automata) becomes its own render-capability change. Rejected: unbounded substrate growth, and each one re-litigates the same texture and uniform questions this surface settles once.
