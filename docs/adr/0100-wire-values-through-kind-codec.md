# ADR-0100: Wire values through the kind codec

- **Status:** Proposed
- **Date:** 2026-06-08
- **Revises:** ADR-0045 §1 (the `Ref<K>` inline-arm wire encoding)

## Context

A substrate wire encoder that holds a typed `K` and postcard-encodes it is a latent correctness bug whenever `K`'s kind declares a *cast* descriptor (`#[repr(C)]` + `Pod`). The recipient decodes the bytes against the kind's declared shape — `Kind::decode_from_bytes` on the guest path, the schema-driven `aether_codec::decode_schema` on the hub/MCP path to a Claude session. A cast kind postcard-encoded against a cast descriptor decodes correctly only when every field is `f32`, because postcard encodes `f32` as four little-endian bytes, the same as the `#[repr(C)]` cast image. A `u16` field (postcard varint vs two raw bytes) decodes to a corrupted value. The pure-`f32` math kinds work by coincidence; the encoding is store-as-cast, decode-as-postcard.

`Kind::encode_into_bytes` already produces the kind's declared wire shape — cast bytes for a cast kind, postcard bytes for a postcard kind. Two substrate wire surfaces bypass it and hardcode postcard against a typed `K`.

**The `Ref<K>` inline arm.** ADR-0045 §1 defines `Ref<K> { Inline(K), Handle { id, kind_id } }` as the wire form a field uses to carry either an inline kind value or a reference into the substrate's handle store. The type is `#[derive(Serialize, Deserialize)]`, so the inline arm holds a typed `K` and serde-encodes it: discriminant `0` followed by `K`'s postcard bytes. The derive bounds every wrapped kind `K: Serialize + Deserialize`. The handle store caches `Kind::encode_into_bytes` output, and the substrate's byte-transparent `walk_and_resolve` (`crates/aether-substrate/src/handle_store.rs`) splices those cached bytes into the inline slot at a resolved `Ref::Handle` without deserializing, then the recipient postcard-decodes the slot. So a handle-resolved inline slot carries cast bytes while the recipient reads postcard bytes — the same store-as-cast / decode-as-postcard coincidence. The serde derive also makes cast kinds second-class in handle slots: a pure-cast math kind cannot ride a `Ref<K>` field without serde derives it otherwise never needs.

**The reply path.** The substrate's reply encoders carry a `K: Kind + serde::Serialize` bound and call `postcard::to_allocvec(result)`: `Mailer::send_reply` (`crates/aether-substrate/src/mail/mailer.rs`, the `Component`-target arm) and `HubOutbound::send_reply` (`crates/aether-substrate/src/mail/outbound.rs`, the Claude-session / remote-engine arm). A reply whose kind has a cast descriptor postcard-encodes against that cast descriptor and is correct only by the same pure-`f32` coincidence. The `serde::Serialize` bound also propagates up the shared per-handler reply traits (`OutboundReply::reply` / `reply_to`, `MailCtx::reply`), which both the FFI `FfiCtx` and the native `NativeCtx` implement, so a `Pod`-without-`Serialize` kind cannot be replied from a handler at all.

## Decision

A substrate wire encoder for a typed `K` honors the kind's declared wire shape — cast or postcard — through `Kind::encode_into_bytes` / `Kind::decode_from_bytes`, never a hardcoded serde/postcard path. This applies to both surfaces above, and a kind that carries or replies a value needs only `K: Kind`.

### `Ref<K>` inline arm (revises ADR-0045 §1)

The inline arm carries the kind's own codec bytes, and `Ref<K>` requires only `K: Kind`.

**Wire format.**

- Inline arm: discriminant `0` + `varint(len)` + `K::encode_into_bytes(K)` (`len` bytes). The body is cast bytes for a cast kind, postcard bytes for a postcard kind — the same bytes the handle store caches.
- Handle arm: unchanged — discriminant `1` + `varint(id)` + `varint(kind_id)`.

The length prefix makes the inline body an opaque, self-delimiting blob: the splice walker skips it by length rather than re-deriving `K`'s byte layout from the schema, and the byte image at the inline slot is identical whether it arrived inline or was spliced from a resolved handle.

**Representation and bound.** `Ref::Inline(K)` keeps its typed payload — construction (`Ref::Inline(value)`) and pattern matching (`match … { Ref::Inline(k) => … }`) are unchanged for callers. The `#[derive(Serialize, Deserialize)]` is replaced by hand-written `Serialize`/`Deserialize` impls bounded `K: Kind`: the inline arm serializes `K::encode_into_bytes` as a byte string and deserializes through `K::decode_from_bytes`; the handle arm serializes the two ids as before. `CastEligible for Ref<K>` stays `false` (a `Ref` field still forces its parent kind postcard-classified), and the `Schema for Ref<K>` tag stays `SchemaType::Ref(inner)` — the schema vocabulary is unchanged; only the inline arm's byte interpretation moves.

