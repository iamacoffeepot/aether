# ADR-0118: Own the Aether Wire Format

- **Status:** Proposed
- **Date:** 2026-06-16

## Context

A kind's payload is one of two shapes (ADR-0019): cast-shaped (`#[repr(C)]`
bytes, read directly as memory for zero-copy slabs), or the structured shape
used by everything else — every control-plane kind, every `Result`, anything
with a string, vector, option, enum, or map field.

The structured shape is what this ADR is about. Today it is produced and
consumed two different ways: the typed path (`aether-data`) encodes Rust values
through the external `postcard` crate, while the schema-walker (`aether-codec`)
encodes agent JSON at the MCP boundary by hand-writing the same byte layout from
a `SchemaType`, with no Rust types in hand. Two implementations of one format,
one of them an external dependency, kept in agreement by a test. That split is
the thing we are removing.

The single fact the design turns on: **the schema is present on both ends, for
every consumer that touches the bytes.** The typed path has the `SchemaType` at
compile time; the schema-walker is handed it at the boundary; the manifest and
`KindId`-hashing paths walk its name-stripped twin `SchemaShape`. Nothing
decodes these bytes without the schema.

That collapses what the bytes must carry. Everything structural — field names,
field order, field types, scalar widths, fixed-array lengths, the variant list —
is in the schema and costs zero wire bytes. The payload carries only what the
schema cannot pin down: scalar leaf values, collection and string lengths,
option presence, enum variant selectors, map entries, and ref selectors. A
format that carries exactly that, designed from aether's data rather than
inherited from any existing serializer, is the goal.

## Decision

Own the structured wire format end to end, designed from first principles, and
remove the external serialization dependency.

### Shape of the implementation

- **`aether_data::wire` is the single reference implementation** — the byte
  primitives plus `to_vec` / `from_bytes` / `take_from_bytes` and a
  `wire::Error`. It lives in `aether-data` because the typed derive runtime calls
  into it and `aether-codec` depends on `aether-data` (never the reverse), so
  this is the only placement reachable from the `no_std` foundation without a
  dependency cycle.
- **Two consumers over the one module.** A `serde` adapter (`Serializer` /
  `Deserializer` implemented over the `wire` primitives) lets any
  `#[derive(Serialize)]` kind encode with no per-type hand-coding; `serde` is
  plumbing that *utilizes* the format and does not define it. The schema-walker
  (`aether-codec`) drives the same `wire` primitives from `SchemaType` + JSON.
  Both must emit identical bytes for the same logical value.

### The format

Schema-driven, little-endian, fixed-width. The schema declares every type, so
integers need not be self-delimiting; fixed-width is then simpler, branchless to
decode, deterministic, and byte-identical to the cast image. A scalar leaf has
**one** representation — fixed little-endian of its declared width — shared by
the cast path and the structured path; the two differ only in struct padding
(the cast path is `#[repr(C)]`) and in the variable-length arms the cast path
cannot hold.

