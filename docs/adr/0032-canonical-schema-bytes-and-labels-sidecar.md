# ADR-0032: Canonical schema bytes and labels sidecar

- **Status:** Accepted
- **Date:** 2026-04-20
- **Accepted:** 2026-04-20

## Context

ADR-0031 made `SchemaType` const-constructible and shifted `Schema` to `const SCHEMA: SchemaType`. It left two threads loose:

1. **`aether.kinds` section bytes.** The per-component wasm manifest (ADR-0028) is still produced by a syntactic walker (`aether-mail-derive::manifest`) that duplicates schema resolution at macro-expansion time and silently skips any field whose type isn't in its hand-rolled vocabulary. ADR-0031 retired the *need* for the walker by making `<T as Schema>::SCHEMA` const-reachable, but punted on the replacement.
2. **ADR-0030 Phase 2 `const ID: u64`.** ADR-0030 proposed computing `fnv1a(name ++ postcard(schema))` at macro-expansion time. After ADR-0031, the schema is only known at *const-eval* time (post-expansion), so that literal-emission path is unreachable.

Both threads want the same thing: **canonical bytes derived from `&SchemaType` at const-eval time**. One serializer, two consumers (hash and section) avoids two parallel walkers kept in lockstep.

A separate question is what *information* belongs in the hashed bytes. Two axes of identification carry across the wire today:

- **Structural** — field slot positions, variant discriminants, primitive types. These *are* the wire shape; a mismatch between producer and consumer here produces garbage bytes on decode.
- **Nominal** — Rust type identifiers, field names, variant names. These are human-readable annotations. Postcard does not write them on the wire for instances — fields are positional, variants are numeric discriminants.

