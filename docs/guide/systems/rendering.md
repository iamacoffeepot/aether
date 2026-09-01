# Rendering & camera

> **Governing ADRs:** [ADR-0025](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0025-art-direction-and-renderer-scope.md)
> (the art direction the renderer serves), [ADR-0066](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0066-per-component-trunk-rlibs-for-shared-types.md)
> (where the render and camera kinds live), [ADR-0074 §Decision 7](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0074-unified-actor-model-for-substrate-and-guests.md)
> (camera folds into the render mailbox), and [ADR-0173](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0173-render-device-loss-recovery-contract.md)
> (the internal device-loss contract). The model — world-space geometry, a
> single `view_proj` uniform, a camera that is an ordinary actor publishing the
> matrix — is **stable**.

The substrate owns the GPU. An actor that wants something drawn mails geometry
to one mailbox, `aether.render`, as ordinary fire-and-forget mail. The geometry
is world-space triangles; the substrate multiplies every vertex by a single 4×4
`view_proj` matrix to produce the on-screen frame. That matrix is the only
camera concept the renderer knows about, and it arrives the same way the
geometry does — as mail. A **camera** is any actor that computes a `view_proj`
and publishes it; the renderer applies whatever the latest one was.

## Why it exists

The renderer serves the generation loop, not graphics fidelity
([ADR-0025](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0025-art-direction-and-renderer-scope.md)):
chunky low-poly flat-shaded forms with palette-indexed per-vertex color, enough
to make generated content feel alive. That target makes the caller surface
small on purpose — submit triangles, set a matrix — so a drawing component or a
camera is a few lines of mail rather than a pipeline to configure.

The load-bearing decision is that **the camera is not a renderer feature**. The
substrate applies one `view_proj` uniform and reads it from mail; it never owns
a camera, a projection mode, or a controller. So camera logic — orbit, top-down,
follow, whatever a game needs — lives in user space as an ordinary actor, and is
swappable by loading a different one. The renderer stays a thin matrix-applier;
everything expressive about how the world is framed is a component decision. The
alternative, a privileged camera baked into the renderer, would pull projection
policy and input handling into the substrate and make every new framing mode a
substrate change.

Geometry is **world-space** for the same reason: a drawing component emits where
things are, not where they land on screen. The camera's matrix does the
world→clip transform at draw time, so the same geometry reframes for free when
the camera moves, and two components drawing into one frame share a coordinate
system without coordinating.

## What it does

**One mailbox, a small kind set.** Everything addresses `aether.render`, owned by
the `RenderCapability` actor. It handles these payload kinds:

| Kind | Shape | Semantics |
|---|---|---|
| `aether.draw_triangle` | `{ verts: [Vertex; 3] }`, cast-shaped | per-tick geometry; accumulates into the frame |
| `aether.view_projection` | `{ view_proj: [f32; 16] }`, cast-shaped | the world→clip matrix; latest value wins |
| `aether.render.create_texture` | `{ width, height, format, sampling, usage, pixels }` → `create_texture_result` | register an `Rgba8`, `R8`, `R32Float`, `R16Float`, or `Rgba16Float` texture; reply carries the `texture_id` |
| `aether.render.update_texture` | `{ texture_id, x, y, width, height, pixels }` | overwrite a sub-rect of a texture (atlas growth) |
| `aether.render.destroy_texture` | `{ texture_id }` | release a registered texture; fire-and-forget |
| `aether.render.draw_textured_quads` | `{ texture_id, space, clip, blend, quads }` | per-tick textured alpha-blended quads; accumulates into the frame |
| `aether.render.draw_solid_quads` | `{ space, clip, quads }` | per-tick flat-colored alpha-blended rects; accumulates into the frame |
| `aether.render.draw_screen_triangles` | `{ clip, triangles }` | per-tick window-pixel triangles at any orientation; accumulates into the frame |
| `aether.render.material.textured` | `{ texture_id, blend, rects }` | per-tick depth-tested world-space textured rects |
| `aether.render.material.coverage` | `{ texture_id, rects }` | per-tick depth-tested world-space coverage bands from an R8 texture |
| `aether.render.capture_frame` | `{ mails, after_mails }` | atomic "set state, read back a PNG, clean up" |
| `aether.render.program.register` | `{ wgsl, bindings, transients, passes }` → `program.register_result` | register an authored render program (ADR-0170); reply carries the `program_id` |
| `aether.render.program.dispatch` | `{ program_id, bindings, uniforms }` | execute a registered program once at the next frame record; fire-and-forget |
| `aether.render.program.destroy` | `{ program_id }` | release a registered program; fire-and-forget |

A `Vertex` is `{ x, y, z, r, g, b }` — a world-space position plus a per-vertex
color. One `DrawTriangle` is three of them; a component batches many per envelope
via `send_many` (each triangle is `DRAW_TRIANGLE_BYTES` on the wire).

