# ADR-0065: Typed Id Newtypes and First-Class Type Ids in the Schema

- **Status:** Proposed
- **Date:** 2026-04-28

## Context

ADR-0064 introduced a 4-bit type tag in the high bits of every id and a
deterministic `<prefix>-XXXX-XXXX-XXXX` string encoding for the MCP
boundary. Phase 1 (tag bits in `mailbox_id_from_name` and the `Kind::ID`
derive) shipped in PR #387; phase 2 (string-encoded `mailbox_id` on
`load_component` / `replace_component` / `describe_component`) shipped
in PR #388. That ADR scoped the conversion deliberately:

> Internal types stay `u64`. Conversion happens at the hub MCP
> serialiser/deserialiser, at the tracing `Display` impl for each id
> type, and nowhere else.

The boundary was the right v1 shape — phase 1 + 2 already touched the
canonical schema, the kind-id derive, every test that hard-codes an id
literal, and the MCP request/response types. Pulling typed newtypes
into the kinds crate at the same time would have made the diff
unreviewable.

The remaining gap is the structured `params` JSON nested inside
`send_mail` and `capture_frame.{mails,after_mails}`. Those carry mail
payloads encoded against a kind's schema; the hub's
`encoder.rs` / `decoder.rs` walks the schema to translate JSON ↔
postcard. Today, every id-bearing field in `aether-kinds` is a raw
`u64`, so the schema sees `Primitive::U64` and the codec writes /
reads a JSON number. Concrete worked example:

1. Agent calls `load_component`. Reply carries
   `{ "mailbox_id": "mbx-q3lr-bv2x-mtdr", ... }` (phase 2 wire).
2. Agent wants to subscribe the loaded component to `Tick` and sends:

   ```json
   {
     "engine_id": "...",
     "recipient_name": "aether.control",
     "kind_name": "aether.control.subscribe_input",
     "params": { "stream": "Tick", "mailbox": ??? }
   }
   ```

3. The schema for `SubscribeInput.mailbox` is `u64`. The codec accepts
   only a JSON number. The agent must either pass the raw 64-bit
   value (defeats phase 1's type guard *and* falls into the JSON 2^53
   precision hole — exactly the failure mode ADR-0064 was written to
   solve), or manually tag-decode the `mbx-...` string back to the raw
   `u64` before sending (re-implements the boundary the hub already
   owns). Both are wrong shapes.

The codec needs a schema-level signal that says *this field is a
mailbox id*. The natural signal is a new schema variant: a typed-scalar
node carrying the tag byte from ADR-0064.

A second forcing function lives next door. `aether-substrate-core`
already defines `MailboxId(u64)` (`crates/aether-substrate-core/src/mail.rs`)
as an opaque newtype with `from_name`, `NONE`, and a phase-2-aware
`Display` impl. But `aether-kinds` and `aether-hub-protocol` use raw
`u64` for every id field they expose. Two homes for what wants to be
one type. As the surface grows (every new id-bearing kind, every host
fn that touches an id), the cost of *not* unifying is paid per author,
per kind, per round of review.

ADR-0064 phase 2's PR body named this work as phase 3. This ADR is that
phase, scoped as its own decision because the schema vocabulary change
is load-bearing on its own — same scope of impact as ADR-0030's switch
to schema-hashed kind ids.

## Decision

Lift typed ids into the schema vocabulary as first-class types,
expose a stable `TYPE_ID` / `TYPE_NAME` pair on each typed wrapper,
and migrate every id-bearing field in `aether-kinds` to the new
types. Codec dispatch on the new schema variant is hard-coded in the
hub's encoder/decoder match arms — no new trait, no runtime
registry.

**1. New schema variant: `SchemaType::TypeId(u64)`.**

Add `SchemaType::TypeId(u64)` (and its cast-shape twin
`SchemaShape::TypeId(u64)`) to `aether-hub-protocol::types`. The `u64`
is the FNV-1a 64-bit hash of the type's canonical name with a disjoint
domain prefix (e.g. `TYPE_DOMAIN ++ "aether.mailbox_id"`), mirroring
ADR-0029/0030's domain discipline so type ids cannot collide with
mailbox ids or kind ids by construction. The canonical schema writer
emits a fresh constant `SCHEMA_TYPE_ID = 12` followed by the eight
id bytes; decode reverses.

