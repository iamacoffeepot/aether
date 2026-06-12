# Rendering & camera

> **Governing ADRs:** [ADR-0025](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0025-art-direction-and-renderer-scope.md)
> (the art direction the renderer serves), [ADR-0066](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0066-per-component-trunk-rlibs-for-shared-types.md)
> (where the render and camera kinds live), [ADR-0074 §Decision 7](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0074-unified-actor-model-for-substrate-and-guests.md)
> (camera folds into the render mailbox). The model — world-space geometry, a
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
| `aether.camera` | `{ view_proj: [f32; 16] }`, cast-shaped | the world→clip matrix; latest value wins |
| `aether.render.create_texture` | `{ width, height, pixels }` → `create_texture_result` | register an RGBA8 texture; reply carries the `texture_id` |
| `aether.render.update_texture` | `{ texture_id, x, y, width, height, pixels }` | overwrite a sub-rect of a texture (atlas growth) |
| `aether.render.draw_textured_quads` | `{ texture_id, space, quads }` | per-tick textured alpha-blended quads; accumulates into the frame |
| `aether.render.capture_frame` | `{ mails, after_mails }` | atomic "set state, read back a PNG, clean up" |

A `Vertex` is `{ x, y, z, r, g, b }` — a world-space position plus a per-vertex
color. One `DrawTriangle` is three of them; a component batches many per envelope
via `send_many` (each triangle is `DRAW_TRIANGLE_BYTES` on the wire).

**Textured quads are the generic image surface** ([ADR-0105](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0105-text-rendering.md)).
`create_texture` stages RGBA8 pixels under a session-scoped `texture_id` (the
reply hands it back); `draw_textured_quads` then draws a batch of quads sampling
that texture, each carrying a pixel-unit rect, a uv sub-rect, and an RGBA tint.
Quads draw through a second alpha-blended pipeline in an overlay pass recorded
after the world pass, so they always land on top. The accumulate-per-frame
contract matches `draw_triangle`: resend the batch every frame it should appear.
The batch's `space` selects the projection — `Screen` rects are window pixels
drawn under an ortho derived from the surface size; `World` anchors the quad in
the scene through the camera's `view_proj`. `Screen`-space quads draw today; the
`World` projection rides the same vocabulary and lands with the world-anchor
path. Sprites, HUD images, and the `aether.text` capability all compose this
surface.

**The `view_proj` uniform, latest wins.** The substrate holds one column-major
4×4 matrix and uploads it verbatim to the shader each frame (column-major matches
wgpu's uniform layout, so the 64 bytes upload with no transpose). Each
`aether.camera` mail overwrites it wholesale; nothing blends or stacks. Before
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

**Headless absorbs draw and camera mail.** The headless and hub chassis have no
GPU, so they compose `HeadlessRenderCapability` on the same `aether.render`
mailbox: `DrawTriangle`, `aether.camera`, `update_texture`, and
`draw_textured_quads` no-op (a desktop-built component mailing them every frame
doesn't warn-storm), and `aether.render.capture_frame` and `create_texture`
reply `Err` so an MCP call fails fast instead of hanging.

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
fn wire(&mut self, ctx: &mut FfiCtx<'_>) {
    let lifecycle = ctx.actor::<LifecycleCapability>();
    lifecycle.subscribe::<Tick>();
    lifecycle.subscribe::<Render>();
}

#[handler]
fn on_render(&mut self, ctx: &mut FfiCtx<'_>, _render: Render) {
    ctx.actor::<RenderCapability>().send_many(&self.triangles);
}
```

Address the cap by type — `ctx.actor::<RenderCapability>()` — and send
`DrawTriangle`s (and, if you're a camera, an `aether.camera`). On a chassis whose
lifecycle graph omits `Render` (headless), subscribing to it rejects fail-fast at
wire time, and the actor simply never submits — a no-op where there's no GPU
anyway.

**From an agent over MCP — stage, then capture.** Use `capture_frame`: its
`mails` bundle dispatches before the readback (the state that should appear) and
`after_mails` after (cleanup), all around one synchronous PNG read. So to see a
camera change, stage the `aether.camera.*` driver mail (or a `DrawTriangle`
directly) in `mails` and read the frame back inline. The renderer's retained
geometry means a capture that doesn't advance a tick still shows the last live
frame.

## How to extend or reuse it

- **A new camera mode** is component work, not substrate work. The reference
  `aether-camera` is the worked example: it hosts N named cameras, advances each
  on `Tick`, and publishes the active one's `view_proj` on `Render`. It boots a
  default `"main"` camera in orbit mode and exposes driver kinds —
  `aether.camera.{create, destroy, set_active, set_mode, orbit.set, topdown.set}`
  — for adding cameras and poking their parameters live. A new mode (follow,
  cinematic, free-fly) is a new `view_proj` computation in a camera component;
  the renderer needs no change because it only ever applies the matrix it's
  handed. Loaded, the camera answers at
  `aether.component/aether.embedded:camera` — the address `LoadResult.name` hands
  back.
- **A new drawing component** subscribes the `Render` stage and emits
  `DrawTriangle`s in world space, with `z` chosen against the depth convention
  (backdrop at `z = 0`, movers above). Multiple components can draw into one
  frame; they share the world coordinate system and the active camera with no
  coordination beyond the depth ordering.
- **Mesh authoring** is a layer above this one: a component that loads mesh files
  and replays their triangles to `aether.render` each frame. That surface is a
  deliberate blank while the DSL is in flux — the mesh-authoring page (its
  SUMMARY entry) is held empty until it lands.

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
