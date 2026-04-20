# ADR-0031: Const-constructible schema representation

- **Status:** Proposed
- **Date:** 2026-04-20

## Context

ADR-0019 introduced `SchemaType` — an owned, heap-backed enum living in `aether-hub-protocol` that describes a kind's payload shape for the hub's encoder/decoder and the substrate's descriptor bookkeeping. ADR-0028 embedded its postcard encoding in each component's wasm custom section. ADR-0030 (Phase 2) then needed to go one step further: compute `K::ID = fnv1a(name ++ postcard(schema))` at compile time, const-eval, emitted by the `Kind` derive.

That's where it stalled. `SchemaType`'s recursive variants use `Box<SchemaType>`; `Box::new` isn't `const`, and `Vec<NamedField>` isn't const-constructible either. A derive macro walking a struct's AST can produce the schema bytes at macro-expansion time (the `aether.kinds` manifest emission already does this), but it can't construct a `SchemaType` value because the type itself isn't usable in `const` contexts.

The current workaround the derive uses — `aether-mail-derive/src/manifest.rs` — is a **parallel syntactic walker**: a second implementation of schema resolution that duplicates what `Schema::schema()` does at runtime, just at macro-expansion time. This is already a smell (two implementations of the same logic, kept in lockstep by hand), and it has a hard ceiling: it can only recognize the vocabulary baked into its `resolve()` fn. User-defined helper types (`Vertex` inside `DrawTriangle`) return `None` and the manifest emission silently skips them. For ADR-0030 that silent skip becomes a correctness problem — the derive would emit a `K::ID` that hashes an incomplete schema, while the substrate (walking `Schema::schema()` at runtime) hashes the full schema. The two sides disagree, mail routing fails, drift detection becomes drift production.

Three forces set the shape of the fix:

1. **Const-constructibility.** `K::ID` must be a `const u64`. That requires the schema input to the hash to be const-reachable — walkable by a `const fn` over a statically-knowable structure.
2. **Cross-crate resolution.** `DrawTriangle` lives in `aether-kinds`; `Vertex` could live in the same crate or a downstream one. The derive has to reach a nested type's schema without the source text in hand. Traits + `'static` references are the language-native way to do this — each type publishes its schema, consumers reference it.
3. **Wire deserialization.** The hub receives postcard-encoded schemas from substrates over the control channel and from agent tool calls. Those bytes need somewhere to live post-deserialize. Today that's the heap via `Box` + `Vec` + `String`. A const-const representation alone can't hold deserialized bytes; some owned variant has to coexist.

The cleanest shape that satisfies all three: **one schema enum used for both const literals and deserialized values**, with a small owned-or-borrowed sum type for the recursive fields. `std::borrow::Cow` almost fits — but `Cow<SchemaType>` by value creates an infinite-size recursion cycle, because `Cow::Owned` holds `T` directly. The fix is a hand-rolled cousin of `Cow` that uses `Box` in its owned variant — breaking the size cycle via indirection while keeping const-construction available through the borrowed variant.

This ADR introduces that type (`SchemaCell`), rewrites `SchemaType` around it, and commits to retiring every parallel schema representation (the `manifest.rs` syntactic walker; the runtime `fn schema()` method). With one representation, both the derive and the substrate can compute the same hash the same way, and ADR-0030 Phase 2 unblocks.

This is a sizable rewrite. It touches the `Schema` trait, the `Schema` derive, the `Kind` derive, the hub's encoder and decoder (which pattern-match `SchemaType` pervasively), the substrate registry, the embedded manifest format, and every fixture that hand-constructs a schema. Pre-1.0, and the current duplication is already fragile — the right time is now, before ADR-0030 Phase 2 commits us to a schema-hashed wire contract.

## Decision

**`SchemaType` becomes const-constructible by replacing `Box<SchemaType>` / `Vec<NamedField>` / `String` with `&'static`-or-owned sum types (`SchemaCell`, `Fields`, `Str`). `Schema` shifts from `fn schema() -> SchemaType` to `const SCHEMA: SchemaType`. The derive emits schema literals; the hub deserializes into the `Owned` variants of the same types. One representation, walked by the same code in const and non-const contexts.**

