# ADR-0059: Content-hashed field tags for upgradable component storage

- **Status:** Proposed (Draft — brainstorm capture; revisit before implementation)
- **Date:** 2026-04-27

## Context

Today every kind payload travels in one of two wire shapes:

- **Cast** (`Struct { repr_c: true }`) — raw `#[repr(C)]` bytes, decoded by `bytemuck::cast`. Field layout is positional in the language itself. Hot-path kinds (`DrawTriangle`, `Vertex`, `Tick`).
- **Postcard** (everything else) — postcard 1.x wire, fields concatenated in declaration order, no per-field tag or length. Control-plane kinds, mail with `Vec`/`Option`/`Enum`/`Map` shape.

Both are positional. Adding, removing, or reordering a field in source produces a different `Kind::ID` (the hash includes the canonical schema bytes — ADR-0030, ADR-0032) *and* a wire-incompatible payload. Sender and receiver have to be exact-id matches; any drift is an undeliverable.

That's fine for live mail, where sender and receiver are in lockstep within a session. It is **not fine for the persistent handle store** (ADR-0049): payloads written against component v1 still need to be readable after the component upgrades to v2. Today an upgrade invalidates every stored payload at the kind layer, even if the schema change was a benign field addition.

A version graph layered on top of the current hashes (sketched in chat 2026-04-27) was one direction. The cleaner direction is **a wire format that is itself version-tolerant** — fields self-identify, receivers tolerate unknown ids, missing ids fall back to defaults. Most of the version-graph problem then dissolves; the residual cases (type changes, semantic renames) shrink to a handful.

This ADR captures the brainstorm of that wire format. It's deliberately under-specified — there's enough open shape that committing now would be premature.

## Decision (sketch)

Add a third wire shape, **TLV with content-hashed field tags**, alongside cast and positional-postcard. The trait surface forks: `Mail` for kinds that ride the wire (cast or postcard, current behavior); `Storage` for kinds that live in durable backing stores via TLV. Both extend a bare `Kind` trait that carries only metadata (`NAME`, `ID`), so neither subtrait can decode the other's bytes — the type system enforces wire-shape correctness rather than relying on runtime checks. Scope: storage payloads written to the handle store (ADR-0049); possibly later, save files via ADR-0041's `save://` namespace.

### Trait hierarchy

Three traits, with `Kind` as the bare metadata supertrait:

```rust
pub trait Kind {
    const NAME: &'static str;
    const ID: u64;
    const IS_INPUT: bool = false;
    // No decode/encode methods on the bare trait — pure metadata.
}

pub trait Mail: Kind {
    fn decode_from_bytes(bytes: &[u8]) -> Option<Self> where Self: Sized;
    fn encode_into_bytes(&self) -> Vec<u8>;
}

pub trait Storage: Kind {
    fn decode_storage(bytes: &[u8]) -> Option<StorageData<Self>> where Self: Sized;
    fn encode_storage(data: &StorageData<Self>) -> Vec<u8>;
}

pub struct StorageData<T> {
    pub value: T,
    pub unknown_fields: Vec<UnknownField>,
}

pub struct UnknownField {
    pub hash: u64,
    pub bytes: Vec<u8>,
}
```

A type implements either `Mail` or `Storage`, never both. Disjoint trait membership prevents wire-shape mistakes at the type level: trying to decode a `Storage` kind's TLV bytes as if they were postcard would require calling `decode_from_bytes`, which doesn't exist on `Storage` — the type system blocks the misuse.

User-facing derive macros:

- `#[derive(Mail)]` produces `impl Kind + impl Mail` — cast or postcard wire (autodetected from `#[repr(C)]`).
- `#[derive(Storage)]` produces `impl Kind + impl Storage` — TLV wire.

The existing `#[derive(Kind)]` becomes an alias for `#[derive(Mail)]` during migration; the substantive split is between `Mail` and `Storage`. Existing call sites that constrain `K: Kind` for decode/encode work migrate to `K: Mail`.

