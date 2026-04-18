# ADR-0020: Symmetric receive_mail decode

- **Status:** Accepted
- **Date:** 2026-04-17

## Context

ADR-0019 unified the outbound (agent → engine) path: every kind has a `SchemaType`, the hub encodes from JSON params, and `payload_bytes` was removed from `send_mail`. Agents now describe what they want to send instead of pre-encoding bytes.

The inbound (engine → agent) path was explicitly out of scope and still works as it did before:

```jsonc
{
  "engine_id": "...",
  "kind_name": "aether.observation.frame_stats",
  "payload_bytes": [12, 0, 0, 0, ...],  // raw postcard / repr_c bytes
  "broadcast": true
}
```

The agent gets the bytes and is on its own. Three concrete pain points:

- **Reply-to-sender results are unreadable without manual decode.** `aether.control.load_result` etc. now have real `Ok | Err { reason }` schemas (per ADR-0019), but agents see them as byte arrays. Determining whether a load succeeded requires the agent to know the postcard wire format and parse it by hand — exactly the friction ADR-0019 set out to remove.
- **Observation mail is opaque.** `aether.observation.frame_stats` and any future broadcast kinds (telemetry, structured logs) ship as bytes. The engine *has* the schema; the hub *speaks* the schema; but the agent never sees a structured value.
- **The asymmetry undermines the unified-encoding mental model.** "Send by params, receive as bytes" is the kind of inconsistency that pushes agents back toward custom decoders — the same drift `payload_bytes` removal was meant to design out.

The hub already has everything it needs: `KindDescriptor` for every kind the engine declared (handshake + `KindsChanged`), a postcard wire-format walker (it built the encoder for ADR-0019), and a `serde_json::Value` representation it already emits for tool results. The work is symmetrizing the encoder, not new infrastructure.

## Decision

`receive_mail` returns a structured `params: serde_json::Value` field for every inbound mail, decoded by the hub against the kind's schema. The current `payload_bytes` field stays alongside it — escape hatch for kinds the hub can't decode (decode error, schema drift) and for agents that want raw bytes for some reason.

### 1. Tool-surface change

Each `receive_mail` item gains a `params` field:

```jsonc
{
  "engine_id": "...",
  "kind_name": "aether.control.load_result",
  "params": { "Ok": { "mailbox": 7 } },
  "payload_bytes": [0, 7, 0, 0, 0],
  "broadcast": false
}
```

