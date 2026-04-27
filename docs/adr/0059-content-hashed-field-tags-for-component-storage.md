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

Add a third wire shape, **TLV with content-hashed field tags**, alongside cast and positional-postcard. Scope: payloads written to durable backing stores (the handle store; possibly later, save files via ADR-0041's `save://` namespace). Live mail dispatch keeps the existing two shapes unchanged — speed and version-rigidity are the right tradeoff there.

### Wire format

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
2. Removing a field reserves its hash forever — no silent semantic reuse.
3. Type is part of the field hash. Type changes (`u32 → u64`, even a "widen") require a new field hash and a remap entry.
4. Renames go through the remap dictionary, never silently.
5. Reordering source code is free (sort order is canonical).
6. The `__` prefix is reserved for system-synthesized identifiers — anonymous record names, the `__variant` discriminant leaf, and any future synthesis patterns. User-supplied names — kind names, field names, variant names, explicit anonymous-record overrides — must not begin with `__`. The derive rejects offending names at compile time, so a future synthesis pattern can't silently collide with a user identifier already in the wild.
7. Variant content hashes follow the same immutability rules as field hashes — once shipped, a variant's hash is fixed; renames or field-set changes inside a variant require a remap entry; removed variant hashes are reserved.

## Consequences

- **Component upgrades survive add/remove/reorder when the changing fields are `Option<T>`.** Reorder is unconditional. Adds and removes require the field to be optional at the type level; required fields are wire-immutable across compat boundaries by design. The compiler catches the discipline lapse: you can't add a required field and have v1 readers silently default it. Pays off ADR-0049 with author-intent visibility instead of silent type-default fallbacks.
- **Cross-crate shared anonymous types.** Two components declaring the same `Vec3`-shaped record without coordination get the same identity. Useful as the component ecosystem grows.
- **Third wire shape to maintain.** Encoder, decoder, kind-manifest reader, handle-store walker all gain a TLV path alongside cast and positional. Bounded and parallel to the existing two paths, but real engineering surface.
- **Hash semantics shift for TLV kinds.** Reorder no longer changes `Kind::ID`. Renames still do (handled by remap). Today's positional hash stays for cast and positional-postcard kinds.
- **Storage compat across upgrades is now an authoring discipline, not a wire-correctness one.** Adding a field is safe; renaming requires a remap entry; type changes require a new hash + remap. Discipline can be derived from CI rules (compare manifests across builds, fail on undeclared field-hash changes).

## Open questions

These are the load-bearing things this draft does *not* answer. Each needs a decision before implementation:

1. **Postcard integration.** Postcard 1.x has no native tagged mode. Three options:
   - Use postcard's experimental schema features if they cover what we need.
   - Layer TLV on top of postcard bodies (envelope is hand-written, body is postcard).
   - Swap serializer for TLV-shape kinds (e.g., use a different crate or hand-roll). Bigger surface change.
2. **Removal vs deprecation.** Is hard removal allowed (a field's hash is permanently retired and no one needs it), or do we require fields to be marked deprecated for a period before removal?
3. **Which kinds use TLV.** Opt-in per kind (`#[kind(storage)]` attribute), opt-out, or implicit by use case (any kind that's ever stored)? Probably opt-in to avoid forcing live-mail kinds onto a slower path.
4. **Field-hash collision policy.** 64 bits gives ample headroom but isn't infinite. Do we cap field count per kind (collision-resistance practical), detect collisions at derive time (compile error if two field hashes collide within one kind), or just accept the birthday-bound argument as we do for kind ids?
5. **Cast + TLV interaction.** A TLV-shape kind almost certainly excludes cast eligibility (no fixed `#[repr(C)]` layout). Same constraint as today's `Vec`/`Option`/`Map`. The derive should reject `#[repr(C)]` + `#[kind(storage)]` with a clear error.
6. **Composition with the version-graph idea.** TLV makes most diffs transparent. Type changes and semantic renames are the residual; do those want explicit migration edges, or does the remap dictionary cover both? Probably remap covers renames; explicit migrations cover type changes.
7. **Manifest format.** Where do TLV field hashes, variant hashes, and remap dictionaries live in the wasm? New custom section (`aether.kinds.fields`?), or extension of `aether.kinds.labels`?
8. **Migration of existing stored payloads.** When a component first opts into TLV storage, is there a one-time migration of old positional payloads, or do we accept that pre-TLV storage is read-only-incompatible?
9. **Adding an enum variant.** A new writer emits a variant the reader doesn't know — `__variant` carries a hash that doesn't match any variant in the receiver's schema. Two options:
   - Strict (probable v1 default): unknown variant hash → decode error. Adding a variant is a breaking change.
   - Tolerant: bucket the entire enum field's leaves as unknown bytes. The typed value can't represent the unknown variant (Rust enums lack a sentinel arm), so the API would have to surface "this enum had an unknown variant" — significant ergonomic cost. Probably defer until a forcing function appears.
10. **Variant rename mechanics.** Same shape as field renames — `(old_variant_hash, new_variant_hash)` entries in the remap dict — but the variant's leaves under the old name (`<path>.<OldVariant>.*`) all need their leaf-path remappings too. Open question: do we synthesize per-leaf remap entries from a single variant-rename declaration, or require the author to enumerate every affected leaf? Auto-synthesis is more ergonomic; explicit enumeration is easier to audit.

## Alternatives considered

- **Positional-only with a version graph** (chat sketch). Tracks every add/remove/rename as an explicit edge between kind ids; receivers traverse edges to read stale payloads. Much higher authoring burden — every diff needs an edge — and doesn't get the cross-crate shared-anonymous-types property. Composes with this ADR for the residual type-change case.
- **Pure structural identity (no name in the hash)**. Two shapes with the same fields collide unconditionally. Maximum cross-crate sharing but creates a `Position`/`Velocity` footgun where wire-identical types are indistinguishable. Synthesized-name-when-nameless (this ADR's path) gets the same property only in the corner where the user opted into anonymity, which is the safe fold.
- **Positional synthesized names** (`anon_0`, `anon_1` indexed by source order). Easy to generate but source-order-dependent; two crates with the same shape in different positions don't collide. Throws away the cross-crate-sharing win that motivates synthesizing names at all.
- **Switch to protobuf or capnp**. Either gives us tagged wire, schema evolution, and field numbers off the shelf. Cost is enormous: every kind retyped, every tool retrained, and the existing cast-shape fast path doesn't have a clean equivalent in proto. Worth keeping in mind as a comparison point but not a path forward.