`TypeId` is a peer to `Struct` and `Enum`, not a flavour of `Scalar`.
A field whose schema is `TypeId(id)` carries no implicit primitive
or layout — those come from the codec's per-id arm. Two distinct
typed fields (a `MailboxId` and a `KindId`) produce distinct
canonical schema bytes, and a kind that embeds a typed id gets a
`Kind::ID` that is sensitive to the typed identity, not just the
underlying `u64`-shape.

**2. Newtype family in `aether-mail`.**

Define `MailboxId(u64)`, `KindId(u64)`, `HandleId(u64)`. Each is:

- `#[repr(transparent)]` over a `u64` — postcard wire identical, no
  layout shift in cast-shape kinds.
- `bytemuck::Pod + Zeroable` — cast-shape kinds stay cast-able.
- Carries `pub const TYPE_ID: u64` (FNV-1a of canonical name with
  `TYPE_DOMAIN` prefix) and `pub const TYPE_NAME: &'static str`
  (e.g. `"aether.mailbox_id"`).
- Implements `Schema` returning `SchemaType::TypeId(Self::TYPE_ID)`.
- Implements `Display` rendering the tagged string form (mirroring
  the `MailboxId::Display` shipped in phase 2).
- Implements `serde::Serialize` / `serde::Deserialize` directly:
  serialize emits the tagged string form (`tagged_id::encode`);
  deserialize accepts a tagged string *or* a JSON number for the
  migration window (back-compat with existing test fixtures and
  examples that pass numbers).

No `WireType` trait. The shape "expose `TYPE_ID` + `TYPE_NAME` as
consts" is enough — the codec arms hand-code the JSON ↔ postcard
translation for each known id. If a future ADR adds enough typed
wrappers that hard-coded dispatch becomes unwieldy, the trait or
registry abstraction lands then with concrete pressure to justify
it.

**3. Hoist `MailboxId` into `aether-mail`.**

The current `aether_substrate_core::mail::MailboxId` moves to
`aether-mail` as the canonical home. `aether-substrate-core::mail`
re-exports it for back-compat with existing call sites (no per-call
migration in this step). `from_name`, `NONE`, and the existing
`Display` impl move with it.

`KindId` and `HandleId` are new — neither has a substrate-core
counterpart today.

**4. Migrate id-bearing fields in `aether-kinds`.**

Roughly twenty fields, sweep at a time. The user-visible win is
`SubscribeInput.mailbox: u64 → MailboxId`, which closes the loop on
the worked example above. The remaining sites are mechanical:
`UnsubscribeInput`, `LoadResult`, `UnresolvedMail.{recipient_mailbox_id,
kind_id}`, the `HandlePublish` / `HandlePin` / `HandleUnpin` /
`HandleRelease` request and `*Result` variants, `Ref::Handle.{id,
kind_id}`, and the few less-obvious id fields scattered through the
crate's mail definitions.

Every kind whose schema mentions a typed id gets new canonical bytes
(the `Scalar(U64)` node becomes `TypeId(...)`), so every one's
`Kind::ID` shifts. No ids are persisted to disk; the build fails
loudly at every test literal site — same migration discipline as
ADR-0030 and ADR-0064 phase 1.

**5. Codec dispatch — hard-coded match arms.**

`aether-substrate-hub::encoder` and `decoder` grow one new arm
matching the shape of every other arm in those files: pattern-match
the `SchemaType::TypeId(id)` variant, then match on the `id` value
to inline the byte-level logic.

```rust
SchemaType::TypeId(id) => match *id {
    MailboxId::TYPE_ID | KindId::TYPE_ID | HandleId::TYPE_ID => {
        // JSON in: read tagged string (or number, back-compat),
        //   decode_with_tag against the appropriate Tag, write u64
        //   varint to postcard.
        // JSON out: read u64 varint from postcard, encode to tagged
        //   string, emit JSON string.
    }
    other => return Err(EncodeError::UnknownTypeId(other)),
},
```

The three id types share one body — every one is a u64 varint on
the postcard side and a tagged string on the JSON side; only the
expected `Tag` differs. The walker
(`handle_store::skip_primitive_postcard`) treats `TypeId(id)` as
"skip a u64 varint" for any registered id; the cast-path size/align
table treats it as `(8, 8)`. Both pieces hard-code the v1 set.

Adding a future typed wrapper edits these match arms. That's
deliberate — the codec already pattern-matches every variant, so a
new id is a couple of lines per file rather than the upfront cost of
a registry abstraction nobody uses yet.

**6. End-to-end test.**

Round-trip `SubscribeInput` through the hub MCP path with the
tagged string form for `mailbox`. Verify the substrate's
`subscribe_input` handler receives a `MailboxId` whose tag bit is
`TAG_MAILBOX` and whose hash matches the loaded component's id, and
that the schema walker reports the field as `aether.mailbox_id`
rather than `u64`.