`params` is the JSON shape symmetric to what `send_mail` accepts: enums as `{ "Variant": {...} }`, structs as objects, scalars as JSON numbers, `Vec<u8>` (`SchemaType::Bytes`) as a JSON array of bytes (matching `send_mail`'s input shape), strings as JSON strings, etc.

If the hub fails to decode (unknown kind, schema mismatch, malformed bytes), `params` is `null` and a `decode_error: String` field appears explaining why. `payload_bytes` is always populated.

### 2. Decoder lives in `aether-hub`

A `decode_schema(bytes, &SchemaType) -> Result<serde_json::Value, DecodeError>` mirroring `encode_schema`. Two internal paths matching the encoder:

- **Cast-shaped (`Struct { repr_c: true }`):** walk the `#[repr(C)]` field layout, lift each field into JSON. For top-level cast-shaped kinds with `mail.count > 1` (today's slab semantics, ADR-0019 §1), produce a JSON array of `count` decoded structs.
- **Postcard:** consume the byte stream per the postcard 1.x spec — varints, zigzag, length-prefixed strings/vecs/bytes, externally-tagged enums — symmetric to what the encoder writes.

Tested by round-trip: for every kind the engine ships, `decode_schema(encode_schema(value, schema), schema)` equals `value` modulo JSON normalization (e.g. integer types).

### 3. Slab semantics

A cast-shaped kind delivered with `mail.count > 1` is decoded as a JSON array of `count` elements (`[{...}, {...}, ...]`). This matches how an agent would have to read the slab anyway and keeps the `count` field's meaning visible.

Postcard kinds always have `count = 1` (slab semantics aren't postcard-shaped per ADR-0019); their `params` is a single decoded value.

### 4. What this ADR does *not* do

- **No SDK-side change.** Components still receive bytes through `Mail::decode<K>` as today. The decode symmetry is a hub-side affordance for agents, not a guest-side one.
- **No streaming.** `receive_mail` is still a non-blocking drain of finished mail; nothing about delivery semantics changes.
- **No schema versioning.** Decode failures from schema drift are loud (`decode_error` is populated), not silent. Migration story remains "rebuild the substrate."

## Consequences

### Positive

- **Closes the ADR-0019 loop.** Send by params, receive by params. The encoding family is unified end-to-end at the agent surface.
- **Result kinds become first-class.** `LoadResult`, `DropResult`, `ReplaceResult` show up as structured JSON. Agents distinguish `Ok`/`Err` by reading `params.Ok` vs `params.Err.reason` instead of hand-decoding postcard bytes.
- **Observation mail becomes inspectable.** `FrameStats` shows up as `{ "frames": 120, "triangles": 1, ... }` instead of an array of bytes the agent has to know the layout of.
- **No new infra.** Decoder reuses the `KindDescriptor` cache and the postcard walker that the encoder already needs. The hub-protocol wire format is unchanged.
- **`payload_bytes` survives as an escape hatch.** Unlike the outbound case (where the hatch enabled silent drift), inbound bytes are never authored — they're produced by an engine the agent doesn't control. Keeping them for decode-failure cases is defensive, not a design hole.

### Negative

- **Two paths through every kind shape.** The hub now carries an encoder *and* a decoder for each `SchemaType` arm. Mitigated: the dispatch is mechanical and tested by round-trip.
- **`receive_mail` payload size grows.** Each item carries `params` plus `payload_bytes`. For large slab kinds (e.g. a thousand-element vertex buffer broadcast back), this roughly doubles the response size. Mitigated: observation mail in practice is small structured records; large slabs are sent agent → engine, not the other way.
- **Decoder bugs surface as silently-wrong JSON.** A postcard-decode bug in the hub could produce structurally-valid but wrong `params`. Mitigated by round-trip tests; not novel — the encoder has the same risk.

### Neutral

- **`payload_bytes` removal is a future ADR, not this one.** Once `params` ships and the decode-error path proves itself, dropping `payload_bytes` is a follow-on. Doing it now would gate this ADR on perfect decoder coverage; doing it later is a one-line tool-schema change.
- **No engine-side cost.** The engine ships the same bytes as today; the hub is the only thing that grows.

## Alternatives considered

- **Drop `payload_bytes` immediately, return only `params`.** Cleaner symmetry with `send_mail`. Rejected (for now): inbound bytes can come from kinds the hub doesn't know how to decode (schema drift, foreign engines, decoder bugs), and the agent has no recourse without raw bytes. Removing the field can come later once the decoder is exercised in production.
- **Decode at the substrate, ship JSON over the wire.** Engine ↔ hub frames stay as bytes (postcard `EngineMailFrame.payload`); only the MCP boundary shifts. Rejected: putting JSON on the engine-to-hub wire bloats the protocol for a problem (agent ergonomics) that lives entirely at the MCP boundary.
- **Add a separate `decode_mail` MCP tool the agent invokes per item.** Lazy decode. Rejected: doubles the round-trips for what's almost always "drain → look at every item." Eager decode at drain time is one network call.
- **Punt symmetric decode until friction shows up.** Status-quo option. Rejected: friction has shown up — every reply-to-sender result lands as bytes today, and reading them is exactly the kind of grunt work ADR-0019 set out to eliminate.

## Follow-up work

- **`aether-hub`**: implement `decode_schema(bytes, &SchemaType) -> Result<serde_json::Value, DecodeError>`. Round-trip test against the existing encoder for every kind shape.
- **`aether-hub` MCP**: add `params` and `decode_error` fields to `receive_mail` items; populate `params` on success, `decode_error` on failure; always populate `payload_bytes`.
- **CLAUDE.md**: update the `receive_mail` description so the params-first reading is the documented path.
- **Parked, not committed:**
  - Removing `payload_bytes` from `receive_mail` (future ADR; gated on decoder maturity).
  - Filtering `receive_mail` by `kind_name` server-side (currently the agent filters client-side).
  - Streaming / long-poll `receive_mail` (today's drain is non-blocking).
