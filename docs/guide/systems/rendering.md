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
| `aether.view_projection` | `{ view_proj: [f32; 16] }`, cast-shaped | the world→clip matrix; latest value wins |
| `aether.render.create_texture` | `{ width, height, format, pixels }` → `create_texture_result` | register an `Rgba8` or `R8` texture; reply carries the `texture_id` |
| `aether.render.update_texture` | `{ texture_id, x, y, width, height, pixels }` | overwrite a sub-rect of a texture (atlas growth) |
| `aether.render.destroy_texture` | `{ texture_id }` | release a registered texture; fire-and-forget |
| `aether.render.draw_textured_quads` | `{ texture_id, space, quads }` | per-tick textured alpha-blended quads; accumulates into the frame |
| `aether.render.material.textured` | `{ texture_id, rects }` | per-tick depth-tested world-space textured rects |
| `aether.render.material.coverage` | `{ texture_id, rects }` | per-tick depth-tested world-space coverage bands from an R8 texture |
| `aether.render.capture_frame` | `{ mails, after_mails }` | atomic "set state, read back a PNG, clean up" |

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
Quads draw through a second alpha-blended pipeline in an overlay pass recorded
after the world pass, so they always land on top. The accumulate-per-frame
contract matches `draw_triangle`: resend the batch every frame it should appear.
The batch's `space` selects the projection — `Screen` rects are window pixels
drawn under an ortho derived from the surface size; `World` anchors the quad in
the scene through the camera's `view_proj`. `Screen`-space quads draw today; the
`World` projection rides the same vocabulary and lands with the world-anchor
path. Sprites, HUD images, and the `aether.text` capability all compose this
surface.

**World-space materials are textured and depth-tested** ([ADR-0140](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0140-render-material-pass.md)).
The material pass records after the triangle pass and before the screen overlay,
loading the main pass depth buffer with writes disabled. Components send typed
material draw kinds rather than shader source. `aether.render.material.textured`
draws sampled rects in the world plane with a tint. `aether.render.material.coverage`
requires an R8 texture, thresholds at iso 127.5, and renders a body/rim band
from the rect parameters. Both are immediate-mode: resend the batch every frame
or it disappears on the next commit-current frame.

`aether.kit.world` stores painted overlay surfaces as one scalar coverage byte
per subcell. Its CPU contour marcher reconstructs the fixed 127.5 boundary and
caches the resulting `DrawTriangle` geometry per chunk. Coverage values between
0 and 255 put crossings between subcell centers, so shape stamps have smooth
edges while legacy binary masks retain their midpoint crossings.

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

