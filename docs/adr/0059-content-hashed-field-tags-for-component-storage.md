# ADR-0059: Content-hashed field tags for upgradable storage

- **Status:** Accepted
- **Date:** 2026-04-27 · **Revised:** 2026-08-26 (accepted; storage TLV shipped) · **Revised:** 2026-08-28 (container elements decode by the element's derive; #5496)
- **Revision note:** the 2026-04 draft targeted the handle store and predates three decisions this revision resolves against: ADR-0118 (the structured wire body is `aether_data::wire`; postcard is gone), ADR-0187 (persisted rows record their writing schema), and ADR-0188 (the wire codec derives from `Schema`). The consumer that takes this ADR out of ADR-0113's parking is the coordinator's own persistence — the journal's views and projections. The draft's `Mail`-trait fork and `Envelope` rename are superseded by a lighter mapping onto today's `Kind`; renames gain a declared alias (`#[storage(was = "…")]`) instead of the draft's no-remap stance. The wire format itself — TLV records, content-hashed tags, flattening, the unknown bucket, the required/`Option` discipline — stands as resolved in 2026-04. Implementation settled four points recorded in the Decision section: the field-hash preimage is a NUL-separated fold, the TLV length is a fixed 32-bit count, the decoded payload type is `StorageData` throughout, and anonymous record names are already provided by nameless canonical schema bytes.

## Context

Today every kind payload travels in one of two wire shapes:

- **Cast** (`Struct { repr_c: true }`) — raw `#[repr(C)]` bytes, decoded by `bytemuck::cast`. Field layout is positional in the language itself. Hot-path kinds (`DrawTriangle`, `Vertex`, `Tick`).
- **Structured** (everything else) — `aether_data::wire` (ADR-0118), fields concatenated in declaration order, no per-field tag or length. Control-plane kinds, mail with `Vec`/`Option`/`Enum`/`Map` shape.

Both are positional. Adding, removing, or reordering a field in source produces a different `Kind::ID` (the hash includes the canonical schema bytes — ADR-0030, ADR-0032) *and* a wire-incompatible payload. Sender and receiver have to be exact-id matches; any drift is an undeliverable.

That's fine for live mail, where sender and receiver are in lockstep within a session. It fails wherever bytes outlive the binary that wrote them, and the coordinator is now the proof: `MemberView` carries eight trailing `#[serde(default)]` appends and `CommissionProjection` several more, all riding the positional wire where the annotation is inert — the rows decode only because one binary writes and reads them. The decisions vocabulary already paid the price in production: journal replay aborted on rows written by an earlier schema until a hand-written `decisions.v2` upcast landed (#5338). ADR-0187 makes that failure honest — persisted rows become `(kind, schema_digest, bytes)` and a missing upcast is a named refusal — but under a positional wire *every* schema change owes an upcast, additive ones included. The handle store (ADR-0049) and kind-typed actor state across hot reload (ADR-0113, which parked this ADR "awaiting a consumer") remain future consumers with the same shape of problem.

The cleaner direction is **a wire format that is itself version-tolerant** — fields self-identify, receivers tolerate unknown ids, missing ids fall back to defaults. Under it, ADR-0187's upcast obligation shrinks to the genuinely breaking changes (type changes, undeclared renames); adds, removes, and reorders decode transparently.

## Decision

Add a third wire shape, **TLV with content-hashed field tags**, alongside cast and positional-structured, behind a `Storage` trait. First consumer: the coordinator's persisted rows (journal views and projections). Future consumers: the handle store (ADR-0049), kind-typed actor state across `replace_component` (ADR-0113), save files via ADR-0041's `save` namespace.

### Trait surface

`Kind` today carries metadata (`NAME`, `ID: KindId`) plus the positional codec (`decode_from_bytes` / `encode_into_bytes`), where `decode_from_bytes` has a default body returning `None` — the strict-receiver miss (`DISPATCH_UNKNOWN_KIND`). This revision keeps that trait untouched and adds one sibling:

```rust
pub trait Storage: Kind {
    /// Reject unknown fields rather than bucketing them. Default
    /// `false` (forgiving for storage forward-compat); `true` for
    /// payloads where silently carrying an unknown field is a
    /// security concern. Set via `#[storage(strict)]`.
    const STRICT: bool = false;

    fn decode_storage(bytes: &[u8]) -> Option<StorageData<Self>> where Self: Sized;
    fn encode_storage(data: &StorageData<Self>) -> Vec<u8>;
}

