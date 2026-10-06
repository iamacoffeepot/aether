# ADR-0246: Retained Draw Sets

- **Status:** Proposed
- **Date:** 2026-10-05

## Context

ADR-0170 gave the render capability authored programs and ADR-0171 gave a program draw passes: `PassStage::Draw(DrawPass)` rasterises one geometry slot per pass. That is enough for one mesh and too little for a scene. A scene is tens of thousands of placed objects sharing a few hundred meshes, and today each drawn object is its own dispatch and its own render pass.

Two rounds of measurement on the branch `spike/mesh-draw-path` (Apple M4 Pro, Metal, 1280×720 offscreen, release) put numbers on that and on the alternatives:

- **A pass per draw does not scale.** A pass costs about 35 µs, and a compute-plus-indirect pair about 90 µs. 300 meshes × 20,000 instances took 94 ms through the one instancing route that exists (`PassStage::Compute` feeding `PassStage::DrawIndexedIndirect`, instance data in a texture), against 3 ms for the same picture baked into static batches on the CPU at 11.5 KB per placed instance.
- **Many draws in one pass do.** With a prototype pass that walks a list of draws, a draw costs about 0.07 µs of CPU with no state change and 0.22–0.40 µs with one, plus 0.10–0.15 µs on the GPU side. 30,000 draws ran in 5.3 ms and 3,000,000 in 420 ms with no failure.
- **A retained list beats a list in the dispatch.** Carrying 30,000 draws in every dispatch is a 960 KB mail and 0.15–0.9 ms of encode, decode and validation per frame. A retained list is a 174-byte dispatch. Editing it is cheap: a 100-entry patch 11 µs, a full 30,000-entry rebuild 1.2 ms, a 10,000-record instance update 48 µs.
- **A texture array beats binding per draw and an atlas.** Array and atlas were equal within noise; binding a texture per draw cost 0.25–0.9 ms more per frame and about 25 µs per texture of setup. The array wraps correctly with an ordinary repeat sampler; the atlas differed on 2.7% of drawn pixels without shader work, and more would be needed under mip filtering.
- **Post-processing already fits the program model.** A bloom chain (threshold, three downsamples, three upsamples, tonemap) registered and ran as an ordinary ADR-0170 program reading the texture the scene pass wrote, for about 0.15 ms per frame. 4× multisampling on the scene cost about 0.43 ms. Writing the last pass into the frame instead of a texture saved nothing (2.07 ms against 2.06 ms).
- **Multi-draw indirect gains nothing here.** On Metal `multi_draw_indexed_indirect` is a loop of single indirect draws, and the GPU-chosen count variant is not implemented in the backend.

The same work listed what the program model refuses today and a scene needs: multisampled targets (`MultisampleState::default()` in `crates/aether-substrate/src/render/program.rs`), culling on draw passes, blending on float targets (`blend_for` in `crates/aether-render/src/runtime/program/mod.rs`), inputs whose size is not the output extent over a whole number (`SlotExtent`, issue 7401), a sampler choice per input, and texture arrays.

Depth is one more refusal, and no measurement covers it. A depth slot can only be attached: `create_program_depth_transient` in `crates/aether-substrate/src/render/program.rs` creates its texture as a render attachment and nothing else, `InputSlot` has no variant that names one, and every rasterizing pass must declare a colour output (`attached_output` in `crates/aether-render/src/runtime/program/validate.rs`). A shadow is a comparison against depth drawn from the light, and fog, water depth fade and soft edges read the scene's own depth, so without a depth read each of them needs a colour target that carries a copy of depth.

The first consumer is the clean-room game client in `research/runite`, which will draw a streamed world from asset bundles (ADR-0163): one mesh per model shared by every placement, with per-placement variation read from a lookup table in the vertex stage.

## Decision

**1. A draw set is a retained, validated list of draws.**

