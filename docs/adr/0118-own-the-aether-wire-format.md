# ADR-0118: Own the Aether Wire Format and Drop the Postcard Dependency

- **Status:** Proposed
- **Date:** 2026-06-16

## Context

Aether's non-cast wire encoding is postcard's format (ADR-0019: a kind is
either cast-shaped `#[repr(C)]` bytes or postcard-shaped). Two separate pieces
of code produce and consume those bytes today, and only one of them is the
postcard crate:

- **The typed path** (`aether-data`): `#[derive(Kind)]` postcard-shaped kinds
  encode through `serde` + the postcard crate (`postcard::to_allocvec` /
  `postcard::from_bytes`, via the derive runtime's `encode_postcard` /
  `decode_postcard`). This is real postcard library code.
- **The schema-walker** (`aether-codec`): `encode_schema` / `decode_schema`
  translate agent JSON ↔ wire bytes at the MCP boundary, where no Rust types are
  available — only a `SchemaType`. This path is **hand-rolled**:
  `write_varint_u64`, `zigzag_i64`, hand-written enum discriminants and
  length prefixes. It calls no postcard library code in production. Its only
  relationship to the postcard crate is a `#[cfg(test)]` conformance oracle that
  asserts its hand-rolled bytes match `postcard::to_allocvec` for equivalent
  values.

The length-prefix stream framing (ADR-0072) and `aether-data` additionally
surface `postcard::Error` in their public error types, and `take_from_bytes`
(decode-and-return-remainder) is used by the kind-manifest parser and the
canonical decoders.

This split is the problem. Half the codec already reimplements the format by
hand; that half draws no benefit from the postcard crate's tested code — it is
our code, with our bugs, checked against postcard only in tests. Meanwhile the
postcard crate's types and traits (`postcard::Error`, the `Serializer` it
implements) sit in scope across the workspace next to that hand-rolled half,
which is a standing source of confusion: a reader cannot tell from a call site
whether "the format" means the library or the hand-rolled twin. Depending on a
crate to gain "ecosystem" and "fuzzing" benefits does not hold when half the
codec never runs that crate's code. The format is effectively already ours; the
dependency mostly buys confusion.

`aether-data` is `#![no_std]` + `alloc`, and the hand-rolled walker already
proves the byte primitives are trivial `no_std` code — so owning the encoder is
not gated on `std`.

## Decision

Own the wire format end to end and remove the postcard crate from the workspace.

1. **The aether wire format is ours.** We keep postcard's byte layout (varint /
   zigzag / length-prefix / enum-discriminant) — copying the format spec is
   fine and keeps the wire bytes unchanged — and publish it as the aether wire
   format, with a single reference implementation we own.

2. **`aether_data::wire` is that reference implementation** — the byte
   primitives plus `to_vec` / `from_bytes` / `take_from_bytes` and a
   `wire::Error`. It lives in `aether-data` because the typed derive runtime
   calls into it and `aether-codec` depends on `aether-data` (never the
   reverse), so this is the only placement that avoids a dependency cycle and
   keeps the format reachable from the `no_std` foundation.

3. **The format has two consumers, both ours, over the one `wire` module:**
   - A **serde adapter** (`Serializer` / `Deserializer` implemented over the
     `wire` primitives) so any `#[derive(Serialize)]` kind encodes in our format
     with no per-type hand-coding. serde is plumbing that *utilizes* our format;
     it is not the format's identity, and the format owes nothing to serde.
   - The **schema-walker** (`aether-codec`), unchanged in role, now calling the
     shared `aether_data::wire` primitives instead of hand-maintaining its own
     copy.

4. **The postcard crate is removed from every `Cargo.toml`.** Every
   `postcard::{to_allocvec, from_bytes, take_from_bytes, Error}` call site moves
   to `aether_data::wire` (or, for kinds, to the `Kind` trait methods
   `encode_into_bytes` / `decode_from_bytes` that wrap it). Once the dependency
   is gone, `postcard::` is a compile error workspace-wide — no lint needed.

The wire bytes do not change, so this is an internal-implementation decision,
not a wire-format break.

## Consequences

**Positive**

- One source of truth for the byte format. The byte primitives are written
  once in `aether_data::wire` and shared by both consumers, instead of a
  library copy and a hand-rolled copy kept in sync by an oracle.
- The postcard crate's types and traits leave the workspace, so a call site can
  no longer be ambiguous about which "format" it means.
- Full ownership of evolution: changing the format (e.g. a future version tag)
  is a change to one module we control, not a negotiation with an external
  crate's spec.
- `postcard::` becoming an unresolved path makes the boundary self-enforcing —
  the clippy `disallowed-methods` ban that was otherwise needed is moot.

**Negative**

- We own correctness. The serde adapter and the schema-walker are two
  traversals that must emit byte-identical output for the same logical value,
  so a conformance suite cross-checking the two consumers (plus golden
  byte-vector fixtures) replaces the former postcard oracle. Serialization bugs
  are the silent-corruption kind, so this suite is load-bearing.
- The serde adapter is real work — a clean-room `Serializer` / `Deserializer`
  over the wire primitives. postcard's published format is the reference for the
  byte layout; the implementation is ours.
- Migration touches every crate that names postcard (~75 files) and every
  `Cargo.toml` that depends on it. It lands as a sequenced arc, not one change.

**Neutral**

- No wire-format break: the bytes are unchanged, so engines, handle-store
  snapshots, and stored mail stay readable across the switch.
- serde stays a workspace dependency — it already backs the JSON side of
  `aether-codec`, and kinds keep `#[derive(Serialize, Deserialize)]`. This
  decision changes who implements the *wire* format under serde, not whether
  serde is used.
- Cross-language interop is unaffected in practice: the bytes remain
  postcard-compatible, so a non-Rust participant can still use any postcard
  library against them (parked per ADR-0005 / ADR-0007 regardless).

### Migration arc

1. Build `aether_data::wire`: primitives, `to_vec` / `from_bytes` /
   `take_from_bytes`, `wire::Error`, the serde adapter, and the conformance
   suite (golden fixtures + serde-adapter-vs-schema-walker cross-check).
2. Switch the `aether-data` derive runtime and `aether-codec` schema-walker onto
   `aether_data::wire`; drop their postcard dependency.
3. Migrate the remaining call sites per crate to the `wire` API or the `Kind`
   trait methods (kinds first — the bulk are roundtrip tests already moving to
   the trait methods).
4. Remove postcard from every `Cargo.toml`.

## Alternatives considered

- **Keep the postcard dependency, enforce the boundary with a clippy
  `disallowed-methods` ban.** Rejected: it leaves the confusing dual
  implementation in place (library copy + hand-rolled copy) and keeps postcard's
  types in scope — it polices the smell instead of removing it.
- **Drop serde on the wire path too; have the derive emit field-by-field encode
  /decode directly.** Rejected: larger macro work (enums, `Option`, `Vec`,
  `String`, nesting all hand-emitted) for no gain on the stated problem — serde
  is not the source of the confusion, the postcard crate is. serde as an adapter
  keeps the derive ergonomics.
- **Define a genuinely different wire layout (deliberately break
  postcard-compatibility).** Rejected: it buys nothing the ownership decision
  doesn't already give us, and it forces a wire-format break with no
  corresponding benefit.
