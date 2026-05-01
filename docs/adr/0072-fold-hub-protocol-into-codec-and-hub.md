# ADR-0072: Fold aether-hub-protocol into aether-codec and aether-hub

- **Status:** Accepted
- **Date:** 2026-05-01

## Context

`aether-hub-protocol` reached this ADR carrying ~600 LOC across two
populations:

1. **Generic stream framing** (lib.rs, ~350 lines): `encode_frame`,
   `read_frame`, `write_frame`, `MAX_FRAME_SIZE`, `FrameError` —
   length-prefixed postcard helpers parameterised over
   `<T: Serialize>` / `<T: DeserializeOwned>`. Nothing hub-specific in
   them.
2. **Hub channel vocabulary** (types.rs, ~250 lines): `EngineToHub`,
   `HubToEngine`, `Hello`, `Welcome`, `MailFrame`, `EngineMailFrame`,
   `ClaudeAddress`, `Goodbye`, `LogEntry` / `LogLevel`,
   `EngineMailToHubSubstrateFrame`, `MailToEngineMailboxFrame`,
   `MailByIdFrame`. These describe what the substrate ↔ hub TCP
   channel exchanges. They use serde-derive postcard, not
   schema-driven encoding.

ADR-0069 introduced the four-crate infrastructure split (`aether-data`,
`aether-codec`, `aether-hub-protocol`, `aether-kinds`). That ADR
deliberately kept `aether-hub-protocol` as a separate crate, citing
two reasons:

- The role boundary between "wire frames + framing" and "everything
  else hub does" was real.
- A future second mail transport (peer-to-peer, in-process bridge,
  unix-socket) would land as a sibling crate to `aether-hub-protocol`,
  reusing `aether-data` for envelope vocabulary.

ADR-0071 phase 7 then folded the substrate-side hub client and the
hub coordinator itself into a new `aether-hub` crate. ADR-0071 phase
7c moved the identity types (`EngineId`, `SessionToken`, `Uuid`) into
`aether-data` so substrate-core could drop its `aether-hub-protocol`
dep and the ADR-0070 invariant ("substrate-core has zero hub
knowledge") could close.

The result, after ADR-0071, was that every consumer of
`aether-hub-protocol` either *was* `aether-hub` or *also depended on*
`aether-hub` — `aether-substrate-test-bench` (loopback driver),
`aether-substrate-desktop` (one integration test), the hub coordinator
itself. No consumer wanted the wire crate without also wanting the
runtime that speaks it. Three more crates (`aether-component`,
`aether-scenario`, `aether-substrate-headless`) carried vestigial
`aether-hub-protocol` Cargo.toml deps with zero source uses, drifted
in via the ADR-0069 split.

The ADR-0069 reservation for "future sibling transport" remained
hypothetical with no concrete on-roadmap forcing function. Six months
of post-ADR-0069 evolution produced exactly one mail transport (the
hub TCP channel); the only candidate sibling architecture (peer-to-peer
between substrates) is parked indefinitely behind several earlier
prerequisites.

A structural review of the post-phase-7 dep graph surfaced this gap
between the ADR-0069 vision (independent wire crate, ready for
siblings) and the actual usage shape (one runtime, one wire, both
always pulled together).

## Decision

Fold `aether-hub-protocol` into two existing crates along the
mechanism / vocabulary seam:

1. **Generic framing helpers → `aether-codec::frame`.** The framing
   primitives (`encode_frame`, `read_frame`, `write_frame`,
   `FrameError`, `MAX_FRAME_SIZE`) are generic over `<T: Serialize>` and
   not hub-specific. They land alongside the schema-driven
   encode/decode functions as `aether_codec::frame`. Codec's scope
   expands from "schema-driven JSON ↔ wire bytes" to "the byte-encoding
   toolkit": schema-driven encode/decode for kind payloads, plus
   stream framing primitives for any postcard-derived enum. The
   crate-level docs are updated to reflect both layers.

2. **Hub wire vocabulary → `aether-hub::wire`.** The frame enum types
   move into a `wire` module under `aether-hub`. The runtime that
   speaks the protocol owns the vocabulary that describes it. Internal
   hub modules switch to `crate::wire::*`; external consumers use
   `aether_hub::wire::*`.

3. **Delete the `aether-hub-protocol` crate.** Workspace member entry
   removed; the package directory is git-removed. The three vestigial
   Cargo.toml deps (`aether-component`, `aether-scenario`,
   `aether-substrate-headless`) are pruned in the same PR.

A future sibling transport (peer-to-peer, unix-socket, browser
WebSocket) under this ADR's framing depends on `aether-codec` for the
generic length-prefix helpers and defines its own frame-enum module
inside its own crate — same shape as `aether-hub::wire`, just for a
different wire. The ADR-0069 vision of "siblings of
`aether-hub-protocol`" reframes as "siblings of `aether-hub`'s wire
module", which is structurally cleaner: the wire and the runtime that
speaks it stay co-located, and the codec primitives are shared.

A future second body format (msgpack, protobuf) under this ADR's
codec scope grows `aether-codec` along the format axis. The current
helpers hardcode postcard inside; a second format will subdivide
`frame` into `frame::postcard` / `frame::protobuf` siblings rather
than parameterising the existing helpers — most callers know which
format their protocol speaks at compile time.

This reverses the ADR-0069 reservation that kept
`aether-hub-protocol` as a separate crate "for future sibling
transport reasons." The reversal is documented here rather than as
an edit to ADR-0069 — that ADR's other three pillars (`aether-data`,
`aether-codec`, `aether-kinds`) remain load-bearing.

