# Authored render programs

> **Governing ADR:** [ADR-0170](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0170-authored-render-programs.md)
> (authored render programs). The record holds the reasoning and the rejected
> alternatives; this chapter documents the shipped surface. The kinds live in
> [`aether-render/src/kinds.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-render/src/kinds.rs),
> the registry and executor in
> [`aether-render/src/runtime/program/`](https://github.com/iamacoffeepot/aether/tree/main/crates/aether-render/src/runtime/program),
> and the stage primitives in
> [`aether-substrate/src/render/program.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-substrate/src/render/program.rs).

An authored render program puts actor-owned per-pixel code on the GPU. The
actor registers one WGSL module plus a declared pass graph; the substrate
compiles, validates, and executes it without knowing what it paints. The medium
— a watercolour develop, a post-process style pass, a cellular-automaton step —
stays with the actor that authors the look, and the substrate stays a thin
executor of programs.

## Mental model

A program has two halves with different lifetimes:

- **Structure is fixed at register.** The WGSL, the slot declarations, and the
  pass sequence are validated and compiled once, and the reply hands back a
  session-scoped `program_id`. A structurally present but unneeded pass is
  neutralized through its uniforms (a zeroed contribution costs one cheap
  pass); restructuring means registering a new program.
- **Data varies per dispatch.** Each `dispatch` names the registry textures the
  run reads and writes and carries one uniform byte blob the passes window
  into. Register once, dispatch per repaint or per frame with fresh uniforms.

A program reads and writes **registry textures** — the same session-scoped
textures `aether.render.create_texture` registers (see
[Rendering & camera](rendering.md)). Its result lands in a texture created
with `usage: Writable`, so the material and overlay passes sample a program's
output exactly as they sample an uploaded one. There is no readback anywhere in
the loop.

## When to reach for a program

The built-in passes are substrate-authored and parameterized by data: world
triangles, textured and coverage materials, overlay quads, text. Reach for
them whenever data fields express what you need — they cost no WGSL and no
register call. Reach for a program when the *code* is the policy: per-pixel
math the substrate has no kind for, chains of image operations (blurs,
thresholds, composites), or work over `R32Float` data planes. The alternative
of computing pixels on the CPU and uploading them through `update_texture`
remains available and is the right call at low rates and small sizes; a
program earns its place when the work is fragment-shader material and the
upload or compute cost stalls the actor.

## Public mail surface

All three kinds address the `aether.render` mailbox.

| Mail kind | Rust payload | Contract |
|---|---|---|
| `aether.render.program.register` | `ProgramRegister { wgsl, bindings, transients, passes }` | validate + compile; reply `aether.render.program.register_result` / `ProgramRegisterResult` (`Ok { program_id }` / `Err { reason }`) |
| `aether.render.program.dispatch` | `ProgramDispatch { program_id, bindings, uniforms }` | fire-and-forget; execute once at the next frame record |
| `aether.render.program.destroy` | `ProgramDestroy { program_id }` | fire-and-forget release, mirroring `destroy_texture` |

`program_id` is session-scoped and assigned like texture and instrument
identifiers. A rejected register consumes no id, so accepted ids stay dense.
Destroying a program releases its compiled pipelines; pooled transient
textures stay in the shared pool for other programs.

## The pass graph

A registered graph declares three lists: `bindings`, `transients`, and
`passes`.

### Slots and extents

A **slot** is a texture a pass samples or renders into. Two declaration lists
exist:

- `bindings: Vec<SlotSpec>` — textures the dispatch supplies. Each dispatch
  names one registry texture id per declared binding, in order.
- `transients: Vec<SlotSpec>` — intermediates the executor owns and pools.
  A dispatch never names them; they exist so a chain of operations has
  scratch surfaces without the actor creating textures for them.

A `SlotSpec` is a `format` (`Rgba8`, `R8`, or `R32Float`) plus an `extent`:

- `SlotExtent::Full` — the reference size.
- `SlotExtent::Divided { divisor }` — the reference size floor-divided by
  `divisor` on both axes, clamped to at least one texel, for pyramid and
  reduced-resolution work. A zero divisor rejects at register.

The **reference extent** is the size of the texture bound at the program's
output binding — the dispatch binding the final pass writes, which must be
declared `Full`. Every other extent scales from it, which is what lets one
registered program dispatch at any canvas size: the graph carries no pixel
dimensions, only ratios.

Each pass reads through `InputSlot` values and writes one `OutputSlot`:

- `InputSlot::Binding { index }` / `InputSlot::Transient { index }` — a
  declared slot by list position.
- `InputSlot::PassOutput { pass }` — whatever slot the pass at that sequence
  index wrote, resolved at register time. A ping-pong chain reads "the
  previous pass's result" without naming the transient twice.