### `SchemaCell`

The core primitive. A `Cow`-shaped sum that handles recursion:

```rust
pub enum SchemaCell {
    Static(&'static SchemaType),
    Owned(Box<SchemaType>),
}

impl core::ops::Deref for SchemaCell {
    type Target = SchemaType;
    fn deref(&self) -> &SchemaType {
        match self {
            SchemaCell::Static(r) => r,
            SchemaCell::Owned(b) => b,
        }
    }
}
```

- `SchemaCell::Static(&'static SchemaType)` — the const constructor. Derive emits `SchemaCell::Static(&NESTED_SCHEMA)`; the `&NESTED_SCHEMA` points into the outer `const SCHEMA` literal's own rodata. Fully const.
- `SchemaCell::Owned(Box<SchemaType>)` — the deserialization target. The hub's postcard decoder allocates each node via `Box::new`, same cost profile as today's `Box<SchemaType>`. Walkers `.deref()` uniformly and don't observe the difference.

Serde impls follow `Deref` on the serialize side (treats both variants identically as `&SchemaType`) and always produce `Owned` on the deserialize side. Round-trip through postcard preserves semantics but not variant identity — a `Static`-built schema serialized and deserialized comes back as `Owned`. That's fine: variant identity isn't part of the type's contract, value equality is.

### Owned-or-borrowed sums for `Vec` and `String`

The same pattern handles collections:

```rust
pub enum Fields {
    Static(&'static [NamedField]),
    Owned(Vec<NamedField>),
}

pub enum Str {
    Static(&'static str),
    Owned(String),
}
```

Both `Deref` to the borrowed form (`[NamedField]`, `str`). Const literals use `Static(&[…])` / `Static("…")`; deserialization produces `Owned(Vec::new()...)` / `Owned(String::from(…))`.

### Rewritten `SchemaType`

```rust
pub enum SchemaType {
    Unit,
    Bool,
    Scalar(Primitive),
    String,
    Bytes,
    Option(SchemaCell),
    Vec(SchemaCell),
    Array { element: SchemaCell, len: u32 },
    Struct { fields: Fields, repr_c: bool },
    Enum { variants: Variants },
}

pub struct NamedField { pub name: Str, pub ty: SchemaType }

pub enum EnumVariant {
    Unit { name: Str, discriminant: u32 },
    Tuple { name: Str, discriminant: u32, fields: Fields /* or a Cells sum */ },
    Struct { name: Str, discriminant: u32, fields: Fields },
}
```

Every variant is const-constructible. `SchemaType` itself is `Sized` (the recursive fields are behind `SchemaCell`, not `SchemaType`-by-value), so it lives in `const` items and `static` literals without the `?Sized` / `'static` dance.

### `Schema` trait

```rust
pub trait Schema {
    const SCHEMA: SchemaType;
}
```

No `fn schema() -> SchemaType` any more. Every reference site reads `<T as Schema>::SCHEMA` — a const value, usable in match arms, const contexts, and `const fn` walkers.

Blanket impls for `u8`, `u16`, …, `f64`, `bool`, `String`, `Vec<u8>`, `Vec<T>`, `Option<T>`, `[T; N]` move from runtime constructors to const:

```rust
impl Schema for u32 {
    const SCHEMA: SchemaType = SchemaType::Scalar(Primitive::U32);
}
impl<T: Schema + 'static> Schema for Option<T> {
    const SCHEMA: SchemaType = SchemaType::Option(SchemaCell::Static(&T::SCHEMA));
}
impl<T: Schema + 'static, const N: usize> Schema for [T; N] {
    const SCHEMA: SchemaType = SchemaType::Array {
        element: SchemaCell::Static(&T::SCHEMA),
        len: N as u32,
    };
}
```

