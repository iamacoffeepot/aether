# ADR-0187: Sealed bytes record their writing schema

- **Status:** Proposed
- **Date:** 2026-08-13

## Context

The bloomery persists bytes in three places whose meaning depends on a schema that is not stored with them. The journal holds wire-encoded events that boot replay decodes with the current binary's types, and a record that fails to decode is a fatal abort — the coordinator refuses to start. The config store holds sealed configuration values as `(kind, bytes)` rows, decoded through the current descriptor inventory at the point of use. And every content address is a sha256 over a value's canonical wire bytes (ADR-0118) behind a domain tag (ADR-0149), so the encoding is identity, not merely transport.

The wire format is positional and untagged by design: canonical bytes are what make content addressing sound, and a tagged self-describing format would trade that away. The cost of that choice is that the bytes alone cannot say what shape wrote them. Today the schema that gives persisted bytes their meaning exists only inside whichever binary happens to be running, which makes every schema change to a persisted type one of three hazards: a journal that no longer replays (fatal abort at boot), sealed rows that no longer decode (a registry address that resolves to bytes nothing can read), or a digest that silently moves (the same logical value re-addressed under a changed encoding).

The pressure is not hypothetical. #4909 extends the price row and must promise, in its own issue text, that "previously sealed tables decode unchanged — the field is additive." #4923 rekeys the price table and must state a decode break as an accepted cost. Additive-first discipline keeps individual changes cheap, but a positional untagged format supports only a narrow additive window (trailing optional fields), and the discipline itself is enforced by review prose — the weakest rung of the enforcement ladder. As sealed history grows, every non-additive change gets more expensive, and the system's own history is the asset the journal exists to protect.

One fact makes a better answer available here than in most event-sourced systems: the schema is already a runtime value. `SchemaType` travels in the descriptor inventory of every native binary, and `encode_schema` / `decode_schema` (aether-codec) already drive encoding from a schema value rather than from a Rust type. Nothing about decoding old bytes requires the old binary — it requires the old schema, and the vocabulary for holding and using schemas as data already exists.

## Decision

Bytes never persist without the schema that wrote them, and schemas are content-addressed values like everything else.

1. **Schemas enter the store.** A `SchemaType` is encoded and stored content-addressed, deduplicated by digest exactly as sealed configuration values are. Schema digests are computed once per kind per binary and change only when the shape actually changes.
2. **Persisted rows name their writing schema.** A config-store row becomes `(kind, schema_digest, bytes)`. Journal records carry the same association at whatever granularity keeps the overhead trivial — per record, or per segment written by one binary, since the schema set changes only when the binary does.
3. **Reads decode with the writer's schema.** When the recorded schema digest equals the current kind's schema digest, decoding proceeds exactly as today — the fast path is the common path, and it costs one digest comparison. When they differ, the bytes decode through the *recorded* schema into value form, and a registered **upcast** — a value-level transform from the written shape to the current shape — carries the result forward. Upcast chains compose across successive shape changes.
4. **The value form is a read tier of its own.** An upcast is owed only to readers that need the current typed shape — journal replay folding into the snapshot, config resolution into the running binary's types. A reader that needs only to understand the bytes — forensics, the calibration ledger, receipt audits, supersede archaeology — stops at the schema-driven value `decode_schema` produces from the recorded schema, which involves no Rust type and no upcast. Those reads never refuse on shape drift, including for shapes the current binary no longer has a type for.
5. **A missing upcast is a loud, named refusal** — "no migration from schema `X` to current `Y` for kind `K`" — at the read that needed the current shape. Replay aborts only on genuine corruption or a genuinely missing migration, never on shape drift alone, and never for a value-form read.
6. **Sealed history is never rewritten.** A digest continues to attest exactly the bytes that were sealed, now alongside the schema they were written under — strictly more attestation than today, since the receipt names not only what was sealed but what it meant at sealing time. Content addresses do not change: the schema digest is a stored association, not an input to the address.

A schema change to a persisted type therefore becomes an explicit, testable obligation — ship the shape change together with its upcast and a fixture proving old bytes still read — rather than a hazard discovered at the next boot or the next forensic read.

## Consequences

- Journal replay survives shape changes. The fatal-abort path narrows to corruption and missing migrations, both of which name themselves.
- Sealed configuration remains readable for the life of the store, and historical forensics (the calibration ledger, receipt audits, supersede archaeology) read old blooms without keeping old binaries around.
- Additive-first stays the preferred discipline — an additive change needs no upcast and the fast path never notices — but non-additive changes stop being breakage and start being priced work. #4923 becomes the first customer.
- The enforcement moves down the ladder from review prose to mechanism: a missing upcast is a failing read with a name, and the fixture corpus (old bytes, current binary, expected value) is the tripwire that catches an encoding drift nobody meant to ship.
- Costs, accepted: schema blobs in the store (small, deduplicated, one per shape per kind); an upcast registry to maintain, which grows only when shapes actually change; and one digest comparison on the read path.
- Follow-on work: the store schema-column migration itself (the one bootstrap step this decision cannot retroactively cover — rows written before it carry an implicit "current at migration time" schema and are stamped as such); wiring the refusal into replay and config resolution; the fixture corpus for existing sealed kinds.

## Alternatives considered

- **Additive-only discipline, forever.** Already the working practice, and it stays; alone it is prose-enforced, limited by the positional format to trailing optional fields, and it forecloses shape corrections like #4923 permanently — the store would ossify around every early mistake.
- **Version tags inside each value.** An envelope or version field per type pushes migration bookkeeping into the value vocabulary itself, leaks versions into every type, still requires the same upcasts, and says less than the actual schema — the writer's schema *is* the version, with structure.
- **A self-describing wire format.** Tagged fields (protobuf-style) would let old binaries' bytes tolerate unknown shapes, but the encoding stops being canonical-compact, and every existing digest moves — the one cost this decision is designed never to pay.
- **Snapshot and archive old journals.** Bounds how far back replay must decode, and the daily roll is a natural boundary for it; it complements this decision but does not answer sealed-config reads or forensics, so it is a companion, not a substitute.
