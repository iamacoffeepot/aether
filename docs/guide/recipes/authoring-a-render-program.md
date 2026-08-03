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
the result.

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

A dispatch names one registry texture per declared binding, in order, and
carries the uniform blob the passes window into. The blob here is two
little-endian `f32` values packed tight: threshold `0.5` in bytes 0..4 and
invert base `1.0` in bytes 4..8. Windows need no alignment — the executor
stages them aligned itself.

```jsonc
// send_mail → aether.render  (kind: aether.render.program.dispatch)
{
  "program_id": 0,
  "bindings": [SOURCE_ID, OUTPUT_ID],
  "uniforms": [0, 0, 0, 63, 0, 0, 128, 63]   // 0.5f32, 1.0f32, little-endian
}
```

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
      "params": { "program_id": 0, "bindings": [SOURCE_ID, OUTPUT_ID],
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
`ctx.actor::<RenderCapability>().send(&ProgramDispatch { program_id, bindings, uniforms })`.
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
  writes, or a blob shorter than a window's reach.
- **Output is stale** — a program executes only when dispatched; confirm the
  dispatch rode the same frame as the capture (stage it in `capture_frame`'s
  `mails`, as above).