The `&T::SCHEMA` expression is const as long as `T::SCHEMA` is a `const` — which the trait now requires.

### `#[derive(Schema)]` rewrite

Generates a const literal instead of a function body:

```rust
impl Schema for DrawTriangle {
    const SCHEMA: SchemaType = SchemaType::Struct {
        repr_c: true,
        fields: Fields::Static(&[
            NamedField {
                name: Str::Static("verts"),
                ty: SchemaType::Array {
                    element: SchemaCell::Static(&<Vertex as Schema>::SCHEMA),
                    len: 3,
                },
            },
        ]),
    };
}
```

Cross-crate resolution works: `<Vertex as Schema>::SCHEMA` resolves against whichever crate defines `Vertex`'s impl. The `Vertex` helper type derives `Schema` in its own crate; its `const SCHEMA` is reachable by reference from anywhere.

### `#[derive(Kind)]` simplifies

With a const schema available via the trait, `Kind` derive stops maintaining its own schema walker. It emits:

```rust
impl Kind for DrawTriangle {
    const NAME: &'static str = "aether.draw_triangle";
    const ID: u64 = aether_mail::kind_id_from_schema(Self::NAME, &Self::SCHEMA);
}
```

Where `kind_id_from_schema` is a `const fn` that walks a `SchemaType` tree via recursive `SchemaCell` deref and fnv1a-chains the bytes it encounters. No postcard at macro time; no duplication with `Schema`.

### Structural, not nominal, hashing

The hash input describes **wire shape**, not source identity. Rust type names (`Vertex`, `Point5D`, module paths, crate origin) do not appear in the byte stream. Nested `SchemaType::Struct` nodes are described purely by their structure: tag byte, `repr_c` flag, field count, and each field's `(name, type)` pair. Only three identifier-ish inputs feed the hash:

- **Kind name** — `K::NAME`, hashed once at the top level. Identifies the kind on the wire and in logs; part of `K::ID` by ADR-0030's construction.
- **Field names** — every `NamedField.name`, at every nesting depth. Field names are part of the wire contract because the hub uses them to map agent JSON params to postcard bytes; a rename is a wire-visible change.
- **Enum variant names** — same reasoning; they're the wire-level discriminants alongside the discriminant integer.

Rust struct identifiers, module paths, and crate names are **not** hashed. Consequences of this structural-only choice, all of them intentional:

- **Crate reorganization is hash-free.** Moving `Vertex` from `aether-kinds` to `aether-mail`, or splitting it into its own crate, does not change any hash that references it. Refactors that don't alter wire shape don't invalidate compiled peers.
- **Two same-shape structs with different Rust names collide.** `struct Vertex { x: f32, y: f32 }` and `struct Point2D { x: f32, y: f32 }` produce identical schemas, identical `const SCHEMA` byte streams, and — if either is used as a kind — identical kind ids. This is correct: on the wire they *are* interchangeable, and the Rust type system has already distinguished them at the source level where it matters. Anyone wanting them distinguished on the wire should differentiate via field shape (different names, different types, different counts) or by making them distinct kinds with distinct `#[kind(name = ...)]`.
- **The `NamedField.name` bytes carry the disambiguation load.** A schema with `{ x: f32, y: f32 }` and one with `{ width: f32, height: f32 }` hash differently because the field names are in the byte stream. That's the primary axis developers use to signal "different concept, same primitive layout."

The `manifest.rs` syntactic walker (`aether-mail-derive/src/manifest.rs`) retires entirely. The `aether.kinds` custom section is still emitted, but via `postcard::to_allocvec(&T::SCHEMA)` at build time — now possible because `T::SCHEMA` is a const value the derive can reference even if it's in another crate. (The derive can't call `postcard::to_allocvec` directly since it runs as a proc-macro; it emits a const byte array computed from the const schema via a const fn postcard serializer, or it emits the bytes by having the Kind derive depend on Schema and walking the const tree directly.)