| Schema type | Encoding |
|---|---|
| `Unit` | zero bytes |
| `Bool` | 1 byte, `0` or `1`; any other value is a decode error |
| `Scalar(U8..U64)` | fixed little-endian, declared width (1/2/4/8 bytes) |
| `Scalar(I8..I64)` | fixed little-endian two's-complement, declared width |
| `Scalar(F32/F64)` | IEEE-754 little-endian (4/8 bytes), bit-faithful |
| `String` | `u32` little-endian byte length, then UTF-8 bytes |
| `Bytes` | `u32` little-endian byte length, then raw bytes |
| `Option(T)` | 1 byte presence (`0` None / `1` Some); if Some, the `T` encoding |
| `Vec(T)` | `u32` little-endian element count, then each element in order |
| `Array { T, len }` | the `len` elements in order — no count (the schema has `len`) |
| `Struct { fields }` | each field in schema order — no names, no count |
| `Enum { variants }` | the variant index as a fixed `u32` little-endian (serde's `variant_index`; the schema-walker matches by declaration position), then the selected variant's fields in order |
| `Ref(T)` | a two-variant sum encoded like any `Enum`: a `u32` selector — `0` inline → `u32` length-prefix + the inline kind's own encoded image (`K::encode_into_bytes`); `1` handle → `id` (8 LE) + `kind_id` (8 LE) |
| `Map { K, V }` | `u32` little-endian entry count, then `(K, V)` pairs in ascending encoded-key byte order |
| `TypeId` (`KindId` / `MailboxId` / `HandleId`) | fixed 8 bytes little-endian |

Three choices in that table carry the most weight:

- **Collection lengths are the one quantity the schema does not bound**, so they
  are a fixed `u32` (a 4 GB ceiling). Payloads that could approach it stage
  out-of-band through a handle or path, never inline mail.
- **Identifiers are high-entropy 64-bit hashes** (`KindId`, `MailboxId`,
  `HandleId`), so they are fixed 8 bytes. A variable-length integer would be
  strictly larger for full-range values — the opposite of compaction.
- **Sum-type selectors (`Enum`, `Ref`) are a fixed `u32`, not a schema-derived
  width.** The serde consumer is schema-less — it receives serde's
  `variant_index` but never the variant count — so a width that depends on the
  schema is not derivable on that side. A fixed `u32` is what both consumers can
  produce identically, which is the property the whole format rests on.

The inline `Ref` body is length-prefixed: `Ref`'s serde impl (ADR-0100) emits the
inline kind's own `encode_into_bytes` image through `serialize_bytes`, which both
consumers render as `u32` length + raw bytes. The prefix lets the handle store
skip or splice a resolved value in place without walking the subtree (ADR-0049),
and the body is the kind's codec image rather than a recursive re-encoding — so a
cast kind's inline value stays a cast image.

### Envelope

The encoding is **unversioned on the wire** — no per-payload format-version byte.
Format agreement is settled out of band: between binaries the RPC handshake
negotiates a `wire_version` once per connection (`aether-rpc` `Hello`/`HelloAck`)
and kicks a mismatched peer, so two peers on the same handshake version share the
same wire format by construction; within one binary the encoding is fixed at
compile time and the schema is present on both ends. A per-payload byte buys
nothing those two mechanisms don't already guarantee, and removing it collapses
the `to_vec`/`from_bytes` family into one (no `bare`/non-`bare` split). Every
kind image — the top-level payload, a nested inline-`Ref` body, a retained
handle body — is read from the front with no version prefix; interior values
(scalars, fields, collection elements, the `Ref` handle arm) were never
versioned and still aren't.

Two version mechanisms remain, each scoped to where it earns its keep. The
build-time **manifest sections** — `aether.kinds`, `aether.kinds.labels`,
`aether.kinds.inputs` — each carry a per-section version byte for their own
encoding evolution, and hash the structural body (`wire::to_vec`) into `KindId`;
the section byte is held out of the hash so `KindId` stays a function of the
schema alone and an id regenerates only when the canonical bytes themselves
change. The **handle store** versions itself at the meta level — `HandleMeta`'s
`schema_version` (vs `SCHEMA_VERSION`), `index.bin`'s `INDEX_FORMAT_VERSION`, the
`v1/` layout dir, and the stored `kind_id` vs the live registry — so a binary
reading bytes its past self wrote detects staleness without a per-value byte;
prior on-disk entries invalidate-and-drop on a `SCHEMA_VERSION` bump. A proper
versioned file format for the handle store is future work; the meta-level
versioning carries persistence versioning for now.

### Determinism

Encoding is deterministic by construction — the same value always produces the
same bytes — from fixed-width leaves, positional fields, count-prefixed
collections, and ascending-key-ordered maps. This is a formal invariant of the
format, and it buys reproducible golden-byte fixtures, stable hashing, and
byte-equality as value-equality.

Floats are encoded bit-faithfully. A normal float has one bit pattern and is
already deterministic; signed zero and NaN payloads are preserved rather than
normalized, so two floats that are IEEE-equal but bit-distinct encode
distinctly. Normalizing floats to a single representation per IEEE-equality
class is **deferred** — it is the only lossy operation the format would carry,
and the need it serves (content-addressing float-bearing values by IEEE
equality) does not exist today. `KindId` hashing is unaffected: `SchemaShape`
contains no floats.

## Consequences

**Positive**

- One owned implementation. The byte primitives are written once and shared by
  both consumers, replacing a library copy plus a hand-rolled copy kept in sync
  by a test.
- Scalars are consistent across the cast and structured paths; decode is
  branchless and faster than a variable-length scheme.
- Deterministic by construction, on aether's own terms, evolvable through one
  module.
- Removing the external crate makes its path unresolvable workspace-wide, so the
  boundary is self-enforcing — no lint needed.

**Negative**

- We own correctness. The `serde` adapter and the schema-walker are two
  traversals that must agree byte-for-byte, so a conformance suite — golden
  byte-vector fixtures plus a cross-check that both consumers emit identical
  bytes for the same value — is load-bearing. Serialization bugs are the
  silent-corruption kind.
- The `serde` adapter is real implementation work: a clean-room `Serializer` /
  `Deserializer` over the `wire` primitives.

**Identity and wire break**

- `KindId` is `hash(KIND_DOMAIN ++ canonical SchemaShape bytes)`. The canonical
  `SchemaShape` bytes change under this format, so **every `KindId` is
  regenerated**. Components rebuild, routing ids shift, and persisted
  handle-store snapshots and saved state written under the old encoding are
  invalidated — wiped or migrated. This is a deliberate clean break, taken
  while pre-1.0 makes it cheap.

**Neutral**

- `serde` stays a workspace dependency: it backs JSON at the MCP boundary and
  the typed-path adapter, and kinds keep their derives. This decision changes
  who implements the wire format under `serde`, not whether `serde` is used.
- The bytes are no longer compatible with any external serializer. Cross-language
  interop, if it is ever wanted, ships an aether-wire implementation in the other
  language (parked per ADR-0005 / ADR-0007).

### Migration arc

1. Build `aether_data::wire`: byte primitives, `to_vec` / `from_bytes` /
   `take_from_bytes`, `wire::Error`, the `serde` adapter, and the conformance
   suite (golden fixtures + adapter-vs-walker cross-check).
2. Switch the `aether-data` derive runtime and the `aether-codec` schema-walker
   onto `aether_data::wire`; regenerate `KindId`s; drop their external-crate
   dependency.
3. Migrate remaining call sites per crate to the `wire` API or the `Kind` trait
   methods (`encode_into_bytes` / `decode_from_bytes`).
4. Remove the `postcard` crate from every `Cargo.toml`; wipe or migrate
   persisted data.

## Alternatives considered

- **Keep the external dependency, enforce the boundary with a clippy
  `disallowed-methods` ban** (the first form of this ADR). Rejected: it preserves
  the two-implementation split and keeps the external crate's types in scope —
  it polices the smell instead of removing it.
- **Variable-length integers for compactness.** Rejected: it sacrifices
  determinism simplicity and cast-consistency, is strictly worse for
  high-entropy ids, and saves size only on the structured path — which is not
  where bulk lives, because cast-shaped slabs carry it.
- **Normalize floats to one representation per IEEE-equality class now.**
  Rejected (deferred): it is the only lossy operation the format would carry
  (dropping signed zero and NaN payloads) and serves a need aether does not have
  today; bit-faithful floats are already deterministic.
- **Preserve byte-compatibility with the prior external format.** Rejected: it
  would constrain a first-principles design to an inherited byte layout for no
  benefit, and the `KindId` regeneration already makes this a clean break.
