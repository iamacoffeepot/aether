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
| `aether.view_projection` | `{ view, projection, eye, near, far, extent }` | a view of the world; the renderer applies `projection * view`, and the latest value wins |
| `aether.render.view_from` | `{ source }` → `view_from_result` | follow the view source at the path `source`, in place of the one followed before |
| `aether.render.create_texture` | `{ width, height, format, sampling, usage, pixels }` → `create_texture_result` | register an `Rgba8`, `R8`, `R32Float`, `R16Float`, or `Rgba16Float` texture; reply carries the `texture_id` |
| `aether.render.update_texture` | `{ texture_id, x, y, width, height, pixels }` | overwrite a sub-rect of a texture (atlas growth) |
| `aether.render.destroy_texture` | `{ texture_id }` | release a registered texture, texture array or volume texture; fire-and-forget |
| `aether.render.create_texture_array` | `{ format, side, layers, mips }` → `create_texture_array_result` | register a texture array of a fixed side and layer count; reply carries the `texture_id` |
| `aether.render.write_texture_layer` | `{ texture_id, layer, pixels }` | replace every level of one layer of a texture array, in place; fire-and-forget |
| `aether.render.create_texture_volume` | `{ format, width, height, depth, pixels }` → `create_texture_volume_result` | register a volume texture with its whole contents; reply carries the `texture_id` |
| `aether.render.draw_textured_quads` | `{ texture_id, space, clip, blend, quads }` | per-tick textured alpha-blended quads; accumulates into the frame |
| `aether.render.draw_screen_triangles` | `{ space, clip, triangles }` | per-tick pixel-space triangles at any orientation; accumulates into the frame |
| `aether.render.draw_shapes` | `{ space, clip, shapes }` | per-tick rounded, stroked, shadowed, optionally textured boxes evaluated as a distance field; accumulates into the frame |
| `aether.render.material.textured` | `{ texture_id, blend, rects }` | per-tick depth-tested world-space textured rects |
| `aether.render.material.coverage` | `{ texture_id, rects }` | per-tick depth-tested world-space coverage bands from an R8 texture |
| `aether.render.capture_frame` | `{ window, mails, after_mails, checks, similarity }` | atomic "set state, read back a PNG, clean up"; a windowed runtime rejects an omitted `window` rather than guessing one |
| `aether.render.program.register` | `{ wgsl, bindings, transients, geometries, depth_transients, passes }` → `program.register_result` | register an authored render program (ADR-0170); reply carries the `program_id` |
| `aether.render.program.dispatch` | `{ program_id, bindings, geometries, draw_sets, uniforms }` | execute a registered program once at the next frame record; fire-and-forget |
| `aether.render.program.destroy` | `{ program_id }` | release a registered program; fire-and-forget |

A `Vertex` is `{ x, y, z, r, g, b }` — a world-space position plus a per-vertex
color. One `DrawTriangle` is three of them; a component batches many per envelope
via `send_many` (each triangle is `DRAW_TRIANGLE_BYTES` on the wire).

