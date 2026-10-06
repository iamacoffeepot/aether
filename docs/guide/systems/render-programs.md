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
| `aether.render.program.register` | `ProgramRegister { wgsl, bindings, transients, geometries, depth_transients, passes }` | validate + compile; reply `aether.render.program.register_result` / `ProgramRegisterResult` (`Ok { program_id }` / `Err { error }`) |
| `aether.render.program.dispatch` | `ProgramDispatch { program_id, bindings, geometries, draw_sets, uniforms }` | fire-and-forget; execute once at the next frame record |
| `aether.render.program.destroy` | `ProgramDestroy { program_id }` | fire-and-forget release, mirroring `destroy_texture` |
| `aether.render.create_geometry` | `CreateGeometry { layout, vertices, indices }` | validate + stage; reply `aether.render.create_geometry_result` / `CreateGeometryResult` (`Ok { geometry_id }` / `Err { error }`) |
| `aether.render.update_geometry` | `UpdateGeometry { geometry_id, vertices, indices }` | fire-and-forget in-place replacement against the created layout |
| `aether.render.destroy_geometry` | `DestroyGeometry { geometry_id }` | fire-and-forget release, mirroring `destroy_texture` |
| `aether.render.create_instances` | `CreateInstances { layout, capacity, records }` | validate + copy; reply `aether.render.create_instances_result` / `CreateInstancesResult` (`Ok { instances_id }` / `Err { error }`) |
| `aether.render.update_instances` | `UpdateInstances { instances_id, first, records }` | fire-and-forget in-place write of a run of records |
| `aether.render.destroy_instances` | `DestroyInstances { instances_id }` | fire-and-forget release, mirroring `destroy_geometry` |
| `aether.render.create_draw_set` | `CreateDrawSet { vertex_layout, instance_layout, draws }` | check every draw + hold its buffers; reply `aether.render.create_draw_set_result` / `CreateDrawSetResult` (`Ok { draw_set_id }` / `Err { error }`) |
| `aether.render.update_draw_set` | `UpdateDrawSet { draw_set_id, first, draws }` | check + patch in place, all or nothing; reply `aether.render.update_draw_set_result` / `UpdateDrawSetResult` (`Ok` / `Err { error }`) |
| `aether.render.destroy_draw_set` | `DestroyDrawSet { draw_set_id }` | fire-and-forget release of the set and what it holds |
| `aether.render.create_texture_array` | `CreateTextureArray { format, side, layers, mips }` | validate against the device's limits; reply `aether.render.create_texture_array_result` / `CreateTextureArrayResult` (`Ok { texture_id }` / `Err { error }`) |
| `aether.render.write_texture_layer` | `WriteTextureLayer { texture_id, layer, pixels }` | fire-and-forget in-place write of every level of one layer |
| `aether.render.create_texture_volume` | `CreateTextureVolume { format, width, height, depth, pixels }` | validate + stage the whole volume, with no device needed; reply `aether.render.create_texture_volume_result` / `CreateTextureVolumeResult` (`Ok { texture_id }` / `Err { error }`) |