- `OutputSlot::Binding { index }` / `OutputSlot::Transient { index }` — a
  dispatch binding (which must resolve to a `Writable` registry texture at
  dispatch) or a transient.

### Sequence order

The graph is a sequence, and a pass may read only slots already written: a
transient must be written by an earlier pass before any pass reads it, a
`PassOutput` must point at an earlier pass, and no pass may read its own
output slot. This makes the acyclicity check a single index comparison at
register time. The final pass must write a dispatch binding — the program's
result texture.

### Uniform windows

A dispatch carries one byte blob, `uniforms`; each pass declares a window into
it — `uniform_offset` and `uniform_length`, in bytes. The window binds at
`@group(0) @binding(0)` in the pass's entry point and must cover the uniform
block the shader declares there (checked at register from naga's layout; a
shorter window rejects). A pass whose entry point declares no uniform block
passes a zero-length window.

Windows need no alignment of their own: the executor copies each window into
an aligned staging arrangement before upload, so the blob packs tight. The
blob is the program's entire per-run parameter space — everything that varies
per dispatch rides it.

### Repeats

A pass may declare `repeat: Some(PassRepeat { count, uniform_stride })`. The
pass records `count` times, iteration `i` binding its window at
`uniform_offset + i * uniform_stride` — one pass entry over a strided
parameter table rather than `count` entries. `count` must be between 1 and
4096; `uniform_stride` may be 0 to rebind the same window every iteration.

