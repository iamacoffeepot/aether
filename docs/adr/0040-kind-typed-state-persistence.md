# ADR-0040: Kind-typed state persistence

- **Status:** Proposed
- **Date:** 2026-04-23

## Context

ADR-0016 shipped per-component state persistence across hot reload: `save_state_p32(version, ptr, len)` as a host fn the guest calls from `on_replace` to deposit opaque bytes, and `on_rehydrate_p32` as an export the new instance's SDK calls so component code can read them back. The substrate is byte-transparent — it stores `(version, bytes)` per mailbox across the `replace_component` boundary and hands them to the next instance with no idea what's inside.

Since ADR-0016 landed, the kind system has grown into a real typed-data layer:

- **ADR-0030** gave every `Kind` a canonical 64-bit `Kind::ID` computed as `fnv1a_64(KIND_DOMAIN ++ canonical(name, schema))`. Changing the schema changes the id — identity is load-bearing.
- **ADR-0019** specified wire-format encoding: cast-shaped (`#[repr(C)]`) kinds serialise by transmute, postcard for everything else. Symmetric decode paths ship in `aether-mail` + `aether-hub-protocol`.
- **ADR-0032** shipped the `aether.kinds` custom section so component manifests ride inside the wasm; agents can `describe_component` to see what a component speaks.

The result: mail is typed end-to-end, but persisted state is still opaque bytes. Every component that saves non-trivial state ends up hand-rolling `postcard::to_allocvec(&my_state)` / `postcard::from_bytes` — duplicating serialisation logic the kind derive already emits for the same structs. Worse, the `version: u32` field in the ADR-0016 API is the component author's manual reminder "when the shape of this state changes, bump this number"; miss the bump and the new instance decodes last-run's bytes with the new layout, producing garbage. The kind system already tracks schema identity automatically via `Kind::ID` — we're duplicating that mechanism poorly.

The motivating forcing function isn't a specific component today — it's consistency. Every other typed-data path in aether (mail, sinks, replies) resolves through the kind system. Save-state is the last loose wire where components talk to the substrate with bytes the substrate can't describe.

## Decision

**The kind-typing layer is imposed at the SDK, not at the host fn.** The `save_state_p32` / `on_rehydrate_p32` signatures are unchanged. Typed behaviour is a guest-SDK convention: `Ctx::save_state_kind<K>` prepends `K::ID.to_le_bytes()` to canonically-encoded value bytes and writes the concatenation through the existing host fn; `PriorState::as_kind<K>` reads the leading 8 bytes, compares to the expected `K::ID`, and decodes the remainder against the kind's schema.

The substrate stays byte-transparent. It sees `(version, bytes)` exactly as it does today. The fact that bytes may start with an 8-byte kind id is invisible to it — guest-side convention, guest-side responsibility.

### 1. SDK surface

