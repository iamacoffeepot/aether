# Authoring a render program

**Class:** drive. No recompile — use a running render-capable engine (desktop,
or a `SubstrateHarness` scenario). The walkthrough uses the MCP harness's
`send_mail` and `capture_frame`; a wasm component sends the same kinds through
`ctx.actor::<RenderCapability>()`, shown at the end. The contract behind every
step is [Authored render programs](../systems/render-programs.md)
([ADR-0170](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0170-authored-render-programs.md)).

The program built here is a two-pass ping-pong: pass one thresholds a source
image's red channel against a uniform value into a transient, pass two inverts
the transient from a second uniform value into a writable output texture. It is
the same graph the canonical harness scenario drives
([`program_scenario.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-render/tests/program_scenario.rs)),
small enough to verify by eye and complete enough to exercise every part of the
surface: bindings, a transient, two uniform windows in one blob, and drawing
the result. [A second walkthrough](#a-minimal-draw-pass) at the end rasterizes
a triangle through a draw pass, which adds the geometry resource and an
authored vertex stage
([ADR-0171](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0171-authored-draw-passes.md)).

## 1. Create the source and output textures

A program reads and writes registry textures. Create a 2 × 2 source with
staged pixels (`usage: Sampled`) and an empty 2 × 2 output render target
(`usage: Writable` — `pixels` must be empty; the texture clears to transparent
black when it is realized).

```jsonc
// send_mail → aether.render  (kind: aether.render.create_texture)
{
  "width": 2, "height": 2,
  "format": "Rgba8", "sampling": "Linear", "usage": "Sampled",
  // Row-major RGBA, top-down. Red channel per texel:
  // (0,0)=0.2, (1,0)=0.8, (0,1)=0.4, (1,1)=0.9.
  "pixels": [51,0,0,255, 204,0,0,255, 102,0,0,255, 230,0,0,255]
}
```

```jsonc
// send_mail → aether.render  (kind: aether.render.create_texture)
{
  "width": 2, "height": 2,
  "format": "Rgba8", "sampling": "Linear", "usage": "Writable",
  "pixels": []
}
```

Each replies `aether.render.create_texture_result` with
`{ "Ok": { "texture_id": … } }`. Keep both ids — the walkthrough below calls
them `SOURCE_ID` and `OUTPUT_ID`.

## 2. Register the program

The WGSL module declares fragment entry points only — the substrate owns the
vertex stage (a fullscreen triangle handing each fragment its
`@location(0) uv`). Both entry points here window the same one-float uniform
block shape at `@group(0) @binding(0)` and sample one input at the group-1
pair `@binding(0)` / `@binding(1)`:

```wgsl
struct WindowParams { value: f32 }
@group(0) @binding(0) var<uniform> window_params: WindowParams;
@group(1) @binding(0) var source_texture: texture_2d<f32>;
@group(1) @binding(1) var source_sampler: sampler;

@fragment
fn fs_threshold(@location(0) uv: vec2<f32>) -> @location(0) vec4<f32> {
    let lit = select(0.0, 1.0, textureSample(source_texture, source_sampler, uv).r >= window_params.value);
    return vec4<f32>(lit, lit, lit, 1.0);
}

@fragment
fn fs_invert(@location(0) uv: vec2<f32>) -> @location(0) vec4<f32> {
    let level = window_params.value - textureSample(source_texture, source_sampler, uv).r;
    return vec4<f32>(level, level, level, 1.0);
}
```

Register it with the declared graph: two bindings (source, output), one
transient for the ping-pong hop, and two passes. Pass 0 reads binding 0 and
writes the transient, windowing bytes 0..4 of the dispatch blob; pass 1 reads
pass 0's output through the `PassOutput` alias and writes binding 1, windowing
bytes 4..8.

```jsonc
// send_mail → aether.render  (kind: aether.render.program.register)
{
  "wgsl": "<the module above, as one string>",
  "bindings": [
    { "format": "Rgba8", "extent": "Full" },   // 0: the source
    { "format": "Rgba8", "extent": "Full" }    // 1: the output (written by the final pass; must be Full)
  ],
  "transients": [
    { "format": "Rgba8", "extent": "Full" }    // 0: the ping-pong hop
  ],
  "geometries": [],                            // no draw pass in this graph
  "depth_transients": [],                      // and so no depth targets
  "passes": [
    {
      "stage": "Fragment",
      "entry_point": "fs_threshold",
      "inputs": [ { "Binding": { "index": 0 } } ],
      "output": { "Transient": { "index": 0 } },
      "uniform_offset": 0, "uniform_length": 4,
      "repeat": null
    },
    {
      "stage": "Fragment",
      "entry_point": "fs_invert",
      "inputs": [ { "PassOutput": { "pass": 0 } } ],
      "output": { "Binding": { "index": 1 } },
      "uniform_offset": 4, "uniform_length": 4,
      "repeat": null
    }
  ]
}
```

The reply is `aether.render.program.register_result`:

```jsonc
{ "Ok": { "program_id": 0 } }
```

Validation happens here, once — naga over the WGSL, the graph checks
(entry points exist, slots written before read, windows cover the shader's
uniform block, the final pass writes a `Full`-extent binding), then wgpu
pipeline creation under an error scope. A failure replies
`{ "Err": { "reason": … } }` with the failing class named — see the
[validation table](../systems/render-programs.md#register-time-validation) —
and consumes no id.

## 3. Dispatch it

A dispatch names one registry texture per declared binding, in order, one
registry geometry per declared geometry slot (none here), and carries the
uniform blob the passes window into. The blob here is two little-endian `f32`
values packed tight: threshold `0.5` in bytes 0..4 and invert base `1.0` in
bytes 4..8. Windows need no alignment — the executor stages them aligned
itself.

```jsonc
// send_mail → aether.render  (kind: aether.render.program.dispatch)
{
  "program_id": 0,
  "bindings": [SOURCE_ID, OUTPUT_ID],
  "geometries": [],
  "uniforms": [0, 0, 0, 63, 0, 0, 128, 63]   // 0.5f32, 1.0f32, little-endian
}
```

Every field is required: the codec rejects a missing one rather than
defaulting it, so a fragment-only program still sends `"geometries": []`.

Dispatch is fire-and-forget: the passes record at the next frame, before the
world, material, and overlay passes, and the result persists in the output
texture until the next dispatch overwrites it. A runtime mismatch — a wrong
binding count, a texture whose format or size disagrees with the graph, a
blob shorter than a window — drops the whole dispatch with a warning in the
render actor's log ring (`actor_logs` on `"aether.render"`) and leaves the
frame intact.

## 4. See the output

The output texture samples like any registry texture. Draw it as a screen
quad and capture in one call, with the dispatch staged in the same `mails`
bundle so the freshly written pixels appear in the captured frame:

```jsonc
// capture_frame
{
  "mails": [
    { "recipient_name": "aether.render", "kind_name": "aether.render.program.dispatch",
      "params": { "program_id": 0, "bindings": [SOURCE_ID, OUTPUT_ID], "geometries": [],
                  "uniforms": [0, 0, 0, 63, 0, 0, 128, 63] } },
    { "recipient_name": "aether.render", "kind_name": "aether.render.draw_textured_quads",
      "params": { "texture_id": OUTPUT_ID, "space": "Screen", "clip": null,
                  "quads": [ { "x": 16.0, "y": 8.0, "width": 128.0, "height": 128.0,
                               "u0": 0.0, "v0": 0.0, "u1": 1.0, "v1": 1.0,
                               "tint": { "r": 1.0, "g": 1.0, "b": 1.0, "a": 1.0 } } ] } }
  ]
}
```

Threshold at `0.5` maps the left texel column (red `0.2`, `0.4`) to `0` and
the right column (`0.8`, `0.9`) to `1`; inverting from `1.0` flips them — the
captured quad shows a white left half and a black right half. The quad draw is
immediate-mode and must ride every frame it should appear; the program's
output pixels are retained and need no re-dispatch to keep being sampled.

## 5. Repaint and release

To recompute — new threshold, new source content — send another dispatch with
a fresh blob; the structure stays as registered. Anything that varies per run
belongs in the blob (including any randomness, pre-rolled on the CPU: the GPU
side is deterministic by convention). When the program is no longer needed:

```jsonc
// send_mail → aether.render  (kind: aether.render.program.destroy)
{ "program_id": 0 }
```

## A minimal draw pass

A draw pass rasterizes resident geometry through an authored vertex stage
instead of running the substrate's fullscreen triangle. The walkthrough below
paints one white triangle into a 16 × 16 writable texture and draws that
texture back to the screen — the same graph
[`draw_pass_scenario.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-render/tests/draw_pass_scenario.rs)
drives. It needs three things the fragment walkthrough did not: a geometry, a
geometry slot in the register, and a vertex entry point.

### 1. Upload the geometry

`create_geometry` takes a layout, the packed vertex bytes, and 32-bit indices.
The layout here is one attribute — a `Float32x3` position at `@location(0)`, so
the stride is 12 bytes and three vertices are 36 bytes. Positions are clip
space: the corners `(-0.8, -0.8)`, `(0.8, -0.8)`, and `(0.0, 0.8)`, each with
`z = 0`.

```jsonc
// send_mail → aether.render  (kind: aether.render.create_geometry)
{
  "layout": [ { "location": 0, "format": "Float32x3" } ],
  // Three vertices, little-endian f32: -0.8 is [205,204,76,191],
  // 0.8 is [205,204,76,63], 0.0 is [0,0,0,0].
  "vertices": [205,204,76,191, 205,204,76,191, 0,0,0,0,
               205,204,76,63,  205,204,76,191, 0,0,0,0,
               0,0,0,0,        205,204,76,63,  0,0,0,0],
  // One triangle: indices 0, 1, 2 as little-endian u32.
  "indices": [0,0,0,0, 1,0,0,0, 2,0,0,0]
}
```

The reply is `aether.render.create_geometry_result` with
`{ "Ok": { "geometry_id": 0 } }` — call it `GEOMETRY_ID`. A rejection names its
class: an empty layout, vertex bytes that do not divide by the stride, index
bytes that do not divide by four, or an index past the vertex count. The bytes
stay staged on the CPU until the first draw pass uses the geometry, when the
GPU buffers are created.

Both byte fields accept a literal array as above; for a real mesh, use the
harness blob embeds instead — `{ "$file": "/absolute/path/mesh.bin" }` reads
the bytes on the harness host and `{ "$base64": "…" }` decodes a base64 string.

### 2. Create the output texture

The program draws into a writable registry texture, exactly as the fragment
walkthrough's final pass did:

```jsonc
// send_mail → aether.render  (kind: aether.render.create_texture)
{
  "width": 16, "height": 16,
  "format": "Rgba8", "sampling": "Linear", "usage": "Writable",
  "pixels": []
}
```

Keep the id as `TARGET_ID`.

### 3. Register the draw program

The module declares one vertex entry and one fragment entry. The vertex stage
reads the position attribute for x and y and takes its clip depth from the
uniform window, which binds at `@group(0) @binding(0)` for the vertex stage and
the fragment stage alike; the fragment stage paints the window's color.

```wgsl
struct DrawParams {
    color: vec4<f32>,
    depth: f32,
}
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

`DrawParams` is a `vec4<f32>` followed by an `f32`, padded out to the struct's
16-byte alignment — 32 bytes, which is what the pass's `uniform_length` must
cover.

```jsonc
// send_mail → aether.render  (kind: aether.render.program.register)
{
  "wgsl": "<the module above, as one string>",
  "bindings": [
    { "format": "Rgba8", "extent": "Full" }      // 0: the output (Full, as the final pass writes it)
  ],
  "transients": [],
  "geometries": [
    { "layout": [ { "location": 0, "format": "Float32x3" } ] }   // 0: matches the created layout
  ],
  "depth_transients": [],                        // this pass does not depth-test
  "passes": [
    {
      "stage": { "Draw": { "vertex_entry_point": "vs_flat",
                           "geometry": 0, "depth": null, "load": "Clear" } },
      "entry_point": "fs_flat",
      "inputs": [],
      "output": { "Binding": { "index": 0 } },
      "uniform_offset": 0, "uniform_length": 32,
      "repeat": null
    }
  ]
}
```

The geometry slot's layout must be the layout the geometry was created with,
attribute for attribute — the register builds the vertex buffer layout from the
slot and checks `vs_flat`'s interface against it, and the dispatch checks the
supplied geometry against it again. Each mismatch replies its own named
reason — reading a location the layout does not declare is one class, reading
a declared location as the wrong WGSL type another.

### 4. Dispatch and see the triangle

The dispatch supplies one geometry id per declared slot, in order, alongside
the bindings and the uniform blob. The blob is the 32-byte `DrawParams` window:
white in the first 16 bytes, clip depth `0.5` in the next four, then padding.

Stage the dispatch and an overlay quad in one `capture_frame` so the freshly
drawn pixels land in the captured frame:

```jsonc
// capture_frame
{
  "mails": [
    { "recipient_name": "aether.render", "kind_name": "aether.render.program.dispatch",
      "params": { "program_id": 0, "bindings": [TARGET_ID], "geometries": [GEOMETRY_ID],
                  // color (1,1,1,1) then depth 0.5, padded to 32 bytes
                  "uniforms": [0,0,128,63, 0,0,128,63, 0,0,128,63, 0,0,128,63,
                               0,0,0,63,   0,0,0,0,    0,0,0,0,    0,0,0,0] } },
    { "recipient_name": "aether.render", "kind_name": "aether.render.draw_textured_quads",
      "params": { "texture_id": TARGET_ID, "space": "Screen", "clip": null,
                  "quads": [ { "x": 16.0, "y": 8.0, "width": 128.0, "height": 128.0,
                               "u0": 0.0, "v0": 0.0, "u1": 1.0, "v1": 1.0,
                               "tint": { "r": 1.0, "g": 1.0, "b": 1.0, "a": 1.0 } } ] } }
  ]
}
```

The captured quad shows a white triangle pointing up — apex near the top edge,
base along the bottom — over the cleared transparent black the `Clear` load
left everywhere the triangle does not cover. Clip `+y` is up on screen and maps
to the top of the sampled texture, which is why the apex at `y = 0.8` lands
there.

### 5. Add depth when passes must occlude each other

Depth arrives with the declaration: a pass depth-tests exactly when it names a
depth slot. Declare one `depth_transients` entry and name it from both passes
to make them agree on occlusion:

```jsonc
{
  // …bindings and wgsl as above. Two meshes now, so two geometry slots,
  // and the dispatch supplies two ids in this order.
  "geometries": [
    { "layout": [ { "location": 0, "format": "Float32x3" } ] },
    { "layout": [ { "location": 0, "format": "Float32x3" } ] }
  ],
  "depth_transients": [ "Full" ],
  "passes": [
    { "stage": { "Draw": { "vertex_entry_point": "vs_flat", "geometry": 0,
                           "depth": 0, "load": "Clear" } },
      "entry_point": "fs_flat", "inputs": [], "output": { "Binding": { "index": 0 } },
      "uniform_offset": 0, "uniform_length": 32, "repeat": null },
    { "stage": { "Draw": { "vertex_entry_point": "vs_flat", "geometry": 1,
                           "depth": 0, "load": "Load" } },
      "entry_point": "fs_flat", "inputs": [], "output": { "Binding": { "index": 0 } },
      "uniform_offset": 32, "uniform_length": 32, "repeat": null }
  ]
}
```

The first pass to name depth slot 0 clears it to the far plane; the second
loads it, so a nearer mesh drawn first survives a farther one drawn second
where they overlap. The second pass declares `load: "Load"` on the color output
for the same reason — `Clear` there would erase the first pass's work. Each
pass windows its own 32 bytes of a 64-byte blob, which is how they carry
different colors and depths. A depth slot must resolve to the same extent as
the color output of every pass naming it.

## From a wasm component

The same kinds flow through the typed capability handle. Registration is a
send whose reply arrives as ordinary mail, so the component keeps the id from
a `ProgramRegisterResult` handler and dispatches once it has one:

```rust
use aether_render::{ProgramDispatch, ProgramRegister, ProgramRegisterResult, RenderCapability};

fn wire(&mut self, ctx: &mut WireCtx<'_, '_>) {
    ctx.actor::<RenderCapability>().send(&self.build_register()); // a ProgramRegister value
}

#[handler::single]
fn on_registered(&mut self, _ctx: &mut WasmCtx<'_>, result: ProgramRegisterResult) {
    match result {
        ProgramRegisterResult::Ok { program_id } => self.program_id = Some(program_id),
        // Err is also the headless chassis's fail-fast reply — disable
        // the feature for the session rather than re-registering.
        ProgramRegisterResult::Err { reason } => tracing::warn!(%reason, "program register refused"),
    }
}
```

The dispatch then rides wherever the repaint cadence lives — a `Tick` or
`Render` handler, a settle gate — as
`ctx.actor::<RenderCapability>().send(&ProgramDispatch { program_id, bindings, geometries, uniforms })`.
A component that draws geometry sends its `CreateGeometry` on the same
reply-driven path as the register, keeping the `geometry_id` from a
`CreateGeometryResult` handler; the upload belongs to subject load, and every
frame after that changes only the uniform blob — an animated mesh poses through
matrices in that blob rather than through a fresh upload.
The in-tree consumer to study at scale is the watercolour easel's wash program
([`aether-puppet/src/easel/program/`](https://github.com/iamacoffeepot/aether/tree/main/crates/aether-puppet/src/easel/program)):
one static graph of several hundred passes, one uniform blob encoded per
develop.

## What to check when it fails

- **Register replies `Err`** — read the `reason`; each validation class names
  the offending pass and slot. Fix the graph or the WGSL and register again
  (nothing was consumed).
- **Dispatch shows nothing** — check `actor_logs` on `"aether.render"` for a
  warn-drop naming the program, pass, and binding; the usual causes are a
  binding list in the wrong order, a `Sampled` texture where the graph
  writes, a geometry list whose length or ids do not match the declared
  slots, or a blob shorter than a window's reach.
- **Output is stale** — a program executes only when dispatched; confirm the
  dispatch rode the same frame as the capture (stage it in `capture_frame`'s
  `mails`, as above).
- **A draw pass paints nothing where geometry should be** — the triangle may
  be outside the clip volume the vertex stage produced, or wound so it lands
  off-target. Culling is off, so both faces paint; check the positions and the
  depth the vertex stage writes, and whether an earlier pass's depth in a
  shared slot is rejecting the fragments.
- **A draw pass erases an earlier pass** — a draw pass's `load` is
  authoritative on its color output; a second pass onto the same slot wants
  `"Load"`, not `"Clear"`.