## Consequences

**Positive**

- One fewer crate in the infrastructure cluster.
- The wire vocabulary lives next to the runtime that speaks it.
  Reading an `EngineToHub` variant and tracing it into the
  coordinator's match arms is one crate hop instead of two.
- `aether-codec` becomes the obvious home for any generic encoding
  primitive — future framing variants, future formats, future
  save-format adapters all land there as siblings of today's modules.
- The three vestigial Cargo.toml deps fall out as a cleanup tax
  rather than as silent dep-graph noise.
- Future sibling transports (peer-to-peer, unix-socket) become
  cleaner under this layout: their wire module sits in their crate,
  not as a sibling of an empty hub-protocol crate.

**Negative**

- Reverses an ADR-0069 decision that was load-bearing at the time.
  Future readers need to read both ADRs to understand the current
  shape. This ADR explicitly notes the reversal.
- `aether-codec`'s scope widens. Callers that want only the
  schema-driven path now pull in postcard transitively for the
  framing helpers. (Postcard was already a transitive dep through
  `aether-data`, so the actual surface delta is small.)
- A future second mail transport that wants to share the framing
  primitives without taking an `aether-codec` dep can't — the
  primitives only ship from `aether-codec` now. The simplest fix
  if that ever matters is a tiny `aether-frame` extraction; today
  the case is hypothetical.

**Neutral**

- Wire format unchanged. Every byte boundary holds — `EngineToHub`'s
  postcard encoding, the 4-byte LE length prefix, the
  `MAX_FRAME_SIZE` cap, and `FrameError`'s variants are all
  preserved verbatim across the move.
- ADR-0069's other three crates (`aether-data`, `aether-codec`,
  `aether-kinds`) are unchanged in role; only `aether-hub-protocol`
  retires.
- ADR-0071's "substrate-core has zero hub knowledge" invariant
  survives — substrate-core didn't depend on `aether-hub-protocol`
  after phase 7c, and it doesn't depend on `aether-codec::frame`
  now either. The hub egress trait (`EgressBackend`) keeps
  substrate-core wire-agnostic.

**Follow-on work**

- None required. The fold is structural cleanup; no behaviour
  changes.
- If a second body format (msgpack, protobuf) lands later,
  subdivide `aether-codec::frame` into per-format submodules at
  that point.
- If a sibling stream protocol (peer-to-peer, unix-socket) lands
  later, define its frame-enum module inside its own crate
  alongside `aether-hub::wire`'s shape.

## Alternatives considered

- **Keep `aether-hub-protocol` separate (status quo).** The
  ADR-0069 position. Rejected — six months of evolution produced
  no sibling transport, every consumer of the wire crate also
  pulls the hub crate, and the role boundary is preserved better
  by splitting the crate's two populations than by leaving them
  bundled.
- **Fold all of `aether-hub-protocol` into `aether-hub`.** Single
  destination, simpler than the split fold. Rejected — the framing
  helpers are genuinely codec-shaped (generic over `<T: Serialize>`,
  reusable by any postcard stream protocol). Putting them inside
  `aether-hub` would mean a future sibling transport either copies
  them or weirdly depends on `aether-hub` for non-hub-specific
  primitives.
- **Fold all of `aether-hub-protocol` into `aether-codec`.** Single
  destination, with codec absorbing both generic framing and the
  specific hub vocabulary. Rejected — `aether-codec` is the
  byte-encoding mechanism crate; adding one specific protocol's
  enum types alongside its generic schema-walking conflates
  mechanism and vocabulary. A future second protocol would not
  belong in codec either; codec would carry one specific
  protocol's frame types and not others, which is incoherent.
- **Extract framing into a new tiny crate (`aether-frame`).**
  Considered for the case where a future thin client wants framing
  without taking `aether-codec`'s deps. Rejected — the case is
  hypothetical, the helpers are ~50 lines, and `aether-codec`
  already carries postcard transitively via `aether-data`. A
  tiny-crate extraction is reversible if the case becomes real.
- **Edit ADR-0069 in-place to acknowledge the reversal.**
  Considered for keeping the historical record tidy. Rejected —
  ADRs are immutable records of decisions at a point in time.
  Superseding edits go in a follow-up ADR (this one) so the
  history is auditable.

## References

- ADR-0006 — engine ↔ hub TCP channel; the wire format this ADR
  relocates.
- ADR-0069 — data-layer split from mail transport; introduced
  `aether-hub-protocol` as a separate crate, partially reversed
  here for the framing/vocabulary halves.
- ADR-0070 — native capabilities and chassis-as-builder; the
  invariant that substrate-core has zero hub knowledge survives
  this ADR unchanged.
- ADR-0071 — driver capabilities and chassis composition; phase 7
  shipped the `aether-hub` crate this ADR folds the wire
  vocabulary into.