Adds typed methods alongside the existing raw ones; raw methods stay for any use case that wants them (e.g. migration across kind-id boundaries, or persisting something that isn't expressible as a kind).

```rust
impl<'a> DropCtx<'a> {
    /// Persist a typed value across replace. Wire layout:
    ///   [0..8]  K::ID (little-endian)
    ///   [8..]   canonical encoding of `value` per K's schema
    /// `version` is passed through to the substrate unchanged — agents
    /// typically set it to 0 for kind-typed saves, since kind id covers
    /// schema identity. Non-zero version lets the component stack a
    /// migration counter on top of kind identity if it wants to.
    pub fn save_state_kind<K: Kind + Schema + Serialize>(
        &mut self,
        version: u32,
        value: &K,
    );

    /// Unchanged — the raw bytes API. Callers that want explicit
    /// framing (older components, migration flows) still reach for it.
    pub fn save_state(&mut self, version: u32, bytes: &[u8]);
}

impl<'a> PriorState<'a> {
    /// Decode the prior state as kind `K`. Returns `Some` when the
    /// leading 8 bytes match `K::ID` and the remainder decodes
    /// cleanly; `None` on id mismatch (schema evolved, foreign kind,
    /// not a kind-typed save). The raw bytes and version are still
    /// reachable via the existing accessors for fallback handling.
    pub fn as_kind<K: Kind + Schema + DeserializeOwned>(&self) -> Option<K>;

    /// Unchanged — raw byte access for migration / inspection.
    pub fn bytes(&self) -> &[u8];
    pub fn version(&self) -> u32;
}
```

### 2. Wire framing

```
[0 ..  8)   kind_id: u64 (little-endian)
[8 ..  N)   encode_canonical::<K>(value)
```

`encode_canonical` picks cast or postcard per ADR-0019's dispatch — the exact byte layout the mail path already uses for the same kind. Sharing the encoder means a kind's state bytes and its wire bytes are byte-identical; a test can roundtrip a state through the mail encoder and the save/load paths interchangeably.

### 3. Id-mismatch policy

On `on_rehydrate` with persisted bytes whose leading 8 bytes don't match the `K::ID` the component asks for, `as_kind::<K>` returns `None`. The component decides what to do:

- Default behaviour: ignore the prior state (boot fresh). Matches the "schema changed, old state is garbage" invariant hot-reload already implies.
- Opt-in migration: call `prior.bytes()` + `prior.version()` and decode manually, or `prior.as_kind::<OldStateV1>()` to attempt a known-old shape.

No automatic migration path. The kind system has no notion of "V1 vs. V2 of the same kind" — they're different kinds with different ids. Migration is manual and explicit, which matches how the rest of the engine handles schema change today.

### 4. Non-kind saves stay legal

A component that wants to persist a blob that isn't a kind (raw bytes from a file, a checkpoint from an external library, etc.) keeps using `DropCtx::save_state(version, bytes)` unchanged. The typed methods are additive, not replacement.

### 5. Host fn unchanged

`save_state_p32(version: u32, ptr: u32, len: u32) -> u32` and `on_rehydrate_p32(ptr: u32, len: u32)` keep their ADR-0016 / ADR-0024 shapes. No wire-level migration, no substrate-side code change, no breaking change to existing compiled components.

## Consequences

### Positive

- **One typed-data layer.** Mail, sinks, replies, and now persisted state all resolve through the same `Kind` + `Schema` machinery. The author writes `#[derive(Kind, Schema, Serialize, Deserialize)]` once and gets every aether data path for free.
- **Schema identity is load-bearing automatically.** Changing a state struct changes `K::ID`; the rehydrate path rejects stale bytes without the component author remembering to bump the `version` manually. ADR-0016's manual-version footgun disappears for kind-typed saves.
- **Substrate stays dumb.** No new substrate code, no new wire surface, no kind-aware persistence store on the native side. The simplicity the ADR-0016 design argued for is preserved.
- **Existing components unaffected.** The raw `save_state(version, bytes)` / `PriorState::bytes()` API keeps working; this is purely additive at the SDK.
- **Testable without a host.** The SDK's encode/decode is pure Rust — `encode_with_id::<K>(&value)` then `decode_with_id::<K>(bytes)` roundtrips under a unit test, no wasm or substrate needed.

### Negative

- **Substrate-side introspection is still absent.** An agent asking "what state does this mailbox hold?" gets bytes-and-a-version-number, same as today. The kind id is *in* the bytes but only the guest SDK knows how to pull it. If we ever want `describe_state` as an MCP tool, it'd need a parallel convention or a substrate-side opt-in. Judged not worth the complexity today.
- **Guest SDK owns the framing.** Two component crates compiled against different SDK versions could disagree on framing (byte order, field ordering, leading-id convention). Since the SDK is path-of-least-resistance and all first-party components share one tree, realistic blast radius is zero — but worth noting for a future third-party ecosystem.
- **Non-kind saves still exist.** Components that use the raw API get no schema validation; nothing prevents two components from writing incompatible bytes to the same mailbox's prior-state slot across replace. Same as today.
- **"Store bytes, not a struct" is a guest-side responsibility.** A buggy guest that calls `raw::save_state(0, junk_bytes, len)` can write garbage that `as_kind::<K>` will politely return `None` for. The failure mode is "we boot fresh," which is benign, but a misbehaving component could surprise an author expecting state to survive.

### Neutral

- **Wire format unchanged.** Host fn signatures, wasm custom sections, and the kind manifest all untouched.
- **`version` field remains.** It stays as a passthrough on `save_state` / `PriorState`; kind-typed callers default it to 0 but may still set it if they want a migration counter on top of kind identity. `on_rehydrate` callers that already read `version` keep seeing it.
- **`replace_component` semantics unchanged.** ADR-0022's drain-on-swap still orchestrates the lifecycle; ADR-0016's prior-state hand-off still happens before the new instance's `on_rehydrate`. This ADR only changes what the SDK does with the bytes flowing through.

## Alternatives considered

- **Change the host fn signature to carry `kind_id: u64`.** Tempting because it lets the substrate validate or introspect. Rejected: it's a breaking wire change for what's architecturally a guest-side concern, and it drags the substrate into knowing about kinds for a path that has worked byte-opaque since ADR-0016. The "substrate stays dumb" instinct that shapes the render / audio / camera sinks should apply here too.
- **Type the Component trait with an associated `State: Kind + Schema`.** `impl Component { type State = MyState; fn save(&mut self) -> Self::State; fn rehydrate(&mut self, s: Self::State); }`. Nicer ergonomics — the author never touches bytes. Rejected *for now*: locks every component into exactly one state kind (what if you want multiple slots? or conditional persistence?), and commits to a trait shape before we have one concrete component using it seriously. Possible ADR-0040-phase-2 on top of this one once we've seen how it gets used.
- **Versioned kinds with automatic migration.** A `KindVersion` trait pointing at an older `Kind`, with an SDK migration chain. Rejected: speculative and adds meaningful complexity for a problem no component has today. Manual migration through `prior.bytes()` covers the corner cases until someone actually needs the automation.
- **Put kind id in the `version` field (truncated).** `version = (K::ID as u32)`. Concise. Rejected: 32 bits of a 64-bit hash leaves a real collision chance across a growing registry; silent decode of garbage on the unlucky path. The 8-byte prefix is 1.5% overhead on a typical state and closes the collision door.
- **Do nothing.** Keep opaque bytes; let each component postcard-encode on its own. Rejected: this is the status quo and it duplicates the derive the kind system already provides. The SDK win is large for an author and the substrate cost is zero.

## Follow-up work

- **PR**: implement `DropCtx::save_state_kind<K>` and `PriorState::as_kind<K>` in `aether-component`; share the `encode_canonical` helper with the mail path so the byte layouts stay identical.
- **PR**: unit tests for roundtrip, id-mismatch (returns None), cast-shaped kind, postcard-shaped kind, non-kind bytes (returns None without panic).
- **PR**: update the doc comments on `Component::on_replace` / `on_rehydrate` to point at the typed methods as the preferred path; keep the raw API documented as "for migration / opaque-blob cases."
- **Parked, not committed**: typed `State` associated type on the `Component` trait (ADR-0040-phase-2 if the typed API gets used heavily).
- **Parked, not committed**: substrate-side state introspection (e.g. `describe_state` MCP tool) — requires the substrate to learn the leading-id convention.
- **Parked, not committed**: automated kind-version migration chains.
