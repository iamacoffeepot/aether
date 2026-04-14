# ADR-0007: Schema-driven mail encoding at the hub

- **Status:** Proposed
- **Date:** 2026-04-13

## Context

ADR-0006's V0 tool surface gives Claude a single `send_mail(engine_id, recipient_name, kind_name, payload, count)` tool, where `payload` is raw bytes (a JSON array of `u8`). The first end-to-end session with the live MCP harness surfaced the consequence: signal-only kinds (`aether.tick`) and simple POD `u32`s are trivial to send, but anything with `f32` fields (`aether.mouse_move`, `aether.draw_triangle`) means the agent hand-packs little-endian float bytes, and postcard-structural kinds would be effectively impossible without running postcard client-side.

This is load-bearing friction. The point of the MCP harness is to be a credible authoring/testing interface for anyone — player, developer, Claude — attached to an engine. A tool surface where "send a mouse-move" requires computing `f32::to_le_bytes(10.5)` by hand isn't credible.

Forces at play:

- **Kinds are Rust types (ADR-0005).** Each kind has a canonical in-process representation. POD kinds are `#[repr(C)]` structs of primitives; structural kinds are serde-derived types encoded with postcard. Both tiers *have* a describable structure — the question is whether we expose it at the hub boundary.
- **The hub is a dumb forwarder today.** It knows kind *names* only for logging. It does not know field layouts or types. Any schema-awareness at the hub is a new responsibility and a new wire frame to ship it over.
- **Agent-authored mail is the primary driver.** The friction only shows up when a non-Rust sender (Claude via MCP) wants to construct mail. Engine-to-engine and substrate-local mail already uses Rust types natively — no encoding tool needed.
- **We don't want two tool calls per mail.** A naive `encode(schema, params) -> bytes` + `send_mail(bytes)` split doubles the round-trip. The agent-facing surface should stay one tool call per mail.
- **Not every kind is describable.** POD and signal kinds are flat and mappable to JSON params. Postcard-structural kinds with enums, `Option`, collections, or custom serde impls are harder. We shouldn't commit to describing all of them before we've felt concrete pressure from each shape.

## Decision

Fold the encode step into `send_mail` at the hub, driven by kind descriptors the engine publishes at connection time. Keep a raw-bytes escape hatch for kinds the hub can't (or hasn't learned to) encode.

### 1. Kind descriptors at handshake

Extend `Hello` (or add a follow-on frame — detail deferred to implementation) with a `Vec<KindDescriptor>`. Each descriptor is something like:

```rust
pub struct KindDescriptor {
    pub name: String,
    pub encoding: KindEncoding,
}

pub enum KindEncoding {
    /// Empty payload. `params` must be absent or empty.
    Signal,
    /// `#[repr(C)]` struct; describable as an ordered field list with primitive types.
    Pod { fields: Vec<PodField> },
    /// Opaque to the hub. Clients must use `payload_bytes`.
    Opaque,
}

pub struct PodField {
    pub name: String,
    pub ty: PodPrimitive, // U8/U16/U32/U64/I8/.../F32/F64, plus fixed-size arrays
}
```

Postcard-structural kinds are `Opaque` at V0. The descriptor format can grow a `Structural` variant later when a real kind needs it.

### 2. `send_mail` accepts structured params or raw bytes

The MCP tool shape becomes:

```
send_mail(
    engine_id: String,
    recipient_name: String,
    kind_name: String,
    params: Object | null,        // structured params, per the kind's descriptor
    payload_bytes: [u8] | null,   // escape hatch
    count: u32 = 1,
)
```

`params` and `payload_bytes` are mutually exclusive. The hub resolves the kind's descriptor, and:

- **Signal kinds:** `params` must be absent/empty; payload is empty.
- **POD kinds with a descriptor and `params` given:** hub encodes fields in declaration order into a `Vec<u8>`, matching `#[repr(C)]` layout.
- **Opaque kinds, or any kind when `payload_bytes` is given:** hub passes bytes straight through. `count` is carried untouched.

Validation errors (missing required field, type mismatch, wrong primitive) surface as MCP `invalid_params`, not silent corruption.

### 3. Kind registry exposure

A read-only MCP tool — `describe_kinds(engine_id)` — returns the descriptors the hub knows for an engine. The agent uses it to discover what `params` shape a given kind accepts. This makes the tool surface self-describing without requiring a separate schema file.

### 4. Mail is plural

Rename/extend `send_mail` to accept an array of mail specs (best-effort, per-mail status in the response). One tool call can carry many mails to one or many addresses. Per-mail failure doesn't abort siblings — the caller decides retry/abort policy from the response. The single-mail case is a one-element array.

## Consequences

### Positive