```rust
#[aether_data::kind(name = "aether.render.create_draw_set")]
pub struct CreateDrawSet {
    pub vertex_layout: Vec<VertexAttribute>,
    pub instance_layout: Vec<VertexAttribute>,
    pub draws: Vec<DrawSpec>,
}
pub struct DrawSpec {
    pub geometry_id: u32,
    pub indices: IndexRange,
    pub instances_id: u32,
    pub instances: InstanceRange,
}
pub struct IndexRange { pub first: u32, pub count: u32 }      // counts indices
pub struct InstanceRange { pub first: u32, pub count: u32 }   // counts records
#[aether_data::kind(name = "aether.render.create_draw_set_result")]
pub enum CreateDrawSetResult { Ok { draw_set_id: u32 }, Err { error: String } }
#[aether_data::kind(name = "aether.render.update_draw_set")]
pub struct UpdateDrawSet { pub draw_set_id: u32, pub first: u32, pub draws: Vec<DrawSpec> }
#[aether_data::kind(name = "aether.render.update_draw_set_result")]
pub enum UpdateDrawSetResult { Ok, Err { error: String } }
#[aether_data::kind(name = "aether.render.destroy_draw_set")]
pub struct DestroyDrawSet { pub draw_set_id: u32 }
```

Creation replies `CreateDrawSetResult` and an update `UpdateDrawSetResult`; a destroy carries no reply. An `Err` names the failing draw and its class, a refused creation consumes no id, and a refused update leaves the set unchanged. The two ranges are separate types because one counts indices and the other records.

An update writes `draws` over the set's entries from position `first`. `first` may be at most the set's length, and a run that passes the end extends the set, so appending is an update at `first` equal to the length. An empty `draws` truncates the set to its first `first` entries. No entry moves unless the sender moves it, and a draw with a zero count is valid and draws nothing.

A set is validated against layouts, not against a program, so one set can be drawn by any pass whose layouts match: a colour pass and a depth-only pass share it. A draw carries no texture and no per-draw constant; what varies between draws rides in the instance records.

**2. A draw inside a set cannot fail at frame time.** Every check runs once, where the sender gets a reply:

- When a geometry is created, every index is checked against the vertex count.
- When a set is created or patched, each draw's ids exist, their layouts match the set's, the index range is inside the geometry and the instance range inside the buffer's capacity.

Four rules keep those checks true afterwards. A set holds the buffers it draws: destroying a geometry or instance buffer a set names retires the id, and the GPU resource lives until the last such set is gone. A geometry a set names may have its vertex contents written in place but not its vertex count or its indices changed; `UpdateGeometry` carries no reply, so a replacement that would change the vertex count or the indices of a geometry a set names is dropped with a warning and leaves the geometry as it was. An instance buffer's capacity is fixed at creation. Growing either means creating a new one and patching the set.

What stays fallible is per dispatch, as a bad binding id is today: an unknown draw-set id, a set whose layouts are not the pass's, wrong bindings or uniforms.

**3. Instance records are a resource.**

```rust
#[aether_data::kind(name = "aether.render.create_instances")]
pub struct CreateInstances { pub layout: Vec<VertexAttribute>, pub capacity: u32, pub records: Blob }
#[aether_data::kind(name = "aether.render.create_instances_result")]
pub enum CreateInstancesResult { Ok { instances_id: u32 }, Err { error: String } }
#[aether_data::kind(name = "aether.render.update_instances")]
pub struct UpdateInstances { pub instances_id: u32, pub first: u32, pub records: Blob }
#[aether_data::kind(name = "aether.render.destroy_instances")]
pub struct DestroyInstances { pub instances_id: u32 }
```

The engine does not interpret a record. It is a vertex buffer stepped per instance, laid out by the same `VertexAttribute` list a geometry uses. `capacity` and `first` count records. Creation replies `CreateInstancesResult`; an update and a destroy carry no reply, and a refused update is logged and leaves every record as it was. The engine keeps the records it is given, so they survive a render device replacement under the same id.

**4. `DrawSets` is a program stage.**

```rust
pub enum PassStage {
    Fragment,
    Draw(DrawPass),
    DrawIndexedIndirect(DrawPass),
    Compute(ComputePass),
    DrawSets(DrawSetsPass),
}
pub struct DrawSetsPass {
    pub vertex_entry_point: String,
    pub vertex_layout: Vec<VertexAttribute>,
    pub instance_layout: Vec<VertexAttribute>,
    pub draw_sets: u32,              // which of the dispatch's lists this pass draws
    pub cull: Cull,
    pub depth: Option<DepthUse>,
    pub load: PassLoad,
}
pub enum Cull { None, Back }
pub struct DepthUse { pub slot: u32, pub write: DepthWrite }
pub enum DepthWrite { Write, TestOnly }

pub struct ProgramDispatch {
    pub program_id: u32,
    pub bindings: Vec<u32>,
    pub geometries: Vec<u32>,
    pub draw_sets: Vec<Vec<u32>>,    // per list slot: the sets drawn this frame, in order
    pub uniforms: Vec<u8>,
}
```

