# ADR-0239: Fixed Byte Strings in the Schema Vocabulary

- **Status:** Proposed
- **Date:** 2026-09-26

Amends [ADR-0032](0032-canonical-schema-bytes-and-labels-sidecar.md)
(`SchemaShape` as `SchemaType`'s positional twin). Extends
[ADR-0065](0065-typed-id-newtypes-and-first-class-type-ids-in-the-schema.md)
(`SchemaType::TypeId`, a schema arm that exists to give a JSON form).
Holds [ADR-0059](0059-content-hashed-field-tags-for-component-storage.md)
rule 1 (storage field hashes) unchanged.

## Context

A Bloomery digest, `aether_bloomery_kinds::Digest`, implements `Schema` as
`[u8; 32]`, and so does `Ref<K>`, which wraps one. The schema-driven codec
(`aether_codec::encode_schema` / `decode_schema`), which carries every
`send_mail` parameter and renders every reply, therefore reads and writes a
digest as an array of 32 numbers. Every operator tool prints a digest as
hex (`Digest`'s `Display`), including `cargo xtask import-commit`, whose
output is `tree=<hex>`. Citing that tree in mail needs a manual conversion,
and a reply's digest cannot be compared with a printed one by eye.

The hex form must not move any hash. A mail `KindId` hashes the kind's name
with its canonical schema bytes (`canonical::kind_id_from_parts`), and an
ADR-0059 storage field hash folds in the field type's canonical schema
bytes (`storage::hash`). Both would change for every kind holding a digest
if the digest's canonical schema changed, and under ADR-0059 rule 1 that
breaks every stored Bloomery journal record with a digest field, for a wire
that stays byte-for-byte identical.

ADR-0032 already keeps JSON-only information out of the hashed bytes:
field and variant names live in `SchemaType` and the labels sidecar, never
in `SchemaShape`. ADR-0065's `TypeId` is the precedent for a schema arm
whose job is a non-default JSON form over an unchanged wire.

## Decision

1. **One new arm, `SchemaType::FixedBytes { len: u32 }`:** an opaque byte
   string of fixed length, such as a digest, a hash, or a key, as opposed to
   `[u8; N]`, which stays an array of small numbers (an RGB triple). On the
   wire it is exactly `len` raw bytes, the same as `[u8; len]`. In JSON it is
   a lowercase hex string of exactly `2 * len` characters. The arm is
   appended after `Blob`, so its `SchemaType` wire discriminant is 13 and
   every existing stored `SchemaType` still decodes. It can only describe
   fixed-length bytes, so no hex marker can land on a non-byte array.

2. **The arm is nominal: it hashes as `[u8; len]`.** The canonical
   serializer, `schema_to_shape`, and the ADR-0059 field-hash fold all write
   `FixedBytes { len }` exactly as `Array { element: Scalar(U8), len }`.
   `SchemaShape` gains no arm. This amends ADR-0032: `SchemaShape` is no
   longer `SchemaType`'s arm-for-arm twin, and `FixedBytes` is the first arm
   that exists only in `SchemaType`. `KindId`s and storage field hashes do
   not move, and tripwire tests pin both against `[u8; 32]`.

3. **Wasm components carry it in the labels sidecar.** `LabelNode` gains an
   appended `FixedBytes` arm. The substrate's sidecar merge
   (`kind_manifest::merge_schema`) turns `SchemaShape::Array { Scalar(U8),
   len }` with `LabelNode::FixedBytes` into `SchemaType::FixedBytes { len }`;
   the label on any other shape falls back to the plain merge, since the
   shape side wins.

4. **One spelling.** Encoding accepts only a JSON string of exactly `2 * len`
   characters from `[0-9a-f]`. Uppercase, a prefix (`0x`, `sha256:`), a wrong
   length, and the old number array are refused with the codec's existing
   error variants. Decoding renders lowercase. This matches `Digest`'s
   `Display` and the lowercase-only digest rule of `aether-workspace`'s
   `ImageRef`.

5. **`Digest` and `Ref<K>` adopt it.** Their `SCHEMA` becomes
   `FixedBytes { len: 32 }` and their `LABEL_NODE` becomes `LabelNode::FixedBytes`.
   Their wire and storage impls still delegate to `[u8; 32]`. Every kind that
   embeds them follows through its `Schema` delegation.

## Consequences

- Operators and agents read and write digests in mail exactly as tools print
  them: `import-commit`'s `tree=` value pastes into a program input.
- The wire, `KindId`s, and storage field hashes are unchanged, so no stored
  journal record, route cache, or loaded component is invalidated.
- `SchemaType`'s own wire form and the labels sidecar each gain one tag.
  Neither is hashed.
- The number-array JSON form of a digest stops being accepted. Anything that
  wrote digests as arrays (scratch operator helpers) must switch to hex.
- `compare_component_contracts` reports `[u8; 32]` → `FixedBytes { len: 32 }`
  as an input-schema change although the wire is identical. That is correct:
  the JSON contract changed.
- Every exhaustive match on `SchemaType` and `LabelNode` gains an arm, so the
  vocabulary, codec, substrate merge, and adopters change in one PR.
- `describe_kinds` renders the arm as `FixedBytes<len>` in its compact shape.
- Deferred: `AssetInfo.sha256: [u8; 32]` in `aether-kinds` can adopt the arm
  later. Maps keyed by hex bytes stay unsupported until a kind needs one.

## Alternatives considered

- **Render every `[u8; N]` as hex.** Changes unrelated fields: an RGB
  `Option<[u8; 3]>` would become `"rrggbb"`. A heuristic over structure,
  blind to meaning.
- **Declare digests as `Vec<u8>` / `Bytes`.** Adds a length prefix on the
  wire, moves every hash, loses the fixed length, and inherits the `Bytes`
  JSON form (UTF-8 string, base64, or a spilled file), which suits payloads,
  not identities.
- **A hashed arm, in `SchemaShape` too.** Moves every `KindId` and storage
  field hash over a digest, breaking stored journal records for an identical
  wire.
- **A hex flag on `SchemaType::Array`.** Admits invalid combinations (hex
  over `[f32; 4]`) and changes the wire body of every stored `Array` node.
- **Widen `TypeId`.** It is a fixed 8-byte `u64` leaf with its own cast size
  and alignment.
- **A `$hex` input embed in aether-mcp.** Input only; replies would still
  render number arrays.
- **Match `Schema::LABEL` strings in the codec.** Brittle type-path matching,
  and `LabelNode::Array` carries no type label to read.
