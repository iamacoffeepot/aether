# ADR-0019: Unified schema encoding for mail kinds

- **Status:** Accepted
- **Date:** 2026-04-17
- **Accepted:** 2026-04-17

## Context

ADR-0007 gave the hub a kind-descriptor vocabulary so agents could send mail by `params` instead of hand-packing bytes. The descriptor enum has three arms: `Signal` (empty), `Pod { fields }` (`#[repr(C)]` scalars and fixed-size scalar arrays), and `Opaque` (raw bytes — the hub can't help, the caller must supply `payload_bytes`).

That cut held while the only kinds in flight were ticks, vertex slabs, and a handful of `u32`s. It's holding less well now. Two concrete pain points:

- **Control-plane payloads are unsendable from MCP without a side binary.** `aether.control.load_component` carries a wasm blob, a `Vec<KindDescriptor>`, and an `Option<String>`. The descriptor surface can't describe any of those shapes, so the kind is `Opaque`. To load a component over MCP today, the agent compiles `crates/aether-substrate/examples/smoke_017_load.rs:46-53`, which postcard-encodes a `LoadComponentPayload` into a byte-array literal that gets pasted back into `send_mail`'s `payload_bytes`. That is the wrong shape for "ergonomic agent harness."
- **Result kinds can't be described.** `aether.control.load_result`, `drop_result`, `replace_result` are all `Ok { mailbox } | Err { reason }`. They're sums of a struct and a struct-with-string. Sums aren't expressible in `Pod`, so they ship as `Opaque` and agents decode them by reading the bytes themselves.

Component-to-component messaging extends the same pressure: now that ADR-0017 lets components exchange runtime-discovered request/response, the second wave of kinds wants strings (names, error messages), byte buffers (snapshots), enums (status codes). All of that hits `Opaque` today.

`Pod`'s defining property is the cast-shortcut: wire bytes equal in-memory layout, so the guest can `bytemuck::cast(payload)` and skip decoding entirely. For the renderer, that means `[DrawTriangle]` lands in a wgpu vertex buffer with no per-element pass. It's a real optimization — but the only kind exercising it today is `DrawTriangle`, the engine has no real workload, and there's no profiling evidence the cast-shortcut is load-bearing. Keeping it as a *user-facing* descriptor variant is what forces the two-family awkwardness ("can my type be `Pod`?") onto every kind author.

Forces at play:

- **The hub already speaks postcard.** Every `EngineToHub` / `HubToEngine` frame is postcard-encoded; `LoadComponentPayload` and friends are postcard-encoded. There is no new dependency cost to making postcard the canonical mail wire format too.
- **Kind descriptors live in a closed protocol crate.** `aether-hub-protocol::KindEncoding` is a serde enum the hub and substrate both depend on. Widening it is a one-shot wire-format change; we are not yet committed to forward compatibility (V0).
- **The SDK is brand-new.** ADR-0014's `Component` trait and ADR-0015's lifecycle hooks landed in the last two weeks. Any descriptor-side change has only one consumer (`aether-hello-component`) and one producer (`aether-substrate-mail`). Migration cost is bounded.
- **Pod's cost on the wire is similar to postcard's.** Postcard encodes scalars as their native LE byte representation, fixed-size arrays inline, structs as concatenated fields. The decode pass is per-field rather than per-array, which is the actual delta — not bandwidth.
- **Best-effort mail contract (ADR-0017).** Encoding errors at the hub are validation failures returned to the agent; they don't change the engine-side delivery semantics.

## Decision

Collapse `Pod` / `Signal` / `Opaque` into a single user-facing `Schema` encoding shaped like postcard. Every kind ships with a schema; the hub encodes from agent params and the guest decodes via a derive-generated helper. The `#[repr(C)]` cast-shortcut survives — but as a derive-picked annotation on `Struct` schemas, not a separate user-facing encoding family. Authors don't choose; `#[derive(Kind)]` picks based on the type's structural properties.

### 1. The new `KindEncoding`

```rust
pub enum KindEncoding {
    Schema(SchemaType),
}

pub enum SchemaType {
    Unit,
    Bool,
    Scalar(Primitive),                  // u8..i64, f32, f64
    String,
    Bytes,                              // Vec<u8>
    Option(Box<SchemaType>),
    Vec(Box<SchemaType>),
    Array { element: Box<SchemaType>, len: u32 },
    Struct { fields: Vec<NamedField>, repr_c: bool },
    Enum { variants: Vec<EnumVariant> },
}

pub struct NamedField { pub name: String, pub ty: SchemaType }

pub enum EnumVariant {
    Unit { name: String, discriminant: u32 },
    Tuple { name: String, discriminant: u32, fields: Vec<SchemaType> },
    Struct { name: String, discriminant: u32, fields: Vec<NamedField> },
}
```

`Pod` dies. `Signal` dies (`SchemaType::Unit` covers it). `Opaque` dies — every kind has a schema. The `payload_bytes` escape hatch on the `send_mail` MCP tool dies with it (see §6); the engine ↔ hub wire still carries `payload: Vec<u8>` because that *is* the transport, but agents lose the ability to inject pre-encoded bytes.

The `repr_c: bool` flag on `Struct` is the cast-shortcut hint: when `true`, both ends serialize the struct as `#[repr(C)]`-laid-out raw bytes (the format today's `Pod` kinds already use) instead of postcard. It is only legal when every field is itself cast-eligible — scalars, fixed `Array`s of cast-eligible elements, or nested `Struct { repr_c: true }`. `String`, `Bytes`, `Vec`, `Option`, and `Enum` fields disqualify a struct from `repr_c: true` (alignment can't survive variable-length framing). The hub's encoder dispatches on the flag; the substrate's decoder does the same.

Cast and slab semantics: a top-level kind whose schema is `Struct { repr_c: true }` keeps today's "mail.count packs N raw repetitions" behavior — the substrate decodes via `bytemuck::cast_slice` exactly as it does for current Pod kinds. Postcard kinds carry one encoded value per mail; multi-element delivery there means `Vec<T>` inside the struct, not `mail.count > 1`.

### 2. Wire format is postcard (with cast-shaped opt-in)

The hub's encoder takes agent-supplied JSON params and a `SchemaType`, and produces either postcard bytes or `#[repr(C)]` bytes depending on `repr_c`. The guest decodes symmetrically. We pick postcard as the default because:

- The hub-protocol crate already depends on it.
- It handles every `SchemaType` variant natively.
- Its wire format is documented and stable.

`#[repr(C)]` stays the wire format for cast-eligible structs because (a) it preserves wire compatibility for every existing cast kind and (b) the hub already has the byte-layout walker that today's `Pod` encoder uses — reusing it is cheaper than retiring it.

### 3. SDK surface — `#[derive(Kind)]`

`aether-component` grows a derive macro that, given:

```rust
#[derive(Kind)]
#[kind(name = "demo.request")]
pub struct Request {
    pub seq: u32,
    pub note: String,
}
```

emits `impl Kind for Request`, the `SchemaType` materializer the substrate ships at handshake, and the encode/decode helpers `Mail::decode` / `ctx.send` already call. Today's authoring path — `#[repr(C)] + Pod + Zeroable + impl Kind { const NAME }` — collapses to one derive plus the name attribute.

The derive picks the wire format from the type's structure:

- **Type is `#[repr(C)]` and every field is cast-eligible** (scalar, fixed array of cast-eligible, or another `#[repr(C)]` cast-eligible struct): emit `Struct { repr_c: true, ... }`. Encode/decode go through `bytemuck` — bytes are the in-memory layout. Equivalent to today's `Pod` path.
- **Otherwise** (any string, vec, option, enum, or non-`#[repr(C)]` field): emit `Struct { repr_c: false, ... }`. Encode/decode go through postcard.

Authors don't pick. They write the type; the derive picks the format. `DrawTriangle` stays cast-shaped because its fields are cast-eligible scalars and arrays. `LoadComponentPayload` becomes postcard-shaped because it has a `String` field. Both look identical at the call site.

### 4. Existing kinds migrate in-place

- `Tick`, `MouseButton` → `SchemaType::Unit`.
- `Key`, `MouseMove`, `Ping`, `Pong`, `FrameStats` → `Struct { repr_c: true, fields: scalars }`. Wire bytes unchanged.
- `Vertex`, `DrawTriangle` → `Struct { repr_c: true }` and `Struct { repr_c: true, fields: [Array { Vertex, len: 3 }] }`. Cast-shortcut preserved; vertex slabs still land in the wgpu buffer with no per-element decode.
- `LoadComponent`, `ReplaceComponent`, `DropComponent`, `LoadResult`, `DropResult`, `ReplaceResult` → become real schemas with `repr_c: false` (each carries strings or sums). The hub can now build these from agent params; `smoke_017_load.rs` goes away.

### 5. Why the cast hint lives on `Struct`, not as a separate arm

Two design choices on the cast variant worth recording:

- **Top-level only.** `bytemuck::cast` requires aligned bytes. A cast-eligible struct *embedded* inside a postcard-framed parent loses alignment — the substrate would have to copy into an aligned buffer, defeating the zero-copy benefit. So `repr_c: true` is meaningful only for the kind's top-level schema. It's still legal to nest `repr_c: true` structs inside other `repr_c: true` structs (the whole subtree is one cast-able blob); but a `repr_c: true` field inside a `repr_c: false` parent is not a useful optimization.
- **No separate `CastableStruct` arm.** A flag on `Struct` keeps the schema vocabulary at 10 arms and makes "this struct is also castable" a property of the struct, not a separate type. The flag is what the derive sets and what the encode/decode dispatch reads — both ends agree by descriptor.

### 6. The `payload_bytes` escape hatch is removed

`send_mail`'s `payload_bytes` parameter is deleted from the MCP tool surface. Every kind has a schema, so every kind is reachable via `params`. There is no path by which an agent supplies pre-encoded bytes.

This is deliberate. Agents (Claude in particular) gravitate to the lowest-friction route, and a bytes hatch is *always* the lowest-friction route the moment a schema is awkward — defeating the entire point of unifying on schema. Removing the hatch makes "the schema is wrong" a forcing function: fix the schema, regenerate the descriptor, restart, rather than work around it. The only acceptable failure mode for "I can't send this kind" is "the engine didn't describe this kind" — which is a real bug, not something to paper over.

Notes on what this *doesn't* break:

- **Genuine binary blobs are still sendable.** `wasm`, snapshots, opaque state bundles become `SchemaType::Bytes` fields. The hub accepts a JSON byte array (or whatever encoding the MCP server picks for `Bytes`) and encodes it through postcard like any other field.
- **The wire is unchanged.** `MailFrame.payload: Vec<u8>` and `EngineMailFrame.payload: Vec<u8>` stay — they're the transport, not the agent surface. Bytes still flow.
- **Foreign engines were always hypothetical.** ADR-0007's "engines can ship empty `kinds` and rely on `payload_bytes`" carve-out has zero consumers and was never exercised. Any future non-Aether engine ships descriptors per the protocol; if it can't, it doesn't get to participate via `send_mail`. That's the correct stance.

### 7. What this ADR does not do

- **No descriptor versioning.** V0 hub-protocol changes the wire format unilaterally. Forward-compatibility versioning is parked until a non-trivial schema-evolution event (e.g., adding a field to a deployed kind) actually surfaces.
- **No cross-language schemas.** Postcard is Rust-flavored; non-Rust engines that want to participate ship matching postcard producers/consumers. WIT remains parked per ADR-0005 / ADR-0007.
- **No agent-side decode of replies.** The hub returns received mail as raw bytes to the agent today (`receive_mail`'s `payload_bytes`). Symmetrizing that to schema-driven decode is a follow-on; this ADR's scope is the agent → engine direction where the cumbersomeness is. (The inbound `payload_bytes` field on `receive_mail` is not the same surface as the outbound one — agents need *some* way to see reply bytes until the symmetric decode lands.)

## Consequences

### Positive

- **One encoding family, one mental model.** Component authors stop asking "is my type Pod-compatible?" Every kind is schema-described; the choice doesn't exist.
- **Hub can build every aether-shipped kind from params.** Control-plane mail (`LoadComponent`, `ReplaceComponent`, `DropComponent`) and result kinds become first-class MCP citizens. The `smoke_017_load.rs` workaround disappears.
- **Component-to-component rich messages just work.** Strings, byte buffers, enums, optionals, nested structs are first-class — not opt-in via `Opaque`.
- **Renderer-style slab kinds keep the cast-shortcut.** `[DrawTriangle]` arrays still land in the wgpu vertex buffer with no per-element decode; the derive picks `repr_c: true` for cast-eligible types automatically.
- **SDK boilerplate shrinks.** `#[derive(Kind)]` replaces `#[repr(C)] + Pod + Zeroable + manual impl Kind`. New kinds are one derive and an attribute.
- **Result-type kinds become uniformly handleable.** `LoadResult` etc. stop being agent-decoded blobs; the hub renders them as JSON like any other kind.
- **Descriptor surface is expressive enough to grow into.** Sums, options, vecs, nested structs cover everything the engine has reached for so far and plausibly will reach for next (state bundles, snapshots, structured logs).
- **No agent escape hatch means schema bugs surface as bugs.** Removing `payload_bytes` from `send_mail` denies the agent its lowest-friction workaround. A wrong/missing schema fails loudly at the hub instead of getting papered over with hand-encoded bytes that drift from the engine's actual decode.

### Negative

- **Wire format breaks for every kind that gains a non-cast-eligible field.** Cast-shaped kinds (vertex/key/mouse/ping/pong/frame_stats) keep their existing wire bytes. Control-plane and result kinds change format because they were `Opaque` and now have real schemas — but those had no stable wire contract worth preserving.
- **Hub encoder grows two paths.** Today's encoder is a flat field walker over POD primitives. The new encoder still has that walker (now reused for `repr_c: true` structs) plus a postcard-aware path for everything else. Substrate decode is symmetric. Mitigated: both walkers are mechanical and the dispatch is one boolean.
- **Two encode/decode paths instead of one.** The "single encoding family" framing has a small footnote: *internally* the substrate and hub each carry two paths (cast and postcard) and pick by `repr_c` flag. User-facing surface stays one family because the derive picks; but maintenance cost is two paths, not one.
- **Descriptor surface is wider.** `SchemaType` has 10 arms vs. `KindEncoding`'s 3. More to validate at handshake; more to render in `describe_kinds`. Offset: still a closed enum, no extension points.
- **Lose the "incentive to keep kinds Pod-shaped" pressure.** Pod's existence nudged authors toward simple, fixed-layout types. With that gone, kinds may bloat into JSON-like blobs more readily. Watch for this in review; not a wire-format problem, an authoring discipline one.
- **No agent fallback when an engine ships a broken descriptor.** Today an agent can `payload_bytes` around a bad schema and keep working; tomorrow it can't. The right fix is "fix the engine"; the cost is the round-trip required to do so. Acceptable trade — the hatch was a worse failure mode (silent drift) than the absence of one (loud, blocking, fixable).

### Neutral

- **Postcard wire bytes ≈ `#[repr(C)]` wire bytes for scalars and fixed arrays.** Bandwidth on postcard-shaped kinds isn't a regression vector; decode CPU is. Cast-shaped kinds pay neither (wire bytes and decode path are unchanged).
- **`describe_kinds` MCP tool surface stays the same shape** — just renders a richer schema.
- **`KindsChanged` (ADR-0010) keeps working.** Components that register new kinds at load time still ship descriptors via `KindsChanged`; the descriptor type widens but the frame doesn't change shape.

## Alternatives considered

- **Pod + Schema as two user-facing encoding families.** Keep today's `Pod` arm and add a parallel `Schema` arm; component authors pick per kind. Rejected: doubles the descriptor surface and makes "can my type be Pod?" a per-kind authoring decision. The accepted design — `repr_c: bool` on `Struct`, picked automatically by the derive — keeps the optimization without exposing the choice.
- **Postcard everywhere; defer the cast-shortcut to a later ADR.** Earlier draft of this ADR. Rejected: doing it now is strictly cheaper than doing it later (avoids a second wire-format break), the implementation cost is small (one boolean dispatch on each end), and the renderer's vertex slabs are the kind of path you don't want to discover is a bottleneck *after* migrating off cast.
- **Opt-in cast hint that authors set explicitly.** Same shape as accepted, but the author sets `repr_c: true` themselves. Rejected: indistinguishable from the accepted design at the wire level, but reintroduces the per-kind authoring decision the auto-picked derive removes. Author-set is fine as a future escape hatch if the derive's heuristic ever gets it wrong.
- **Status quo: keep `Opaque` + manual encoding for rich types.** Rejected: this is exactly the friction motivating the ADR. `smoke_017_load.rs` is the existence proof.
- **Keep `payload_bytes` as a deliberate escape hatch.** Rejected: agents will reach for it the moment a schema is awkward, and that's exactly the regression we're trying to design out. Removing it makes "the schema is wrong" a forcing function rather than a workaround. Genuine binary blobs are still expressible via `SchemaType::Bytes`.
- **WIT / component-model schemas.** Canonical cross-language answer. Rejected for the same reasons ADR-0005 and ADR-0007 rejected it: heavy infra for a problem that hasn't surfaced. Postcard is the right rung on the ladder for V0.
- **Invent an Aether-native wire format instead of using postcard.** Rejected: postcard already covers the variant set, the hub-protocol crate already depends on it, and there's no Aether-specific constraint that motivates a custom format. Reusing postcard keeps the wire-format ownership cost at zero.

## Follow-up work

- **`aether-hub-protocol`**: collapse `KindEncoding` to `Schema(SchemaType)`; introduce the new `SchemaType` / `NamedField` / `EnumVariant` types; remove `Pod`/`PodField`/`PodFieldType`/`PodPrimitive` once consumers have migrated.
- **`aether-hub`**: postcard-aware param encoder consuming `SchemaType` + `serde_json::Value`. Symmetric to today's POD encoder but recursive over `SchemaType`. Drop `payload_bytes` from the `send_mail` tool definition; surface a clear error if an agent supplies it.
- **`aether-substrate-mail`**: rewrite every kind through `#[derive(Kind)]`. Cast-eligible kinds (vertex/key/mouse/ping/pong/frame_stats) keep `#[repr(C)]` + `bytemuck` derives so the proc macro can detect them. Control-plane and result kinds drop `Pod`/`Zeroable` and gain real schemas (strings, sums).
- **`aether-component`**: `#[derive(Kind)]` proc macro emitting `Kind` impl + `SchemaType` materializer + the right encode/decode helper based on detected `repr_c` eligibility. Detection rule: `#[repr(C)]` attribute present and every field type is itself cast-eligible (recursively).
- **`aether-substrate`**: dual-path dispatch in `Component::deliver` — `bytemuck::cast` for `repr_c: true` kinds (today's path), postcard decode for the rest. Renderer's vertex-buffer ingest stays cast-based.
- **Smoke tests**: end-to-end MCP `send_mail` → engine → MCP `receive_mail` for at least one kind from each `SchemaType` arm (string, vec, enum, nested struct), plus a cast-eligible kind to verify the slab path is unchanged. Delete `smoke_017_load.rs` once `LoadComponent` is hub-encodable.
- **Parked, not committed:**
  - Author-set `#[kind(repr_c = false)]` opt-out for the auto-picked cast hint — additive if the derive's heuristic ever needs an override.
  - Schema-driven decode of mail returned through `receive_mail` (agent-side decode symmetry).
  - Cross-language descriptors (WIT-ish schema interchange).
  - Descriptor schema versioning beyond V0's "change the wire and migrate everyone."
