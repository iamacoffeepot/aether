# ADR-0251: Staged Render Resources Upload Ahead of Use

- **Status:** Proposed
- **Date:** 2026-10-07

This ADR is text only; the engine change is its own issue (7635).

Three terms are used throughout. A resource is **staged** when the renderer
holds its bytes and has made no device object for it. It is **resident** when
its device object exists and holds every staged byte. A **piece** is the
smallest upload the renderer makes in one step.

## Context

Create and update mail to `aether.render` only stages. The handlers validate,
allocate an id, keep the bytes, and answer. The wgpu buffer or texture is made
later, inside the frame handler (`on_frame`,
`crates/aether-render/src/runtime/mod.rs`), the first time a recorded pass
names the resource. Each family does this through its own `ensure_realized`:
`StagedGeometry`, `StagedInstances`, `StagedTexture`, `StagedTextureArray` and
`StagedTextureVolume`, under `crates/aether-render/src/runtime/`.

So everything a newly drawn piece of content needs is uploaded in the one
frame that first draws it, and an actor that staged resources cannot learn
when they are on the device.

Measured 2026-10-07, release build at `1e4ddf974`, one Runite map square (456
models staged as 455 geometries and one instance buffer, 8.5 MB; 114 textures
as layers of three scene-owned texture arrays), 240 Hz display. A steady frame
spends about 4 ms in the frame handler, of which about 1 ms is work and the
rest is `present` waiting on the display.

| First-draw frame of the square | Total | Upload calls | `queue.submit` | Next frame |
|---|---|---|---|---|
| As built | 16–26 ms | 3.9–6.9 ms | 9.9–18.4 ms | 8–11 ms |
| Arrays fully written at creation, upload at first draw | 7.0–9.1 ms | 5.1–6.9 ms | 0.24–0.35 ms | 4.7–5.0 ms |
| Arrays fully written, geometry uploaded in earlier frames | 3.0–4.4 ms | 0.05 ms | 0.24–0.28 ms | 3.9–4.0 ms |

Three facts follow from the runs.

- **The upload calls follow the number of device objects, not the bytes.** 455
  geometries are 1,365 buffers and cost 3.8–6.1 ms for 8.1 MB. Two plain
  textures of 5.2 MB cost 0.5 ms.
- **Uploading in earlier frames costs nothing visible.** At 32 geometries a
  frame the square's geometry went up over 15 frames and the slowest of them
  took 4.3 ms.
- **The long submit is a one-time cost of the texture arrays, not of the
  square.** The scene's three arrays total 35.9 MB and a square writes some of
  their layers. The first submit whose draws use the arrays is long, and a
  second square loaded into the same engine submits in 0.22–0.25 ms. Writing
  every layer and level when the array is created removes the long submit;
  writing all 35.9 MB in one frame costs 26 ms in the write calls. The reading
  is that wgpu clears never-written layers the first time a draw uses the
  texture; that is inferred from these runs and not confirmed in wgpu's
  source.

Content that streams in is loaded before it is seen. The engine should use
that time: upload when a resource is staged, a little each frame, so the frame
that first draws it uploads nothing.

A reply that is owed later without holding the caller's chain already exists:
`ctx.defer::<R>()` returns `(Pending<R>, Held<R>)` and arms no settlement
hold ([ADR-0243](0243-typed-held-replies.md) §1). The renderer uses it for
requests that arrive before the first device
(`crates/aether-render/src/runtime/awaiting_device.rs`).

## Decision

### 1. The renderer keeps an upload queue

The renderer holds one queue of staged resources as plain actor state. A
create, an update, or a layer write that leaves a resource not resident puts
it on the queue, once; a second staging mail keeps its first position. A
destroy removes it. The registries stay the source of truth for whether a
resource is resident; the queue holds ids only.

### 2. Each frame uploads a fixed number of pieces

`on_frame` runs one upload step after the wait on the previous submission and
before it records passes. The step takes pieces from the front of the queue
until it has uploaded the configured number, then stops. A resource that is
already resident when the step reaches it, because a draw got there first,
leaves the queue at no cost.

A piece is:

| Family | Piece |
|---|---|
| Geometry | the whole geometry |
| Instance buffer | its staged range |
| Texture | the whole texture |
| Texture volume | the whole volume |
| Texture array | one layer, with all its levels |

Arrays alone are divisible, because an array is staged by many mails and can
be far larger than what one frame should carry.

The count is one config knob on the render capability, read at boot through
the existing config path ([ADR-0090](0090-application-configuration.md)). Its
default is 32 pieces per frame. The allowance is a count and not bytes or
time, because the measured cost follows the number of device objects, and a
fixed count needs no clock.

### 3. An array's unwritten layers are cleared by the queue

A texture array is resident only when every layer has been written or
cleared on the device. A layer no mail has written is cleared, as a piece
like any other, behind the layers that were staged. An array therefore
reaches the device whole over several frames, and no later draw is the first
use of an unwritten layer.

