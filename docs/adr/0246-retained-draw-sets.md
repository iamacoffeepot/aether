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

Four rules keep those checks true afterwards. A set holds the buffers it draws: destroying a geometry or instance buffer a set names retires the id, and the GPU resource lives until the last such set is gone. A geometry a set names may have its vertex contents written in place but not its vertex count or its indices changed; `UpdateGeometry` with different sizes is refused with an error while a set names it. An instance buffer's capacity is fixed at creation. Growing either means creating a new one and patching the set.

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

**5. A binding says what it takes and how it is sampled.**

```rust
pub struct SlotSpec { pub format: TextureFormat, pub shape: SlotShape, pub sampling: Sampling }
pub enum SlotShape {
    Target(SlotExtent),   // sized from the output; a pass may write it
    Texture,              // any size, read only
    TextureArray,         // any size and layer count, read only
}
pub enum Sampling { Filtered { wrap: Wrap, mips: Mips }, Texel }
```

A read-only binding is visible to the vertex stage as well as the fragment stage, and `Texel` reads exact values, so a table of data is an ordinary texture. This is the whole of issue 7401.

**6. Texture arrays are a resource.**

```rust
#[aether_data::kind(name = "aether.render.create_texture_array")]
pub struct CreateTextureArray { pub format: TextureFormat, pub side: u32, pub layers: u32, pub mips: Mips }
#[aether_data::kind(name = "aether.render.write_texture_layer")]
pub struct WriteTextureLayer { pub texture_id: u32, pub layer: u32, pub pixels: Blob }
```

Side and layer capacity are fixed at creation and a layer's contents are written in place. The device requests the adapter's array-layer and buffer-size limits rather than the defaults of 256 layers and 256 MiB, which the measurements hit first.

**7. Targets gain samples and passes gain blend.**

```rust
pub struct TransientSpec { pub format: TextureFormat, pub extent: SlotExtent, pub samples: Samples }
pub struct DepthSpec { pub extent: SlotExtent, pub samples: Samples }
pub enum Samples { One, Four }
pub struct ProgramPass { pub stage: PassStage, pub blend: Blend, /* as today */ }
pub enum Blend { Replace, Alpha, Additive }
```

A pass that reads a `Four` transient reads its resolved image; the pass graph is fixed at registration, so the engine resolves once, after the last writer before each reader. Blend is the pass's, on every colour format including float.

**8. A program never writes the frame.** Every pass writes a texture, and the frame shows a program's output through the composite path that exists today. One fullscreen quad costs about 0.3 ms at 1280×720 on an otherwise empty frame and was cheaper than drawing the scene into the always-multisampled frame.

## Consequences

- A static scene is a set per streamed region built at load, plus small sets for what moves. Per frame the engine receives a few hundred bytes and walks retained lists.
- A consumer that varies placements of one mesh (colour, texture layer, anything a table can hold) does it with an index in the instance record and a `Texel` table read in the vertex stage. The engine knows nothing of the scheme.
- `UpdateGeometry` gains a refusal. Callers that resize a geometry no set names are unaffected.
- The program model grows in six places at once (stage, binding shape, sampling, samples, blend, dispatch lists). Each is a registration-time shape with validation there, in keeping with ADR-0170; the work is several issues, not one.
- `PassStage::Draw` and `DrawIndexedIndirect` stay. Whether `DrawSets` should replace them is left until it has a second consumer.
- Depth stays attachment-only. Sampling depth is wanted for fog and soft edges and is not designed here because nothing measured needs it.
- Not measured: the stage inside the program executor (the prototype ran beside it), any backend but Metal, device-loss recovery, and adapters whose array-layer limit stays at 256.
- The measurements also showed the frame wait rounding every frame to about 1.27 ms, because the Metal fence wait in wgpu-hal sleeps in 1 ms steps. That bounds frame rate independently of this decision and is its own issue.

## Alternatives considered

- **A draw-list executor beside programs** — what the prototype was. Ships sooner, and leaves two sets of rules for targets, uniforms and frame order, with post-processing coordinated across them from outside.
- **The draw list in each dispatch** — no retained state to edit, at 0.15–0.9 ms and about 1 MB of mail per frame at 30,000 draws, and every draw validated every frame.
- **A texture per draw** — up to 0.9 ms more per frame, a bind group per texture, and a texture id in every draw that must be validated.
- **A texture atlas** — one bind and no layer limit, but wrong at repeat seams without shader work and gutters under mip filtering.
- **CPU-baked static batches** — the fastest frame measured, at 11.5 KB per placed instance and a rebake whenever a placement changes.
- **GPU-driven multi-draw** — no gain on Metal and unavailable where the backend lacks the count variant.
- **Let a pass write the frame** — measured no saving, and the frame's fixed format, sample count and depth make it a different kind of target from every other.
- **Allow a geometry to resize under a set and re-validate each frame** — puts a check per distinct geometry in every frame to serve a case (a mesh changing its topology while placed) that replacing the geometry already covers.