**Textured quads are the generic image surface** ([ADR-0105](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0105-text-rendering.md)).
`create_texture` stages `Rgba8` or `R8` pixels under a session-scoped
`texture_id` (the reply hands it back); `draw_textured_quads` then draws a batch
of quads sampling that texture, each carrying a pixel-unit rect, a uv sub-rect,
and an RGBA tint. `R8` samples contribute their scalar value in the red channel
(`vec4(r, 0, 0, 1)`), which is mainly a substrate for material passes; ordinary
sprite/text atlas callers use `Rgba8`. `destroy_texture` releases a registered
texture when the producer knows it is no longer used; headless absorbs it as a
no-op.

`blend` picks how the sampled texel lays over what is already there, and the
choice is about what the source's colour channels already carry. `Straight` —
the default, and what an uploaded image or a glyph atlas wants — treats colour
and coverage as independent and weights one by the other. `Premultiplied` is for
a source whose colour has already been scaled by its own coverage, which is what
a texture written by a [render program](render-programs.md) always is: a
fragment pass alpha-blends onto a transparent clear, so writing `(colour, a)`
stores `(colour * a, a)`. Compositing that as `Straight` weights it a second
time and squares its coverage, so a half-covered texel arrives at a quarter
strength. `material.textured` carries the same field with the same meaning. An
opaque source is unaffected either way, which is why the distinction only
surfaces once a program's output is partially transparent.

The create carries two role knobs (ADR-0170). `sampling` selects `Linear`
filtering for color content or `Nearest` for label planes whose texel values
are identities — interpolating between region labels would manufacture values
no texel holds. `usage` selects `Sampled` (CPU-staged pixels, the default role
above) or `Writable` — a GPU render target created without staged pixels and
cleared to transparent black at realization, the output surface authored
render programs draw into; `update_texture` against a writable texture
warn-drops, since it has no CPU staging. Two formats store data planes rather than
colour. `R32Float` keeps one `f32` per texel; core WebGPU cannot linear-filter
it, so it requires `Nearest` sampling and binds through a non-filtering layout.
`R16Float` keeps one `f16` — about eleven bits of mantissa — and is filterable,
which is what a pass reading between texels needs. `Rgba16Float` carries four
such filterable lanes when several independent quantities share one pass.
All three are read by authored-program passes; the color material and overlay
paths are not data-plane consumers.