How a layer is cleared is the implementation's choice. A clear issued on the
device, which sends no bytes, is the first thing to try; uploading zeros is
the fallback. Whichever is used must leave wgpu with nothing to clear at
first use, which the implementation shows by measuring the first-draw
submit.

### 4. First use still uploads

A resource that a recorded pass names before the queue reached it is realized
then, as today. Correctness never depends on the queue; the queue only moves
work earlier. Drawing a resource is how a sender says it is needed now.

### 5. An actor asks to be answered when resources are resident

One request, sent to `aether.render`:

```rust
#[kind(name = "aether.render.await_resident")]
pub struct AwaitResident {
    pub resources: Vec<RenderResource>,
}

pub enum RenderResource {
    Texture { texture_id: u32 },      // textures, arrays and volumes share one id space
    Geometry { geometry_id: u32 },
    Instances { instances_id: u32 },
}

pub enum AwaitResidentResult {
    Ok { bytes: u64 },                      // what the named resources hold on the device
    Err(AwaitResidentError),                // a named id names nothing, or was destroyed while awaited
}
```

The handler answers at once when every named resource is already resident or
when an id names nothing. Otherwise it owes the reply with `ctx.defer`, so the
sender's chain settles as the handler returns and a request sent from a `Tick`
handler does not hold the frame loop. The upload step answers a held request
in the frame its last named resource becomes resident, whether the step or a
draw made it so. If a named resource is destroyed first, the request is
answered with the error. If the renderer closes, the in-flight ledger answers
it (ADR-0243 §1).

The sender chooses the grain: one request for everything it staged, or
several for parts it wants to hear about separately.

### 6. Awaited resources go first

The step uploads resources named by an unanswered request before the rest, and
each group in arrival order. A resource nobody awaits and nobody draws is
uploaded at the steady rate behind them. This is the only ordering signal; no
priority field is added to any create kind.

### 7. Updates and device replacement

An update or layer write that makes a resident resource stale puts it back on
the queue and is uploaded as a piece. An actor that wants to hear about it
asks again; mail from one sender arrives in order, so a request sent after its
own writes covers them.

When the device is replaced ([ADR-0173](0173-render-device-loss-recovery-contract.md)),
every resource is staged again and none is put on the queue by the
replacement. What is drawn realizes at first use on the new device, and what
is still queued uploads as before. A request already answered is not answered
again.

## Consequences

- A resource that is staged and then given frames reaches the device without
  being drawn, and a first draw of resident content creates and uploads
  nothing.
- Content loaded ahead of view no longer costs a long frame when it comes into
  view. Content drawn in the frame it is staged costs what it costs today.
- A scene can hold content back until it is resident and add it whole. For
  Runite, a bundle stages its own resources, awaits them, and tells the scene
  the square is ready.
- Every sender's resources upload ahead of use, including senders that never
  ask. A sender that does not ask pays nothing on its side.
- Every layer of a texture array is written or cleared even when few are
  used. A scene that reserves far more layers than it writes pays for that
  over frames, where today it pays in one submit.
- With no device or no frames, nothing uploads and no request is answered,
  which matches today: nothing realizes without a frame. The headless chassis
  composes no render actor and is unchanged.
- No kind changes shape. Two kinds and three schema types are added to
  `aether-render`.
- The guide pages that say a resource is created at first use
  (`docs/guide/systems/rendering.md`, `docs/guide/systems/render-programs.md`,
  `docs/guide/recipes/authoring-a-render-program.md`) change with the
  implementation.
- Left for later, and not blocked by this shape: a mail that changes the
  pieces per frame while the engine runs, for a loading screen that wants to
  upload flat out; and placing many geometries in a few shared device buffers,
  which would cut the upload calls for a group of static geometry that is
  created and destroyed together.

## Alternatives considered

- **A budget in bytes or in time, adapted from measured cost.** The cost
  follows object count, the renderer's own timing sees only a quarter of a
  frame's upload cost (most is in the submit and the next frame's wait), and a
  fixed count already keeps frames flat.
- **An event to the creator when its resource is resident.** An array's bytes
  arrive by later layer writes, so "created" is not "staged"; a sender that
  updates an instance buffer every frame would get an event every frame; and
  every sender would pay a cast per staging mail.
- **A flag on each create that holds its reply until resident.** A texture
  array's id is needed to write its layers, so its create cannot wait.
- **One create that takes a batch and answers when it is resident.** It works,
  but it repeats the shape of every create kind in one composite kind and
  leaves later updates without a way to wait.
- **A tell answered by a batched event each frame.** It gives per-frame
  progress nobody reads; a request has a typed reply, needs no subscriber
  proof, and is answered if the renderer closes.
- **The scene paces what it stages.** The renderer is where the cost is and
  the only actor that sees every sender; a scene pacing its own mail cannot
  account for another sender's.
- **A separate submission for uploads, outside the frame.** The renderer keeps
  one submission in flight and waits for it before the next frame, so a second
  submission moves the cost to that wait. Revisit if that wait is removed.
- **Clearing a whole array when it is created.** 35.9 MB of writes in one
  frame cost 26 ms.