Repeat semantics follow the output-slot write rules. The first write a
dispatch makes to each output slot clears it to transparent black; later
writes — a repeat's iterations, a second pass onto the same slot — load the
existing content. What "load" composes is the pass's blend, and the blend is
fixed by the output format: `Rgba8` and `R8` outputs alpha-blend onto the
target, while `R32Float` outputs replace it (core WebGPU cannot blend 32-bit
floats). So a repeated pass accumulates on a blendable target, and on a float
target each iteration overwrites the last — a repeat there keeps only its
final iteration. A multi-step chain over float planes is therefore laid
structurally — each step its own pass entry with its own window — rather than
as one repeated pass; the [wash program](#the-worked-consumer-the-wash) below
is the worked example of that shape.

### The shader contract

The substrate owns the vertex stage — a fullscreen triangle — so the module
declares fragment entry points only. An entry point may take
`@location(0) uv: vec2<f32>` — `(0, 0)` top-left to `(1, 1)` bottom-right,
texture convention — and returns `@location(0) vec4<f32>`.

Bindings inside the shader:

- `@group(0) @binding(0) var<uniform>` — the pass's uniform window.
- Group 1 — the pass's input slots, in declaration order, as texture /
  sampler pairs: input `n` is `@binding(2 * n)` (`texture_2d<f32>`) plus
  `@binding(2 * n + 1)` (`sampler`).

The sampler an input receives follows the bound texture: nearest when the
registry texture was created with `Nearest` sampling or its format cannot be
linear-filtered (`R32Float`), linear otherwise. Transients sample by their
declared format the same way.

## Register-time validation

Validation happens at register, once, and every failure class replies a
distinguishable `ProgramRegisterResult::Err { reason }` — a
bad-but-parseable program replies an error instead of crashing the substrate.
The classes, in check order:

| Class | Reason shape |
|---|---|
| WGSL | `invalid wgsl: …` — naga parse or validation failure |
| Empty graph | `program declares no passes` |
| Extent | `binding N: extent divisor must be at least 1` (also for transients) |
| Entry point | ``pass N: no fragment entry point named `X` in the module`` |
| Slot range | `pass N: binding slot B is out of range (M declared)` (also for transients) |
| Sequence | `pass N reads the output of pass P, which does not run before it`; `pass N input I reads transient T before any earlier pass writes it` |
| Self-read | `pass N reads its own output slot` |
| Uniform window | `pass N: uniform window (L bytes) is shorter than the shader's uniform block (B bytes)` |
| Repeat | `pass N: repeat count must be at least 1`; `pass N: repeat count C exceeds the supported maximum 4096` |
| Final output | `the final pass must write a dispatch binding (the program's result texture)`; `binding N: the program's output binding must declare Full extent …` |
| Pipeline | `pipeline creation failed: …` — a wgpu validation error caught by the register's error scope (for example a sampler-versus-layout mismatch naga alone cannot see) |

The validation source is
[`runtime/program/validate.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-render/src/runtime/program/validate.rs).

## Dispatch-time behavior

`dispatch` is fire-and-forget. The program's passes record into the frame's
command encoder **before** the world, material, and overlay passes, in
dispatch arrival order — so a `draw_textured_quads` or material draw in the
same frame samples the program's freshly written output. The written pixels
persist in their writable registry textures between dispatches: a program
re-executes only when dispatched again. That distinguishes a program's output
from the immediate-mode draw kinds — the draws must be resent every frame,
while a dispatched program's result is retained pixels that later frames keep
sampling.

Runtime mismatches **warn-drop the whole dispatch**: the checks run before any
recording, so a rejected dispatch records nothing and the frame survives —
other draws still render, and the output texture keeps its prior content. The
drop classes:

- an unknown `program_id`;
- a binding count that disagrees with the registered graph;
- a binding naming an unknown texture id;
- a binding whose format disagrees with the declared `SlotSpec`;
- a binding whose size disagrees with its extent resolved against the
  reference;
- a non-`Writable` texture bound where the graph writes;
- a uniform blob shorter than a pass's window reach
  (`uniform_offset + (count - 1) * uniform_stride + uniform_length`);
- one texture bound as both a pass's input and its output.

Each drop logs a warning naming the program, pass, and binding, under the
`aether_render` target, into the render actor's log ring — the same
convention as an unknown texture id in `draw_textured_quads`. Query it with
the MCP `actor_logs` tool against mailbox `"aether.render"` (see
[Logging](logging.md)). A `destroy` naming an unknown `program_id` warn-drops
the same way.

## Transient pooling

Transients are pooled by resolved size and realized format, and the pool is
shared across programs and persistent across dispatches — a repaint reuses
its allocations. Within one dispatch, the executor assigns physical textures
by live range: a transient's texture is reusable once the last pass reading
it has recorded, strictly before the next holder's first write, so a pass
never samples a texture it is simultaneously attached to. A ping-pong chain
of any length settles on two physical allocations per size-and-format class
— declaring one fresh transient per intermediate is cheap, and the graph
author never manages reuse by hand.

## Determinism

Nothing on the GPU rolls dice. Accidents — jitters, noise windows, spatter
positions — are pre-rolled by the authoring actor into the uniform blob, and
shared noise fields upload once per canvas size as ordinary textures. This
keeps a dispatch a pure function of its bindings and blob, which is what
makes parity testable: the convention is a CPU implementation as the oracle
and a `SubstrateHarness` similarity scenario over the program's output,
thresholded rather than bit-exact, since an iterated-tap GPU blur
legitimately differs from a CPU running sum in the last bits. Per-operation
confidence comes from small single-pass scenarios;
[`aether-render/tests/program_scenario.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-render/tests/program_scenario.rs)
is the canonical set.

## Chassis behavior

- **Desktop** executes programs. A `register` sent before the render GPU
  boots (before the first window attaches) replies `Err` rather than
  parking.
- **Headless** replies `Err` to `register` (fail-fast, the same as
  `create_texture`) and absorbs `dispatch` / `destroy` as no-ops, so a
  desktop-built component mailing them does not warn-storm.
- **SubstrateHarness** executes programs for real — it has a wgpu adapter —
  which is what makes a parity scenario an ordinary `cargo test`. Driverless
  machines skip such tests cleanly.
- The minimal hub chassis installs no `aether.render` mailbox, so program
  mail cannot resolve there.

## The worked consumer: the wash

The watercolour easel in
[`aether-puppet/src/easel/program/`](https://github.com/iamacoffeepot/aether/tree/main/crates/aether-puppet/src/easel/program)
is the large-scale consumer and the best reference for program authoring at
scale. Its develop is one registered program of several hundred passes laid
statically from the palette — coverage masks, separable blurs, thresholds,
rims, granulation, flow smears, coat absorption, and a final composite into
an `Rgba8` sheet binding — over a dozen `R32Float` data-plane bindings.
Everything that varies per develop rides the uniform blob: the sequencer
bump-allocates one window per operation while laying the graph, and the
dispatch encoder writes the palette's parameters and the pre-rolled accident
stream into those windows over a zeroed base, so an absent region is
neutralized through zeroed strengths rather than restructured. Its float
chains are laid pass-by-pass rather than repeated, per the
[repeat semantics](#repeats) above, and its CPU implementation remains the
oracle its parity scenarios compare against.

## Where to read more

- The decision record — [ADR-0170](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0170-authored-render-programs.md).
- A minimal end-to-end walkthrough —
  [Authoring a render program](../recipes/authoring-a-render-program.md).
- The texture registry, `Writable` usage, and the `R32Float` data-plane
  format — [Rendering & camera](rendering.md).
- The exact kind schemas —
  [`aether-render/src/kinds.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-render/src/kinds.rs),
  or `describe_kinds` with prefix `aether.render.program` against a live
  engine.