### Supported type vocabulary

The derive commits to an explicit, closed set. Anything outside it is a compile error at the `#[derive(Schema)]` site — no silent skipping, no "best effort" fallback, no latent wire-shape divergence:

- **Primitives**: `u8`, `u16`, `u32`, `u64`, `i8`, `i16`, `i32`, `i64`, `f32`, `f64`, `bool`.
- **Strings and bytes**: `String` (length-prefixed UTF-8), `Vec<u8>` (canonical `SchemaType::Bytes`, distinct from `Vec<T>` for hub-side bytes handling).
- **Containers**: `Vec<T>`, `Option<T>`, `[T; N]` where `N` is a literal integer.
- **Structs**: named, tuple, and unit forms. `#[repr(C)]` flag propagates into `SchemaType::Struct { repr_c }`.
- **Enums**: unit, tuple, and struct variants. Discriminants are source-order indices by default; see "Explicit discriminants" below.

**Explicitly unsupported** (derive emits a compile error naming the unsupported type and pointing at the supported vocabulary):

- **`usize` / `isize`** — platform-dependent width. Postcard serializes them as varint, which is deterministic, but the ambiguity between "host pointer width" and "wire shape" is a wart we'd rather not paper over. Users pick `u32` / `u64` explicitly.
- **Generic type parameters** (`struct Msg<T> { v: T }`) — the derive can't produce a `const SCHEMA` for a non-monomorphized type. Users with generic kinds either instantiate at the kind site (`type Msg = GenericMsg<u32>` with an explicit `Schema` impl on the alias), or expand the type.
- **`HashMap<K, V>` / `BTreeMap<K, V>`** — hash iteration order is nondeterministic; BTreeMap is deterministic but cross-version ordering guarantees are thin. Users explicitly sort into `Vec<(K, V)>`.
- **References, raw pointers, function pointers, trait objects** — no wire-shape meaning.
- **`Cow<'_, _>`, `Box<dyn Trait>`, `PhantomData<T>`, `()` as a field type** — various flavors of "not a wire value."
- **Complex const expressions** in array lengths (`[T; Self::LEN]`, `[T; N + 1]`) — the derive needs a literal `u32` to emit into `SchemaType::Array { len }`.

**Type aliases** (`type Foo = u32;`) work transparently because `<Foo as Schema>::SCHEMA` dispatches through the trait impl — aliases never needed resolution at macro time under this design. This is a free improvement over the current syntactic walker, which fails on aliases.

### Forbidden serde customizations

The schema ↔ wire invariant is: **bytes on the wire are exactly what `postcard::serialize(&value)` produces for a type whose derived `Serialize` matches its declared `SCHEMA`**. Any serde attribute that makes those diverge is a compile error at the `#[derive(Schema)]` site:

- `#[serde(rename = "...")]` / `#[serde(rename_all = "...")]` — changes wire field/variant names; SCHEMA would carry the Rust identifier.
- `#[serde(skip)]` / `#[serde(skip_serializing)]` / `#[serde(skip_deserializing)]` — removes a field from the wire; SCHEMA would still list it.
- `#[serde(flatten)]` — embeds one struct's fields into another on the wire; SCHEMA would describe the nested shape.
- `#[serde(transparent)]` — single-field newtype serializes as the inner type; SCHEMA would describe it as a struct.
- `#[serde(default)]` — affects deserialization but can mask missing fields; SCHEMA has no way to represent "optional on the wire, required in the type."
- `#[serde(tag = ...)]`, `#[serde(content = ...)]`, `#[serde(untagged)]` — alternate enum encodings; SCHEMA assumes postcard's default tagged encoding.
- **Manual `impl Serialize` / `impl Deserialize`** on a type that also derives `Schema` — the derive has no way to verify the manual impl matches the declared SCHEMA. Compile error; users pick one side.