pub struct StorageData<T> {
    pub value: T,
    pub unknown_fields: Vec<UnknownField>,
}

pub struct UnknownField {
    pub hash: u64,
    pub bytes: Vec<u8>,    // verbatim TLV body, ready to re-emit
}
```

`#[derive(Storage)]` emits the `Kind` metadata impl **without** a positional codec — `decode_from_bytes` keeps its `None` default and there is no `encode_into_bytes` body to call — plus the `Storage` TLV codec. Disjointness therefore falls out of what already exists: handing a `Storage` kind's bytes to the mail dispatcher fails closed as an ordinary strict-receiver miss, and nothing can positionally encode a `Storage` value because the derive never emits that path. The 2026-04 draft reached the same guarantee by forking a `Mail` subtrait out of `Kind` and renaming the runtime `Mail<'a>` type to `Envelope<'a>`; with the fail-closed default already in the trait, that churn buys nothing and is dropped.

**Wire reachability.** Storage kinds do not ride mail. They live in rows — the store writes `(kind, schema_digest, bytes)` per ADR-0187 and reads back through `Storage::decode_storage`. When the handle store becomes a consumer, mail reaches a storage value only through handle indirection (ADR-0045): mail carries the handle id, the store holds the TLV bytes.

### Wire format

The wire format described below applies to `Storage` kinds. Everything else uses the existing cast or positional-structured shape unchanged.

A struct payload is a sequence of `[field_hash][length][bytes]` records, concatenated in field-hash sort order. Receivers walk the records, look each `field_hash` up in their local schema, dispatch the bytes against the matched field's type, skip unknown ids, and default missing ids.

```
+----------------+---------------+------------------------+
| field_hash u64 | len u32 LE    | aether_data::wire body |
+----------------+---------------+------------------------+
```

Length is a fixed 32-bit little-endian count, matching every other count in the ADR-0118 format. The 2026-04 draft drew a varint; ADR-0118 removed variable-length integers from the format, so the envelope follows the rest of the crate.

Field bodies are encoded against the field's declared type by the same owned, `Schema`-derived codec ADR-0188 gives the positional wire — varint scalars, length-prefixed strings, the closed vocabulary `Schema` already enforces. The TLV layer adds only the `(field_hash, length)` envelope; primitive bytes inside don't carry their own type tags. Receivers that don't know a field id skip `length` bytes and continue. One codec family drives both wire shapes, so TLV bodies and positional bodies can never disagree about how a leaf value is spelled.

### Field hash

For each field, a stable 64-bit content hash:

```
field_hash = fold(FIELD_DOMAIN ++ path_bytes ++ 0x00 ++ canonical_schema(field_type))
```

`FIELD_DOMAIN` is a new prefix disjoint from `KIND_DOMAIN` and `MAILBOX_DOMAIN` so the id spaces don't overlap. The path is dotted (`addr.street`) but never materialized: each segment is folded onto an in-progress carry (`fold_path_segment`), then a NUL terminator, then the type's canonical schema bytes (the same encoding `canonical_serialize_schema` already produces). `canonical_serialize_kind` length-prefixes its name, which cannot be written before the whole path is known and would forbid the incremental fold; NUL is unambiguous because Rust identifiers and the dot join both exclude it.

Renames shift the field hash (the name is in the canonical bytes). On the wire a rename is therefore remove-plus-add — and the author declares that the two are one field:

```rust
struct Record {
    #[storage(was = "note")]
    remark: Option<String>,
}
```