**The production headless chassis absorbs draw and camera mail.** It composes
`HeadlessRenderCapability` on the same `aether.render` mailbox:
`DrawTriangle`, `aether.view_projection`, `update_texture`, `destroy_texture`,
`draw_textured_quads`, and `aether.render.material.*` no-op (a desktop-built
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
fn wire(&mut self, ctx: &mut WasmCtx<'_>) {
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

**Author a sub-cell world shape with one stamp.** Send this payload as
`aether.kit.world.stamp_hexagon` to the loaded `aether.kit.world` component:

```json
{
  "center": {
    "x_octimeters": 2048,
    "z_octimeters": 2048
  },
  "radius_octimeters": 768,
  "material": 3
}
```

That paints a Stone (`Material` byte `3`) hexagon centered at world cell
`(8, 8)`, with a three-meter center-to-vertex radius. The component generates
the six vertices, area-rasterizes the ring into `0..255` subcell coverage, and
remeshes every touched chunk and apron. Use `stamp_disc` with the same center,
radius, and material fields, or `stamp_polygon` with `points` containing named
`{ "x_octimeters": ..., "z_octimeters": ... }` world points. A stamp is
bounded to 1,024 polygon vertices, 4,096 subcells per axis, and 1,048,576
subcells of raster area, with a 33,554,432-operation conservative scanline
work budget; an oversized stamp paints nothing. Repeated stamps of the same
material max-compose coverage sample by sample. A different material takes
painter-order ownership at cell granularity and clears that cell's previous
mask before writing its new samples, because the overlay plane stores one
material per cell. The existing `set_cell_points` and `set_chunk` raw-array
paths remain available for direct plane authoring and save compatibility.

**Run repeatable terrain work through a bounded operator.** The world actor also
accepts `aether.kit.world.apply_brush` and
`aether.kit.world.run_automaton`. Every request carries the revisioned mark
reference that motivated it, raw execution geometry, and an explicit step and
subcell budget. Coordinates remain named records rather than positional arrays:

```json
{
  "source": { "id": { "0": 7 }, "revision": 3 },
  "path": [
    { "x_octimeters": 512, "z_octimeters": 512 },
    { "x_octimeters": 1536, "z_octimeters": 512 }
  ],
  "brush": {
    "radius_octimeters": 128,
    "spacing_octimeters": 128,
    "material": 3
  },
  "budget": { "max_steps": 16, "max_subcells": 8192 }
}
```

The shared `aether.kit.world.operator_result` reply echoes `source` and reports
`steps_run`, `subcells_written`, and named `touched_chunks`. If either budget is
exhausted, the reply is `Failed` with a typed step/subcell error and statistics
for the accepted prefix; the rejected over-cap write never occurs, and the
consistent partial world is still remeshed. `RunAutomaton` takes a named
`seed: { cell_x, cell_z }` and a `Grow { material, generations }` rule. Each
accepted automaton cell costs one step and all 256 of its material subcells.
These request kinds remain immediate mutations. To inspect the same bounded
work before changing committed terrain, wrap it in
`aether.kit.world.propose` as a named `ProposalOperation` variant:

```json
{
  "operation": {
    "ApplyBrush": {
      "request": {
        "source": { "id": { "0": 7 }, "revision": 3 },
        "path": [
          { "x_octimeters": 3968, "z_octimeters": 2048 },
          { "x_octimeters": 4224, "z_octimeters": 2048 }
        ],
        "brush": {
          "radius_octimeters": 128,
          "spacing_octimeters": 256,
          "material": 3
        },
        "budget": { "max_steps": 2, "max_subcells": 4096 }
      }
    }
  }
}
```

The `aether.kit.world.proposal_result` reply is `Staged` with a named
`proposal_id`, the ordinary mutation/operator result, and a deterministic
digest. The digest reports sorted named chunk addresses, the checked triangle
count, and optional named meter-space bounds of changed geometry. Staging does
not change the committed world or its mesh cache. Send
`set_proposal_preview { proposal_id: Some(...) }` to render that proposal in
place, or `None` to return to committed rendering. Preview rendering preserves
the committed cache's chunk-key ordering and resolves visible terrain-mark
heights against the proposed terrain.

Finish with `commit_proposal { proposal_id }` or
`discard_proposal { proposal_id }`. A successful commit returns the same
digest and advances the committed revision; concurrent peers staged against
the prior revision then reject commit or preview as `StaleProposal` rather
than overwriting the accepted terrain. Discard accepts fresh or stale
proposals. Unknown ids reject as `UnknownProposal`, a bounded operation that
touches nothing rejects as `NoTouchedChunks` without consuming an id, and an
exhausted session allocator rejects as `ProposalIdExhausted`.

One component session retains at most 64 staged proposals. An otherwise-valid
65th proposal rejects as `StagedProposalLimitReached` without allocating an id
or changing the committed world, mesh cache, or active preview. Committing or
discarding a retained proposal reopens one slot; a later proposal can use that
slot, but proposal ids remain monotonic and never reuse the removed id.

Proposal ids are monotonic from 1 and scoped to the loaded component session.
`replace_component` starts a fresh session: it drops proposals, preview,
revision, and allocator state, so an id from the replaced instance is unknown
before any new allocation. Direct bounded mutation, successful `load`, and
successful proposal commit each clear the active preview and advance the
committed revision. `set_region` and `load` remain immediate whole-world/table
operations rather than proposal variants. ADR-0143 records the merged
architectural direction and remains marked Proposed in its document.

**Pick and project revisioned terrain marks.** Load the MarkBook under its
default component name (`aether.kit.mark`) alongside the world component.
`aether.kit.world.pick_terrain` accepts a named meter-space ray rather than a
screen-space tuple:

```json
{
  "ray": {
    "origin": {
      "x_meters": 2.0,
      "y_meters": 8.0,
      "z_meters": 2.0
    },
    "direction": {
      "x_unitless": 0.0,
      "y_unitless": -1.0,
      "z_unitless": 0.0
    },
    "max_distance_meters": 16.0
  }
}
```

The typed result is either `Hit`, `Miss`, or `Rejected`. A hit reports
the continuous meter-space position, the owning `CellPos`, the sampled
surface height, the ray distance, and a nearest-octimeter `WorldPoint` that
can be passed directly to `aether.kit.mark.create`. Picking follows the
rendered top surface, including relief and water; missing/Void terrain is not a
mark anchor. The bounded march accepts only a present above-to-below bracket
whose two sides converge on the sampled height; entering terrain from the side
or crossing a discontinuous cliff with a horizontal ray is not reclassified as
a top-surface hit. Its step and convergence epsilon are both derived from the
shared subcell resolution.

The world does not own or mutate marks. Send
`aether.kit.world.set_mark_overlay_visibility { "visible": true }` to start a
correlated `MarkList` projection from the default-loaded MarkBook. The reply
reports whether the first full snapshot has synchronized. While visible, each
render refreshes at most one snapshot in flight and draws the cached point,
path, and area geometry after the ground mesh. Every overlay vertex resamples
terrain height, so an unchanged mark follows a later terrain edit.

Select only an exact cached revision:

```json
{
  "selected": {
    "id": { "0": 7 },
    "revision": 3
  }
}
```

`set_mark_overlay_selection_result` distinguishes `Selected`, `Cleared`,
`Stale` (the cache has a newer revision), and `Unsynchronized` (the
requested revision is ahead or missing and a refresh was requested). A later
snapshot that edits or deletes the selected mark clears the highlight instead
of silently moving it to another revision.

The reference terrain workbench owns camera policy locally rather than asking
the renderer or `WorldView` to unproject pixels. Its `TerrainViewport` retains
the named eye/target, vertical field of view, clip distances, viewport region,
and maximum pick distance. On `Render` it publishes the corresponding
`ViewProjection`; on a raw pointer press it uses that same basis, field of view,
aspect, and non-zero region origin to construct the named `TerrainRay`. The root
coordinator sends that ray as a correlated `aether.kit.world.pick_terrain`
request and returns the typed result to its inline viewport. The viewport also
maps its camera projection into its declared editor region, so the ray through a
region-local pixel agrees with the world geometry rendered at that framebuffer
pixel.

Load the workbench only after `aether.kit.mark`, `aether.kit.world`, and the
configured `aether.kit.terra` peer. The mark book must use the exact component
name `aether.kit.mark`: `WorldView` currently resolves overlay refreshes by that
loaded name even though Terra and the workbench receive its mailbox id. Load the
root with selector `aether_kit_workbench@aether.kit.workbench`. Its inspection flow uses
only the present proposal lifecycle: `aether.kit.world.propose`,
`aether.kit.world.set_proposal_preview`,
`aether.kit.world.commit_proposal`, and
`aether.kit.world.discard_proposal`, all observed through
`aether.kit.world.proposal_result`. A staged proposal is inspected and then
accepted or discarded; `ProposalResult::Rejected` is the typed staging gate.
There is no separate validation request.

Scalar overlay-only ground uses the contour library's continuous reconstructed
coverage at the same 127.5 crossing as the rendered mesh, rather than treating a
whole subcell as its stored byte. Mark geometry is capped per frame at 10,240
triangles / 30,720 vertices — enough for one maximum-size selected area. Marks
are traversed in stable id order; geometry after the cap is omitted and the
first omitted mark plus the emitted counts are warning-logged once when the
view enters overflow.

**From an agent over MCP — stage, then capture.** Use `capture_frame`: its
`mails` bundle dispatches before the readback (the state that should appear) and
`after_mails` after (cleanup), all around one synchronous PNG read. So to see a
camera change, stage the `aether.kit.camera.*` driver mail (or a `DrawTriangle`
directly) in `mails` and read the frame back inline. The renderer's retained
geometry means a capture that doesn't advance a tick still shows the last live
frame.

## How to extend or reuse it

- **A new camera mode** is component work, not substrate work. `aether-kit`'s
  `camera` export is the worked example: it hosts N named cameras, advances each
  on `Tick`, and publishes the active one's `view_proj` on `Render`. It boots a
  default `"main"` camera in orbit mode and exposes driver kinds —
  `aether.kit.camera.{create, destroy, set_active, set_mode, orbit.set, topdown.set}`
  — for adding cameras and poking their parameters live. A new mode (follow,
  cinematic, free-fly) is a new `view_proj` computation in a camera component;
  the renderer needs no change because it only ever applies the matrix it's
  handed. Loaded by the `aether_kit@aether.kit.camera` selector, the camera answers at
  `aether.component/aether.embedded:aether.camera` — the address `LoadResult.name` hands
  back.
- **Driving a camera from the keyboard** is a peer component's job, not the
  camera's. `aether-kit`'s `camera-controller` export subscribes `Key` /
  `KeyRelease` / `Tick`, keeps a shadow of the pose it drives, and mails
  `aether.kit.camera.orbit.set` / `aether.kit.camera.topdown.set` deltas to a peer
  camera — WASD pan the target across the ground, the arrows yaw and pitch, Z/X
  dolly the distance, and an idle tick produces no mail. It loads by the
  `aether_kit@aether.kit.camera-controller` selector with an
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