## Consequences

**Closes the worked-example loop.** An agent reads `mbx-...` from
`load_component`, passes it directly as `params.mailbox` to
`send_mail`, and the substrate dispatches against the right component.
No manual tag-decoding, no JSON precision loss, no number-vs-string
guessing. Symmetric on the reply path: any kind whose payload contains
a `MailboxId` field renders that field as `mbx-...` when the codec
emits JSON for `receive_mail` / capture replies.

**Compile-time type guards on every id field.** A `KindId` cannot be
passed where a `MailboxId` is expected. ADR-0064's runtime integrity
property — type encoded in two places (tag bits + domain-prefixed
hash) — is preserved on the wire. Most call-site bugs now become
type errors at the boundary between substrate code and the kinds
crate, before any value reaches a serialiser.

**`TypeId` is one identifier reused across the stack.** The same
`u64` that names the type in the schema also names it in
`describe_component`'s output, in tracing fields, in error messages,
and in any future tooling that walks types. An agent inspecting a
kind descriptor sees `field "mailbox" : aether.mailbox_id` instead of
the structurally-correct-but-opaque `field "mailbox" : u64`. The
schema is the contract, and the type's name *is* the contract's
vocabulary.

**Schema vocabulary stops growing.** Future typed wrappers — `Uuid`,
`Decimal`, `DateTime`, generation-tagged handles, sequence counters —
each become *a new newtype that exposes its `TYPE_ID` + a per-id
match arm in the codec*, not a new schema variant.
`aether-hub-protocol` does not need a new release every time a typed
wrapper appears.

**Contradicts ADR-0064's "internal types stay u64".** That sentence
was a phase boundary, not a permanent constraint. ADR-0064 said so
implicitly by naming phase 3 in phase 2's PR body; this ADR says so
explicitly. The integrity property ADR-0064 cared about (the tag-bit
and domain-prefix cross-check) is preserved verbatim — typed
newtypes wrap the same tagged `u64`s as before.

**Public API rearrangement in `aether-mail` and
`aether-substrate-core`.** `aether-mail` gains three newtypes (each
exposing `TYPE_ID` + `TYPE_NAME` consts), growing into the canonical
home for id types. `aether-substrate-core::mail::MailboxId` becomes
a re-export. No new trait surface.

**Cast-path layouts stable.** Each typed id is `#[repr(transparent)]`
over a `u64` (8 bytes, 8-byte align), so `#[repr(C)]` cast-shape kinds
keep their layouts after migration. A
`LoadResult { mailbox_id: MailboxId }` memcpy's like
`LoadResult { mailbox_id: u64 }` did. No silent layout shifts.

**Pre-1.0 kind-id churn.** Every kind whose schema mentions a typed
id gets a new `Kind::ID`. ~20 kinds in aether-kinds, plus any
downstream kind in user-space components that embeds a typed id. The
build fails loudly at every literal site, so re-baseline is
mechanical and grep-driven, not detective work. No persisted ids
exist on disk; the migration is single-PR, no version skew.

**Splittable into three PRs:**

1. *Vocabulary + newtypes + codec arms + first kind.* Adds
   `SchemaType::TypeId`, the newtype family in `aether-mail` with
   their `TYPE_ID` / `TYPE_NAME` consts and `Schema` / `Display` /
   serde impls, the hard-coded match arms in
   `aether-substrate-hub::{encoder,decoder}.rs`, and migrates
   `SubscribeInput.mailbox` end to end with a round-trip test. Ships
   standalone value: tagged-string `subscribe_input` works and
   `describe_component` reports `aether.mailbox_id`.
2. *Hoist `MailboxId` into `aether-mail`.* Mechanical refactor; large
   file count (every site that imports `MailboxId` shifts crates). No
   behaviour change.
3. *Migrate remaining id fields in `aether-kinds`.* Each kind id
   shifts; tests re-baseline. No new functionality.

Each PR lands on its own; (1) is the only one that adds capability,
(2) and (3) are tidy.

**Type-id collision space.** `TYPE_DOMAIN ++ TYPE_NAME` hashes
through the same FNV-1a 64-bit construction ADR-0029/0030 use for
mailbox and kind ids. Birthday threshold ~2³² distinct type names;
realistic count is dozens. Margin is enormous, but the discipline
(disjoint domain prefix) is what keeps type-id space from leaking
into mailbox or kind space.