The pass draws its list's sets in order and each set's draws in order; nothing is sorted. The frame chooses what is drawn by which set ids it lists, so showing or hiding a whole set costs nothing but the dispatch. A scene pass and the post-processing that reads it are one program, under the one set of rules ADR-0170 already has for targets, transients, uniforms, timing and order.

The pass binds a draw's geometry as vertex buffer 0, stepped per vertex, and its instance records as vertex buffer 1, stepped per instance. An attribute binds at the `@location` its layout declares, as a geometry slot's attributes do, so the two layouts share no location and a set's layouts compare equal to the layouts of a pass that draws it.

`DrawSetsPass.draw_sets` indexes the dispatch's lists. A program's list count is one more than the highest index a pass names, and every index below it is named by some pass, so a dispatch carries no list that nothing draws. Two passes may name one list.

`depth` attaches a `depth_transients` slot under a `LessEqual` test; `Write` writes the depth of each fragment that passes and `TestOnly` leaves the slot as it was. The first pass of a dispatch to attach a depth slot clears it to the far plane, whatever its stage and whatever its `write`. `Cull::Back` discards clockwise triangles.

A `DrawSets` pass is timed as a draw pass: `PassStageKind` gains no variant.

**5. A binding says what it takes and how it is sampled.**

```rust
pub struct SlotSpec { pub format: TextureFormat, pub shape: SlotShape, pub sampling: Sampling }
pub enum SlotShape {
    Target(SlotExtent),   // sized from the output; a pass may write it
    Texture,              // any size, read only
    TextureArray,         // any size and layer count, read only
    TextureVolume,        // any width, height and depth, read only
}
pub enum Sampling { Filtered { wrap: Wrap, mips: Mips }, Texel }
pub enum Wrap { Clamp, Repeat }
pub enum Mips { Base, Chain }
```

A read-only binding is visible to the vertex stage as well as the fragment stage, and `Texel` reads exact values, so a table of data is an ordinary texture. This is the whole of issue 7401.

`Wrap` is the sampler's address mode: `Clamp` extends the edge texel and `Repeat` tiles. A `TextureVolume` binding takes a volume texture (decision 6) and nothing else, the shader declares it `texture_3d<f32>`, and `Wrap` addresses all three of its axes. `Mips::Base` reads the base level only and `Mips::Chain` filters across the whole mip chain. `Sampling` carries no filter: linear or nearest stays a property of the bound texture, which the quad and material paths draw under the same setting.

A `Texel` input binds a texture and no sampler. Input `n` of a pass keeps its numbering whatever its sampling: its texture is `@binding(2 * n)`, and for a `Texel` input `@binding(2 * n + 1)` is left out of the layout, so the inputs after it do not shift. The texture entry is declared unfilterable, so a texture of any format binds there, and the shader reads it with `textureLoad`.

Among bindings only a `Target` has an extent, so every binding a pass writes is a `Target`, and the final pass's binding is `Target(SlotExtent::Full)`. A transient declares no shape: it carries its own extent (decision 7), and a pass may write it or read it.

**6. Texture arrays and volume textures are resources.**

```rust
#[aether_data::kind(name = "aether.render.create_texture_array")]
pub struct CreateTextureArray { pub format: TextureFormat, pub side: u32, pub layers: u32, pub mips: Mips }
#[aether_data::kind(name = "aether.render.create_texture_array_result")]
pub enum CreateTextureArrayResult { Ok { texture_id: u32 }, Err { error: String } }
#[aether_data::kind(name = "aether.render.write_texture_layer")]
pub struct WriteTextureLayer { pub texture_id: u32, pub layer: u32, pub pixels: Blob }
#[aether_data::kind(name = "aether.render.create_texture_volume")]
pub struct CreateTextureVolume { pub format: TextureFormat, pub width: u32, pub height: u32, pub depth: u32, pub pixels: Blob }
#[aether_data::kind(name = "aether.render.create_texture_volume_result")]
pub enum CreateTextureVolumeResult { Ok { texture_id: u32 }, Err { error: String } }
```

