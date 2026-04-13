# ADR-0005: Mail typing system

- **Status:** Accepted
- **Date:** 2026-04-13

## Context

The substrate's mail transport (envelope: `{recipient, kind, payload, count}`) was deliberately left untyped through the milestone-1/2/3 arc. Kinds are bare `u32` constants duplicated between substrate and component; payload byte layout is an implicit contract between sender and receiver; mismatches silently corrupt or crash. `mail.rs` carries the comment *"typed facade over this is deferred to a later milestone per issue #18."* This ADR picks that deferred work up.

Forces at play:

- **Both sides are Rust today.** Substrate is native Rust; components compile to `wasm32-unknown-unknown` from Rust. No non-Rust components exist or are planned near-term. Cross-language components are hypothetical and paying a cross-language tax (IDL, codegen, varint encoding) for them now would be speculative per ADR-0002's "don't over-architect" posture.
- **Payloads are not uniform in shape.** Two distinct payload populations are already visible and will only diverge further:
  - *Structural / control* messages (Tick, Key, MouseMove, component-to-component commands): small, sparse, 10s of bytes, field-level structure matters, ergonomics dominate throughput concerns.
  - *Bulk / POD arrays* (vertex streams, instance data, future audio buffers): kilobytes to megabytes per frame, array-of-struct, throughput dominates ergonomics. Per-field encoding overhead (protobuf-style varints, tag bytes) would be catastrophic here.
  A single format optimized for the first tier hurts the second; optimized for the second, it loses ergonomics on the first.
- **Ownership is asymmetric.** The substrate defines a fixed vocabulary (tick, input, draw) that every component may receive or send. Components define their own vocabularies that other actors subscribe to — this is the cross-actor mail case, not yet realized but forecast as soon as a second component exists.
- **Kind ids cannot be centrally assigned.** If each actor owns its kinds, a `u32` kind number can collide. We already have a precedent for registry-at-init name resolution (`Registry` maps mailbox names to `MailboxId`s at substrate boot); the same shape fits kinds.
- **No pressure exists today to serialize cross-language or over the wire.** Mail stays in-process, crossing only the WASM/native linear-memory boundary, where byte layout is the only thing that matters.

## Decision

Adopt a **Rust-types-as-schema, per-actor-owned mail** model with a two-tier payload contract and registry-at-init kind resolution.

### 1. Schema is the Rust type

Each kind is represented by a Rust type. The type *is* the schema — no IDL, no separate descriptor file. Both sides of a mail exchange compile against the same type definition (imported from a shared crate), so schema drift is a compile error, not a runtime corruption.

### 2. `MailBody` trait with two-tier implementation

A trait in a new `aether-mail` crate defines the encode/decode contract:

```rust
pub trait MailBody: Sized {
    fn encode(&self, out: &mut Vec<u8>);
    fn decode(bytes: &[u8]) -> Result<Self, DecodeError>;
}
```

Two canonical implementation paths:

- **POD tier (bytemuck).** For `#[repr(C)]` types containing only POD fields. Encode is `extend_from_slice(bytemuck::bytes_of(self))`; decode is a bounds-checked `bytemuck::try_from_bytes`. Zero-copy on both sides, zero per-field overhead. This is the tier used for bulk data (vertex streams, instance arrays) and for any small fixed-layout struct where the overhead of a serialization library is unwanted. Use `MailBody for [T]` blanket for arrays.
- **Structural tier (postcard).** For types with `Option`, `Vec`, enum variants, or other non-POD shapes. Derives `serde::Serialize/Deserialize`; the trait impl calls `postcard::to_extend` / `postcard::from_bytes`. Compact varint encoding, Rust-native, `no_std`-friendly (matters for WASM guests). Owned decode — not suitable for large buffers.

An actor chooses the tier per type. A type does not support both — the choice is part of the type's contract.

### 3. Per-actor-owned mail crates

The mail vocabulary is distributed across crates, one per actor that defines mail:

- `aether-mail` — the trait and shared machinery. No concrete kinds.
- `aether-substrate-mail` — substrate-owned kinds (`Tick`, `Key`, `MouseButton`, `MouseMove`, `DrawTriangle`). Everything the substrate sends or exposes as a sink vocabulary.
- `{component}-mail` — each component's own kinds, created when the component first defines one. No such crate exists today; hello-component only consumes substrate kinds.

An actor that wants to send mail to actor X depends on `X-mail`. There is **no central catalog** — no `aether-mail::AllKinds` enum, no global registry. This avoids the protobuf failure mode where adding a kind requires editing a shared schema file every actor rebuilds against.

### 4. Kind-name registry at init

Kind ids (`u32`) are assigned at substrate boot, not hardcoded. Each actor's mail crate exports a `KINDS` manifest of namespaced string names (e.g., `"aether.tick"`, `"hello.npc_health"`); the substrate registers them alongside mailbox names and assigns dense sequential ids. The assigned ids are handed back to components at init via a yet-to-be-designed bootstrap call, so guests can cache them.

