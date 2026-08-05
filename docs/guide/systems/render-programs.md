# Authored render programs

> **Governing ADRs:**
> [ADR-0170](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0170-authored-render-programs.md)
> (authored render programs) and
> [ADR-0171](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0171-authored-draw-passes.md)
> (authored draw passes). The records hold the reasoning and the rejected
> alternatives; this chapter documents the shipped surface. The kinds live in
> [`aether-render/src/kinds.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-render/src/kinds.rs),
> the registry and executor in
> [`aether-render/src/runtime/program/`](https://github.com/iamacoffeepot/aether/tree/main/crates/aether-render/src/runtime/program),
> the geometry registry in
> [`aether-render/src/runtime/geometry.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-render/src/runtime/geometry.rs),
> and the stage primitives in
> [`aether-substrate/src/render/program.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-substrate/src/render/program.rs).

An authored render program puts actor-owned per-pixel code on the GPU. The
actor registers one WGSL module plus a declared pass graph; the substrate
compiles, validates, and executes it without knowing what it paints. The medium
— a watercolour develop, a post-process style pass, a cellular-automaton step —
stays with the actor that authors the look, and the substrate stays a thin
executor of programs.

A pass comes in two classes. A **fragment pass** runs the substrate's
fullscreen triangle under an authored fragment entry point, so every pixel of
the output is touched once. A **draw pass** rasterizes a bound geometry through
an authored vertex entry point, so what the output receives is whatever the
triangles cover. Both classes share one vocabulary — the same slots, extents,
uniform windows, and validation taxonomy — and both may appear in one graph.

## Mental model

A program has two halves with different lifetimes:

- **Structure is fixed at register.** The WGSL, the slot declarations, and the
  pass sequence are validated and compiled once, and the reply hands back a
  session-scoped `program_id`. A structurally present but unneeded pass is
  neutralized through its uniforms (a zeroed contribution costs one cheap
  pass); restructuring means registering a new program.
- **Data varies per dispatch.** Each `dispatch` names the registry textures the
  run reads and writes, names one registry geometry per declared geometry slot,
  and carries one uniform byte blob the passes window into. Register once,
  dispatch per repaint or per frame with fresh uniforms.

A program reads and writes **registry textures** — the same session-scoped
textures `aether.render.create_texture` registers (see
[Rendering & camera](rendering.md)). Its result lands in a texture created
with `usage: Writable`, so the material and overlay passes sample a program's
output exactly as they sample an uploaded one. There is no readback anywhere in
the loop.

A draw pass additionally reads a **registry geometry** — session-scoped vertex
and index bytes registered by `aether.render.create_geometry`, resident on the
GPU across dispatches. Geometry keeps a lifetime of its own between those two:
uploaded when the subject loads, re-read by every dispatch after that.

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

Reach for a **draw pass** inside that program when the per-pixel work needs a
view of resident geometry: a plane bake that rasterizes a mesh's class labels,
tone, and facing through the current camera; a depth-sorted layering of several
meshes into one target; an outline or coverage plane a later fragment pass
reads. The alternative — rasterizing on the CPU and uploading the result as a
texture every repaint — costs a full-canvas pixel shipment per repaint, which
is what a draw pass removes.

## Public mail surface

Every kind below addresses the `aether.render` mailbox.

| Mail kind | Rust payload | Contract |
|---|---|---|
| `aether.render.program.register` | `ProgramRegister { wgsl, bindings, transients, geometries, depth_transients, passes }` | validate + compile; reply `aether.render.program.register_result` / `ProgramRegisterResult` (`Ok { program_id }` / `Err { reason }`) |
| `aether.render.program.dispatch` | `ProgramDispatch { program_id, bindings, geometries, uniforms }` | fire-and-forget; execute once at the next frame record |
| `aether.render.program.destroy` | `ProgramDestroy { program_id }` | fire-and-forget release, mirroring `destroy_texture` |
| `aether.render.create_geometry` | `CreateGeometry { layout, vertices, indices }` | validate + stage; reply `aether.render.create_geometry_result` / `CreateGeometryResult` (`Ok { geometry_id }` / `Err { reason }`) |
| `aether.render.update_geometry` | `UpdateGeometry { geometry_id, vertices, indices }` | fire-and-forget in-place replacement against the created layout |
| `aether.render.destroy_geometry` | `DestroyGeometry { geometry_id }` | fire-and-forget release, mirroring `destroy_texture` |

`program_id` and `geometry_id` are session-scoped and assigned like texture and
instrument identifiers. A rejected register or create consumes no id, so
accepted ids stay dense. Destroying a program releases its compiled pipelines;
pooled transient textures stay in the shared pool for other programs.
Destroying a geometry releases its staged bytes and any realized GPU buffers,
and the released id is never handed out again.

A fragment-only program leaves `geometries` and `depth_transients` empty and
registers exactly as it does with no draw pass anywhere in the graph. Both
remain required fields on the mail — the codec rejects a missing field rather
than defaulting it — so send `[]`.

## The geometry resource

A geometry is a triangle list: packed vertex attribute bytes, 32-bit indices,
and the layout that says how to read them. It carries no material, no
transform, and no meaning — what the attributes stand for is the authoring
actor's business, and the substrate binds them where the layout says.

### Layout vocabulary

`layout: Vec<VertexAttribute>` declares the attributes in packing order. Each
`VertexAttribute` is a `location` (the WGSL `@location` index the vertex stage
binds it at) and a `format` from a closed set:

| `VertexFormat` | Bytes | Read in WGSL as | Typical use |
|---|---|---|---|
| `Float32x3` | 12 | `vec3<f32>` | positions, normals |
| `Float32x2` | 8 | `vec2<f32>` | texture coordinates |
| `Float32` | 4 | `f32` | a scalar attribute — a class label, a weight |
| `Uint8x4` | 4 | `vec4<u32>` | skinning joint indices |
| `Unorm8x4` | 4 | `vec4<f32>`, each channel `0.0..=1.0` | skinning weights, packed colors |

Attributes pack in declaration order with no padding, so the **stride** of one
vertex is the sum of its formats' byte widths — a position plus joint indices
plus weights is `12 + 4 + 4 = 20` bytes. Every format is a four-byte multiple,
so a stride always satisfies the buffer alignment the GPU wants. The
declaration order fixes the byte offsets; the `location` values fix the shader
side, and the two are independent — attributes may be declared in any location
order.

The set is closed. The four-channel integer and normalized forms are in it so
that a rigged mesh — joint indices and their weights alongside the position —
is expressible without a wider vocabulary.

### Lifecycle

`create_geometry` validates before it assigns an id, and each failure class
replies its own reason:

| Class | Reason shape |
|---|---|
| Empty layout | `geometry layout declares no attributes` |
| Vertex stride | `vertices length N does not divide evenly by the layout stride S` |
| Index width | `indices length N does not divide evenly by 4 (indices are 32-bit)` |
| Index range | `index I at position P is out of range for N vertices` |

Indices are little-endian `u32` values, and every one of them must fall inside
the vertex count the vertex bytes imply. Validation and id assignment are
CPU-side, so a `create_geometry` reply arrives without a booted GPU; the wgpu
vertex and index buffers are realized lazily at the first draw pass that uses
the geometry.

`update_geometry` replaces both byte arrays wholesale against the layout fixed
at create — the lengths may change, so a mesh may grow or shrink. It is
fire-and-forget: an unknown id, or a replacement that fails the create-time
rules, logs a warning under the `aether_render` target and leaves the previous
content staged. `destroy_geometry` releases the entry, and an unknown id
warn-drops the same way.

### Deformation is program content

Geometry uploads happen at subject-load cadence. When a subject animates, the
base mesh stays resident, the pose rides the dispatch's uniform blob as
matrices, and the authored vertex stage applies the skin from the joint-index
and weight attributes it reads. The substrate carries no skinning, deformer, or
mesh-manipulation vocabulary — deformation is program content, expressed in the
vertex stage the actor authors.

Sending `update_geometry` for a deforming mesh every frame puts the whole mesh
back on the mail path every frame, which is the cost resident geometry exists
to avoid. View-dependent geometry that is small by nature — a handful of
ribbons regenerated per frame — may ride per-frame `update_geometry` at that
scale. The measure is size and cadence together: a few kilobytes per frame is
mail like any other, a character mesh per frame is not.

## The pass graph

A registered graph declares five lists: `bindings`, `transients`, `geometries`,
`depth_transients`, and `passes`.

### Slots and extents

A **slot** is a texture a pass samples or renders into. Two declaration lists
exist:

- `bindings: Vec<SlotSpec>` — textures the dispatch supplies. Each dispatch
  names one registry texture id per declared binding, in order.
- `transients: Vec<SlotSpec>` — intermediates the executor owns and pools.
  A dispatch never names them; they exist so a chain of operations has
  scratch surfaces without the actor creating textures for them.

Two further lists declare what draw passes need, and both stay empty for a
graph with no draw pass in it:

- `geometries: Vec<GeometrySlotSpec>` — geometry slots the dispatch supplies
  by id, in the same shape bindings use. Each `GeometrySlotSpec` is a `layout`,
  the vertex layout the slot's geometry must have been created with; the
  register builds the pass's vertex buffer layout from it and checks the
  authored vertex stage against it.
- `depth_transients: Vec<SlotExtent>` — pooled `Depth32Float` targets draw
  passes clear and test against. These carry an extent alone, since their
  format is fixed.

A `SlotSpec` is a `format` (`Rgba8`, `R8`, `R32Float`, or `R16Float`) plus an
`extent`. The two float formats are both data planes — texels carrying
quantities rather than colours — and choosing between them is a question about
what the texel holds and who reads it. `R32Float` keeps a full 24-bit mantissa
and cannot be linear-filtered in core WebGPU, so it is what a label, an index,
or anything a pass reads point by point stands at. `R16Float` keeps about
eleven bits and *is* filterable, so a pass may take a fractional coordinate
through one and be handed the interpolation instead of computing it from four
point fetches — which is what a separable blur wants, since pairing its taps
into filtered reads halves its texture fetches.

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

Repeat semantics follow the output-slot write rules. For a fragment pass, the
first write a dispatch makes to each output slot clears it to transparent
black; later writes — a repeat's iterations, a second pass onto the same
slot — load the existing content. A draw pass states its own color load
semantic instead, described under [draw passes](#color-load-semantics) below.
What "load" composes is the pass's blend, and the blend is
fixed by the output format: `Rgba8` and `R8` outputs alpha-blend onto the
target, while a float data plane replaces it — which is what a pass writing a
quantity rather than a colour means by writing it. So a repeated pass accumulates on a blendable target, and on a float
target each iteration overwrites the last — a repeat there keeps only its
final iteration. A multi-step chain over float planes is therefore laid
structurally — each step its own pass entry with its own window — rather than
as one repeated pass; the [wash program](#the-worked-consumer-the-wash) below
is the worked example of that shape.

### The shader contract

Every pass names a **fragment** entry point in `entry_point`. For a fragment
pass the substrate owns the vertex stage — a fullscreen triangle — so that
entry point may take `@location(0) uv: vec2<f32>` — `(0, 0)` top-left to
`(1, 1)` bottom-right, texture convention — and returns
`@location(0) vec4<f32>`. A draw pass names a vertex entry point of its own,
and its fragment stage receives that stage's outputs instead; see
[the draw shader contract](#the-draw-shader-contract).

Bindings inside the shader, identical for both pass classes:

- `@group(0) @binding(0) var<uniform>` — the pass's uniform window.
- Group 1 — the pass's input slots, in declaration order, as texture /
  sampler pairs: input `n` is `@binding(2 * n)` (`texture_2d<f32>`) plus
  `@binding(2 * n + 1)` (`sampler`).

The sampler an input receives follows the bound texture: nearest when the
registry texture was created with `Nearest` sampling or its format cannot be
linear-filtered (`R32Float`), linear otherwise. Transients sample by their
declared format the same way. So a pass that wants a filtered read has to be
handed a plane standing at a filterable format — filtering is a property of
the texture, not of the pass, and a plane another program wrote stands at
whatever format that program declared.

## Draw passes

A pass becomes a draw pass by declaring `stage: PassStage::Draw(DrawPass { … })`
in place of `stage: PassStage::Fragment`. Everything else on the pass entry
keeps its meaning: the fragment `entry_point`, the `inputs` it samples, the
`output` it writes, the uniform window, and `repeat`.

### The draw declaration

```rust
DrawPass {
    vertex_entry_point: String,  // a @vertex entry in the program's module
    geometry: u32,               // index into ProgramRegister.geometries
    depth: Option<u32>,          // index into ProgramRegister.depth_transients
    load: PassLoad,              // Clear or Load, on the color output
}
```

Over mail, `stage` reads `"Fragment"` for a fragment pass and
`{ "Draw": { "vertex_entry_point": …, "geometry": …, "depth": …, "load": … } }`
for a draw pass, with `depth` as `null` when the pass does not depth-test.

`geometry` names the slot whose id the dispatch supplies, so one registered
program draws a different mesh each dispatch by naming a different geometry id
in the same slot. Two passes may name the same slot (drawing one mesh twice
with different uniforms) or different slots (drawing two meshes into one
target).

The rasterizer state is fixed and carries no declaration: an indexed triangle
list, 32-bit indices, counter-clockwise front faces, and no culling — winding
is the authoring actor's business, and both faces of every triangle are
painted.

### The draw shader contract

The vertex entry point reads the geometry slot's attributes at their declared
`@location` indices and returns at least `@builtin(position) vec4<f32>` in clip
space. It may read the pass's uniform window at `@group(0) @binding(0)`, which
binds for the vertex stage and the fragment stage alike — a view-projection
matrix, a pose, a per-pass depth all ride there.

The fragment entry point receives whatever the vertex stage returns as
varyings, and returns `@location(0) vec4<f32>` into the pass's color output. It
may also sample the pass's `inputs` through the group-1 texture and sampler
pairs, exactly as a fragment pass does — a draw pass that reads a mask texture
while rasterizing is an ordinary declaration.

A minimal pair, over a position-only layout:

```wgsl
struct DrawParams { color: vec4<f32>, depth: f32 }
@group(0) @binding(0) var<uniform> draw_params: DrawParams;

@vertex
fn vs_flat(@location(0) position: vec3<f32>) -> @builtin(position) vec4<f32> {
    return vec4<f32>(position.xy, draw_params.depth, 1.0);
}

@fragment
fn fs_flat() -> @location(0) vec4<f32> {
    return draw_params.color;
}
```

The register checks the vertex stage's interface against the geometry slot's
layout through naga's reflection: every `@location` the stage reads must be
declared by the layout, and the WGSL type must be the one that location's
format is consumed as (the [layout table](#layout-vocabulary) above). A layout
attribute the stage ignores is accepted — the vertex buffer supplies it and
nothing reads it. Agreement between the vertex stage's outputs and the fragment
stage's inputs is wgpu's own check, and a mismatch there surfaces as the
`pipeline creation failed` class.

### Depth

The declaration is the depth rule: **a pass depth-tests exactly when it names a
depth slot.** Naming one attaches that slot's `Depth32Float` target with
`LessEqual` comparison and depth writes on, so a smaller depth value wins.
Naming none rasterizes in draw order with no depth attachment at all, and the
later triangle paints over the earlier one.

Sharing is by naming. Within one dispatch, the first pass to name a given depth
slot clears it to the far plane and every later pass naming that same slot
loads it, so two consecutive draw passes agree on occlusion by naming one slot
— a mesh pass and a ribbon pass over the same depth hide each other correctly.
Naming two distinct slots gives two independent depth buffers, and the pool
never merges them however disjoint their use looks.

A depth slot must resolve to the same extent as the color output of every pass
naming it, since a depth attachment has to match the size of the color
attachment it tests for. A fragment entry point that writes
`@builtin(frag_depth)` needs a depth slot to write it into, and a pass that
writes it without declaring one rejects at register.

### Color load semantics

`load` states what the pass does to its color output before drawing:

- `PassLoad::Clear` — clear the output to transparent black, then draw.
- `PassLoad::Load` — load whatever the output already holds and draw over it.

The declaration is authoritative, so a layered bake states its own composition
rather than inferring it from position in the sequence. What `Load` finds is
whatever that texture already carries: an earlier pass's work within this
dispatch, the retained pixels of a writable binding from an earlier dispatch,
or — for a pooled transient no earlier pass wrote — whatever its physical
texture last held. Declare `Clear` on the first pass to write a slot unless
accumulating onto retained pixels is the intent.

Repeats compose with the declaration directly: under `Clear` the first
iteration clears and every later iteration loads, so a repeated draw pass
accumulates through its blend; under `Load` no iteration clears.
The blend is fixed by the output format, as it is for fragment passes —
`Rgba8` and `R8` alpha-blend, `R32Float` replaces.

### Channel-packed outputs

A pass writes one color output. Several planes of data therefore ride the
channels of one target: a bake that wants a region class, a key-light tone, and
a facing term packs them into the red, green, and blue channels of one `Rgba8`
output and a later fragment pass unpacks them. The surface declares no
multiple-render-target machinery, and a plane that needs full float precision
takes its own `R32Float` output from its own pass, whose single channel carries
it exactly.

Packing into `Rgba8` quantizes each channel to 256 levels, which suits labels
and low-frequency terms and does not suit an accumulator. Choose per plane:
labels and tone pack together, quantities that later math amplifies get a float
target.

## Register-time validation

Validation happens at register, once, and every failure class replies a
distinguishable `ProgramRegisterResult::Err { reason }` — a
bad-but-parseable program replies an error instead of crashing the substrate.
The classes, in check order:

| Class | Reason shape |
|---|---|
| WGSL | `invalid wgsl: …` — naga parse or validation failure |
| Empty graph | `program declares no passes` |
| Extent | `binding N: extent divisor must be at least 1` (also for transients and depth transients) |
| Geometry slot | `geometry slot N: layout declares no attributes`; `geometry slot N: layout declares location L twice` |
| Entry point | ``pass N: no fragment entry point named `X` in the module`` |
| Slot range | `pass N: binding slot B is out of range (M declared)` (also for transients) |
| Sequence | `pass N reads the output of pass P, which does not run before it`; `pass N input I reads transient T before any earlier pass writes it` |
| Self-read | `pass N reads its own output slot` |
| Uniform window | `pass N: uniform window (L bytes) is shorter than the shader's uniform block (B bytes)` |
| Repeat | `pass N: repeat count must be at least 1`; `pass N: repeat count C exceeds the supported maximum 4096` |
| Final output | `the final pass must write a dispatch binding (the program's result texture)`; `binding N: the program's output binding must declare Full extent …` |
| Pipeline | `pipeline creation failed: …` — a wgpu validation error caught by the register's error scope (for example a sampler-versus-layout mismatch naga alone cannot see) |

The draw-pass classes, checked for every pass that declares `stage: Draw`:

| Class | Reason shape |
|---|---|
| Vertex entry | ``pass N: no vertex entry point named `X` in the module`` |
| Geometry range | `pass N: geometry slot G is out of range (M declared)` |
| Unbound location | `pass N: the vertex stage reads @location(L), which geometry slot G's layout does not declare` |
| Attribute type | `pass N: the vertex stage reads @location(L) as vec2<f32>, but geometry slot G declares it Float32x3, which is consumed as vec3<f32>` |
| Depth range | `pass N: depth transient D is out of range (M declared)` |
| Depth extent | `pass N: depth transient D declares extent E, which does not match its color output's extent O — a depth attachment must be the size of the color attachment it tests for` |
| Undeclared depth | ``pass N: entry point `X` writes @builtin(frag_depth), so the pass must declare a depth transient to write it into`` |

The uniform-window class covers both stages of a draw pass: the window must
cover the block whichever stage reads it, so a pass whose vertex stage is the
only reader of group 0 still needs a window long enough for that block.

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
- a geometry count that disagrees with the registered graph;
- a binding naming an unknown texture id;
- a binding whose format disagrees with the declared `SlotSpec`;
- a binding whose size disagrees with its extent resolved against the
  reference;
- a non-`Writable` texture bound where the graph writes;
- a geometry slot naming an unknown geometry id;
- a geometry whose created layout disagrees with the slot's declared layout;
- a uniform blob shorter than a pass's window reach
  (`uniform_offset + (count - 1) * uniform_stride + uniform_length`);
- one texture bound as both a pass's input and its output.

Each drop logs a warning naming the program, pass, and binding, under the
`aether_render` target, into the render actor's log ring — the same
convention as an unknown texture id in `draw_textured_quads`. Query it with
the MCP `actor_logs` tool against mailbox `"aether.render"` (see
[Logging](logging.md)). A `destroy` naming an unknown `program_id` warn-drops
the same way.

GPU errors raised after those CPU checks retain the same address. Resource
setup is wrapped in validation, internal, and out-of-memory error scopes; each
pass is wrapped in a fresh set around its bind-group and command recording.
The resulting `aether_render` error includes the program id, error class,
supplied texture and geometry bindings, and either `phase = "setup"` or the
pass index, entry point, resolved input/output slots, and draw plan. A setup
error drops that dispatch's pass recording; a pass error stops its remaining
passes. Errors outside authored-program dispatches still reach the device's
generic uncaptured-error handler.

On the native wgpu-core backend these scope pops remove thread-local CPU scope
entries and return ready futures; they do not wait for submitted GPU work. The
ignored `empty_error_scope_cost` test is the repeatable adapter-backed
instrument for the per-setup/per-pass CPU cost.

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

Depth transients pool by resolved extent alongside color transients, with one
difference in policy: they are not packed by live range. Sharing a depth buffer
is what the declaration is for, so each declared depth slot that some pass
names gets its own physical texture, and two distinct slots never land on the
same one however disjoint their use looks. A declared depth slot no pass names
allocates nothing.

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
is the canonical set, and
[`draw_pass_scenario.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-render/tests/draw_pass_scenario.rs)
alongside it covers the draw stage in rasterized pixels — a triangle observed
through the overlay path, two passes sharing a depth transient, the register
classes, and a dispatch naming a geometry id that does not exist. The registry
lifecycle over mail has its own scenario in
[`geometry_scenario.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-render/tests/geometry_scenario.rs).

## Chassis behavior

- **Desktop** executes programs. A `register` sent before the render GPU
  boots (before the first window attaches) replies `Err` rather than
  parking.
- **Headless** replies `Err` to `register` and to `create_geometry`
  (fail-fast, the same as `create_texture`) and absorbs `dispatch` /
  `destroy` / `update_geometry` / `destroy_geometry` as no-ops, so a
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

- The decision records — [ADR-0170](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0170-authored-render-programs.md)
  for the program surface, [ADR-0171](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0171-authored-draw-passes.md)
  for the draw stage and the geometry resource.
- A minimal end-to-end walkthrough, fragment passes then a draw pass —
  [Authoring a render program](../recipes/authoring-a-render-program.md).
- The texture registry, `Writable` usage, and the `R32Float` data-plane
  format — [Rendering & camera](rendering.md).
- The exact kind schemas —
  [`aether-render/src/kinds.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-render/src/kinds.rs),
  or `describe_kinds` against a live engine with prefix
  `aether.render.program` for the program kinds, or `aether.render.` for the
  whole render family, geometry included.
