# ADR-0100: Ref inline values through the Kind codec

- **Status:** Proposed
- **Date:** 2026-06-08
- **Revises:** ADR-0045 §1 (the `Ref<K>` inline-arm wire encoding)

## Context

ADR-0045 §1 defines `Ref<K> { Inline(K), Handle { id, kind_id } }` as the wire form a field uses to carry either an inline kind value or a reference into the substrate's handle store. The type is `#[derive(Serialize, Deserialize)]`, so the inline arm holds a typed `K` and serde-encodes it: discriminant `0` followed by `K`'s postcard bytes. That derive bounds every wrapped kind `K: Serialize + Deserialize`.

Two facts about the rest of the pipeline make the postcard inline arm wrong for cast kinds:

- A transform stores its output through `Kind::encode_into_bytes`. For a cast kind (`#[repr(C)]` + `Pod`, e.g. `Vec4`, `Mat4`) that is the raw `#[repr(C)]` byte image, not postcard.
- The substrate's `walk_and_resolve` (`crates/aether-substrate/src/handle_store.rs`) is byte-transparent. At a `Ref::Handle` position it splices the cached bytes — those `encode_into_bytes` bytes — into the inline slot without deserializing, then the recipient decodes the slot.

So a handle-resolved inline slot carries cast bytes while the wire contract (the serde derive, and the recipient's postcard decode) reads postcard bytes. The two byte images agree only for kinds whose every field is `f32`, because postcard encodes `f32` as four little-endian bytes, the same as the cast image. A kind with a `u16` field (postcard varint vs two raw bytes) would decode to a corrupted value. The pure-`f32` math kinds work by coincidence; the encoding is store-as-cast, decode-as-postcard.

The serde derive also makes cast kinds second-class in handle slots: a pure-cast math kind cannot ride a `Ref<K>` field without adding `Serialize`/`Deserialize` derives it otherwise never needs.

## Decision

The `Ref<K>` inline arm carries the kind's own codec bytes, and `Ref<K>` requires only `K: Kind`.

**Wire format (revising ADR-0045 §1).**

- Inline arm: discriminant `0` + `varint(len)` + `K::encode_into_bytes(K)` (`len` bytes). The body is cast bytes for a cast kind, postcard bytes for a postcard kind — the kind's declared wire shape, the same bytes the handle store caches.
- Handle arm: unchanged — discriminant `1` + `varint(id)` + `varint(kind_id)`.

The length prefix makes the inline body an opaque, self-delimiting blob: the splice walker skips it by length rather than re-deriving `K`'s byte layout from the schema, and the byte image at the inline slot is identical whether it arrived inline or was spliced from a resolved handle.

**Representation and bound.** `Ref::Inline(K)` keeps its typed payload — construction (`Ref::Inline(value)`) and pattern matching (`match … { Ref::Inline(k) => … }`) are unchanged for callers. The `#[derive(Serialize, Deserialize)]` is replaced by hand-written `Serialize`/`Deserialize` impls bounded `K: Kind`: the inline arm serializes `K::encode_into_bytes` as a byte string and deserializes through `K::decode_from_bytes`; the handle arm serializes the two ids as before. `CastEligible for Ref<K>` stays `false` (a `Ref` field still forces its parent kind postcard-classified), and the `Schema for Ref<K>` tag stays `SchemaType::Ref(inner)` — the schema vocabulary is unchanged; only the inline arm's byte interpretation moves.

**Consumers that follow the format.**

- The handle-store splice walker reads the inline arm as `varint(len)` + `len` bytes (skip by length) and, at a resolved handle, writes discriminant `0` + `varint(payload.len)` + payload.
- The schema-driven JSON codec (`aether-codec`, the MCP `submit_dag` descriptor path) encodes the inline body via the cast-or-postcard dispatch keyed on the inner schema, then length-prefixes it; it decodes by reading the length, slicing, and decoding the inner. The externally-tagged JSON descriptor form clients send — `{"Inline": <value>}` / `{"Handle": {"id", "kind_id"}}` — is unchanged.

## Consequences

- A cast kind rides a `Ref<K>` field with no serde derives. Pure-cast math kinds stay pure cast.
- The inline value round-trips through the kind's declared codec end-to-end: cast kinds store cast and decode cast, with no dependence on the postcard/cast layout coincidence. A non-`f32` cast field is now correct.
- The inline arm gains a varint length prefix (one to a few bytes per inline `Ref`). Existing kinds that declare no `Ref` field are unaffected; the field walker remains a no-op on them.
- The codec, the splice walker, and the hand-written serde impl must land together — they share one wire contract. The MCP descriptor's JSON shape does not change, but the wire bytes the codec emits for the inline arm do, so the codec change ships in the same unit as the format change.
- This is a wire-format change to an unreleased subsystem (ADR-0045 handles, pre-1.0); no migration path is owed.

## Alternatives considered

- **Opaque-bytes inline variant (`Ref::Inline(Box<[u8]>)`).** Store `K::encode_into_bytes` output directly in the variant. Produces the identical wire format, but erases the typed payload: every `Ref::Inline(value)` construction and `match` site moves to byte-level helpers, and the value is no longer readable by pattern match. Same wire, worse ergonomics, larger blast radius — rejected in favor of keeping `Inline(K)` typed with a hand-written serde impl.
- **Keep the serde derive, add serde derives to the math kinds.** Bolts `Serialize`/`Deserialize` onto pure-cast kinds purely to satisfy `Ref<K>`, and leaves the store-as-cast / decode-as-postcard coincidence in place. Rejected — it makes cast kinds carry serde they never use and does not fix the inline-decode correctness.