**Consumers that follow the format.**

- The handle-store splice walker reads the inline arm as `varint(len)` + `len` bytes (skip by length) and, at a resolved handle, writes discriminant `0` + `varint(payload.len)` + payload.
- The schema-driven JSON codec (`aether-codec`, the MCP `submit_dag` descriptor path) encodes the inline body via the cast-or-postcard dispatch keyed on the inner schema, then length-prefixes it; it decodes by reading the length, slicing, and decoding the inner. The externally-tagged JSON descriptor form clients send — `{"Inline": <value>}` / `{"Handle": {"id", "kind_id"}}` — is unchanged.

### Reply encoding

The reply encoders route the reply payload through `K::encode_into_bytes` instead of `postcard::to_allocvec`, and the reply bound relaxes from `K: Kind + serde::Serialize` to `K: Kind`:

- `Mailer::send_reply` (`Component` arm) and `HubOutbound::send_reply` (`Session` / `EngineMailbox` arms) encode `result` via `encode_into_bytes`. The two synthesized concrete-kind replies in `route_mail` (`TraceTailResult`, `LogTailResult`) encode through the same path.
- The relay `NativeBinding::send_reply_for_handler` and the shared reply traits `OutboundReply::reply` / `reply_to` and `MailCtx::reply` drop the `serde::Serialize` half of their bound; the FFI and native impls follow. The FFI impls already encode through `encode_into_bytes`, so the bound is the only change there; the native impls inherit the encoder switch above.

This is not a wire-format break. Every reply kind in the workspace is postcard-classified, so its `encode_into_bytes` output is byte-identical to the prior `postcard::to_allocvec` output, and the reply-decode consumers already honor the declared shape — the guest decodes via `Kind::decode_from_bytes`, the hub/MCP path via `aether_codec::decode_schema` over the kind's `SchemaType`. The change is byte-identical for existing reply kinds and makes a cast reply kind shape-correct and repliable.

## Consequences

- A cast kind rides a `Ref<K>` field and is repliable from a handler with no serde derives. Pure-cast math kinds stay pure cast.
- An inline `Ref` value and a reply value both round-trip through the kind's declared codec end-to-end: cast kinds store/encode cast and decode cast, with no dependence on the postcard/cast layout coincidence. A non-`f32` cast field is now correct on both surfaces.
- The `Ref` inline arm gains a varint length prefix (one to a few bytes per inline `Ref`). Kinds that declare no `Ref` field are unaffected; the field walker remains a no-op on them. The reply path gains no wire bytes.
- The `Ref` codec, the splice walker, and the hand-written serde impl must land together — they share one wire contract. The MCP descriptor's JSON shape does not change, but the wire bytes the codec emits for the inline arm do, so the codec change ships in the same unit as the format change.
- The `Ref` inline arm is a wire-format change to an unreleased subsystem (ADR-0045 handles, pre-1.0); no migration path is owed. The reply path carries no format change.

## Alternatives considered

- **Opaque-bytes inline variant (`Ref::Inline(Box<[u8]>)`).** Store `K::encode_into_bytes` output directly in the variant. Produces the identical wire format, but erases the typed payload: every `Ref::Inline(value)` construction and `match` site moves to byte-level helpers, and the value is no longer readable by pattern match. Same wire, worse ergonomics, larger blast radius — rejected in favor of keeping `Inline(K)` typed with a hand-written serde impl.
- **Keep the serde derive, add serde derives to the math kinds.** Bolts `Serialize`/`Deserialize` onto pure-cast kinds purely to satisfy `Ref<K>`, and leaves the store-as-cast / decode-as-postcard coincidence in place on both surfaces. Rejected — it makes cast kinds carry serde they never use and does not fix the inline-decode or reply-encode correctness.
- **Fix only the `Ref<K>` inline arm, leave the reply path on postcard.** Keeps the reply encoders' `serde::Serialize` bound and their `postcard::to_allocvec` call. The bound is vestigial for the FFI reply (whose body already encodes via `encode_into_bytes`) but load-bearing for the native reply, so it cannot relax in isolation, and the native reply keeps the same cast/postcard coincidence the `Ref` fix removes. Rejected — a cast kind would be first-class in a handle slot but second-class on the reply path, and the same coincidence bug would persist on one of two wire surfaces. The reply path is folded into this decision so the principle holds uniformly.