`program_id` and `geometry_id` are session-scoped and assigned like texture and
instrument identifiers. A rejected register or create consumes no id, so
accepted ids stay dense. Destroying a program releases its compiled pipelines;
pooled transient textures stay in the shared pool for other programs.
Destroying a geometry retires its id at once and for good: nothing can name it
again and it is never handed out again. Its staged bytes and any realized GPU
buffers are released with it, unless a [draw set](#the-draw-set-resource) names
the geometry, in which case they live until the last such set lets go.

A fragment-only program leaves `geometries` and `depth_transients` empty and
registers exactly as it does with no draw pass anywhere in the graph. Both
remain required fields on the mail — the codec rejects a missing field rather
than defaulting it — so send `[]`. A dispatch is the same: `geometries` and
`draw_sets` are required, and a program with no pass that uses one sends `[]`.

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

The `vertices` and `indices` fields arrive as `Blob`s and are staged as received,
with JSON callers sending the same byte arrays as before.

`update_geometry` replaces both byte arrays wholesale against the layout fixed
at create — the lengths may change, so a mesh may grow or shrink. It is
fire-and-forget: an unknown id, or a replacement that fails the create-time
rules, logs a warning under the `aether_render` target and leaves the previous
content staged. `destroy_geometry` releases the entry, and an unknown id
warn-drops the same way. A geometry a draw set names is held by that set, which
narrows both verbs; see [the draw set resource](#the-draw-set-resource).

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

## The instance resource

An instance buffer holds **records**: one instance's attributes each, packed as
a `layout: Vec<VertexAttribute>` declares, under the same
[layout vocabulary](#layout-vocabulary) and stride rule a geometry uses. The
substrate does not interpret a record — a placement, a table index, a tint are
the authoring actor's business. The buffer is a vertex buffer stepped once per
instance rather than once per vertex ([ADR-0246](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0246-retained-draw-sets.md)
decision 3). No stage draws from one yet; the draw-set stage is its first
reader.

`capacity` and `first` count records, never bytes. `create_instances` fixes the
capacity for the buffer's life: `records` is the initial contents from record
0, it may hold fewer records than the capacity, and the rest start zeroed.
`update_instances` overwrites the run of records starting at record `first`,
in place. Neither the capacity nor the `instances_id` changes, so whatever
names the buffer keeps naming the same one; a larger buffer is a new
`create_instances`. An update is the per-frame verb for things that move, and
its cost is the bytes it carries: write the records that changed, not the
buffer.

The substrate keeps its own copy of every record, and that copy is the source
of truth. A create and any number of updates are accepted before a GPU device
exists; the GPU buffer is created at the first use and afterwards receives only
the bytes written since the last one. After a render device replacement the
records come back under the same id with the contents they had, as texture and
geometry bytes do.

`create_instances` validates before it assigns an id, and each failure class
replies its own reason:

| Class | Reason shape |
|---|---|
| Empty layout | `instance layout declares no attributes` |
| Zero capacity | `instance capacity is zero records` |
| Buffer limit | `capacity of N records at stride S exceeds the device limit max_buffer_size = M` |
| Not resident | `instance record bytes are not resident in this process` |
| Record stride | `records length N does not divide evenly by the layout stride S` |
| Capacity | `C records from record F run past the capacity of N records` |

`update_instances` is fire-and-forget. An unknown id, bytes that are not
resident, a length off the stride, or a run that ends past the capacity logs a
warning under the `aether_render` target and leaves every record as it was; a
refused update is never partly applied. An empty `records` is accepted and
writes nothing. `destroy_instances` retires the id at once: a later update or
draw naming it finds nothing, the id is never handed out again, and an unknown
id warn-drops the same way. The records and the GPU buffer are released with
it, unless a draw set names the buffer, in which case they live, with the
contents they had, until the last such set lets go.

## The texture array resource

A texture array is `layers` square layers of `side` texels in one `format`,
bound whole at a `SlotShape::TextureArray` binding so a program picks a layer
per fragment ([ADR-0246](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0246-retained-draw-sets.md)
decision 6). It is how a scene binds many surface textures at once.

`create_texture_array` fixes the side and the layer count for the array's
life; a larger array is a new `create_texture_array`. The array is created
with no pixels, and **a layer that was never written reads as zero in every
channel**. It has no sampling setting of its own: it is read linear when its
format can be filtered and nearest when it cannot (`R32Float`). All five
texture formats are accepted.

`mips` says which levels the array has. `Mips::Base` is the base level alone.
`Mips::Chain` is `floor(log2(side)) + 1` levels, level `n` having side
`max(1, side >> n)`, so side 5 has levels of side 5, 2 and 1. The substrate
generates no level. `write_texture_layer` supplies them: `pixels` carries
every level the array has for that layer, base level first, each level
row-major and top-down, with nothing between levels. Its length is the sum
over the levels of `level_side * level_side * format.bytes_per_pixel()`. A
write is all of a layer's levels or none of them, so a layer never holds part
of a chain. A `Filtered` binding reads past the base level only when it
declares `Mips::Chain`.

An array's id comes from the sequence `create_texture` draws from. One
`texture_id` names a texture, an array or a
[volume](#the-volume-texture-resource) and never two of them, and
`destroy_texture` releases any of the three; there is no second destroy kind. Every
path that takes a plain texture (`update_texture`, the quad, shape and
material draws) treats an array's id as unknown and warn-drops.

The layer ceiling is the render device's, so creation needs a device. On the
SubstrateHarness the first create boots it; **on desktop a create sent before
the first window attaches is answered once the device is up**, as a program
`register` is. A sender asks once from `wire` and continues from its response
handler: it learns the id only from the reply, so it writes layers
(`write_texture_layer`) or dispatches from there. The request's own chain
settles first, so over MCP such a call settles with no reply.
`create_texture_array` validates before it assigns an id, and each failure
class replies its own reason:

| Class | Reason shape |
|---|---|
| Zero side | `texture array side is zero` |
| Zero layers | `texture array has zero layers` |
| Side limit | `texture array side N exceeds the device limit max_texture_dimension_2d = M` |
| Layer limit | `texture array layer count N exceeds the device limit max_texture_array_layers = M` |
| Byte size | `a F layer of side N overflows the addressable byte count` |

`write_texture_layer` is fire-and-forget. An unknown id, an id that names a
plain texture or a volume, a layer at or past the layer count, bytes that are not
resident in this process, or a length that is not every level of one layer
logs a warning under the `aether_render` target and leaves the layer as it
was. An accepted write replaces the layer in place: the id does not change,
and only that layer is uploaded.

The substrate keeps the blob of each written layer, and those blobs are the
source of truth. Writes are accepted at any time after creation; the GPU
texture is created at the first dispatch that binds the array. After a render
device replacement the array comes back under the same id with every written
layer as it was.

## The volume texture resource

A volume texture is `width` by `height` by `depth` texels in one `format`,
bound at a `SlotShape::TextureVolume` binding and read at a three-component
coordinate ([ADR-0246](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0246-retained-draw-sets.md)
decision 6). It is how a program reads a value interpolated along a third
axis with one sample: a small repeating float volume whose third axis is
time, for example, read at a fractional coordinate so the sampler blends
between slices and wraps from the last slice back to the first.

`create_texture_volume` carries the whole contents. `pixels` is `depth`
slices, slice 0 first, each slice row-major and top-down, with nothing
between slices: exactly `width * height * depth * format.bytes_per_pixel()`
bytes. Texture coordinate `w = 0` is the near face of slice 0, so **slice `k`
is centred at `w = (k + 0.5) / depth`**, as a texel is centred half a texel in
on the other two axes.

A volume is immutable. No kind writes it after creation; new contents are a
new volume and a `destroy_texture` of the old one. It has one mip level, so a
binding that declares `Mips::Chain` reads it as `Mips::Base` does.

It has no sampling setting of its own, as an array has none: it is read
linear when its format can be filtered and nearest when it cannot
(`R32Float`). All five texture formats are accepted. A linear read
interpolates on all three axes, and a `Filtered` binding's `Wrap` is the
address mode on all three: under `Repeat` a coordinate half a slice before
the first slice's centre reads the even blend of the last slice and the
first.

A volume's id comes from the sequence `create_texture` draws from, shared
with plain textures and arrays, and `destroy_texture` releases it. Every path
that takes a plain texture (`update_texture`, the quad, shape and material
draws) treats a volume's id as unknown and warn-drops, and
`write_texture_layer` naming a volume warn-drops as it does for a plain
texture.

Each dimension is checked against `max_texture_dimension_3d` at its default,
2048, which is the limit every render device is requested at. The check
therefore reads no device, and **a create is answered inside the call, before
the render device exists as after**: on desktop a component may create a
volume from `wire`, before the first window attaches, and has its id when the
response arrives. `create_texture_volume` validates before it assigns an id,
and each failure class replies its own reason:

| Class | Reason shape |
|---|---|
| Bytes not resident | `pixel bytes are not resident in this process` |
| Zero dimension | `texture volume dimensions WxHxD have a zero dimension` |
| Dimension limit | `texture volume dimensions WxHxD exceed the device limit max_texture_dimension_3d = M` |
| Byte size | `a F volume WxHxD overflows the addressable byte count` |
| Length | `pixels length N does not match WxHxD F = M` |

The substrate keeps the blob it was given, and that blob is the source of
truth. The GPU texture is created, and the blob uploaded, at the first
dispatch that binds the volume. After a render device replacement the volume
comes back under the same id with its contents as they were.

## The draw set resource

A draw set is a retained list of draws
([ADR-0246](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0246-retained-draw-sets.md)
decisions 1 and 2). Each draw is a run of one geometry's indices, drawn once
per record of a run of one instance buffer:

```rust
pub struct IndexRange { pub first: u32, pub count: u32 }      // counts indices
pub struct InstanceRange { pub first: u32, pub count: u32 }   // counts records
pub struct DrawSpec {
    pub geometry_id: u32,
    pub indices: IndexRange,
    pub instances_id: u32,
    pub instances: InstanceRange,
}
```

A draw carries no texture and no per-draw constant; what varies between draws
rides in the instance records. The set fixes two layouts at create,
`vertex_layout` and `instance_layout`, and every draw's geometry and instance
buffer must have been created with them. A set is checked against layouts, not
against a program, so any pass with the same two layouts may draw it. The
stage that draws a set is a [draw-set pass](#draw-set-passes).

Every check runs when the set is made or patched, where the sender gets a
reply, so that a pass walking the set has nothing left to check. The first
failure refuses the whole mail, and each class replies its own reason. A
per-draw reason starts `draw N:`, with `N` the draw's position in the mail's
`draws`:

| Class | Reason shape |
|---|---|
| Empty vertex layout (create) | `draw set vertex layout declares no attributes` |
| Empty instance layout (create) | `draw set instance layout declares no attributes` |
| Unknown set (patch) | `unknown draw set id D` |
| Gap (patch) | `first F is past the set's N draws` |
| Unknown geometry | `draw N: unknown geometry id G` |
| Geometry layout | `draw N: geometry G was created with a layout that is not the set's vertex layout` |
| Index range | `draw N: C indices from index F run past geometry G's M indices` |
| Unknown instances | `draw N: unknown instances id I` |
| Instance layout | `draw N: instance buffer I was created with a layout that is not the set's instance layout` |
| Instance range | `draw N: C records from record F run past instance buffer I's capacity of M records` |

A range's end is `first + count`, summed without wrapping, and it may sit
exactly at the buffer's end. A count of zero is inside every buffer and draws
nothing, which blanks one entry without moving the others. A refused create
consumes no id and holds nothing; a refused patch leaves the set exactly as it
was, never partly applied.

### Patching a set

`update_draw_set` writes `draws` over the set's entries from position `first`,
counted in draws. One rule covers every edit:

- `first` may be at most the set's length. Entries inside the set are
  overwritten in place.
- A run that passes the end extends the set, so appending is a patch with
  `first` equal to the length.
- An empty `draws` truncates the set to its first `first` entries.

No entry moves unless the sender moves it, so a position stays a stable name
for a draw. A patch costs what it carries: change a hundred entries in the
middle of thirty thousand by sending the hundred.

### What a set holds

A set holds the geometries and instance buffers its draws name, from the patch
that first names one until the patch or destroy that removes its last draw of
it. Holding is what keeps a checked draw valid, and it changes three things for
the buffers held:

- `destroy_geometry` and `destroy_instances` retire the id and keep the bytes
  and GPU buffers alive for the sets still drawing them. The retired id cannot
  be named by a new draw, an update, or a dispatch. The buffer is released for
  good when its last set lets go.
- `update_geometry` may replace a held geometry's vertex contents and nothing
  else. A replacement with a different vertex count, or with indices that are
  not byte-for-byte the ones staged, logs a warning under the `aether_render`
  target and leaves the geometry as it was. `update_geometry` carries no reply,
  so the log is where a refusal shows. Once no set names the geometry it may be
  resized again.
- An instance buffer's capacity is fixed at create, so `update_instances`
  needs no extra rule.

Growing a held geometry or buffer means creating a new one and patching the
set onto it. `destroy_draw_set` releases the set and everything it holds; the
released `draw_set_id` is never handed out again, and an unknown id warn-drops.

## The pass graph

A registered graph declares five lists: `bindings`, `transients`, `geometries`,
`depth_transients`, and `passes`.

### Slots and extents

A **slot** is a texture a pass samples or renders into. Two declaration lists
exist:

- `bindings: Vec<SlotSpec>` — textures the dispatch supplies. Each dispatch
  names one registry texture id per declared binding, in order.
- `transients: Vec<TransientSpec>` — intermediates the executor owns and
  pools. A dispatch never names them; they exist so a chain of operations has
  scratch surfaces without the actor creating textures for them.

Two further lists declare what draw passes need, and both stay empty for a
graph with no draw pass in it:

- `geometries: Vec<GeometrySlotSpec>` — geometry slots the dispatch supplies
  by id, in the same shape bindings use. Each `GeometrySlotSpec` is a `layout`,
  the vertex layout the slot's geometry must have been created with; the
  register builds the pass's vertex buffer layout from it and checks the
  authored vertex stage against it.
- `depth_transients: Vec<DepthSpec>` — pooled `Depth32Float` targets draw
  passes clear and test against. A `DepthSpec` is an `extent` and a `samples`,
  since the format is fixed. Its `extent` is a `DepthExtent`:
  `Output(SlotExtent)` sizes the slot from the program's output, as a transient
  is sized, and `Fixed { side }` makes it a square of its own size, which is
  what a shadow map is.

A `SlotSpec` is a binding's declaration: a `format` (`Rgba8`, `R8`, `R32Float`,
`R16Float`, or `Rgba16Float`), a `shape`, and a `sampling`. A `TransientSpec`
is a `format` from the same five, an `extent`, and a `samples`; a transient is
always sized from the program's output and is always read the same way, so it
declares neither a shape nor a sampling. The float formats are data planes — texels carrying
quantities rather than colours — and choosing between them is a question about
what the texel holds and who reads it. `R32Float` keeps a full 24-bit mantissa
and cannot be linear-filtered in core WebGPU, so it is what a label, an index,
or anything a pass reads point by point stands at. `R16Float` keeps about
eleven bits and *is* filterable, so a pass may take a fractional coordinate
through one and be handed the interpolation instead of computing it from four
point fetches — which is what a separable blur wants, since pairing its taps
into filtered reads halves its texture fetches. `Rgba16Float` gives the same
filterability and per-lane precision to four quantities in one target, so
same-kernel scalar operations can travel independently through its channels.

A binding's `shape` says what it takes and whether a pass may write it:

- `SlotShape::Target(SlotExtent)` — a texture sized from the program's output.
  A pass may write it or read it, and the texture bound there must be exactly
  the resolved size. Every binding a pass writes is a `Target`.
- `SlotShape::Texture` — a texture of any size, read only: a lookup table, a
  tile sheet, a table of per-instance data. A pass naming it as its output
  rejects at register.
- `SlotShape::TextureArray` — a [texture array](#the-texture-array-resource)
  of any size and layer count, read only. The shader declares it
  `texture_2d_array<f32>`. The kind of texture has to match in both
  directions: a dispatch that binds a plain texture here is dropped, and so is
  one that binds an array at a `Target` or `Texture` binding.
- `SlotShape::TextureVolume` — a [volume texture](#the-volume-texture-resource)
  of any width, height and depth, read only. The shader declares it
  `texture_3d<f32>` and reads it with a `vec3<f32>` coordinate, or with
  `textureLoad` and a `vec3<i32>` under `Texel`. Only a volume binds here,
  and a volume binds nowhere else.

A `Target` binding, a transient and a `DepthExtent::Output` depth transient
each carry one of two extents:

- `SlotExtent::Full` — the reference size.
- `SlotExtent::Divided { divisor }` — the reference size floor-divided by
  `divisor` on both axes, clamped to at least one texel, for pyramid and
  reduced-resolution work. A zero divisor rejects at register.

The **reference extent** is the size of the texture bound at the program's
output binding — the dispatch binding the final pass writes, which must be
declared `Target(Full)`. Every other extent scales from it, which is what
lets one registered program dispatch at any canvas size: the graph carries no
pixel dimensions, only ratios. A `Texture`, `TextureArray` or `TextureVolume`
binding stands outside that rule and keeps the size it was created with, and so does a
`DepthExtent::Fixed { side }` depth transient: it is `side` texels square
whatever the reference extent is, and it keeps that size when the output is
resized. `side` is at least 1 and at most the device's
`max_texture_dimension_2d`; a side outside that rejects at register. A `Fixed`
slot attaches only under a [depth-only pass](#depth-only-passes).

A binding's `sampling` says how a pass that reads it does so:

- `Sampling::Filtered { wrap, mips }` — the binding binds with a sampler. `wrap`
  is `Wrap::Clamp` (the edge texel extends outward) or `Wrap::Repeat` (the
  texture tiles). `mips` is `Mips::Base` (the base level only) or `Mips::Chain`
  (the whole mip chain, where the texture has one).
- `Sampling::Texel` — the binding binds with no sampler, and the shader reads
  it texel by texel with `textureLoad`. This is what a table of exact values
  declares.

A transient is read as a `Filtered { wrap: Clamp, mips: Base }` binding is: a
texture and a sampler that clamps and reads the base level.

Each pass reads through `InputSlot` values and writes one `OutputSlot`:

- `InputSlot::Binding { index }` / `InputSlot::Transient { index }` — a
  declared slot by list position.
- `InputSlot::PassOutput { pass }` — whatever slot the pass at that sequence
  index wrote, resolved at register time. A ping-pong chain reads "the
  previous pass's result" without naming the transient twice.
- `InputSlot::Depth { index, read }` — a depth transient by list position,
  read as `read` declares: `DepthRead::Texel` loads the stored depth and
  `DepthRead::Compare` compares a reference depth against it. The slot is a
  `Samples::One` slot that an earlier pass attaches and this pass does not; see
  [Reading a depth slot](#reading-a-depth-slot).
- `OutputSlot::Binding { index }` / `OutputSlot::Transient { index }` — a
  dispatch binding (which must be declared a `Target` and resolve to a
  `Writable` registry texture at dispatch) or a transient.
- `OutputSlot::None` — no color output. A compute pass declares it, and so
  does a [depth-only pass](#depth-only-passes): a `Draw`, `DrawIndexedIndirect`
  or `DrawSets` pass that attaches its depth slot alone. A `Fragment` pass
  attaches no depth slot, so `None` on one rejects at register.

Every pass but a depth-only one writes a texture, and none writes the frame: a
program's output reaches the frame as a registry texture the quad and material
paths draw.

### Samples

A transient and a depth transient declare `samples`: `Samples::One` or
`Samples::Four`, the number of samples each texel holds.

A pass rasterizes at the sample count of its color output, or of its depth
slot when it is depth-only. A binding is a
registry texture and always has one sample, and a transient has what it
declares, so a pass that writes a `Four` transient rasterizes at four samples
per texel and the edge of a triangle covers a texel by quarters. The rule is
the same for every render stage. Since the final pass writes a binding, an
antialiased scene is drawn into a `Four` transient and a later pass carries it
to the output.

A pass that reads a `Four` transient reads its resolved image: one value per
texel, the average of the four samples. A `Four` transient is two textures. The
multisampled one is what passes attach, and it keeps its samples between
passes, so a second pass drawing into it under `PassLoad::Load` draws over the
first pass's samples. The single-sample one is what readers bind. The executor
resolves the first into the second once, in the last iteration of the last pass
to write the transient before each pass that reads it: a writer followed by
another writer does not resolve, and two readers with no writer between them
share one resolve. A `Four` transient no pass reads has no single-sample
texture and is never resolved.

Two declarations are refused at register:

- A depth slot whose `samples` is not that of the color output of a pass that
  names it. The attachments of one pass share one sample count, so a pass
  writing a `Four` transient names a `Four` depth slot. A depth slot is never
  resolved.
- A `Four` transient whose format cannot be resolved. Of the five formats that
  is `R32Float`, which a device can multisample and cannot resolve; it is
  refused whether or not a pass reads it.

### Blend

Every pass declares `blend`, how what its fragment entry returns composes with
what its output already holds:

| `blend` | Color | Alpha |
|---|---|---|
| `Blend::Replace` | `source` | `source.a` |
| `Blend::Alpha` | `source * source.a + destination * (1 - source.a)` | `source.a + destination.a * (1 - source.a)` |
| `Blend::Additive` | `source + destination` | `source.a + destination.a` |

The declaration applies whatever the output's format. `Alpha` is straight-alpha
source over, what a pass painting colour onto a canvas wants. `Replace` is what
a pass writing a quantity means by writing it, and it is how a pass erases: a
source of alpha zero overwrites what the output held. `Additive` accumulates,
which is what a bloom chain adding its levels onto a float target wants.

A compute pass and a depth-only pass have no color output, so each declares
`Blend::Replace`; any other value is refused at register.

`Rgba8`, `R8`, `R16Float` and `Rgba16Float` blend on every device. `R32Float`
blends where the device offers `float32-blendable`, which the render device
takes whenever its adapter has it. On a device without it, a register whose
pass declares `Alpha` or `Additive` onto an `R32Float` output replies
`pipeline creation failed`, naming the format; a pass that replaces an
`R32Float` output registers everywhere.

### Sequence order

The graph is a sequence, and a pass may read only slots already written: a
transient must be written by an earlier pass before any pass reads it, a
`PassOutput` must point at an earlier pass that has a color output (a compute
pass and a depth-only pass have none to name), and no pass may read its own
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

A program may be dispatched any number of times in a frame, each dispatch with its
own blob: every dispatch's passes read the uniforms that dispatch carried.

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
What "load" composes is the pass's declared [blend](#blend). Under `Alpha` or
`Additive` a repeated pass accumulates, each iteration composing with the ones
before it, on a float target as on any other. Under `Replace` each iteration
overwrites the last, so a repeat keeps only its final iteration. A multi-step
chain whose steps each replace a plane is therefore laid structurally — each
step its own pass entry with its own window — rather than as one repeated
pass; the [wash program](#the-worked-consumer-the-wash) below is the worked
example of that shape. A repeated pass writing a `Four` transient resolves
after its last iteration, never between iterations.

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
- Group 1 — the pass's input slots, in declaration order. Input `n` is the
  texture at `@binding(2 * n)` and, for a transient or a `Filtered` binding,
  the `sampler` at `@binding(2 * n + 1)`. The texture is `texture_2d<f32>` for
  a transient or a `Target` or `Texture` binding, `texture_2d_array<f32>`
  for a `TextureArray` and `texture_3d<f32>` for a `TextureVolume`.

A `Texel` binding has no sampler. Its input still takes the texture at
`@binding(2 * n)` and leaves `@binding(2 * n + 1)` unused, so the inputs after
it keep their numbers whatever the inputs before them declare:

```wgsl
// inputs: [ a Texel table, a Filtered texture ]
@group(1) @binding(0) var table: texture_2d<f32>;        // input 0; nothing at binding 1
@group(1) @binding(2) var tint_texture: texture_2d<f32>; // input 1
@group(1) @binding(3) var tint_sampler: sampler;
```

Group 1 is visible to the fragment stage of every pass, to the authored vertex
stage of a draw pass, and to a compute pass. A vertex stage has no implicit
derivatives, so it reads an input with `textureLoad` or `textureSampleLevel`.

The binding and the bound texture each decide part of how an input is read. The
binding's `sampling` decides whether there is a sampler, how it addresses a
coordinate outside the texture, and which mip levels it reads. The bound
texture decides linear or nearest: nearest when the registry texture was
created with `Nearest` sampling or its format cannot be linear-filtered
(`R32Float`), linear otherwise. A transient always has a sampler, which clamps
and reads the base level, and filters by its declared format the same way; a
shader may still read one with `textureLoad` and leave the sampler unused. A
`Four` transient is read resolved, as a `texture_2d<f32>` like any other. So a pass that wants a filtered read has to be handed a plane
standing at a filterable format — filtering is a property of the texture, not
of the pass, and a plane another program wrote stands at whatever format that
program declared.

A module whose entry point disagrees with its slots — it reads an array where
the slot is a `Texture`, samples through a sampler on a `Texel` input, declares
a `texture_2d<f32>` where the input is a depth slot, or reads a `Compare` input
through a plain `sampler` — fails pipeline creation, and the register replies
`Err`.

### Reading a depth slot

An `InputSlot::Depth { index, read }` input binds the depth transient at
`index` under the numbering every input has. Input `n` is a `texture_depth_2d`
at `@binding(2 * n)`, and `read` decides the rest:

- `DepthRead::Texel` — no sampler. `@binding(2 * n + 1)` is left out of the
  layout, as it is for a `Texel` binding, and `textureLoad` returns the stored
  depth.
- `DepthRead::Compare` — a `sampler_comparison` at `@binding(2 * n + 1)`, read
  with `textureSampleCompare` in a fragment stage and
  `textureSampleCompareLevel` in any stage. A `Compare` input can still be read
  with `textureLoad`.

The read is declared on the input, not on the slot, so one pass may compare a
slot that another loads. Every stage that sees group 1 may read one: the
fragment stage of any pass, the authored vertex stage of a draw pass, and a
compute pass.

The comparison sampler is fixed. It compares `LessEqual`, the test a pass
attaches a depth slot under, so a reference depth passes exactly where a
fragment at that depth would have been drawn into the slot, and a texel nothing
was drawn to holds the far plane and passes every reference. It clamps, reads
the slot's one level, and is linear: at the edge of what was drawn the result
is a fraction between 0 and 1, filtered from the comparisons around the
coordinate. The engine applies no depth bias, so a pass that compares against a
shadow map offsets its reference depth in its own shader.

```wgsl
// inputs: [ a Compare depth slot, a Texel depth slot ]
@group(1) @binding(0) var shadow_map: texture_depth_2d;      // input 0
@group(1) @binding(1) var shadow_compare: sampler_comparison;
@group(1) @binding(2) var scene_depth: texture_depth_2d;     // input 1; nothing at binding 3

@fragment
fn fs_lit(@builtin(position) position: vec4<f32>, @location(0) light: vec3<f32>) -> @location(0) vec4<f32> {
    // `light` is the fragment in the light's clip space: xy mapped to 0..1, z its depth there.
    let lit = textureSampleCompare(shadow_map, shadow_compare, light.xy, light.z - 0.002);
    let behind = textureLoad(scene_depth, vec2<i32>(position.xy), 0);
    return vec4<f32>(vec3<f32>(lit * behind), 1.0);
}
```

```jsonc
"inputs": [ { "Depth": { "index": 0, "read": "Compare" } },
            { "Depth": { "index": 1, "read": "Texel" } } ]
```

What a slot can be read as follows from what it is:

- Only a `Samples::One` slot is read. A multisampled depth texture can be
  neither compared nor sampled and a depth attachment is never resolved, so a
  consumer that wants the depth of a scene drawn at `Four` draws the same
  geometry again in a [depth-only pass](#depth-only-passes) onto a `One`
  `Output` slot and reads that.
- An earlier pass of the program attaches the slot, and the reading pass does
  not: a depth slot gets its contents only from the passes that attach it, and
  the device refuses a texture that is a written attachment and a bound texture
  in one pass. A pass may attach one slot and read another.
- A slot holds nothing between dispatches. The first pass of a dispatch to
  attach it clears it, so the passes that draw a shadow map and the passes that
  read it are passes of one program, and the map is drawn in every dispatch
  that reads it.

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
varyings, and returns `@location(0) vec4<f32>` into the pass's color output. An
integer varying must be declared `@interpolate(flat)` on both stages, and a
program that omits it is refused at register with the `invalid wgsl:` class. It
may also sample the pass's `inputs` through group 1, exactly as a fragment
pass does — a draw pass that reads a mask texture while rasterizing is an
ordinary declaration. The vertex entry point reads the same inputs, which is
how a table of per-instance data declared `Texel` reaches the geometry.

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

Beside a color output a depth slot declares `DepthExtent::Output` of that
output's `SlotExtent`, since a depth attachment has to match the size of the
color attachment it tests for, and it declares that output's
[`samples`](#samples): `One` beside a binding or a `One` transient, `Four`
beside a `Four` transient. A `DepthExtent::Fixed` slot beside a color output
rejects at register whatever its side: the output's size is the size of the
texture a dispatch binds, so whether the two agree is not known when the
program registers. A fragment entry point that writes
`@builtin(frag_depth)` needs a depth slot to write it into, and a pass that
writes it without declaring one rejects at register.

### Depth-only passes

A `Draw`, `DrawIndexedIndirect` or `DrawSets` pass that declares
`output: OutputSlot::None` is **depth-only**: it attaches its depth slot and no
color target. A shadow map is a `Fixed` slot written by depth-only passes and
[read](#reading-a-depth-slot) by the passes that light the scene, and a depth
pre-pass is the scene drawn depth-only into an `Output` slot that a later color
pass names. The rules, each refused at register when broken:

- The pass names a depth slot and writes it. `depth: None` would attach
  nothing, and a `DrawSets` pass with `DepthWrite::TestOnly` would write
  nothing.
- The slot may be of either extent and either sample count: there is no color
  output for it to match. The pass rasterizes at the slot's sample count.
- The pass declares `Blend::Replace` and `PassLoad::Load`. Both describe a
  color output the pass does not have. Whether the pass clears its depth slot
  is still decided by whether it is the first of the dispatch to name it.
- `entry_point` still names a fragment entry point. It returns nothing or
  `@builtin(frag_depth)` alone, and it may `discard`, which is how a cut-out
  texture leaves a hole of its own shape in the depth it writes. An entry point
  that returns a color rejects.
- `PassOutput` cannot name the pass, and the final pass of a program still
  writes a binding.

```wgsl
@fragment
fn fs_depth() {}
```

```jsonc
{ "stage": { "Draw": { "vertex_entry_point": "vs_light", "geometry": 0,
                       "depth": 0, "load": "Load" } },
  "blend": "Replace", "entry_point": "fs_depth",
  "inputs": [], "output": "None",
  "uniform_offset": 0, "uniform_length": 64, "repeat": null }
```

An `aether.render.program.timings` row reports a depth-only pass with stage
`Draw`, the width and height its depth slot resolved to on the most recent
dispatch, and `divisor` from an `Output` extent or `1` for a `Fixed` one.
Before the first accepted dispatch the size is `0 / 0`, as it is in every row.

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
The blend is the one the pass declares, as it is for fragment passes — see
[Blend](#blend).

### Channel-packed outputs

A pass writes one color output. Several planes of data therefore ride the
channels of one target: a bake that wants a region class, a key-light tone, and
a facing term packs them into the red, green, and blue channels of one `Rgba8`
output and a later fragment pass unpacks them. The surface declares no
multiple-render-target machinery, and a plane that needs full float precision
takes its own `R32Float` output from its own pass, whose single channel carries
it exactly.

Packing into `Rgba8` quantizes each channel to 256 levels, which suits labels
and low-frequency terms and does not suit an accumulator. `Rgba16Float` is the
packed alternative when independent quantities need float precision or
filtering: a pass can carry them through separate channels without quantizing
them to eight bits. Choose per plane: labels and tone can pack into `Rgba8`;
quantities that later math amplifies get a float target.

## Draw-set passes

A pass draws retained [draw sets](#the-draw-set-resource) by declaring
`stage: PassStage::DrawSets(DrawSetsPass { … })`
([ADR-0246](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0246-retained-draw-sets.md)
decision 4). One such pass is one render pass that issues every draw of every
set the dispatch lists for it: the sets in list order and each set's draws in
order, with nothing sorted. As for a draw pass, everything else on the pass
entry keeps its meaning: the fragment `entry_point`, the `inputs`, the
`output`, the uniform window, and `repeat`.

### The draw-sets declaration

```rust
DrawSetsPass {
    vertex_entry_point: String,           // a @vertex entry in the program's module
    vertex_layout: Vec<VertexAttribute>,  // every drawn geometry's layout
    instance_layout: Vec<VertexAttribute>, // every drawn instance buffer's layout
    draw_sets: u32,                       // index into ProgramDispatch.draw_sets
    cull: Cull,                           // None or Back
    depth: Option<DepthUse>,              // { slot, write: Write | TestOnly }
    load: PassLoad,                       // Clear or Load, on the color output
}
```

Over mail the stage reads:

```json
{ "DrawSets": {
    "vertex_entry_point": "vs_placed",
    "vertex_layout":   [{ "location": 0, "format": "Float32x3" }],
    "instance_layout": [{ "location": 4, "format": "Float32x3" },
                        { "location": 5, "format": "Unorm8x4" }],
    "draw_sets": 0,
    "cull": "Back",
    "depth": { "slot": 0, "write": "Write" },
    "load": "Clear" } }
```

with `depth` as `null` for a pass that does not depth-test. The pass names no
geometry slot and `ProgramRegister.geometries` does not grow for it: the two
layouts are the pass's own, and a set is drawn by a pass whose two layouts
equal the set's.

### The two-buffer shader contract

The pass binds two vertex buffers for each draw. Buffer 0 is the draw's
geometry, laid out by `vertex_layout` and stepped once per vertex. Buffer 1 is
the draw's instance records, laid out by `instance_layout` and stepped once per
instance, so the vertex stage runs once per vertex per record of the draw's
record run. Each attribute binds at the `@location` its layout declares, which
is the rule a geometry slot already follows; nothing is renumbered. The two
layouts therefore share no location, and neither declares one twice.

```wgsl
struct Placed {
    @builtin(position) position: vec4<f32>,
    @location(0) color: vec4<f32>,
}

@vertex
fn vs_placed(
    @location(0) corner: vec3<f32>,   // vertex_layout, per vertex
    @location(4) offset: vec3<f32>,   // instance_layout, per instance
    @location(5) color: vec4<f32>,    // instance_layout, per instance
) -> Placed {
    return Placed(vec4<f32>(corner + offset, 1.0), color);
}
```

The vertex stage's `@location` inputs are checked at register against the two
layouts together, each with the type its format is consumed as, the way a draw
pass is checked against its geometry slot. An attribute the stage does not read
is fine. The uniform window and the group-1 inputs are visible to the vertex
stage, so a record can carry an index into a `Texel` table the stage reads with
`textureLoad`.

### Cull and depth

`Cull::None` draws both windings. `Cull::Back` discards clockwise triangles;
the front face is counter-clockwise, as it is for a draw pass.

`depth: Some(DepthUse { slot, write })` attaches
`ProgramRegister.depth_transients[slot]` under a `LessEqual` test.
`DepthWrite::Write` writes each fragment that passes and `DepthWrite::TestOnly`
tests and leaves the slot as it was, which is what a pass drawing transparent
surfaces over an opaque scene wants. The [depth rules of a draw
pass](#depth) hold unchanged: beside a color output the slot must have that
output's extent and its sample count, a fragment entry that writes
`@builtin(frag_depth)` needs a slot, and the first
pass of a dispatch to name a slot clears it to the far plane whatever its stage
and whatever its `write`. A `TestOnly` pass that is the first to name its slot
tests against the far plane, so everything it draws passes. A draw-sets pass
may be [depth-only](#depth-only-passes), and it then declares
`DepthWrite::Write`: a depth-only `TestOnly` pass would write nothing and
rejects at register.

### List slots

`DrawSetsPass.draw_sets` is an index into `ProgramDispatch.draw_sets`, which is
a list of draw-set ids per slot. The program's list count is one more than the
highest index any pass names, and every index below it must be named by some
pass, so a dispatch never carries a list nothing draws. Two passes may name one
list: a colour pass and a depth-only pass draw the same sets that way.

The frame chooses what is drawn by which set ids it lists, so showing or hiding
a whole set costs nothing but the dispatch. A list may be empty; its passes
then clear or load their output as declared and draw nothing. A draw with a
zero index count or a zero record count is skipped.

Each frame, a dispatch pays one layout comparison per listed set per pass that
draws its list and one lookup per distinct buffer per listed set, and nothing
per draw: a draw was checked when its set was made, and a set holds the buffers
it draws, so a geometry or instance buffer destroyed under a listed set is
still drawn. An `aether.render.program.timings` row reports a draw-sets pass
with stage `Draw`.

## Register-time validation

Validation happens at register, once, and every failure class replies a
distinguishable `ProgramRegisterResult::Err { error }` — a
bad-but-parseable program replies an error instead of crashing the substrate.
The classes, in check order:

| Class | Reason shape |
|---|---|
| WGSL | `invalid wgsl: …` — naga parse or validation failure |
| Empty graph | `program declares no passes` |
| Extent | `binding N: extent divisor must be at least 1` (also for transients and depth transients) |
| Fixed side | `depth transient N: fixed side S is outside 1..=L, the device limit max_texture_dimension_2d` |
| Unresolvable transient | `transient N: a Four transient is read resolved, and R32Float cannot be resolved — declare it One, or in a format that resolves` |
| Read-only output | `pass N: binding B is declared Texture, which is read only — a pass writes only a Target binding` |
| Geometry slot | `geometry slot N: layout declares no attributes`; `geometry slot N: layout declares location L twice` |
| Entry point | ``pass N: no fragment entry point named `X` in the module`` |
| Slot range | `pass N: binding slot B is out of range (M declared)` (also for transients) |
| Sequence | `pass N reads the output of pass P, which does not run before it`; `pass N input I reads transient T before any earlier pass writes it` |
| Outputless alias | `pass N reads pass P through PassOutput, but that pass has no texture output — a compute pass and a depth-only pass write none` |
| Self-read | `pass N reads its own output slot` |
| Uniform window | `pass N: uniform window (L bytes) is shorter than the shader's uniform block (B bytes)` |
| Repeat | `pass N: repeat count must be at least 1`; `pass N: repeat count C exceeds the supported maximum 4096` |
| Final output | `the final pass must write a dispatch binding (the program's result texture)`; `binding N: the program's output binding must declare Target(Full) …` |
| Pipeline | `pipeline creation failed: …` — a wgpu validation error caught by the register's error scope (for example an array-typed input against a `Texture` slot, a sampler used on a `Texel` input, or an `Alpha` or `Additive` pass onto an `R32Float` output on a device that cannot blend it, none of which naga alone can see) |

The draw-pass classes, checked for every pass that declares `stage: Draw`:

| Class | Reason shape |
|---|---|
| Vertex entry | ``pass N: no vertex entry point named `X` in the module`` |
| Geometry range | `pass N: geometry slot G is out of range (M declared)` |
| Unbound location | `pass N: the vertex stage reads @location(L), which geometry slot G's layout does not declare` |
| Attribute type | `pass N: the vertex stage reads @location(L) as vec2<f32>, but geometry slot G's layout declares it Float32x3, which is consumed as vec3<f32>` |
| Depth range | `pass N: depth transient D is out of range (M declared)` |
| Depth extent | `pass N: depth transient D declares extent E, which does not match its color output's extent O — a depth attachment must be the size of the color attachment it tests for` |
| Depth samples | `pass N: depth transient D declares samples S, which does not match its color output's samples T — the attachments of one pass share one sample count` |
| Fixed depth | `pass N: depth transient D declares a fixed side S, but the pass has a color output, whose size is not known at register — a Fixed depth slot attaches only under a depth-only pass` |
| Undeclared depth | ``pass N: entry point `X` writes @builtin(frag_depth), so the pass must declare a depth transient to write it into`` |

The depth-only classes, checked for every `Draw`, `DrawIndexedIndirect` or
`DrawSets` pass that declares `OutputSlot::None`. The depth-range class above
applies to it; the depth-extent, depth-samples and fixed-depth classes do not,
since it has no color output to match:

| Class | Reason shape |
|---|---|
| No depth slot | `pass N: a rasterizing pass with OutputSlot::None is depth-only, so it must name a depth transient — with neither it would write nothing` |
| Depth-only blend | `pass N: a depth-only pass has no color output to blend onto, so it declares Blend::Replace, not Additive` |
| Depth-only load | `pass N: a depth-only pass has no color output to clear, so it declares PassLoad::Load, not Clear` |
| Depth-only entry | ``pass N: entry point `X` returns a color, but a depth-only pass has no color target — its fragment entry point returns nothing or @builtin(frag_depth) alone`` |
| Depth-only test | `pass N: a depth-only pass declaring DepthWrite::TestOnly would write nothing — its depth slot is all it attaches, so it declares DepthWrite::Write` (a `DrawSets` pass) |

A `Fragment` pass that declares `OutputSlot::None` keeps its own class: `pass N:
a fragment pass must declare a texture output`.

The depth-input classes, checked in this order for every `InputSlot::Depth` a
pass declares:

| Class | Reason shape |
|---|---|
| Depth input range | `pass N input I reads depth transient D, which is out of range (M declared)` |
| Multisampled depth input | `pass N input I reads depth transient D, which is declared Four — a multisampled depth slot can be neither compared nor sampled, so a pass reads a One slot` |
| Self-attached depth input | `pass N input I reads depth transient D, which the same pass attaches — a pass cannot read the depth slot it draws into` |
| Undrawn depth input | `pass N input I reads depth transient D before any earlier pass attaches it` |

The compute class, checked for every pass that declares `stage: Compute`
beside the compute-output class (`pass N: a compute pass must declare
OutputSlot::None`):

| Class | Reason shape |
|---|---|
| Compute blend | `pass N: a compute pass has no color output to blend onto, so it declares Blend::Replace, not Additive` |

The draw-sets classes, checked for every pass that declares `stage: DrawSets`.
The vertex-entry, depth-range, depth-extent, depth-samples, fixed-depth and
undeclared-depth classes above apply to it with the same reasons:

| Class | Reason shape |
|---|---|
| Empty layout | `pass N: the vertex layout declares no attributes` (also for the instance layout) |
| Repeated location | `pass N: the vertex layout declares location L twice` (also for the instance layout) |
| Shared location | `pass N: the vertex layout and the instance layout both declare location L — the two vertex buffers of a draw-sets pass share no location` |
| Unbound location | `pass N: the vertex stage reads @location(L), which the pass's vertex or instance layout does not declare` |
| Attribute type | `pass N: the vertex stage reads @location(L) as vec4<f32>, but the pass's vertex or instance layout declares it Uint8x4, which is consumed as vec4<u32>` |
| Unnamed list | `a pass names draw-set list L, but no pass names list M — the lists a program's passes name are numbered from 0 with none left out` |

The uniform-window class covers both stages of a draw or draw-sets pass: the
window must cover the block whichever stage reads it, so a pass whose vertex
stage is the only reader of group 0 still needs a window long enough for that
block.

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

A frame's program passes may span several queue submissions, in order. The
backend counts command buffers not yet submitted (Metal refuses the 4,097th and
loses the device, and each pass costs two), so the executor submits the frame's
encoder after every 1,024 program passes and continues in a fresh one. Dispatch
arrival order, and the rule that programs run before the world, material and
overlay passes that sample their outputs, hold across those submissions; a frame
under 1,024 program passes submits once.

Device replacement is the one exception to writable-pixel persistence
([ADR-0173](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0173-render-device-loss-recovery-contract.md)).
The program keeps its `program_id`, authored WGSL, validated plan, and folded
timing samples. Replacement rebuilds its pipelines and dispatch cache, drops
device-bound transient views and timing-query machinery, and leaves its
writable registry textures cleared under their existing ids. The actor
redispatches on its next ordinary repaint; the runtime never guesses at or
replays a dispatch whose submission outcome is unknown.

Programs rebuild independently. If one program's pipelines cannot compile on
the replacement device, that id is quarantined and later dispatches to it
warning-drop; sibling programs and the recovered frame continue. Initial
registration still rejects before assigning an id — quarantine applies only to
an already registered program during replacement. No generation or quarantine
signal is sent to the actor, and no program kind or payload changes.

Runtime mismatches **warn-drop the whole dispatch**: the checks run before any
recording, so a rejected dispatch records nothing and the frame survives —
other draws still render, and the output texture keeps its prior content. The
drop classes:

- an unknown `program_id`;
- a binding count that disagrees with the registered graph;
- a geometry count that disagrees with the registered graph;
- a binding naming an unknown texture id;
- a binding whose format disagrees with the declared `SlotSpec`;
- a `Target` binding whose size disagrees with its extent resolved against the
  reference (a `Texture` binding takes any size);
- a binding whose texture is not the kind its shape takes: a plain texture
  for `Target` and `Texture`, an array for `TextureArray`, a volume for
  `TextureVolume`;
- a non-`Writable` texture bound where the graph writes;
- a geometry slot naming an unknown geometry id;
- a geometry whose created layout disagrees with the slot's declared layout;
- a draw-set list count that disagrees with the registered graph;
- a list naming an unknown draw-set id;
- a listed draw set whose layouts are not the layouts of a pass that draws its
  list;
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
supplied texture and geometry bindings and draw-set lists, and either `phase = "setup"` or the
pass index, entry point, resolved input/output slots, and draw plan. A setup
error drops that dispatch's pass recording; a pass error stops its remaining
passes. Errors outside authored-program dispatches still reach the device's
generic uncaptured-error handler.

Because dispatch validation rejects every documented malformed input before
command recording, there is no supported public API for deliberately producing
one of these scoped GPU errors. The scopes diagnose backend validation failures
that escape the CPU checks, not a consumer-triggerable error class. Consumers
can verify log retrieval with a CPU warn-drop; deterministic exercise of GPU
error attribution requires test-only fault injection.

On the native wgpu-core backend these scope pops remove thread-local CPU scope
entries and return ready futures; they do not wait for submitted GPU work. The
ignored `empty_error_scope_cost` test is the repeatable adapter-backed
instrument for the per-setup/per-pass CPU cost.

## Transient pooling

Transients are pooled by resolved size, realized format and sample count, and
the pool is shared across programs and persistent across dispatches — a
repaint reuses its allocations. Within one dispatch, the executor assigns physical textures
by live range: a transient's texture is reusable once the last pass reading
it has recorded, strictly before the next holder's first write, so a pass
never samples a texture it is simultaneously attached to. A ping-pong chain
of any length settles on two physical allocations per class — declaring one
fresh transient per intermediate is cheap, and the graph author never manages
reuse by hand.

A `Four` transient takes a texture of its four-sample class, and one that some
pass reads also takes the single-sample texture it resolves into. That texture
comes from the class `One` transients of its size and format use and is packed
by the same live range, so a resolve texture and a `One` transient reuse each
other's allocations and never overlap.

Depth transients pool by resolved extent and sample count alongside color
transients, with one difference in policy: they are not packed by live range. Sharing a depth buffer
is what the declaration is for, so each declared depth slot that some pass
names gets its own physical texture, and two distinct slots never land on the
same one however disjoint their use looks. A declared depth slot no pass names
allocates nothing. A `DepthExtent::Fixed` slot's class is its own side, so a
resize of the output leaves it in the class, and on the texture, it had; an
`Output` slot moves to the class of its new size as a transient does.

A `One` depth texture is created as an attachment a pass can also bind, as a
`One` transient is, so a slot some pass reads and one that is only attached are
the same class and reading a slot allocates nothing more. A `Four` depth
texture is an attachment only. The sample count is part of the class, so the
two never stand in for each other.

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
through the overlay path, two passes sharing a depth transient, a color pass
comparing against a depth slot a depth-only pass drew and a fragment pass
loading one, the register classes, and a dispatch naming a geometry id that
does not exist. The registry
lifecycle over mail has its own scenario in
[`geometry_scenario.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-render/tests/geometry_scenario.rs),
and the draw-set lifecycle in
[`draw_set_scenario.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-render/tests/draw_set_scenario.rs).
[`draw_sets_pass_scenario.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-render/tests/draw_sets_pass_scenario.rs)
covers the draw-sets stage in rasterized pixels: two sets sharing a geometry,
a dispatch listing an unknown set, a geometry destroyed under its set, a
`TestOnly` pass, and back-face culling.
[`samples_blend_scenario.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-render/tests/samples_blend_scenario.rs)
covers samples and blend: a half-covered texel of a `Four` transient, a `Four`
transient read between two writers, an additive accumulation on a float
transient, and a `Replace` pass erasing what an `Rgba8` output held.

## Chassis behavior

- **Desktop** executes programs. A `register` sent before the render device
  exists (before the first window attaches) is answered once it is up. A
  sender asks once from `wire` and continues from its response handler: it
  learns the `program_id` only from the reply, so it dispatches from there.
  The request's own chain settles first, so over MCP such a call settles with
  no reply; one sent after a window is listed is answered inside the call.
- **Headless** composes no render actor, so a component that depends on
  render is refused at load there rather than mailing programs into a
  stand-in.
- **SubstrateHarness** executes programs for real — it has a wgpu adapter —
  which is what makes a parity scenario an ordinary `cargo test`. Driverless
  machines skip such tests cleanly.
- The minimal hub chassis installs no `aether.render` mailbox, so program
  mail cannot resolve there.
- Every render device is requested with the adapter's own
  `max_texture_array_layers` and `max_buffer_size` in place of the defaults
  of 256 layers and 256 MiB; every other limit stays at its default. The
  layer count a `create_texture_array` admits is therefore the adapter's.

## The worked consumer: the wash

The watercolour easel in
[`aether-puppet/src/easel/program/`](https://github.com/iamacoffeepot/aether/tree/archive/aether-puppet/crates/aether-puppet/src/easel/program)
is archived on the `archive/aether-puppet` tag, and it is the large-scale
consumer and the best reference for program authoring at scale. Its develop is one registered program of several hundred passes laid
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