Attributes that don't affect wire shape (`#[serde(with = "...")]` for borrowing-only custom serializers, `#[serde(bound = "...")]` for generic constraints) remain allowed — but these are outside the kind-payload vocabulary anyway since kinds don't have borrow lifetimes.

### Explicit enum discriminants

`enum E { A = 5, B = 10, C }` — postcard uses varint-encoded declaration-index discriminants (`0, 1, 2`), ignoring the source-code `= 5` annotations. Today's derive also uses declaration indices. No mismatch against postcard, but the source-code discriminants are misleading and invite "the wire uses 5" confusion.

Two options, picking one in the derive:
- **Forbid explicit discriminants on kinds** — compile error at the `#[derive(Kind)]` site. Cleanest; forces users to accept "source order is wire order" as the contract.
- **Allow explicit discriminants but encode declaration index anyway** — what today's derive does. Preserves Rust ergonomics (FFI enums sometimes need explicit reprs) at the cost of the naming-vs-wire drift risk.

Decision: **forbid explicit discriminants on `#[derive(Kind)]` types**. Kinds are wire vocabulary, and the wire uses declaration order. Helper types that derive only `Schema` (not `Kind`) can keep explicit discriminants — they're not kinds, they're payload leaves, and the enclosing kind's SCHEMA captures whatever the helper's `Schema::SCHEMA` says.

### Substrate-side

`Registry` stores schemas the same way it does today — owned values, now built from `SchemaType::Owned`-variant trees after deserialization. Nothing visible to callers changes at the `register_kind` API. The hash computation changes: the registry gains a `schema_hash(&SchemaType) -> u64` that walks the tree via the same const fn the derive uses, producing identical bytes on both sides.

### Hub-side encoder/decoder

The biggest downstream surface. `encoder.rs` and `decoder.rs` match on `SchemaType` pervasively — every variant's field access changes from `fields: &Vec<NamedField>` to `fields: &Fields` (which derefs to `&[NamedField]`, so most match arms work unchanged). Recursive fields change from `&Box<SchemaType>` to `&SchemaCell`, but since both deref to `&SchemaType`, the walker bodies don't change. The match syntax changes slightly; the logic doesn't.

### Wire format compatibility

`postcard(SchemaType)` bytes change because the type layout changed (Fields / SchemaCell / Str wrappers affect the postcard encoding). Pre-1.0, no migration path required — bumping the `aether.kinds` manifest version byte (ADR-0028 §Versioning) signals the new format. Old manifests become unloadable; new substrates reject them with a clear error.

## Consequences