`#[storage(was = "old_name")]` adds a **read alias**: the derive computes the alias hash from `(old_name, current_type)` and binds it into the reader's lookup set beside the current hash. A record tagged with either hash decodes into the field; writers emit only the current hash, so aliases never appear in new bytes and the wire format carries no remap table — the declaration lives in source, costs one attribute, and compiles into the same lookup the reader already does. The attribute repeats for a chain of renames (`was = "a"`, `was = "b"`), and alias hashes join the within-kind collision check like any other.

What `was` does not cover is a rename-plus-retype: the alias hash uses the *current* type, so bytes written under `(old_name, old_type)` still miss and bucket — a type change is a breaking schema change with or without a rename riding on it, and the migration story below applies. An **undeclared** rename is simply what the wire sees: the old value defaults away (or errors, for a required field) and the new field reads as absent. That silent loss is exactly what the ADR-0187 fixture corpus exists to catch — a schema-digest change whose fixture row shows a defaulted-away value fails the build until the author either declares the alias or writes the upcast.

**Hash width: 64-bit.** All id spaces (`Kind::ID`, `MailboxId`, field hashes, variant hashes) use 64-bit FNV-1a. Per-kind collision probability stays below 10⁻¹⁰ at realistic ecosystem scope; the derive-time within-kind collision check (rule 2) catches the rare birthday strike as a compile error. 128-bit was considered and rejected on FFI grounds — wasm32 has no native 128-bit type, so every host fn carrying ids would split into pairs of i64. Issue [#320](https://github.com/iamacoffeepot/aether/issues/320) tracks the trigger conditions for revisiting if ecosystem growth or threat-model shifts (third-party kinds from untrusted sources, real observed collisions) ever justify the upgrade.

### Anonymous record names

Canonical schema bytes already omit field and variant names, so two crates declaring the same record shape already produce byte-identical canonical bytes — the cross-crate structural identity a synthesized `__<hash>` name was invented to buy is free, and no synthesized name is emitted. The `__` prefix still stands for `__variant` (and any future synthesis). The consequence is that discipline rule 1 holds only up to structural equality: retyping a field between two structurally identical records — a position and a velocity, both three floats — leaves the field hash unchanged, so old bytes decode silently into the new type.

### Nested struct and enum flattening

Plain nested structs and enums flatten into the top-level field set so recursive evolution gets the same version-tolerance properties as flat fields. There is no nested TLV envelope; only leaves emit TLV records.

**What flattens, what stays opaque:**

| shape | flattens? | rationale |
|---|---|---|
| Plain nested struct | yes | depth-recursive `path.field` leaves; recursive evolution survives the same rules |
| Enum (incl. `Option<T>`) | yes | `__variant` discriminant leaf + variant-prefixed leaves (only the active variant emits) |
| `Vec<T>`, `Map<K, V>`, fixed `Array` | one record, element-encoded body | dynamic cardinality; flattening to `path[i].*` would leak runtime counts into the field-hash space, and that rejection stands |

A container is one TLV record whose **body encoding the element type selects** through the `StorageElement` trait (#5496). Exactly one impl exists per type — the derive a type declares is the selector, and there is deliberately no blanket impl over the wire codec:

- A `#[derive(Schema)]` element contributes its **positional wire bytes**. The record body is byte-identical to the pre-#5496 opaque form and the record tag stays the schema-folded hash, so element drift moves the tag and the reader refuses by name rather than misreading positional bytes. No tolerance promise, exactly as before.
- A `#[derive(Storage)]` element contributes a **`u32`-length-framed record stream rooted at the element type**. Element tags are the element type's own compile-time root hashes, so cardinality never enters the hash space. The container's record tag folds the reserved `__elements` segment and terminates against the `Bytes` schema instead of the container schema, so evolving the element type never moves the container's own tag: element drift inside a container decodes the way root-level drift does — unknown fields skip, missing `Option`s default, missing required fields refuse by name.

Composite elements propagate the class: `Vec<T>` / `[T; N]` / `Option<T>` are tagged when `T` is, `Map<K, V>` when either side is; an all-positional composite reproduces the ordinary wire layout byte for byte (maps keep canonical ascending encoded-key order).

**Elements shed their unknowns on rewrite.** A container of tagged elements assembles plain values; element-level unknown fields have no side-channel, so a rewrite by an older binary sheds fields a newer writer added inside elements. Reads stay tolerant; rewrites shed. Stated and accepted while the coordinator is a row's only writer; the upgrade, if a multi-writer consumer arrives, is a derive-required unknown-fields member on storage types — recorded here so the door stays visibly open, not built now. The root-level `StorageData` bucket still round-trips.

**Derive consequences.** `#[derive(Storage)]` now emits the schema core (the `Schema` impl, cast eligibility, and the positional wire codec) itself, so a storage kind lists one derive, not two — deriving both is a duplicate-impl compile error by design. The wire-codec emission keeps legacy positional rows readable (`POSITIONAL_ROW_SCHEMA` decode); the mail-path guarantee is unchanged (`Kind::encode_into_bytes` still panics). The static leaf-uniqueness walk keeps checking the schema-folded container hash; a tagged container's runtime tag derives from its path alone, so a collision there would require a same-path sibling, which the path fold already precludes.

**Migration.** Flipping an element type from `Schema` to `Storage` moves the container tag and its body layout: an ordinary breaking schema change under ADR-0187 — digest bump, named upcast, fixture row. Tolerance applies after the flip. `Ref<K>` handle indirection (ADR-0045) remains the answer when elements need identity and independent lifecycle, not just evolvable shape.

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

`VARIANT_DOMAIN` is disjoint from `FIELD_DOMAIN`, `KIND_DOMAIN`, and `MAILBOX_DOMAIN`. Variant renames or field-set changes inside a variant produce a new variant hash and remain **breaking schema changes** in v1 — old wire data preserves in the unknown-fields bucket; typed access requires migration. The `#[storage(was = "…")]` alias covers *fields* only; extending it to variants is a follow-up if a real rename shows up there, not a v1 surface.

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
- *Kind ID*: nominal for storage kinds (see below) — flattening changes never touch it; shape identity rides the ADR-0187 `schema_digest`.
- *Unknown bucket*: a leaf path the receiver doesn't recognize gets bucketed verbatim. v1 reading v2's `addr.apartment` leaf → bucket → round-trips on re-emit.
- *Typed field access*: `.get::<T>("addr.street")` — full path is the lookup key. Optional v2 ergonomic: `.get_at::<Address>("addr")` walks all `addr.*` leaves and assembles a sub-struct.
- *`SchemaType` vocabulary*: unchanged. The existing `Option`/`Vec`/`Struct`/`Enum`/`Map`/`Ref` arms drive flattening logic at the derive and codec layer; no new schema variants.

### Kind ID

For TLV-shape kinds:

```
Kind::ID = fnv1a_64_prefixed(KIND_DOMAIN, name)
```

**Nominal, not structural** — a deliberate departure from the 2026-04 draft, which hashed the sorted leaf set into the id. A storage kind's id is the discriminator rows are keyed by, and rows outlive schemas: an id that moves whenever the leaf set moves re-keys the store on every schema change, orphaning exactly the rows this wire format exists to keep readable. Shape identity has its own carrier — the ADR-0187 `schema_digest` recorded beside every row — so baking shape into the id was redundant for storage kinds and actively harmful. Mail kinds keep their existing schema-inclusive hash: there, skew *should* be an undeliverable, and the id is the enforcement. Version tolerance for storage lives entirely in the TLV field records and the digest column, never in the kind id.

### Unknown fields

On read, fields the receiver's schema doesn't bind are preserved verbatim in an unknown-fields bucket alongside the typed value. The bucket carries `(field_hash, raw_bytes)` per unknown field. On re-encode, unknowns merge back into field-hash sort order alongside known fields, so a payload round-trips exactly through a receiver that doesn't fully understand it — v1 reading v2's payload, then writing it back, doesn't lose v2's additions.

The decoded payload type is `StorageData` throughout (the 2026-04 draft also called it `DecodedPayload`; that name is retired).

```rust
struct StorageData<T> {
    value: T,
    unknown_fields: Vec<UnknownField>,
}

struct UnknownField {
    hash: u64,
    bytes: Vec<u8>,    // verbatim TLV body, ready to re-emit
}
```

Strict mode: kinds where preserving unknown bytes is a security risk (capability-style payloads where an unknown field might be an authorization marker that v1 silently drops) opt in via `#[storage(strict)]` on the derive, which sets `Storage::STRICT = true`. The substrate's decoder branches on the const at compile time and errors on unknown fields rather than bucketing. Default is `STRICT = false` — forgiving for storage; strict is opt-in for the cases that need it. The flag rides on the trait, not the manifest, so there's no wire-side surface for it.

Memory cost is the bucket bytes per decoded payload. Typically zero (no version skew), occasionally small (a v2 added a few fields), pathologically larger (a v3 added a megabyte blob field; v1 holds it on round-trip). Worth noting; not a blocker.

### Typed field access

A name-based accessor that hashes `(name, type)` and looks the field up across known fields and the unknown bucket in one call:

```rust
impl<T> StorageData<T> {
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
    fn get_raw<U: Schema>(&self, name: &str) -> Option<(u64, &[u8])>;
}
```

Because the field hash includes the field's type, asking for a name with the wrong type returns `None` rather than misdecoding bytes. Two flavors: typed (`get::<T>`) for the common case; raw (`get_raw`) for tooling that wants bytes against a schema it knows out-of-band (e.g., the labels manifest of a newer component version).

The `T: Schema + Decode` bound is satisfied by primitives, `String`, `bool`, `Vec<T>`, `Option<T>`, `BTreeMap<K, V>`, and any user struct/enum carrying both derives. Open question whether to extend this to arbitrary user types via a separate `#[derive(FieldDecode)]` (out of v1; punted to a follow-up).

### Required fields and `Option<T>`

Every field declared on a TLV kind is **required by default** — its absence on the wire is a decode error, not a silent fallback. Optionality is expressed in the type system: `Option<T>` fields tolerate version-skew absence and decode missing as `None`. Wire shape per type follows the flattening rule above (primitive/String → single leaf; nested struct → multiple leaves under a dotted path; enum including `Option<T>` → `__variant` + variant-prefixed leaves; container → single leaf with an opaque `aether_data::wire` body).

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
| leaf under a declared `was` alias hash | leaf present | decodes into the renamed field |
| renamed-from, alias undeclared | leaf present | bucketed verbatim — the ADR-0187 fixture gate fails the build until the alias or an upcast exists |

The Option-None and Option-version-skew cases both decode to `None` at the API — sender intent between "explicit None" and "schema didn't have the field" isn't observable. If an author needs that distinction, `Option<Option<T>>` works: `None` for skew, `Some(None)` for explicit None, `Some(Some(T))` for value.

`#[field(default = "...")]` for non-`None` defaults on optional fields stays a v2 extension if a use case forces it.

### Discipline (the strict rules)

1. Once shipped, a field's content hash is determined by its `(name, type)` pair. Changing the type produces a new hash and is a breaking schema change (old wire data preserves in the unknown bucket; typed access requires migration). Changing the name alone is a breaking change *unless declared* — `#[storage(was = "old_name")]` binds the old hash as a read alias and keeps typed access continuous.
2. Within a single kind, no two field hashes may collide. The derive cross-checks all current fields' hashes pairwise at compile time; the rare birthday strike between distinct `(name, type)` pairs in one schema fails to compile rather than producing two fields with the same wire id. Author nudges one name to disambiguate.
3. Reordering source code is free (sort order is canonical).
4. The `__` prefix is reserved for system-synthesized identifiers — anonymous record names, the `__variant` discriminant leaf, and any future synthesis patterns. User-supplied names — kind names, field names, variant names, explicit anonymous-record overrides — must not begin with `__`. The derive rejects offending names at compile time, so a future synthesis pattern can't silently collide with a user identifier already in the wild.
5. Senders always emit one TLV record per schema-declared field. There is no "omit because empty" mode; wire-absence of a leaf is unambiguously version skew at the sender. The encoder is rule-bound to walk every leaf in the kind's schema.

## Consequences

- **Additive schema evolution stops owing upcasts.** Under ADR-0187 alone, every digest change owes an upcast; under this ADR, adds, removes, and reorders of `Option<T>` fields decode transparently, and the upcast obligation narrows to type changes, undeclared renames, and variant-set changes — a property knowable from the source diff. The `MemberView`/`CommissionProjection` append pattern becomes real instead of inert, and the `decisions.v2` class of replay abort (#5338) cannot recur for additive drift.
- **Declared renames are one attribute.** `#[storage(was = "…")]` keeps typed access across a rename with no wire-side remap table; the ADR-0187 fixture corpus turns an undeclared rename into a failing build instead of silent data loss.
- **Sealed history is never rewritten.** Adoption applies to newly written rows. Bytes already sealed — signed subjects especially, whose digests are load-bearing — keep their recorded schema digest and decode by the path that digest names. A storage kind's TLV adoption is a new schema digest for new rows, not a migration of old ones.
- **Cross-crate shared anonymous types.** Two crates declaring the same `Vec3`-shaped record without coordination get the same identity. Useful as the component ecosystem grows.
- **Third wire shape to maintain.** Encoder, decoder, and every store walker gain a TLV path alongside cast and positional — in `aether-data` / `aether-data-derive`, sharing the ADR-0188 leaf codec so the body spelling has one origin. Bounded and parallel to the existing two paths, but real engineering surface.
- **Minimal trait churn.** `Kind` is untouched; `Storage` is additive; no call-site migration and no runtime-type rename. The one behavioral edge is deliberate: a `Storage` kind reaching the mail dispatcher fails closed as a strict-receiver miss.

## Resolved in chat (2026-04-27)

These were Open Questions in earlier drafts; resolutions are folded into the Decision section above. Listed here so the journey is recoverable.

- **Body-format integration** → TLV envelope is hand-written; the body reuses the workspace's own structured encoding per the field's declared type. *Revised 2026-08-26:* originally resolved against postcard; ADR-0118 replaced the body format with `aether_data::wire` and ADR-0188 makes the leaf codec derive-owned — the resolution's shape (envelope hand-written, body borrowed from the ordinary wire) is unchanged.
- **Removal vs deprecation** → Hard removal allowed. Old payloads with the removed field's hash bucket on new readers; new senders don't emit. The "what if a future field hash-collides with a removed one" footgun is either (a) not a footgun (re-add of identical `(name, type)` is semantically the same field), (b) astronomically improbable (accidental 64-bit collision between distinct `(name, type)` pairs), or (c) author-level "don't reuse names for different concepts" discipline that no wire format can enforce. Deprecation period stays a CI-rule concern if anyone wants it; not a wire-format requirement.
- **Which kinds use TLV** → Opt-in per kind via `#[derive(Storage)]` (vs the ordinary `#[derive(Kind)]` for the live wire). *Revised 2026-08-26:* the draft's `Mail` subtrait fork is gone; disjointness now rests on the `Storage` derive emitting no positional codec, so cross-decoding fails closed at dispatch.
- **Cast + TLV interaction** → `Storage` is TLV-only; `#[repr(C)]` on a `Storage` type is a derive-time error. The trait fork makes the question moot at the type level.
- **Field-hash collision policy** → Stay at 64-bit FNV-1a across all id spaces (`Kind::ID`, `MailboxId`, field hashes, variant hashes). Derive-time within-kind collision check on the current field set surfaces the rare birthday strike as a compile error. At realistic ecosystem scope (10⁴–10⁵ cumulative ids), P(collision) stays below ~3 × 10⁻¹⁰; the 128-bit defense was rejected because the FFI cost (every wasm host fn carrying ids splits into pairs of i64; every wire structure widens) outweighs insurance against an event that effectively never happens. Issue [#320](https://github.com/iamacoffeepot/aether/issues/320) tracks the trigger conditions and migration shape if the ecosystem ever grows past ~10⁷ ids and a switch becomes warranted.
- **Composition with the version-graph idea** → Dropped. TLV envelopes + flattening cover add/remove/reorder transparently at the wire layer; no cross-kind migration edges or graph traversal at decode. The residual cases (type changes, undeclared renames, variant-set changes) are breaking schema changes — old wire data preserves in the unknown-fields bucket; authors who need typed access to that data run an explicit migration tool (read old storage, decode the bucket entry into the new field, re-encode under the new schema). Migration is outside the wire format's responsibility.
- **Manifest format** → No new section. Field/variant hashes are derived at load time by walking the labeled schema in `aether.kinds` + `aether.kinds.labels`; reserved-hash sets and remap dictionaries were dropped (renames are breaking; reserved-hash tracking solves a non-problem). Strict mode rides on the trait as `Storage::STRICT: bool`, set by `#[storage(strict)]` — no wire-side surface for it.
- **Variant rename mechanics** → Variant renames stay breaking schema changes; old wire data buckets; typed access requires migration. *Revised 2026-08-26:* field renames left this bucket — `#[storage(was = "…")]` declares them — but the alias does not extend to variants in v1.
- **Migration of existing stored payloads** → *Revised 2026-08-26:* the first consumer's rows already exist (the coordinator's journal views and projections, positional today). They are not migrated: sealed bytes keep their recorded schema digest (ADR-0187) and their positional decode path; TLV is the shape of newly written rows. A future durable backend retrofitted onto pre-existing data gets its own one-time migration story at adoption time.
- **Adding an enum variant** → Strict for v1. Unknown `__variant` hash on the wire is a decode error; adding a variant is a breaking schema change, same migration story as field/variant renames and type changes. Tolerant mode (bucket the whole enum field as raw bytes) was rejected because Rust enums lack a sentinel arm — the typed API would have to surface "this enum had an unknown variant" through every consumer, a significant ergonomic cost for a forward-compat property that's rarely needed in single-org schema evolution. Revisit if a forcing function appears (third-party kinds with independent variant evolution, ecosystem-wide enum-evolution coordination pain).

## Open questions

None remaining as of the 2026-08-26 revision. The 2026-04 brainstorm questions are folded into the Decision section with their resolutions captured in *Resolved in chat*; the revision resolved the remaining drift against ADR-0118/0187/0188 and the coordinator-persistence consumer. Questions raised during implementation accumulate here.

## Alternatives considered

- **Positional-only with a version graph** (chat sketch). Tracks every add/remove/rename as an explicit edge between kind ids; receivers traverse edges to read stale payloads. Much higher authoring burden — every diff needs an edge — and doesn't get the cross-crate shared-anonymous-types property. Subsumed by this ADR's TLV mechanism for add/remove/reorder; renames and type changes stay breaking under either approach (the version-graph alternative wouldn't have made them transparent either, just relabeled the migration step).
- **Pure structural identity (no name in the hash)**. Two shapes with the same fields collide unconditionally. Maximum cross-crate sharing but creates a `Position`/`Velocity` footgun where wire-identical types are indistinguishable. Synthesized-name-when-nameless (this ADR's path) gets the same property only in the corner where the user opted into anonymity, which is the safe fold.
- **Positional synthesized names** (`anon_0`, `anon_1` indexed by source order). Easy to generate but source-order-dependent; two crates with the same shape in different positions don't collide. Throws away the cross-crate-sharing win that motivates synthesizing names at all.
- **Switch to protobuf or capnp**. Either gives us tagged wire, schema evolution, and field numbers off the shelf. Cost is enormous: every kind retyped, every tool retrained, and the existing cast-shape fast path doesn't have a clean equivalent in proto. Worth keeping in mind as a comparison point but not a path forward.