Field names currently live in `NamedField.name` inside `SchemaType::Struct`. Including them in the hash makes renames wire-breaking; excluding them makes renames free at the wire level (consistent with how postcard actually serializes instances). ADR-0031 set the nominal/structural precedent for Rust type names (type renames don't change the hash); this ADR extends it to field names and variant names — total erasure of nominal information from the hashed bytes, with a parallel labels sidecar carrying the full reconstruction for consumers that want it.

## Decision

**A const-fn canonical serializer in `aether-hub-protocol` produces postcard-compatible bytes from `&SchemaType` at const-eval time, omitting all nominal fields (field names, variant names). Those bytes are (a) the content of the `aether.kinds` section, (b) the input to `fnv1a_64` for `K::ID`. A separate const-fn labels serializer walks a parallel `LabelNode` tree (emitted per-type alongside `SCHEMA`) and produces the `aether.kinds.labels` section. The labels section is required for hub load; other consumers may skip it.**

### Canonical serializer

`const fn canonical_serialize<const N: usize>(&SchemaType) -> [u8; N]` walks `SchemaType` recursively and emits postcard bytes of a positional-only shape:

- `Struct { fields: [NamedField { name, ty }], repr_c }` → `tag(STRUCT) ++ bool(repr_c) ++ varint(len) ++ concat(canonical(f.ty))`. `NamedField.name` is skipped.
- `Enum { variants }` → `tag(ENUM) ++ varint(len) ++ concat(variant_bytes(v))`. Per-variant: `Unit { discriminant }` → `tag(UNIT) ++ varint(discriminant)`; `Tuple { discriminant, fields }` → `tag(TUPLE) ++ varint(discriminant) ++ varint(len) ++ concat(canonical(f))`; `Struct { discriminant, fields }` → `tag(STRUCT_V) ++ varint(discriminant) ++ varint(len) ++ concat(canonical(f.ty))`. Variant names and `NamedField.name`s are skipped.
- Primitives, `Bool`, `Unit`, `String`, `Bytes`, `Option`, `Vec`, `Array` — carried as-is (no names to begin with).

A parallel `const fn canonical_len(&SchemaType) -> usize` computes the byte length in a separate const pass so callers can size the output:

```rust
const LEN: usize = canonical_len(&<Triangle as Schema>::SCHEMA);
const BYTES: [u8; LEN] = canonical_serialize::<LEN>(&<Triangle as Schema>::SCHEMA);
```

Two passes is the cost of stable Rust — `generic_const_exprs` would return `[u8; len(SCHEMA)]` in one go but is nightly. Cost is microseconds at compile time.

The closed vocabulary (only `KindDescriptor`/`SchemaType` shapes) is why this fits in a `const fn` at all — `serde::Serialize` is not const-dispatchable.

### Wire shape of the canonical bytes

Postcard-compatible but of a smaller positional type — effectively `postcard(SchemaShape)` where `SchemaShape` mirrors `SchemaType` without name fields. The hub, at load time, decodes canonical bytes as `SchemaShape` and merges them with the labels sidecar into its in-memory `SchemaType` (which keeps names for encode-from-JSON).

### `K::ID` derivation

```rust
impl Kind for Triangle {
    const NAME: &'static str = "test.triangle";
    const ID: u64 = aether_mail::fnv1a_64_prefixed(
        aether_mail::KIND_DOMAIN,
        &__AETHER_CANONICAL_BYTES_TRIANGLE,
    );
}
```

`fnv1a_64_prefixed(prefix, payload) -> u64` hashes `prefix ++ payload` in one pass; same algorithm and constants as `mailbox_id_from_name`. The `KIND_DOMAIN` prefix (`b"kind:"`) disjoins the `Kind::ID` space from `MailboxId` (issue #186); the mailbox side uses `MAILBOX_DOMAIN = b"mailbox:"` symmetrically.

Two structurally-identical kinds with different Rust type paths or field names produce the same `ID`. Consistent with ADR-0031's structural-not-nominal stance — they *are* interchangeable on the wire, the Rust type system distinguishes them at source level, and the labels sidecar disambiguates them for human consumers.

### Labels sidecar

Each `#[derive(Schema)]` emits, alongside `const SCHEMA`:

```rust
impl Schema for Vertex {
    const SCHEMA: SchemaType = /* ... */;
    const LABEL: Option<&'static str> =
        Some(concat!(module_path!(), "::", stringify!(Vertex)));
    const LABEL_NODE: LabelNode = /* built below */;
}
```

`LABEL` is the Rust type path — `"my_component::geom::Vertex"`. Primitive blanket impls (`u8`, `bool`, `String`, `Vec<T>`, `Option<T>`, `[T; N]`) set `LABEL = None` — they're anonymous.

`LABEL_NODE` is the parallel-to-schema tree of nominal info. Shape:

```rust
pub enum LabelNode {
    Leaf,                                   // primitives, String, Bytes
    Option(LabelCell),
    Vec(LabelCell),
    Array(LabelCell),
    Struct {
        type_label: Option<&'static str>,
        field_names: &'static [&'static str],
        fields: &'static [LabelNode],
    },
    Enum {
        type_label: Option<&'static str>,
        variants: &'static [VariantLabel],
    },
}

pub enum VariantLabel {
    Unit { name: &'static str },
    Tuple { name: &'static str, fields: &'static [LabelNode] },
    Struct {
        name: &'static str,
        field_names: &'static [&'static str],
        fields: &'static [LabelNode],
    },
}

pub enum LabelCell {
    Static(&'static LabelNode),
    Owned(Box<LabelNode>),
}
```

Derive emits `Vertex::LABEL_NODE` like `SCHEMA` — referencing nested types' `LABEL_NODE` via `<FieldT as Schema>::LABEL_NODE` for recursion. Field names come from the Rust source (struct fields) or are positional indices (`"0"`, `"1"`) for tuple structs and tuple variants.

The labels section contains one record per kind: `kind_label: &'static str` + `root: LabelNode`, postcard-encoded via a sibling `const fn canonical_serialize_labels`.

### Section framing

`aether.kinds` and `aether.kinds.labels` both use length-prefixed record framing: `[length: varint][record bytes]` repeated. Version byte `0x02` on each prefixes the section (bumped from ADR-0028's `0x01` to signal canonical-bytes producer). Per-kind records in the two sections line up by declaration order.

### Hub load

1. Read `aether.kinds` records → decode each as `SchemaShape` → call registry.
2. Read `aether.kinds.labels` records → decode each as `(kind_label, LabelNode)`.
3. Merge each `(SchemaShape, LabelNode)` pair into an in-memory `SchemaType` whose fields carry names from the label tree. This is the structure the encoder/decoder consult.
4. If the labels section is missing or a record fails to decode, load fails with a clear error. The labels side is load-bearing at the hub.

Other consumers (substrate-only decoders that work off raw postcard bytes, tooling that only cares about wire shape) can read `aether.kinds` alone and skip labels.

### Derive output

The Schema derive emits three consts: `SCHEMA`, `LABEL`, `LABEL_NODE`. The Kind derive, which was previously building its manifest via the syntactic walker, now:

1. Emits `const ID: u64 = fnv1a_64_prefixed(KIND_DOMAIN, &CANONICAL_BYTES)`.
2. Emits both section statics from `canonical_serialize(&SCHEMA)` and `canonical_serialize_labels(&kind_label, &LABEL_NODE)`.

`aether-mail-derive::manifest` retires entirely. No more syntactic type resolution; no more silent skipping; types outside the supported vocabulary fail to compile at the `#[derive(Schema)]` site via trait-bound failures.

### `SchemaType` stays named in memory

The hub's encode path reads `NamedField.name` off the live tree to map MCP JSON keys to postcard field positions. Stripping names from `SchemaType` would force a merge-at-every-encode step. Cheaper to keep names in `SchemaType` for the hub's convenience; the canonical serializer drops them only when producing hashed bytes.

## Consequences

- **One serializer, two consumers.** The canonical bytes are authoritative for both wire and hash. A bug in the serializer fails both symmetrically (hash mismatch between producer and consumer), which is loud.
- **Full structural-nominal separation.** The hash carries wire shape only. All identifying information — type paths, field names, variant names — lives in the labels sidecar. Renames are hash-free; only wire-shape changes bump the id.
- **Labels required at the hub.** The hub's JSON-param encode path needs field names. A component without a labels section fails to load at the hub. Substrates and other consumers can operate on canonical bytes alone.
- **Same-shape-different-names collide on id.** `Vertex { x: f32, y: f32 }` and `Position { row: f32, col: f32 }` hash identically. Consistent with ADR-0031's Rust-type-name stance; source-level distinction handles disambiguation where it matters.
- **ADR-0030 Phase 2 unblocks.** `const ID: u64` is four lines of derive output once canonical bytes are in a const. No nightly features.
- **ADR-0028 wire bump.** Section framing version byte goes `0x01 → 0x02`. Old component binaries don't load until recompiled. ADR-0031 already committed to a one-shot break.
- **New required section.** `aether.kinds.labels` is new. Components built before this ADR don't have it and fail to load at the hub — same recompile-required story as the canonical-bytes change.
- **Two const passes at compile time.** `canonical_len` + `canonical_serialize`, mirrored for labels. Per-kind cost is microseconds; invisible in practice.
- **Closed vocabulary in the serializer.** Extending `SchemaType` (future `Bitfield`, `Fixed<N>`, etc.) requires updating `canonical_*` and the labels walker in lockstep. Acceptable — all live in `aether-hub-protocol` next to the types.
- **No runtime postcard at manifest-emission time.** The derive stops depending on the `postcard` crate.
- **Derive adds `LABEL` and `LABEL_NODE` consts.** Two extra consts per `Schema` impl. Hand-rolled `impl Schema` has to provide them explicitly or the kinds load at the hub without structure labels.

## Alternatives considered

- **Include field names in the hashed schema.** What the earlier ADR-0032 draft proposed. Rejected after discussion — nominal info in the hash makes renames wire-breaking, which conflates "I renamed a struct field for clarity" with "I changed the wire contract." Pushing all nominal info to labels is the cleaner model.
- **Const-fn schema-hash walker with no canonical bytes.** Rejected — we'd still need separate bytes for the `aether.kinds` section. One walker doing both halves is cheaper.
- **Runtime export harvest (`aether_describe_kinds_p32`).** Rejected — amends ADR-0028's section model and forces the substrate to instantiate before learning kinds.
- **Labels optional at the hub, with positional MCP params when absent.** Rejected — two accept paths on the hub for the same MCP shape, and agents would have to know which components need which form. Requiring labels at the hub collapses the surface.
- **Separate `SchemaLabel` trait.** Keep `Schema` to `SCHEMA` only and put `LABEL` / `LABEL_NODE` on a second trait. Rejected as ceremony — the three consts live on the same types for the same reason and the derive already produces them together.
- **Full module path vs terminal type name for `LABEL`.** Chose full path (`my_component::geom::Vertex`). Catches crate/module reorganization as a label diff, disambiguates same-named types in different modules, stable for a given source tree.
- **Name the const `PATH` or `TYPE_PATH`.** Rejected — "path" overloads with filesystem-path usage elsewhere in the codebase (`binary_path` on `load_component`, etc.). `LABEL` has no such collision and reads naturally.