- **Structural hashing, not nominal.** Rust type names and crate paths don't enter the hash — only kind names, field names, and shape. Crate reorganization leaves hashes intact; two structurally-identical structs collide (correctly — they're wire-equivalent).
- **Closed type vocabulary, compile-time enforced.** Unsupported types (`usize`, `HashMap`, generics, etc.) and wire-shape-divergent serde attributes (`rename`, `flatten`, `transparent`, `skip`, custom `Serialize`) produce compile errors at the derive site, not silent skips or runtime mismatches. The SCHEMA ↔ wire invariant is the derive's promise.
- **Type aliases work for free.** `type Foo = u32` dispatches through `<Foo as Schema>::SCHEMA` — no special handling needed. Today's syntactic walker can't resolve aliases; the new design sidesteps the problem entirely.
- **Explicit enum discriminants forbidden on kinds.** Kinds commit to "source order is wire order"; helper types under `#[derive(Schema)]` alone keep Rust-ergonomic discriminant control.
- **Unified representation.** One `SchemaType`, one walker, one hash. The `manifest.rs` syntactic-walker hack retires. Schema and Kind derives stop carrying two implementations of the same logic.
- **ADR-0030 Phase 2 unblocks.** `const ID: u64` on `Kind` is a four-line derive change once `Schema::SCHEMA` is a const. Nested types (`DrawTriangle { verts: [Vertex; 3] }`) hash correctly on both sides because both sides see the same const tree.
- **Size cycle handled explicitly.** `SchemaCell` is the size-breaking indirection; no arena, no new dependency. One heap allocation per recursive node on deserialize — same cost envelope as today's `Box<SchemaType>`.
- **Hub rewrite.** Encoder and decoder match arms update to the new field access shape. Mostly mechanical; the logic is unchanged because `Deref` bridges the representation.
- **Breaking wire change.** `aether.kinds` manifest format bumps; old component binaries don't load until recompiled. Acceptable pre-1.0; `describe_kinds` output format stabilizes simultaneously.
- **Const-traversal story.** Once `Schema::SCHEMA` is const, any future analysis — stability hashing, doc-extraction, diff tooling between schema versions — runs against const data. Pays dividends beyond ADR-0030.
- **Const-fn postcard.** The `Kind` derive needs a const postcard serializer (or a const schema-hash walker that doesn't go through postcard bytes). The latter is simpler: walk `SchemaType` via recursive `const fn`, fnv1a-chain each node's tag byte + primitive bytes + name bytes directly. No intermediate bytes array; no postcard at const time. The `aether.kinds` section still uses runtime postcard — a build script or a deferred initialization can produce the bytes.
- **Retired ADR sections.** ADR-0028's schema-bytes-in-section claim stays, but the section is now postcard of a `SchemaType` built from the same const tree rather than a parallel syntactic resolution. The "unresolvable types skip emission" carve-out disappears — every type with a `Schema` impl gets emitted.
- **Derive tests rewrite.** `aether-mail-derive/tests/derive.rs` fixtures that check `fn schema()` output rewrite to check `const SCHEMA` values. Same assertions, different access pattern.
- **InputObserved and other hand-rolled `impl Kind`s** now also need `impl Schema` (or drop to just `impl Kind { const NAME = ...; }` if they keep Kind's `ID` default-free — see the "orphan" alternative below).

## Alternatives considered

- **Arena allocation on the hub side** (`bumpalo` per engine connection, deserialize into `&'a` refs, extend lifetime to `'static`). Viable but adds a dep and a lifetime-extension unsafe; `SchemaCell` solves it without either.
- **`Cow<'static, SchemaType>` directly.** Rejected: `Cow::Owned` holds `T` by value, so `SchemaType` containing `Cow<SchemaType>` creates infinite-size recursion. `SchemaCell` is essentially `Cow` with `Box` in the owned arm to break the cycle — the cleaner fit.
- **Two representations kept in sync (`SchemaNode` const + `SchemaType` owned, with a `From` conversion).** Rejected: reintroduces the "two walkers, one logic" problem this ADR is trying to retire. Divergence risk is real.
- **Force every nested type to be its own `Kind`.** Rejected: pollutes the kind namespace with helpers (`aether.vertex`, `hello.cell_position`, …), forces users into naming decisions for internal types, and doesn't generalize to primitives like `Option<u32>` that also need schemas without deserving kind-ness.
- **Build-script-generated schema tables.** Rejected: heavyweight, opaque to the derive, and out-of-band from Rust's normal compilation. The in-tree derive already has everything it needs once the trait shape is right.
- **Keep the status quo and hash name-only.** Rejected upstream in ADR-0030 (drift-detection regression); this ADR exists specifically to make the schema-inclusive story workable.
- **Require `Schema` derive to also derive `Kind`.** Rejected: `Vertex` derives `Schema` (it's a payload field type) but *isn't* a kind. Coupling the two reintroduces the "every helper is a kind" overreach.
- **Gate `SchemaCell` on `'static` bounds only.** Rejected: forces the deserialize path to `Box::leak` every allocation, which accumulates unbounded memory across engine reconnects. The `Owned` variant is load-bearing for the hub's per-engine lifetime story.