**Authored render programs** ([ADR-0170](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0170-authored-render-programs.md))
put actor-owned per-pixel work on the GPU without a substrate change.
`program.register` carries one WGSL module (fragment entry points only — the
substrate owns a fullscreen-triangle vertex stage) plus a declared graph:
`bindings` are the registry textures a dispatch supplies, `transients` are
executor-pooled intermediates (each an extent — full reference size or an
integer divisor — plus a format), and each pass names its fragment entry
point, input slots, output slot, byte-window into the dispatch's uniform blob,
and an optional repeat with per-iteration uniform stride. Validation happens
at register, once — naga over the WGSL, then the graph (entry points exist,
slots written before read, windows cover the shader's uniform block), then
wgpu pipeline creation under an error scope — so every failure class is a
distinguishable `register_result` `Err { reason }` and a bad program never
crashes the substrate. `program.dispatch` executes the passes once at the next
frame record, before the material and overlay passes, so drawing a program's
writable output texture in the same frame shows the freshly computed pixels;
runtime binding mismatches warn-drop naming the program, pass, and binding.
Headless replies `Err` to `register` and absorbs `dispatch` / `destroy`.
The full contract — slots, extents, uniform windows, repeat semantics,
validation classes, pooling, determinism conventions — is the subject of
[Authored render programs](render-programs.md).

Quads draw through a second alpha-blended pipeline in an overlay pass recorded
after the world pass, so they always land on top. The accumulate-per-frame
contract matches `draw_triangle`: resend the batch every frame it should appear.
The batch's `space` selects the projection — `Screen` rects are window pixels
drawn under an ortho derived from the surface size; `World` anchors the quad in
the scene through the camera's `view_proj`. `Screen`-space quads draw today; the
`World` projection rides the same vocabulary and lands with the world-anchor
path. Sprites, HUD images, and the `aether.text` capability all compose this
surface.

**Screen triangles are the overlay's free-form primitive.**
`draw_screen_triangles` takes triangles whose three corners are window pixels
— top-left origin, y down, one linear RGBA per corner interpolated across the
face — and records them in the same overlay pass, through the same pipeline, in
submission order with the quad batches. Either winding draws; the batch carries
the same optional `clip` scissor. It exists because 2D content built from
rotated geometry had no aspect-correct path: a quad is `{x, y, width, height}`
with no orientation, and `draw_triangle` is world-space, so with no camera
loaded its identity `view_proj` spans `-1..=1` on both axes and stretches
everything by the window's aspect ratio. Pixel coordinates are absolute, so a
ribbon at an angle, a gauge, or a graph edge holds its proportions on any
window without a camera actor publishing a projection for flat content.

**World-space materials are textured and depth-tested** ([ADR-0140](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0140-render-material-pass.md)).
The material pass records after the triangle pass and before the screen overlay,
loading the main pass depth buffer with writes disabled. Components send typed
material draw kinds rather than shader source. `aether.render.material.textured`
draws sampled rects with a tint. `aether.render.material.coverage`
requires an R8 texture, thresholds at iso 127.5, and renders a body/rim band
from the rect parameters. Each rect carries its own `right`/`up` basis: draped
planar content passes the world axes, while content registered to a view — an
underpainting standing behind its subject — orients the rect toward the eye. Both are immediate-mode: resend the batch every frame
or it disappears on the next commit-current frame.

**The `view_proj` uniform, latest wins.** The substrate holds one column-major
4×4 matrix and uploads it verbatim to the shader each frame (column-major matches
wgpu's uniform layout, so the 64 bytes upload with no transpose). Each
`aether.view_projection` mail overwrites it wholesale; nothing blends or stacks. Before
any camera publishes, the matrix is identity, so vertices render in clip space
1:1.

**Depth test is on.** The offscreen target carries a `Depth32Float` depth buffer
tested `LessEqual`, so **larger world-z draws on top**. The convention that
follows: floors and backdrops sit at `z = 0`, movers at `z ≥ 0.1`. Geometry at
the same depth draws in submission order.

**Geometry is retained per tick.** `DrawTriangle` mail accumulates into a
per-frame buffer; when the frame records, that buffer becomes the frame and the
accumulator resets. A component redraws its geometry every frame it wants it
visible — stop emitting and the geometry is gone next frame. When a frame
records with nothing freshly emitted (a capture that didn't advance a tick), the
renderer replays the last submitted geometry, so a still frame shows what the
last live frame drew.

**Device loss is generation-aware and bounded.** Every installed wgpu device
has an internal generation. The first frame after that generation reports loss
makes one replacement attempt; callbacks arriving late from an older device
are ignored. A complete replacement is published at once — fresh built-in
pipelines and targets plus rebuilt registry state — rather than exposing a
partly reconstructed GPU. Failure emits one structured error and makes render
terminally unusable for the session: request/reply GPU operations and captures
return `Err`, while fire-and-forget draw, update, dispatch, and destroy mail is
warning-dropped. It does not retry or spin.

The surfaceless `SubstrateHarness` replaces its fixed offscreen target. A
desktop runtime instead walks every retained window in ascending `WindowId`
order. The first canonical window selects the replacement adapter, device, and
surface format; every later window must attach to that same context and format.
Only after all surfaces succeed does the runtime replace the full target map,
preserving each `WindowId` and occlusion flag together with the shared GPU and
wireframe overlay. If any later surface fails, none of the staged surfaces or
device state becomes live and the whole render capability becomes unusable.

Public ids do not change across a successful replacement. Sampled textures
upload again from their retained CPU pixels and registered geometry realizes
again from its retained vertex/index bytes. GPU-only writable textures keep
their ids but restart transparent; an actor that needs their contents sends its
ordinary program dispatch on the next repaint. A capture that was ready but
had not begun may cross the successful transaction and record once. Loss with
an ambiguous submission, poll, map, or readback instead returns that capture's
`Err`; its frame is not replayed and its `after_mails` are not released twice.
All of this is host policy — there is no recovery kind, generation callback, or
guest-visible wire change.

**The production headless chassis absorbs draw and camera mail.** It composes
`HeadlessRenderCapability` on the same `aether.render` mailbox:
`DrawTriangle`, `aether.view_projection`, `update_texture`, `destroy_texture`,
`draw_textured_quads`, `draw_solid_quads`, `draw_screen_triangles`, and
`aether.render.material.*` no-op (a desktop-built
component mailing them every frame doesn't warn-storm), and
`aether.render.capture_frame` and `create_texture` reply `Err` so a request
fails fast instead of hanging. The minimal hub chassis does not install an
`aether.render` mailbox at all, so render mail cannot resolve there. `SubstrateHarness`
instead composes the real offscreen `RenderCapability` for render/capture tests.

## How to use it

There are two seats: a component drawing into frames, and an agent staging a
frame to read it back.

**From a component — submit on the `Render` stage.** A render-producing actor
computes its per-frame state on `Tick` and submits geometry on the `Render`
lifecycle stage, so the submission integrates the fully-settled cross-actor state
of the frame rather than racing other actors' tick handlers
([ADR-0082](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0082-application-declared-lifecycle-sequence.md)).
Both are frame-lifecycle stages, subscribed on `aether.lifecycle` from the `wire`
hook:

```rust
fn wire(&mut self, ctx: &mut WireCtx<'_, '_>) {
    let lifecycle = ctx.actor::<LifecycleCapability>();
    lifecycle.subscribe::<Tick>();
    lifecycle.subscribe::<Render>();
}

#[handler::single]
fn on_render(&mut self, ctx: &mut WasmCtx<'_>, _render: Render) {
    ctx.actor::<RenderCapability>().send_many(&self.triangles);
}
```

Address the cap by type — `ctx.actor::<RenderCapability>()` — and send
`DrawTriangle`s (and, if you're a camera, an `aether.view_projection`). On a chassis whose
lifecycle graph omits `Render` (headless), subscribing to it rejects fail-fast at
wire time, and the actor simply never submits — a no-op where there's no GPU
anyway.

**From an agent over MCP — stage, then capture.** Use `capture_frame`: its
`mails` bundle dispatches before the readback (the state that should appear) and
`after_mails` after (cleanup), all around one synchronous PNG read. So to see a
camera change, stage the `aether.kit.camera.*` driver mail (or a `DrawTriangle`
directly) in `mails` and read the frame back inline. The renderer's retained
geometry means a capture that doesn't advance a tick still shows the last live
frame.

## How to extend or reuse it

- **A new camera mode** is component work, not substrate work. `aether-kit-commons`'s
  `camera` export is the worked example: it hosts N named cameras, advances each
  on `Tick`, and publishes the active one's `view_proj` on `Render`. It boots a
  default `"main"` camera in orbit mode and exposes driver kinds —
  `aether.kit.camera.{create, destroy, set_active, set_mode, orbit.set, topdown.set}`
  — for adding cameras and poking their parameters live. A new mode (follow,
  cinematic, free-fly) is a new `view_proj` computation in a camera component;
  the renderer needs no change because it only ever applies the matrix it's
  handed. Peers that need the current eye send the source-bound
  `aether.kit.camera.eye` request. Its `aether.kit.camera.eye_result` reply
  carries the active orbit or top-down eye as world-space `(x, y, z)`, or
  `None` while the active binding is absent or no longer live. This is a
  request/reply read-back, not a subscription stream; the camera still
  publishes only `view_proj` to `aether.render`. Loaded by the
  `aether_kit_commons@aether.kit.camera` selector, the camera answers at
  `aether.component/aether.embedded:aether.kit.camera` — the address `LoadResult.name` hands
  back.
- **Driving a camera from the keyboard** is a peer component's job, not the
  camera's. `aether-kit-commons`'s `camera-controller` export subscribes `Key` /
  `KeyRelease` / `Tick`, keeps a shadow of the pose it drives, and mails
  `aether.kit.camera.orbit.set` / `aether.kit.camera.topdown.set` deltas to a peer
  camera — WASD pan the target across the ground, the arrows yaw and pitch, Z/X
  dolly the distance, and an idle tick produces no mail. It loads by the
  `aether_kit_commons@aether.kit.camera-controller` selector with an
  `aether.kit.camera-controller.config` init-config that picks the target
  camera, mode, per-tick rates, and clamps, so the camera stays a pure
  projection state machine while the keyboard policy lives in the controller.
- **A new drawing component** subscribes the `Render` stage and emits
  `DrawTriangle`s in world space, with `z` chosen against the depth convention
  (backdrop at `z = 0`, movers above). Multiple components can draw into one
  frame; they share the world coordinate system and the active camera with no
  coordination beyond the depth ordering.
- **Mesh authoring** is a layer above this one: a component that loads mesh files
  and replays their triangles to `aether.render` each frame. The DSL, parser,
  tessellation, OBJ compatibility, and viewer path are covered in
  [Mesh authoring](mesh-authoring.md).

## Where to read more

- The art direction the renderer serves —
  [ADR-0025](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0025-art-direction-and-renderer-scope.md).
- Where the render and camera kinds live, and why a camera is an ordinary
  component —
  [ADR-0066](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0066-per-component-trunk-rlibs-for-shared-types.md).
- Camera folding into the render mailbox —
  [ADR-0074 §Decision 7](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0074-unified-actor-model-for-substrate-and-guests.md).
- The textured-quad surface text and sprites compose, and the screen-vs-world
  projection split —
  [ADR-0105](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0105-text-rendering.md).
- The `Tick` / `Render` frame stages and why submission waits for settlement —
  [ADR-0082](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0082-application-declared-lifecycle-sequence.md);
  the `wire` hook and writing handlers — [Components & lifecycle](components.md).
- Subscribing input and lifecycle stages from a component —
  [Input streams](input.md).