This mirrors the existing `Registry` flow for mailbox names and inherits the same properties: namespaced names prevent collisions without coordination, dense `u32` ids stay cheap in the hot path, crash traces show human-readable names at the boundary.

## Consequences

### Positive

- **Schema drift is a compile error.** Sender and receiver share the Rust type; mismatched field layout, missing variant, or wrong size fails at `cargo build`, not at a runtime stack smash.
- **POD tier keeps draw calls cheap.** `bytes_of` and `cast_slice` are zero-cost in release builds. Vertex streams pay nothing for schema ownership beyond what they already pay for byte-level routing. This is the load-bearing performance argument.
- **Structural tier keeps small messages ergonomic.** Serde derives, postcard encoding, no manual offset math. Adding a new control message is a struct definition plus a `derive`.
- **Decentralized vocabulary matches the actor model.** Adding a kind to an actor doesn't churn any other actor's crate. Consumers opt in by importing the actor's mail crate.
- **Registry-at-init extends the existing mailbox pattern.** One registration mechanism, two tables (mailboxes and kinds). Reuses the namespacing discipline already in use.
- **Cross-language is not foreclosed.** If a non-Rust component arrives later, the WASM component model / WIT can be layered on as a second serialization tier (or replace postcard for structural types) without rewriting the POD path.

### Negative

- **Two tiers is a design choice each author makes.** Picking POD vs structural requires understanding the tradeoff (zero-copy + rigid layout vs ergonomic + owned decode). A style guide in `aether-mail`'s docs will need to exist.
- **Per-actor crates inflate the workspace.** Every component that defines mail gets a `{component}-mail` sibling crate. Acceptable cost for the isolation, but workspace-level clutter grows linearly with the actor count.
- **Registry-at-init adds a bootstrap step.** Components must fetch assigned kind ids before their first send. The existing mailbox-resolution bootstrap (still TBD, per the `MailboxId(0)`/`MailboxId(1)` hardcoding in main) will need to be designed in lockstep.
- **Postcard is a new top-level dependency.** One more crate to keep current. Mitigated by postcard's small surface and Rust-wide adoption.

### Neutral

- **Cross-language is not solved, deliberately.** When a non-Rust component becomes concrete, this ADR is superseded or amended. Until then, it stays out of scope.
- **Actor-defined kinds have no implementation yet.** The machinery supports them; the first real test arrives with the second component or an MCP-server sender.

## Alternatives considered

- **Keep bare `u32` + untyped `Vec<u8>`.** Zero machinery, maximum flexibility, zero safety. Current state. Rejected because the first structural payload (milestone 4's `Tick { elapsed: f32 }`) is enough to justify a framed answer, and retrofitting the shape is cheaper before more kinds exist.
- **Single shared enum of all kinds in `aether-mail`.** Centralizes vocabulary. Rejected because it couples every actor to every other actor's mail and makes adding a kind a workspace-level change — the opposite of the actor-isolated model.
- **Protobuf for everything.** Cross-language, industry-standard, battle-tested. Rejected because varint-per-field encoding tanks vertex-stream throughput and cross-language is not a forcing requirement today. Revisitable if the premise changes.
- **Flatbuffers or Cap'n Proto everywhere.** Zero-copy, schema-driven, cross-language. Closer to acceptable than protobuf. Rejected because it requires a codegen step in the build for a benefit (cross-language, self-describing bytes) that isn't paid for today, and `#[repr(C)]` + bytemuck beats its per-field vtable lookup on dense numeric arrays.
- **Full WIT / WASM component model.** The canonical cross-language answer. Rejected for now because it's heavy infrastructure (wasmtime component-model support, wit-bindgen) for a problem that doesn't exist yet. Kept on the table as the migration path if cross-language components become real.
- **Static kind-id ranges per actor (substrate gets 0–999, each component gets a block).** Avoids collisions without a registry. Rejected as brittle: adding a new actor requires human coordination, and the fixed blocks become incorrect the moment two components are developed in parallel.
- **Hash-based kind ids (`fnv("hello.npc_health")`).** Collision-resistant statistically, no coordination. Rejected because debugging and crash traces read like UUIDs — the registry-at-init approach produces the same namespaced-name safety with human-readable ids at the boundary.

## Follow-up work

- **`aether-mail` crate** with `MailBody` trait, bytemuck POD helper, postcard structural helper, unit tests.
- **`aether-substrate-mail` crate** with the five substrate kinds migrated to typed structs. Substrate emits via the types.
- **Kind-name registry** in substrate, parallel to the existing mailbox registry. Component bootstrap that resolves kind names to ids at init.
- **Component-side typed decode** — `receive` dispatches via `MailBody::decode` rather than a raw `u32` match.
- **A `derive(MailBody)` macro** — optional, post-V1; the first implementors can be hand-written to validate trait ergonomics before committing to a macro shape.

None of the follow-up work commits to actor-defined kinds landing in a specific milestone; that pressure arrives with the second component or the MCP server, whichever comes first.