**Runtime type rename.** The current `Mail<'a>` runtime type — passed to `#[fallback]` handlers — renames to `Envelope<'a>` to free up `Mail` as the trait name. The semantics are unchanged; the new name reflects the role (it's the carrier you open to find the typed value, not the content). Pre-1.0 so the rename is mechanical: `#[fallback]` signatures and `Mail::decode_kind::<K>()` calls update in lockstep.

**Wire reachability.** Storage kinds cannot ride mail directly. Mail reaches them only through `Ref<S>` (handle indirection per ADR-0045): mail carries a handle id; the substrate's handle store holds the TLV bytes; the receiver resolves the handle and decodes via `Storage::decode_storage`. The bytes-format never crosses the trait boundary, so the wire-shape disjointness stays clean and there's no "wrap a Storage value as Bytes-on-mail" path needed for v1.

### Wire format

The wire format described below applies to `Storage` kinds. `Mail` kinds use the existing cast or positional-postcard shape unchanged.



A struct payload is a sequence of `[field_hash][length][bytes]` records, concatenated in field-hash sort order. Receivers walk the records, look each `field_hash` up in their local schema, dispatch the bytes against the matched field's type, skip unknown ids, and default missing ids.

```
+----------------+----------+----------------+
| field_hash u64 | len varint | postcard body |
+----------------+----------+----------------+
```

Field bodies are encoded against the field's declared type using existing postcard rules (varint scalars, length-prefixed strings, etc.). The body is self-describing only at the `(field_hash, length)` envelope; primitive bytes inside don't carry their own type tags. Receivers that don't know a field id skip `length` bytes and continue.

### Field hash

For each field, a stable 64-bit content hash:

```
field_hash = fnv1a_64_prefixed(FIELD_DOMAIN, canonical(field_name, field_type))
```

`FIELD_DOMAIN` is a new prefix disjoint from `KIND_DOMAIN` and `MAILBOX_DOMAIN` so the id spaces don't overlap. The canonical bytes mirror today's `canonical_serialize_kind` but at the field granularity.

Renames change the field hash (the name is in the canonical bytes). A remap dictionary — `[(old_field_hash, new_field_hash)]` per kind, declared by the kind's author at rename time — bridges the gap. The dict is small, rarely consulted, ships in a wasm custom section adjacent to the kinds manifest (ADR-0028 / ADR-0032).

**Hash width: 64-bit.** All id spaces (`Kind::ID`, `MailboxId`, field hashes, variant hashes) use 64-bit FNV-1a. Per-kind cumulative collision probability stays below 10⁻¹⁰ at realistic ecosystem scope; the derive-time collision check (rule 2) catches the rare birthday strike as a compile error. 128-bit was considered and rejected on FFI grounds — wasm32 has no native 128-bit type, so every host fn carrying ids would split into pairs of i64. Issue [#320](https://github.com/iamacoffeepot/aether/issues/320) tracks the trigger conditions for revisiting if ecosystem growth or threat-model shifts (third-party kinds from untrusted sources, real observed collisions) ever justify the upgrade.

### Anonymous record names

Nested record types (e.g., a `Vec3`-shaped triple used inside a kind without a top-level kind name) get a **synthesized name** content-derived from their field blob:

```
synthesized_name = "__" + hex(short_hash(field_blob))
```

Two crates declaring the same anonymous shape get the same synthesized name → same field hash for any field of that type → **cross-crate structural identity for free**. Top-level kinds with explicit names (`#[kind(name = "...")]`) keep their nominal identity, so `Position { x, y, z }` and `Velocity { x, y, z }` stay distinct. The footgun (two genuinely different concepts both declared anonymously with identical shape) lives in a corner where you'd have to deliberately go nameless on both — convention says don't.

The `__` prefix is reserved for system-synthesized identifiers (see rule 6 below), so user-supplied names can never collide with a synthesis output.

### Nested struct and enum flattening

Plain nested structs and enums flatten into the top-level field set so recursive evolution gets the same version-tolerance properties as flat fields. There is no nested TLV envelope; only leaves emit TLV records.

**What flattens, what stays opaque:**

| shape | flattens? | rationale |
|---|---|---|
| Plain nested struct | yes | depth-recursive `path.field` leaves; recursive evolution survives the same rules |
| Enum (incl. `Option<T>`) | yes | `__variant` discriminant leaf + variant-prefixed leaves (only the active variant emits) |
| `Vec<T>`, `Map<K, V>`, fixed `Array` | no | dynamic cardinality; flattening to `path[i].*` would leak runtime counts into the field-hash space |

Containers stay as a single TLV record with postcard-encoded body. To get version-tolerance for a container's element type, lift the element to its own TLV kind and reference via `Ref<K>` (handle indirection per ADR-0045).

**Path delimiter.** `.` joins parent path to nested field name (`addr.street`, `result.Ok.profile.bio`). User-supplied identifiers cannot contain `.` — Rust idents already exclude it, so the reservation is free.

**Plain struct flattening.**

```rust
struct Outer { addr: Address }
struct Address { street: String, city: String }
```

emits leaves:
```
addr.street: String
addr.city:   String
```

The `Address` type doesn't appear as its own TLV record; only its leaves do. Recurses through arbitrary depth.

**Enum flattening.** Each enum field synthesizes a `<path>.__variant: u64` leaf carrying the active variant's content hash. The variant's body flattens under `<path>.<VariantName>.*`. Only the active variant's leaves appear on the wire; other variants emit nothing.

```rust
enum Action {
    Idle,
    Move(Vec3),
    Attack { target: u64, damage: u32 },
}
struct Vec3 { x: f32, y: f32, z: f32 }
field: Action
```

emits leaves:
```
field.__variant: u64                 (active variant's content hash)
field.Move.x: f32                    (Vec3 flattened — single-field tuple variant
field.Move.y: f32                     unwraps the inner struct's leaves)
field.Move.z: f32
field.Attack.target: u64             (named-field variant — leaves use field names)
field.Attack.damage: u32
                                     (Idle has no leaves; __variant alone signals it)
```

**Variant identity.** Variant discriminants are content-hashed alongside fields, with their own domain prefix:

```
variant_hash = fnv1a_64_prefixed(VARIANT_DOMAIN, canonical(variant_name, variant_fields))
```

`VARIANT_DOMAIN` is disjoint from `FIELD_DOMAIN`, `KIND_DOMAIN`, and `MAILBOX_DOMAIN`. Variant renames or field-set changes inside a variant produce a new variant hash; the remap dictionary that bridges field renames extends to variant renames the same way (`[(old_variant_hash, new_variant_hash)]`).

**Tuple-variant rules:**

- **Single struct field** (`Move(Vec3)`) — flattens the inner struct's leaves directly under the variant prefix.
- **Single primitive field** (`Ok(u64)`) — single leaf at `<path>.<Variant>` of that primitive type.
- **Multi-field tuple** (`Foo(u32, String)`) — leaves at `<path>.<Variant>.0`, `<path>.<Variant>.1`.
- **Struct variant** (`Attack { target, damage }`) — leaves at `<path>.<Variant>.<field_name>`.
- **Unit variant** (`Idle`) — no leaves; only `<path>.__variant` indicates it's active.

**`Option<T>` is the 2-variant case** of the general rule — no special-case mechanism:

```rust
addr: Option<Address>
```

emits leaves:
```
addr.__variant: u64                  (variant hash for None or Some)
addr.Some.street: String             (only when variant=Some)
addr.Some.city: String
```

Version-skew of an `Option<T>`-typed field — receiver's schema has the field but sender omitted all leaves including `addr.__variant` — decodes to `None` per the existing Option-tolerates-absence rule. Sender that emits `__variant=None` omits the variant-prefixed leaves entirely.

**Composition with what's already in this ADR:**

- *Field hash*: leaf paths feed `fnv(FIELD_DOMAIN, canonical(path, type))` directly. The path string changes from `bio` to `addr.bio` to `result.Ok.profile.bio` as flattening descends; the hash function is unchanged.
- *Anonymous record names*: an anonymously-named nested struct still gets its `__<hash>` synthesized name for *type identity* (when used as a field type elsewhere), but the flattening path uses the *field's* name from the parent, not the type name. `Outer { addr: __abcd { x, y } }` → leaves `addr.x`, `addr.y`.
- *Kind ID*: now hashes the leaf-set, not the source-level field-set. Reorder-free at every nesting level, not just at the top.
- *Unknown bucket*: a leaf path the receiver doesn't recognize gets bucketed verbatim. v1 reading v2's `addr.apartment` leaf → bucket → round-trips on re-emit.
- *Typed field access*: `.get::<T>("addr.street")` — full path is the lookup key. Optional v2 ergonomic: `.get_at::<Address>("addr")` walks all `addr.*` leaves and assembles a sub-struct.
- *`SchemaType` vocabulary*: unchanged. The existing `Option`/`Vec`/`Struct`/`Enum`/`Map`/`Ref` arms drive flattening logic at the derive and codec layer; no new schema variants.

### Kind ID

For TLV-shape kinds:

```
Kind::ID = fnv1a_64_prefixed(KIND_DOMAIN, name ++ sorted_field_hash_blob)
```

Where `sorted_field_hash_blob` is the canonical bytes of `field_hashes.sort().concat()`. Reorder-free at the source layer — moving a field's source position doesn't shift the kind id. Renames shift the kind id (since the field hash changes); the remap dict is what carries the equivalence.

### Unknown fields

On read, fields the receiver's schema doesn't bind are preserved verbatim in an unknown-fields bucket alongside the typed value. The bucket carries `(field_hash, raw_bytes)` per unknown field. On re-encode, unknowns merge back into field-hash sort order alongside known fields, so a payload round-trips exactly through a receiver that doesn't fully understand it — v1 reading v2's payload, then writing it back, doesn't lose v2's additions.

```rust
struct DecodedPayload<T> {
    value: T,
    unknown_fields: Vec<UnknownField>,
}

struct UnknownField {
    hash: u64,
    bytes: Vec<u8>,    // verbatim TLV body, ready to re-emit
}
```

Strict mode: kinds where preserving unknown bytes is a security risk (capability-style payloads where an unknown field might be an authorization marker that v1 silently drops) opt out via `#[kind(strict)]`, which errors on unknown fields rather than bucketing. Default is bucket — forgiving for storage; strict is opt-in for the cases that need it.

Memory cost is the bucket bytes per decoded payload. Typically zero (no version skew), occasionally small (a v2 added a few fields), pathologically larger (a v3 added a megabyte blob field; v1 holds it on round-trip). Worth noting; not a blocker.

### Typed field access

A name-based accessor that hashes `(name, type)` and looks the field up across known fields, the remap dict, and the unknown bucket in one call:

```rust
impl<T> DecodedPayload<T> {
    /// Fetch a field by name and decode it as `U`. The lookup hash
    /// is `field_hash(name, U::SCHEMA)`, so a name match with a
    /// type mismatch returns None — there is no way to misdecode
    /// bytes by asking for the wrong T.
    fn get<U: Schema + Decode>(
        &self,
        name: &str,
    ) -> Option<Result<U, DecodeError>>;

    /// Loose lookup by name only — for tooling that knows the name
    /// but wants raw bytes against an out-of-band schema.
    fn get_raw(&self, name: &str) -> Option<(u64, &[u8])>;
}
```

Because the field hash includes the field's type, asking for a name with the wrong type returns `None` rather than misdecoding bytes. Two flavors: typed (`get::<T>`) for the common case; raw (`get_raw`) for tooling that wants bytes against a schema it knows out-of-band (e.g., the labels manifest of a newer component version).

The `T: Schema + Decode` bound is satisfied by primitives, `String`, `bool`, `Vec<T>`, `Option<T>`, `BTreeMap<K, V>`, and any user struct/enum carrying both derives. Open question whether to extend this to arbitrary user types via a separate `#[derive(FieldDecode)]` (out of v1; punted to a follow-up).

### Required fields and `Option<T>`

Every field declared on a TLV kind is **required by default** — its absence on the wire is a decode error, not a silent fallback. Optionality is expressed in the type system: `Option<T>` fields tolerate version-skew absence and decode missing as `None`. Wire shape per type follows the flattening rule above (primitive/String → single leaf; nested struct → multiple leaves under a dotted path; enum including `Option<T>` → `__variant` + variant-prefixed leaves; container → single leaf with opaque postcard body).

```rust
struct Record {
    id: u64,                  // required — version-skew absence is a decode error
    note: Option<String>,     // optional (2-variant enum) — version-skew absence decodes to None
}
```

Two rules fall out for evolving a kind across an upgrade boundary:

- **Adding a field**: the new field must be `Option<T>`. v1 readers seeing v2-written payloads bucket the new field's leaves as unknown; v2 readers seeing v1-written payloads (where the field's leaves are wire-absent because v1's schema lacked it) get `None`. A new required field would error on every v1 payload — which is the correct behavior, so the type signature is the discipline.
- **Removing a field**: only `Option<T>` fields can be removed safely. Required fields are wire-immutable for storage-compat purposes; removing one breaks readers compiled against the old schema.

Required fields define the irreducible identity of the kind; `Option<T>` fields are the evolving surface. Authoring rule of thumb: require what the kind cannot mean without; `Option` what comes and goes.

**Sender discipline: always emit one TLV record per schema-declared field.** There is no "omit because the value is None/empty" mode. `None` for an `Option<T>` still emits the `__variant=None-hash` leaf; an empty `Vec` still emits a record with body `[varint(0)]`. The encoder walks every leaf in the kind's schema and emits a record, period. Wire-absence of a leaf is therefore unambiguously "the sender's schema didn't have this field" (version skew), never "sender chose not to emit." That's what makes the receiver-side absence rules unambiguous: required leaf absent → schema mismatch → error; optional leaf absent → schema mismatch → tolerated as `None`.

Receiver-side semantics across the wire/schema product:

| receiver's schema says | sender's wire | decoded value |
|---|---|---|
| required leaf field | leaf present | `T` |
| required leaf field | leaf absent *(version skew)* | **error** |
| `Option<T>` field, sender wrote `Some` | `__variant`=Some-hash + `Some.*` leaves | `Some(T)` |
| `Option<T>` field, sender wrote `None` | `__variant`=None-hash, no Some leaves | `None` |
| `Option<T>` field | all leaves absent *(version skew)* | `None` |
| unknown leaf | leaf present | bucketed verbatim |
| renamed (old leaf hash on wire) | leaf present + remap entry | decoded as the renamed-to leaf |

The Option-None and Option-version-skew cases both decode to `None` at the API — sender intent between "explicit None" and "schema didn't have the field" isn't observable. If an author needs that distinction, `Option<Option<T>>` works: `None` for skew, `Some(None)` for explicit None, `Some(Some(T))` for value.

`#[field(default = "...")]` for non-`None` defaults on optional fields stays a v2 extension if a use case forces it.

### Discipline (the strict rules)

1. Once shipped, a field's content hash is immutable. Changing the field's name or type produces a new hash.
2. Removing a field reserves its hash forever — no silent semantic reuse. The kind's manifest carries an explicit reserved-hash set; the derive cross-checks new field hashes against (a) other current fields in the same kind and (b) the kind's reserved-hash set at compile time, and rejects collisions in either direction. Removal-then-re-add of a name+type that hashes to a retired slot is caught loudly rather than silently inheriting old wire data, and the rare within-kind birthday strike between distinct fields fails compile rather than producing two fields with the same wire id.
3. Type is part of the field hash. Type changes (`u32 → u64`, even a "widen") require a new field hash and a remap entry.
4. Renames go through the remap dictionary, never silently.
5. Reordering source code is free (sort order is canonical).
6. The `__` prefix is reserved for system-synthesized identifiers — anonymous record names, the `__variant` discriminant leaf, and any future synthesis patterns. User-supplied names — kind names, field names, variant names, explicit anonymous-record overrides — must not begin with `__`. The derive rejects offending names at compile time, so a future synthesis pattern can't silently collide with a user identifier already in the wild.
7. Variant content hashes follow the same immutability rules as field hashes — once shipped, a variant's hash is fixed; renames or field-set changes inside a variant require a remap entry; removed variant hashes are reserved (same manifest set as field hashes, distinguished by domain prefix).
8. Senders always emit one TLV record per schema-declared field. There is no "omit because empty" mode; wire-absence of a leaf is unambiguously version skew at the sender. The encoder is rule-bound to walk every leaf in the kind's schema.

## Consequences

- **Component upgrades survive add/remove/reorder when the changing fields are `Option<T>`.** Reorder is unconditional. Adds and removes require the field to be optional at the type level; required fields are wire-immutable across compat boundaries by design. The compiler catches the discipline lapse: you can't add a required field and have v1 readers silently default it. Pays off ADR-0049 with author-intent visibility instead of silent type-default fallbacks.
- **Cross-crate shared anonymous types.** Two components declaring the same `Vec3`-shaped record without coordination get the same identity. Useful as the component ecosystem grows.
- **Third wire shape to maintain.** Encoder, decoder, kind-manifest reader, handle-store walker all gain a TLV path alongside cast and positional. Bounded and parallel to the existing two paths, but real engineering surface.
- **Hash semantics shift for TLV kinds.** Reorder no longer changes `Kind::ID`. Renames still do (handled by remap). Today's positional hash stays for cast and positional-postcard kinds.
- **Storage compat across upgrades is now an authoring discipline, not a wire-correctness one.** Adding a field is safe; renaming requires a remap entry; type changes require a new hash + remap. Discipline can be derived from CI rules (compare manifests across builds, fail on undeclared field-hash changes).
- **Trait hierarchy fork.** `Kind` becomes bare metadata (`NAME`, `ID`); existing `decode_from_bytes` / `encode_into_bytes` migrate to a new `Mail` subtrait. Existing call sites that constrain `K: Kind` for encode/decode work need to upgrade to `K: Mail`. The runtime `Mail<'a>` type renames to `Envelope<'a>`. Mechanical refactor across `aether-component`, `aether-mail`, `aether-mail-derive`, and the `#[fallback]` signatures; pre-1.0 so the rename is allowed.

## Resolved in chat (2026-04-27)

These were Open Questions in earlier drafts; resolutions are folded into the Decision section above. Listed here so the journey is recoverable.

- **Postcard integration** → TLV envelope is hand-written; body reuses existing postcard rules per the field's declared type. No coupling to postcard's experimental schema features; no serializer swap.
- **Removal vs deprecation** → Hard removal allowed; rule 2 (reserved-hash manifest with derive-time collision check) handles the only real footgun. Deprecation period stays a CI-rule concern, not a wire-format requirement.
- **Which kinds use TLV** → Opt-in per kind via `#[derive(Storage)]` (vs `#[derive(Mail)]` for the live wire). The trait split (`Storage: Kind`, `Mail: Kind`) makes the choice a type-system property and prevents cross-decoding.
- **Cast + TLV interaction** → `Storage` is TLV-only; `#[repr(C)]` on a `Storage` type is a derive-time error. The trait fork makes the question moot at the type level.
- **Field-hash collision policy** → Stay at 64-bit FNV-1a across all id spaces (`Kind::ID`, `MailboxId`, field hashes, variant hashes). Derive-time per-kind collision check on `(current fields ∪ reserved fields)` surfaces the rare birthday strike as a compile error rather than a runtime hope. At realistic ecosystem scope (10⁴–10⁵ cumulative ids), P(collision) stays below ~3 × 10⁻¹⁰; the 128-bit defense was rejected because the FFI cost (every wasm host fn carrying ids splits into pairs of i64; every wire structure widens) outweighs insurance against an event that effectively never happens. Issue [#320](https://github.com/iamacoffeepot/aether/issues/320) tracks the trigger conditions and migration shape if the ecosystem ever grows past ~10⁷ ids and a switch becomes warranted.

## Open questions

These are the load-bearing things this draft does *not* answer. Each needs a decision before implementation:

1. **Composition with the version-graph idea.** TLV makes most diffs transparent. Type changes and semantic renames are the residual; do those want explicit migration edges, or does the remap dictionary cover both? Probably remap covers renames; explicit migrations cover type changes.
2. **Manifest format.** Where do TLV field hashes, variant hashes, reserved-hash sets, and remap dictionaries live in the wasm? New custom section (`aether.kinds.fields`?), or extension of `aether.kinds.labels`?
3. **Migration of existing stored payloads.** When a component first opts into TLV storage, is there a one-time migration of old positional payloads, or do we accept that pre-TLV storage is read-only-incompatible?
4. **Adding an enum variant.** A new writer emits a variant the reader doesn't know — `__variant` carries a hash that doesn't match any variant in the receiver's schema. Two options:
   - Strict (probable v1 default): unknown variant hash → decode error. Adding a variant is a breaking change.
   - Tolerant: bucket the entire enum field's leaves as unknown bytes. The typed value can't represent the unknown variant (Rust enums lack a sentinel arm), so the API would have to surface "this enum had an unknown variant" — significant ergonomic cost. Probably defer until a forcing function appears.
5. **Variant rename mechanics.** Same shape as field renames — `(old_variant_hash, new_variant_hash)` entries in the remap dict — but the variant's leaves under the old name (`<path>.<OldVariant>.*`) all need their leaf-path remappings too. Open question: do we synthesize per-leaf remap entries from a single variant-rename declaration, or require the author to enumerate every affected leaf? Auto-synthesis is more ergonomic; explicit enumeration is easier to audit.

## Alternatives considered

- **Positional-only with a version graph** (chat sketch). Tracks every add/remove/rename as an explicit edge between kind ids; receivers traverse edges to read stale payloads. Much higher authoring burden — every diff needs an edge — and doesn't get the cross-crate shared-anonymous-types property. Composes with this ADR for the residual type-change case.
- **Pure structural identity (no name in the hash)**. Two shapes with the same fields collide unconditionally. Maximum cross-crate sharing but creates a `Position`/`Velocity` footgun where wire-identical types are indistinguishable. Synthesized-name-when-nameless (this ADR's path) gets the same property only in the corner where the user opted into anonymity, which is the safe fold.
- **Positional synthesized names** (`anon_0`, `anon_1` indexed by source order). Easy to generate but source-order-dependent; two crates with the same shape in different positions don't collide. Throws away the cross-crate-sharing win that motivates synthesizing names at all.
- **Switch to protobuf or capnp**. Either gives us tagged wire, schema evolution, and field numbers off the shelf. Cost is enormous: every kind retyped, every tool retrained, and the existing cast-shape fast path doesn't have a clean equivalent in proto. Worth keeping in mind as a comparison point but not a path forward.