**Forecloses bidirectional `From<u64>` for the newtypes.** Mirroring
ADR-0064's stance: no implicit conversion from raw `u64`. Constructors
are explicit (`MailboxId::from_name`, `MailboxId::from_tagged_u64`)
so a copy-pasted integer literal cannot silently become a `MailboxId`.

## Alternatives considered

**`SchemaType::TaggedScalar(u8)` — narrow variant per encoding kind.**
The original draft. The variant carried the tag value (`TAG_MAILBOX`
etc.) and the codec hard-coded the "u64 wire, base32-tagged-string
JSON" mapping. Rejected: every future typed wrapper that didn't fit
the tag-bit-plus-base32 pattern (`Uuid`, `Decimal`, `DateTime`) would
need its own schema variant, growing the schema vocabulary
proportionally to the type count. `TypeId(u64)` collapses the whole
family into one variant whose dispatch is data-driven.

**`SchemaType::ScalarJson(Primitive, JsonCodecId)` — JSON-only
override on top of an unchanged scalar.** Rejected: half-measured.
The schema would still record `Primitive::U64` for both a `MailboxId`
and a `KindId` field, so two semantically-different fields would
produce identical canonical schema bytes for the scalar portion (only
the JSON-codec sidecar would differ), and the `describe_component`
surface would still surface `u64` rather than `aether.mailbox_id`.
Treating the typed wrapper as a first-class type in the schema is
strictly more honest.

**`WireType` trait + runtime registry of vtables.** Considered: each
typed wrapper implements a `WireType` trait carrying
`postcard_*` / `json_*` / `size_align` methods, and aether-mail seeds
a process-global `OnceLock<HashMap<u64, &'static dyn WireTypeErased>>`
at hub start. The codec dispatches `TypeId(id)` through one indirect
call. Rejected for v1: with three typed wrappers and a codec that
already pattern-matches every variant inline, the registry buys
extensibility we don't have a use for. Hard-coded match arms keep
the codec consistent with every other `SchemaType` arm in the same
files. If a future ADR adds enough typed wrappers (a real `Uuid`
landing, plus a `Decimal`, plus a `DateTime`) that the match arms
get unwieldy, the trait + registry abstraction lands then with
concrete pressure to justify it.

**Heuristic in the codec ("guess from field name or co-occurring
fields").** ~50 LOC. Rejected: asymmetric. JSON → postcard might
guess from a string-shaped value or a name like `mailbox_id`, but
postcard → JSON sees only a u64 varint with no signal at all about
typedness. Output never carries tagged strings; round-trip breaks
silently in one direction. Footgun.

**Defer phase 3, keep raw `u64` in `params`.** Rejected. Phase 2's PR
body scoped phase 3 explicitly. The agent harness drives every cold
start, so a structural friction here hits naive-Claude on every fresh
session — exactly the *forcing function has been there the whole
time* pattern the auto-memory feedback flags. The principled answer
is the right v1 shape.

**Single `Id(u64)` newtype with a runtime tag check.** Rejected.
`MailboxId` and `KindId` are categorically different addresses;
mixing them is not a runtime concern, it's a type concern. A unified
`Id` newtype gives up the compile-time guard for a cosmetic
simplification.

**Add `TypeId` to the schema only at the MCP boundary; leave
`aether-kinds` on raw `u64`.** Rejected. The codec needs a schema
signal regardless of where the typed types live; once the schema
encodes typedness, leaving the in-crate types as `u64` is
half-measured. Every new id-bearing kind author hits the same
papercut and has to remember to opt their field into the typed
schema by *some* mechanism. A typed newtype that derives `Schema` to
emit `TypeId` *is* that mechanism, with no per-kind ceremony.

**Larger newtype family up front (`SessionId`, `EngineId`,
`CorrelationId`).** Defer. ADR-0064 picked three tag values for v1
based on three id spaces in actual use. Adding more typed wrappers
later is a backward-compat extension — existing `TYPE_ID`s keep
their semantics — so over-scoping now buys nothing. When `SessionId`
or `EngineId` grows a real reason to be typed (today they're already
`Uuid` and `SessionToken`, not opaque hashes), a follow-up
implementation adds the newtype, its `TYPE_ID` const, and the codec
match arms.

**Use `serde::Deserialize` with a single string-only variant (no
number back-compat).** Rejected for the migration window. Today's
hub MCP test corpus and the `send_mail` examples in CLAUDE.md still
pass numbers. Accepting both keeps the migration painless for one
release; a follow-up ADR can tighten to string-only after every doc
example and test fixture has migrated.