**Textured quads are the generic image surface** ([ADR-0105](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0105-text-rendering.md)).
`create_texture` stages `Rgba8` or `R8` pixels under a session-scoped
`texture_id` (the reply hands it back); `draw_textured_quads` then draws a batch
of quads sampling that texture, each carrying a pixel-unit rect, a uv sub-rect,
and an RGBA tint. The pixels arrive as a `Blob` and are staged as received, with
JSON callers sending the same byte array as before. `R8` samples contribute their scalar value in the red channel
(`vec4(r, 0, 0, 1)`), which is mainly a substrate for material passes; ordinary
sprite/text atlas callers use `Rgba8`. `destroy_texture` releases a registered
texture when the producer knows it is no longer used. It releases a
[texture array](render-programs.md#the-texture-array-resource) or a
[volume texture](render-programs.md#the-volume-texture-resource) the same way:
the three share one id space, and an array or a volume is read only by a
render program.

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

The field's presence follows one rule: `blend` appears exactly on the verbs
that composite a caller-supplied *image* — `draw_textured_quads` and
`material.textured` — because only the caller that produced those texels knows
whether they were already scaled by their coverage. A verb whose colours the
substrate rasterizes itself — `draw_shapes`, `draw_screen_triangles`,
`material.coverage` — carries no `blend`: the fragment stage knows what it
wrote, so there is nothing for the caller to declare.

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
The full contract — slots, extents, uniform windows, repeat semantics,
validation classes, pooling, determinism conventions — is the subject of
[Authored render programs](render-programs.md).

Quads draw through a second alpha-blended pipeline in an overlay pass recorded
after the world pass, so they always land on top. The accumulate-per-frame
contract matches `draw_triangle`: resend the batch every frame it should appear.
The batch's `space` selects the projection — `Screen` rects are window pixels
drawn under an ortho derived from the surface size; `World` anchors the quad in
the scene through the camera's `view_proj` and reads its coordinates as pixel
offsets from the projected anchor. All three overlay verbs carry the same
field with the same meaning. Sprites, HUD images, and the `aether.text`
capability all compose this surface.

**Screen triangles are the overlay's free-form primitive.**
`draw_screen_triangles` takes triangles whose three corners are pixels — one
linear RGBA per corner interpolated across the face — and records them in the
same overlay pass, through the same pipeline, in submission order with the quad
batches. Either winding draws; the batch carries the same optional `clip`
scissor and the same `space`, so a gauge or a graph edge can hang off a
world-space anchor exactly as a label does. It exists because 2D content built from
rotated geometry had no aspect-correct path: a quad is `{x, y, width, height}`
with no orientation, and `draw_triangle` is world-space, so with no camera
loaded its identity `view_proj` spans `-1..=1` on both axes and stretches
everything by the window's aspect ratio. Under `Screen` the pixel coordinates
are absolute, so a ribbon at an angle, a gauge, or a graph edge holds its
proportions on any window without a camera actor publishing a projection for
flat content.

**Shapes are the overlay's distance-field primitive**
([ADR-0213](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0213-gpu-shapes-for-the-widget-kit.md)).
`draw_shapes` takes axis-aligned boxes, each with a `corner_radius`, an optional
`fill`, an optional inside `stroke { width_pixels, color }`, and an optional
`shadow { blur_pixels, offset, color }`. The substrate expands each to one quad
grown by its shadow extent and a fragment stage evaluates a rounded-box signed
distance per pixel — shadow under fill under stroke, every edge anti-aliased
over one `fwidth` — so a one-pixel edge at a fractional position is one soft
edge rather than two half-covered rows, and the same six numbers draw a plate,
a circle (a radius at or above half the shorter side), a ring (a stroke with no
fill), or a soft halo (a shadow with neither) at any scale. The batch takes the
same painter position and the same `clip` scissor as any other overlay batch,
through its own pipeline: one more overlay draw, not a pass and not a layer.
The vocabulary is fixed and substrate-owned — callers supply parameters, never
WGSL — so the overlay lane stays a closed contract a caller can reason
about. A `corner_radius` of `0.0` with a `fill` alone is a
flat rectangle, which is why there is no separate flat-quad verb: the overlay's
three verbs are one per fragment stage — sample a texture, evaluate a distance
field, rasterize caller geometry.

A shape's optional `texture { texture_id, u0, v0, u1, v1, blend }` draws an
image *inside* the fill's coverage, so the corner radius, the circle, and the
anti-aliased edge apply to the image exactly as they apply to a flat colour —
a rounded avatar, a thumbnail at the panel's radius, a circular icon. The uv
sub-rect stretches across the shape's box, `fill` multiplies the sampled texel
the way `draw_textured_quads`' `tint` does (so `Rgba::WHITE` draws the image
unmodified, and a shape with a texture but no fill draws no image at all), and
`blend` means what it means on `draw_textured_quads`. The record path splits a
batch into draws at each texture transition, so one batch may mix untextured
plates with images over several textures and still keep its authored painter
order.

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
wgpu's uniform layout, so the 64 bytes upload with no transpose). An
`aether.view_projection` mail carries the two halves apart, as `aether-math`
values: `view` (world → view) and `projection` (view → clip), with the `eye` in
world space, the `near` and `far` depth planes, and the viewport `extent` in
physical pixels the projection was built for. The renderer stores
`projection * view`, and each mail overwrites it wholesale; nothing blends or
stacks. Before any view arrives, the matrix is identity, so vertices render in
clip space 1:1.

**The renderer follows a view source.** A view source is an actor that
publishes `aether.view_projection` to whoever subscribed to it. It covers the
`ViewSource` protocol: a viewer sends it `aether.render.view_subscribe`, and it
sends that viewer its current view at once and each later one as the view
changes, until the viewer sends `aether.render.view_unsubscribe`. Both kinds are
empty; the source takes its subscriber from the mail's sender. The renderer is
one such viewer, and `aether.render.view_from { source }` tells it which source
to follow: it unsubscribes from the source it followed before, subscribes to the
one at the path `source`, and replies `view_from_result`. So the active camera
is the source the renderer follows, one mail switches it, and two cameras do not
overwrite each other through this path. A path that does not prove — no actor
has stood at it, the actor there does not handle both subscription kinds, or it
has closed — is answered `Err` naming the path and why, and the renderer keeps
the source it had. When the followed source closes, the renderer follows nothing
and keeps the last view it was sent. A component that draws through its own
render program subscribes to the same source and is sent the same views. An
actor that computes its own view may still send `aether.view_projection`
straight to `aether.render`; it overwrites whatever a followed source sent.

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
desktop runtime instead walks every retained window in ascending window path
order. The first canonical window selects the replacement adapter, device, and
surface format; every later window must attach to that same context and format.
Only after all surfaces succeed does the runtime replace the full target map,
preserving each window path and occlusion flag together with the shared GPU and
wireframe overlay. If any later surface fails, none of the staged surfaces or
device state becomes live and the whole render capability becomes unusable.

**A window surface's present mode is chosen from what the surface offers,
never fallen back to.** The window's owner tells render one of two things
about a surface (`SurfacePresent`), on `attach_window` and again on
`set_window_present`, and render maps it over the present modes the surface
reports:

| Asked | wgpu mode chosen | When the surface lacks it |
|---|---|---|
| `InStep`: the present waits for the display | `Fifo` | `Err` naming the modes the surface offers |
| `Unsynced`: the present returns at once | `Immediate`, else `Mailbox` | `Err` naming the modes the surface offers |

A refusal leaves the surface configured as it was. wgpu's `AutoVsync` and
`AutoNoVsync` are not used, because each is a fallback chain and the second
ends in `Fifo`: a window asked to run unsynced would be paced by the display
with no word to its owner. `FifoRelaxed` is not used either: it is `Fifo`
that tears when a frame is late. The chosen mode is logged at info under
`aether_substrate::render`, since `Unsynced` is served by `Immediate` (which
may tear) on one platform and `Mailbox` (which does not) on another. Render
knows nothing of window kinds or frame rates: the desktop driver maps a
window's [presentation](window.md#presentation) onto these two values, and a
device replacement asks each rebuilt surface for the value it last had.

Public ids do not change across a successful replacement. Sampled textures
upload again from their retained CPU pixels, texture arrays from the retained
blob of each written layer, volume textures from the blob they were created
with, and registered geometry realizes again from its
retained vertex/index bytes. GPU-only writable textures keep
their ids but restart transparent; an actor that needs their contents sends its
ordinary program dispatch on the next repaint. A capture that was ready but
had not begun may cross the successful transaction and record once. Loss with
an ambiguous submission, poll, map, or readback instead returns that capture's
`Err`; its frame is not replayed and its `after_mails` are not released twice.
All of this is host policy — there is no recovery kind, generation callback, or
guest-visible wire change.

**The production headless chassis composes no render actor.** A chassis
composes only the capabilities it serves
([ADR-0232 §6](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0232-flat-ctx-send-verbs.md)),
so a component that declares `depends(RenderCapability)` is refused at load on
headless, naming `aether.render` as the dependency that is not live, and a
`capture_frame` there is answered `NotPresent`. The minimal hub chassis does not
install an `aether.render` mailbox either. `SubstrateHarness` composes the real
offscreen `RenderCapability` for render/capture tests.

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
// In an `#[actor(depends(LifecycleCapability, RenderCapability))]` block.
fn wire(&mut self, ctx: &mut WireCtx<'_, '_>) -> Result<(), ActorInitError> {
    ctx.subscribe::<LifecycleCapability, Tick>();
    ctx.subscribe::<LifecycleCapability, Render>();
    Ok(())
}

#[handler::event]
fn on_render(&mut self, ctx: &mut WasmCtx<'_>, _render: Render) {
    ctx.send_many::<RenderCapability>(&self.triangles);
}
```

A ctx that omits its actor is typed by it: the macro reads `WasmCtx<'_>` as
`WasmCtx<'_, Self>`, so the ctx reaches only the actors the component declares
with `depends(R)`. The actor is the first parameter, the reply mode the second
(`WasmCtx<'_, Self, Unchecked>`); spell `WasmCtx<'_, Erased>` for the untyped view.

Address the cap by type — `ctx.send::<RenderCapability>(..)` — and send
`DrawTriangle`s. If you're a camera, be a view source: handle `ViewSubscribe` by
typing `ctx.sender()` with `ctx.cast::<Subscriber<ViewProjection>>()`, keep the
reference, and send each `ViewProjection` through it with `ctx.send_to`; handle
`ViewUnsubscribe` by dropping it. A camera then names no recipient, and the
renderer takes its view once `aether.render.view_from` names the camera. A
component that depends on render does not stand up on headless at all; one
that subscribes `Render` without depending on render gets the lifecycle's
refusal at wire time on a graph that omits the stage.

**From an agent over MCP — stage, then capture.** Use `capture_frame`: its
required `window` names the render target (the window's actor path as
`aether.window.list` reports it, or its short form `aether.window/:main`; the
tool never guesses a primary or focused window), its `mails` bundle dispatches before the readback (the state that should
appear) and `after_mails` after (cleanup), all around one synchronous PNG read.
So to see a
camera change, stage an `aether.kit.camera.pose` or `.frame` addressed to the
camera instance (or a `DrawTriangle` directly) in `mails` and read the frame
back inline. The renderer's retained
geometry means a capture that doesn't advance a tick still shows the last live
frame.

## How to extend or reuse it

- **A camera** is component work, not substrate work, and `aether-kit`'s
  `camera` export is the one to use: an instanced actor, one camera per
  instance at `aether.kit.camera:<key>`, spawned with an
  `aether.kit.camera.config` (`{ lens, viewport, pose }`) and removed by
  dropping the instance. It is a view source: it publishes its
  `ViewProjection` to its subscribers when the view changes and at no other
  time, and the renderer is one once `aether.render.view_from` names it, so
  the active camera is whichever one the renderer follows.
  - **One pose and one lens.** A pose is `{ target, yaw, pitch, distance }`:
    the eye sits `distance` from `target` and looks back at it. A lens is
    `Perspective { fov }` or `Orthographic { extent }`, where `extent` is the
    half-height at the target per unit of distance. The view is the inverse of
    the eye's rigid transform, so a pitch of `-PI/2` (straight down) is an
    ordinary pose, and top-down is that pitch with an orthographic lens. The
    scalars are validated: a pitch outside `[-PI/2, PI/2]`, a distance or an
    extent that is not above zero, or a field of view outside `(0, PI)` does
    not decode.
  - **Depth follows distance.** A perspective lens puts the planes at
    `distance / 100` and `distance * 100`, an orthographic one at
    `∓distance * 100`, and `ViewProjection`'s `near` and `far` carry them. A
    scene a thousand units across needs no tuning; a drawer that knows better
    bounds can still send its own `ViewProjection`.
  - **The viewport** is `Fixed { width, height }` or `Window(path)`. A window
    camera follows its window's size and publishes nothing until it has
    learned it.
  - **Mail.** `aether.kit.camera.pose` sets the pose; `.frame { bounds }`
    looks at a box from far enough back to see all of it; `.glide { to,
    over_millis }` eases to a pose, stepping on `Tick` only while it runs;
    `.where` is answered with the current pose; `.ray { pixel }` is answered
    with the world-space ray through a pixel (`ray_result`), ready for
    `aether-math`'s `Ray::plane_hit`.
  - **A republish** of the kit module keeps a camera's pose, glide and learned
    viewport size and drops its viewers, which the camera logs at warn: send
    `aether.render.view_from` again, and have any other viewer subscribe
    again.
- **Driving a camera with the mouse and the keyboard** is a peer component's
  job, not the camera's. `aether-kit`'s `camera-controller` export, an
  instance at `aether.kit.camera-controller:<key>`, subscribes the window's
  key, mouse and focus events and `Tick`, and sends the camera only
  `aether.kit.camera.pose`, the message a script or an agent sends: at most
  one a tick, and none while nothing is held.
  - **Controls.** Left-drag orbits. The wheel zooms, multiplying the distance
    per step within the config's `nearest` and `farthest`. Right-drag or
    middle-drag pans with the grabbed point staying under the cursor. WASD
    and the arrows pan the target across the ground at a rate that scales
    with the camera's distance, and Q/E turn the camera about its target.
    Key rates use the tick's elapsed time.
  - **It reads before it writes.** A gesture starts when input arrives while
    nothing is held: the controller asks the camera
    `aether.kit.camera.where` and steps from the answer, and it forgets the
    pose when the last key and button are released. A `pose`, `frame` or
    `glide` sent from elsewhere between gestures stands, and the next
    gesture continues from wherever the camera is.
  - **It is a viewer of its camera.** It sends the camera
    `aether.render.view_subscribe` and casts a drag pan's rays through the
    view the drag began in, so the pan is exact for either lens with no
    request per mouse move.
  - **One window.** Its `aether.kit.camera-controller.config` names the
    camera (`"camera": "aether.kit.camera:main"`) and the window whose input
    it reads (`"window": "aether.window/aether.window.instance:main"`), and
    sets the rates and the zoom range. Input from any other window is
    ignored, and when its window loses focus it drops every held key and
    button, whose releases went to another window.
  - Load the camera first: a controller whose camera is not live fails its
    load.
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