Side and layer capacity are fixed at creation and a layer's contents are written in place. The device requests the adapter's array-layer and buffer-size limits rather than the defaults of 256 layers and 256 MiB, which the measurements hit first.

Creation replies `CreateTextureArrayResult`, and a refused creation consumes no id. A write carries no reply; a refused write is logged and leaves the layer as it was. An array's id and a volume's come from the sequence a plain texture's does, so one id names a texture, an array or a volume and never two of them, and `aether.render.destroy_texture` is the destroy path of all three.

Mips are supplied, not generated. A `Mips::Base` array has one level. A `Mips::Chain` array has `floor(log2(side)) + 1` levels, level `n` having side `max(1, side >> n)`, and `WriteTextureLayer.pixels` carries every level of the layer, base level first. A write is all of a layer's levels or none of them. A layer that was never written reads as zero in every channel. The engine keeps the pixels of each written layer, so they survive a render device replacement under the same id.

A volume is `width` by `height` by `depth` texels and is given whole at creation: `pixels` holds `depth` slices, slice 0 first, each row-major and top-down. It is immutable, so there is no write kind and new contents are a new volume, and it has one level. It is read linear when its format can be filtered, which interpolates between slices as between texels. Each dimension is checked against the default three-dimensional limit every render device is requested at, so creation reads no device: it replies `CreateTextureVolumeResult` at once, before the first device exists as after, and a refused creation consumes no id. The engine keeps the pixels, so a volume survives a render device replacement under the same id.

**7. Targets gain samples and passes gain blend.**

```rust
pub struct TransientSpec { pub format: TextureFormat, pub extent: SlotExtent, pub samples: Samples }
pub struct DepthSpec { pub extent: DepthExtent, pub samples: Samples }   // DepthExtent: decision 9
pub enum Samples { One, Four }
pub struct ProgramPass { pub stage: PassStage, pub blend: Blend, /* as today */ }
pub enum Blend { Replace, Alpha, Additive }
```

A pass that reads a `Four` transient reads its resolved image; the pass graph is fixed at registration, so the engine resolves once, after the last writer before each reader. Blend is the pass's, on every colour format including float.

A transient declares no sampling. A pass reads one as a texture with a sampler that clamps and reads the base level, linear where the format can be filtered and nearest where it cannot.

A pass's sample count is its colour output's. A binding is a registry texture and is single-sample, so a multisampled scene is drawn into a `Four` transient and a later pass carries the resolved image to a binding. The rule covers every render stage. A pass's depth slot declares the same count as its colour output, and one that differs is refused at registration, as a depth slot of another extent is.

The multisampled texture keeps its samples between passes, so a second writer under a load draws over the first writer's samples. The resolve happens in the last iteration of the last pass to write the transient before each pass that reads it: a writer followed by another writer does not resolve, and two readers with no writer between them share one resolve. A depth slot is never resolved. A `Four` transient of a format that cannot be resolved is refused at registration, whether or not a pass reads it; of the five formats that is `R32Float`.

`Replace` overwrites the output on every channel. `Alpha` is straight-alpha source over. `Additive` adds source to destination on every channel, alpha included. A compute pass has no colour output and declares `Replace`; any other value is refused at registration. The engine keeps no format rule for blend: `R32Float` blends where the device offers `float32-blendable`, which the engine requests whenever the adapter has it, and on a device without it an `Alpha` or `Additive` pass onto `R32Float` is refused in the register reply.

**8. A program never writes the frame.** Every pass writes a texture, and the frame shows a program's output through the composite path that exists today. One fullscreen quad costs about 0.3 ms at 1280×720 on an otherwise empty frame and was cheaper than drawing the scene into the always-multisampled frame.

**9. A pass can read a depth slot.**

```rust
pub enum DepthExtent {
    Output(SlotExtent),      // sized from the output, as a transient is
    Fixed { side: u32 },     // a square of its own size
}
pub enum InputSlot {
    Binding { index: u32 },
    PassOutput { pass: u32 },
    Transient { index: u32 },
    Depth { index: u32, read: DepthRead },   // a depth_transients slot
}
pub enum DepthRead { Texel, Compare }
```