- **Ergonomic for the describable majority.** Every POD and signal kind currently in `aether-substrate-mail` (the full V0 set) becomes `{params: {x: 10.5, y: 20.0}}` on the wire. No `f32::to_le_bytes`. No source-diving to reconstruct layout.
- **One tool call per mail stays the norm.** Encode is a side effect of `send_mail`, not a separate round-trip. Agents can still introspect via `describe_kinds` when they want to, but it's an independent lookup, not a sequenced dependency.
- **Batching + encoding compose cleanly.** Because `send_mail` takes a list and best-effort semantics, N→1 and N_i↔M_i batches drop out of the same tool. No separate `send_mails` API.
- **Escape hatch means no kind is permanently unreachable.** `payload_bytes` is always available. Structural and custom-encoded kinds keep working; adding a descriptor is a pure upgrade, never a precondition.
- **Hub discovery surface matches MCP conventions.** `describe_kinds` is the same shape as any other MCP tool and plays well with the rmcp tool surface we already have.

### Negative

- **Hub is no longer a dumb forwarder.** It owns a kind registry per connected engine and a POD encoder. That's new code, new state, and new failure modes (descriptor/engine-kind drift, malformed descriptors). The substrate's registry is still source of truth; the hub mirrors it per engine.
- **Descriptor wire format is a commitment.** `KindDescriptor` lives in `aether-hub-protocol` and versioning it is the hub's problem from here on. POD primitive set is small and stable; the field-ordering contract is the load-bearing part.
- **POD layout rules live in two places.** The engine trusts Rust's `#[repr(C)]` layout; the hub must match exactly (including `f32` alignment, implicit padding between differently-sized fields). Simple cases are fine; nested structs would need recursive descriptors, which V0 explicitly does not do. Any kind that's non-trivially laid out falls to `Opaque`/`payload_bytes`.
- **Describable vs opaque is a per-kind design decision.** Authors now think about "can the hub encode this?" in addition to the POD-vs-structural choice from ADR-0005. More surface to reason about per kind; offset by the fact that it's fine to default to opaque and upgrade later.

### Neutral

- **Postcard-structural kinds stay opaque at V0.** Not foreclosed. The descriptor enum has room for a `Structural { serde_schema }` variant; it lands when a structural kind's hand-encoding becomes real friction, same policy as ADR-0005's two-tier split.
- **Engines other than `aether-substrate` are assumed to publish descriptors.** Any future engine that connects to a hub is expected to ship its kind vocabulary at handshake. Non-Rust engines will have to materialize descriptors in the same shape; cheap given the primitive set.
- **No per-session caching is needed.** Descriptors are sent at `Hello`; the hub caches them per-engine for that connection's lifetime. Reconnect re-fetches.

## Alternatives considered

- **Separate `encode(kind, params) -> bytes` tool + existing raw `send_mail`.** Rejected: two tool calls per mail doubles the round-trip cost and adds a correctness risk (encode with one kind, send under another).
- **Client-side kind library Claude references.** Ship a `.json` schema file in the repo; Claude reads it and encodes client-side. Rejected: duplicates the source of truth (substrate's registry is authoritative), goes stale on any kind change, and requires Claude to implement the POD layout rules itself for every session.
- **Schema-driven engine-side decode.** Have the hub forward `{kind, params_json}` opaquely and let the engine decode on receipt. Rejected for V0: every kind would need a JSON decoder in addition to its POD/postcard bytes contract, roughly doubling the per-kind boilerplate. The hub-side POD encoder is small and centralized. Revisitable if the hub grows feature creep.
- **Full WIT / component-model schemas over the wire.** Canonical cross-language answer. Rejected at V0 for the same reason ADR-0005 rejected it: heavy infrastructure for a problem that doesn't exist yet. The descriptor enum is a small subset that can be superseded by WIT later without changing the tool-level API.
- **Describe every kind, no opaque tier.** Forces every kind — including future postcard kinds with enums and collections — to be describable before it can be sent. Rejected: forecloses the opaque escape hatch and ties a per-kind authoring decision to hub capability. Keeping `payload_bytes` means structural kinds can ship the moment they're written, even before the hub learns them.
- **Collapse the descriptor into ADR-0005.** Fold into the mail typing system as a retroactive addition. Rejected: the two decisions have different scopes. ADR-0005 is about in-process Rust types and their serialization tiers; this ADR is about the external (hub → agent) boundary. Keeping them separate lets the hub descriptor evolve without touching the substrate's encode/decode plumbing.

## Follow-up work

- **`KindDescriptor` wire format in `aether-hub-protocol`** — enum + POD primitive set, serde-derived.
- **Engine-side descriptor emission** — substrate collects descriptors from `aether-substrate-mail` (a `describe()` associated on POD/signal kinds, or a manifest per crate) and ships them at `Hello`.
- **Hub POD encoder** — consumes `params: serde_json::Value` + descriptor, produces bytes matching Rust `#[repr(C)]` layout. Table-driven, one function.
- **`describe_kinds` tool** — read-only introspection.
- **`send_mail` becomes a list + best-effort** — schema change, per-mail status response.
- **Parked, not committed:** `Structural` descriptor variant for postcard kinds, nested-struct descriptors, cross-language descriptors (WIT), engine-side JSON decode. Each is additive and covered by the escape hatch.
