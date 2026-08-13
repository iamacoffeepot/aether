# ADR-0188: The wire codec derives from Schema

- **Status:** Proposed
- **Date:** 2026-08-13

## Context

The workspace owns its wire format (ADR-0118), but one format has two independent drivers. The typed path — `wire::to_vec` / `wire::from_bytes` — walks a Rust value through its serde `Serialize` / `Deserialize` impls. The schema path — `encode_schema` / `decode_schema` (aether-codec) — walks a JSON value through a `SchemaType`, with no Rust type in sight; the MCP mail surface rides this path end to end, so JSON interop for kinds already needs nothing from serde. Both drivers must produce identical bytes for the same logical value: every content address is a sha256 over canonical wire bytes, sealed configuration is authored on the schema path and re-derived on the typed path, and the two addresses must coincide. That equivalence is load-bearing and, until #4922, was pinned by nothing; #4922 pins it with fixture tests — the strongest enforcement available to a promise the type system does not hold.

The deeper mismatch is one of data models. Serde's model is maximal by design: it exists to encode arbitrary Rust, `Rc` / `Arc` included behind its `rc` feature gate. Aether's vocabulary is deliberately closed — the format was built for packets, files, and sealed rows, and the `Schema` bound already refuses unrepresentable fields at compile time. The stack therefore has its constraint layer (`Schema`) sitting on top of an encoder (`serde`) that does not share the constraint: the encoder can walk shapes the vocabulary cannot state, and only convention keeps the two aligned. The equivalence tripwires police a gap that exists because the encoder is more general than the language it encodes.

## Decision

The `Schema` derive emits the wire codec, and serde leaves the data plane.

1. **One origin, two artifacts.** The derive in `aether-data-derive` already turns a field list into a `SchemaType`. It additionally emits owned encode/decode impls that walk the same field list in declaration order to exactly the bytes the schema states. The schema value and the codec are two renderings of one source, so driver agreement stops being a tested promise and becomes agreement by common origin.
2. **Byte identity is the migration contract.** The derived codec produces byte-identical output to the serde-driven bytes for every schema-expressible value. No content address moves. The #4922 equivalence fixtures are the proof harness for the swap and remain afterward as regression tripwires.
3. **Serde exits the data plane.** Kind crates and value vocabularies drop `Serialize, Deserialize` from their derive lists; `wire::to_vec` / `from_bytes` call sites move to the owned traits. Serde remains only where wire bytes are never produced — host configuration file parsing and harness tool envelopes — so no dual encoding of addressed bytes survives anywhere.
4. **Borrowed decode is kept.** `wire::from_bytes` is `Deserialize<'a>` today and the vocabulary admits exactly two borrowable leaves (string and byte-slice fields). The owned decode trait carries the same lifetime and borrows those leaves from the input buffer; everything else decodes owned. The closed vocabulary is what makes this cheap — borrowing is a per-leaf decision made once in the derive, not a per-type negotiation.
5. **The schema path stays.** `encode_schema` / `decode_schema` remain the dynamic driver for schema-directed reads — the MCP front, and ADR-0187's value-form read tier, which decodes historical bytes through their recorded schema with no Rust type at all. A test states the common-origin invariant: for every kind, the derived codec and the schema path agree on a representative fixture.

## Consequences

- The #4922 hazard class — silent divergence between drivers — becomes unrepresentable for the typed path: an encoder change is a derive change, which changes the schema and the codec together. The fixtures downgrade from load-bearing to tripwire.
- Compile-time refusal gains a second layer that agrees with the first by construction: a field outside the vocabulary (an `Arc`, a foreign map type) fails one bound and produces no codec, instead of failing `Schema` while serde would happily encode it.
- Costs, accepted: owned derive machinery to maintain in `aether-data-derive`; a workspace-wide, mechanical derive-list migration; the loss of serde's representation conveniences (enum tagging knobs, field defaults) in the data plane — which the closed vocabulary defines for itself, and which is the point.
- Sequencing: after #4922's fixtures are in the base (they gate the swap) and after ADR-0187's schema stamping lands (drift during the migration is then detectable by mechanism, not forensics). Implementation is #4928.
- Non-goals: JSON and TOML edges keep serde; nothing changes for the tool envelope or host config parsing.

## Alternatives considered

- **Keep serde plus tripwires, permanently.** The status quo after #4922. Workable, but it holds a structural invariant by test and review prose forever, and the maximal encoder stays beneath a closed vocabulary — every future contributor can reach for a serde feature the format cannot express and learn it from a failing fixture rather than from the compiler.
- **A restricting serde `Serializer`** that errors on out-of-vocabulary shapes at encode time. Narrows the gap but keeps two walkers, keeps the equivalence tested rather than structural, and moves refusal from compile time to runtime — the wrong direction for a format whose encoding is identity.
- **Drop the typed path; schema-walk everything.** One driver, maximal uniformity, but every hot-path encode interprets a `SchemaType` at runtime, and shape errors surface at walk time instead of compile time. The derived codec keeps one driver's semantics with the typed path's cost profile.
- **A self-describing wire format.** Rejected in ADR-0187 for the same reason it is rejected here: every existing digest moves, which is the one cost this design is built never to pay.