`InputSlot::Depth` names a `depth_transients` slot. Input `n` keeps the numbering of decision 5: its texture is `@binding(2 * n)`, and the shader declares it `texture_depth_2d`. The read is declared on the input, so one pass may compare a slot that another loads. Every stage that binds group 1 may read one: the fragment stage of any pass, the authored vertex stage of a rasterizing pass, and a compute pass.

`Texel` is `Sampling::Texel` for depth. It binds no sampler, `@binding(2 * n + 1)` is left out of the layout, and `textureLoad` returns the stored depth. `Compare` binds a `sampler_comparison` at `@binding(2 * n + 1)`, read with `textureSampleCompare` in a fragment stage and `textureSampleCompareLevel` in any stage. The comparison is `LessEqual`, the test a pass attaches a depth slot under (decision 4), so a reference depth passes exactly where a fragment at that depth would have been drawn into the slot, and a texel nothing was drawn to holds the far plane and passes every reference. The sampler clamps, reads the slot's one level, and is linear, so at the edge of what was drawn the result is a fraction between 0 and 1, filtered from the comparisons around the coordinate. A `Compare` input can still be read with `textureLoad`.

Four things about a depth input are refused at registration:

- An index past `depth_transients`.
- A slot no earlier pass attaches. A depth slot gets its contents only from the passes that attach it, as a transient is written before it is read.
- A slot the same pass attaches. The device refuses a texture that is a written attachment and a bound texture in one pass.
- A `Four` slot. A multisampled depth texture binds only as `texture_depth_multisampled_2d`, which is loaded one sample at a time and can be neither compared nor sampled, and a render pass has no depth resolve.

An entry point that disagrees with the declaration, reading a `texture_2d<f32>` where a depth input is declared or using a plain sampler on a `Compare` input, fails pipeline creation and replies `Err`, as a mismatch under decision 5 does. A `One` depth texture is created as an attachment that can also be bound, as a `One` transient is, so reading a slot adds no pool class; a `Four` one stays an attachment only.

A rasterizing pass may declare `OutputSlot::None`. It is then depth-only: it attaches its depth slot and no colour output. `Draw`, `DrawIndexedIndirect` and `DrawSets` may all be depth-only, because the three share one pipeline builder and one depth rule and an exception for two of them would be the added code. A `Fragment` pass attaches no depth slot and a compute pass attaches nothing, so `OutputSlot::None` on a `Fragment` pass stays refused.

- A depth-only pass names a depth slot and writes it: `depth: None`, or a `DrawSets` pass with `DepthWrite::TestOnly`, would write nothing and is refused.
- It declares `Blend::Replace` and `PassLoad::Load`, and any other value is refused, as a compute pass's blend is: both fields describe a colour output the pass does not have. Whether the pass clears its depth slot is still decided by whether it is the first of the dispatch to attach it.
- `entry_point` still names a fragment entry point, and the pipeline has no colour target. The entry point returns nothing or `@builtin(frag_depth)` alone, and may `discard`, which is how a cut-out texture casts a shadow of its own shape. One that returns a colour is refused at registration.
- It rasterizes at its depth slot's sample count.
- `PassOutput` cannot name it, and the final pass of a program still writes a binding.
- It is timed as a draw pass. Its `PassTimingRow` reports the size its depth slot resolved to, with the divisor of an `Output` extent and `1` for a `Fixed` one.

A pass with a colour output still attaches a depth slot of that output's extent and sample count (decision 7): the slot declares `Output` with the output's `SlotExtent`. A `Fixed` slot under a colour output is refused at registration, whatever its side. The output's size is the size of the texture a dispatch binds, so whether the two agree is not known when the program registers, and a mismatch found at dispatch could only drop the dispatch. A depth-only pass has no colour output to match and attaches a slot of either extent. A shadow map is therefore a `Fixed` slot written by depth-only passes. `Fixed.side` is at least 1 and at most the device's `max_texture_dimension_2d`, and a side outside that is refused at registration. A `Fixed` slot keeps its size, and its pooled texture, when the output is resized.

A depth slot holds nothing between dispatches: the first pass of a dispatch to attach it clears it. The passes that draw a shadow map and the passes that read it are passes of one program, and the map is drawn in every dispatch that reads it. A render device replacement has nothing of a depth slot to restore; its texture is created again by the next dispatch, and the comparison sampler is rebuilt with the program samplers. Two distinct depth slots never share a texture, so a pass may attach one slot and read another.

Scene depth under 4× multisampling is read from a second slot. The scene pass attaches a `Four` slot, which no pass can read, so a consumer that wants the scene's depth lists the same draw sets for a depth-only pass onto a `One` `Output` slot and reads that. The two differ only where a triangle edge crosses a texel, which fog, a depth fade and a soft edge tolerate.

## Consequences

- A static scene is a set per streamed region built at load, plus small sets for what moves. Per frame the engine receives a few hundred bytes and walks retained lists.
- A consumer that varies placements of one mesh (colour, texture layer, anything a table can hold) does it with an index in the instance record and a `Texel` table read in the vertex stage. The engine knows nothing of the scheme.
- `UpdateGeometry` gains a drop class: a replacement that would change the vertex count or the indices of a geometry a set names is dropped with a warning and leaves the geometry as it was, since the kind carries no reply. Callers that resize a geometry no set names are unaffected.
- The program model grows in seven places at once (stage, binding shape, sampling, samples, blend, dispatch lists, depth reads). Each is a registration-time shape with validation there, in keeping with ADR-0170; the work is several issues, not one.
- `PassStage::Draw` and `DrawIndexedIndirect` stay. Whether `DrawSets` should replace them is left until it has a second consumer.
- A single-sample depth slot is readable, so a shadow, fog, a water depth fade and a soft edge need no colour target carrying a copy of depth. A depth pre-pass needs nothing more either: a depth-only pass writes the slot and the colour pass attaches it `TestOnly`, which the `LessEqual` test admits at equal depth.
- A shadow map is redrawn in every dispatch that reads it. A depth slot that keeps its contents across dispatches, for a light and a scene that did not move, is not designed here.
- The engine applies no depth bias and `Cull` has no front-face variant, so a pass that compares against a shadow map offsets its reference depth in its own shader. A bias on the depth-only pass is left until a consumer shows that the shader offset is not enough.
- A `Four` depth slot stays unreadable, and scene depth under 4× multisampling costs a second geometry pass onto a `One` slot.
- Not measured: the stage inside the program executor (the prototype ran beside it), every part of decision 9 (a depth-only pass, a comparison read, the second geometry pass under 4× multisampling), any backend but Metal, device-loss recovery, and adapters whose array-layer limit stays at 256.
- The measurements also showed the frame wait rounding every frame to about 1.27 ms, because the Metal fence wait in wgpu-hal sleeps in 1 ms steps. That bounds frame rate independently of this decision and is its own issue.

## Alternatives considered

- **A draw-list executor beside programs** — what the prototype was. Ships sooner, and leaves two sets of rules for targets, uniforms and frame order, with post-processing coordinated across them from outside.
- **The draw list in each dispatch** — no retained state to edit, at 0.15–0.9 ms and about 1 MB of mail per frame at 30,000 draws, and every draw validated every frame.
- **A texture per draw** — up to 0.9 ms more per frame, a bind group per texture, and a texture id in every draw that must be validated.
- **A texture atlas** — one bind and no layer limit, but wrong at repeat seams without shader work and gutters under mip filtering.
- **CPU-baked static batches** — the fastest frame measured, at 11.5 KB per placed instance and a rebake whenever a placement changes.
- **GPU-driven multi-draw** — no gain on Metal and unavailable where the backend lacks the count variant.
- **Let a pass write the frame** — measured no saving, and the frame's fixed format, sample count and depth make it a different kind of target from every other.
- **A colour target carrying a copy of depth** — needs no change to depth slots, and costs a colour target and a fragment stage on every pass that writes it, in `R32Float`, which cannot be filtered or resolved and cannot be read through a comparison sampler.
- **The read declared on the depth slot** — one field fewer on each input, and a slot one pass compares could not be loaded by another.
- **Let a `TestOnly` pass read the slot it attaches** — the device allows a depth attachment to be bound as a texture in the same pass when the pass does not write it, which a soft edge drawn in the scene pass would use. It needs the attachment declared read-only for the whole pass and is left until a consumer needs it.
- **Read a `Four` depth slot per sample** — keeps one geometry pass, and gives every reader a loop over four loads with no comparison sampler and no filtering.
- **Allow a geometry to resize under a set and re-validate each frame** — puts a check per distinct geometry in every frame to serve a case (a mesh changing its topology while placed) that replacing the geometry already covers.
