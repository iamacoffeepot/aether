# ADR-0069: Data layer split from mail transport

- **Status:** Proposed
- **Date:** 2026-04-30

## Context

The non-component infrastructure crates have grown into a five-crate cluster:

| Crate | Holds | LOC |
|---|---|---|
| `aether-id` | typed-id newtypes (`MailboxId` / `KindId` / `HandleId`), tag-bit constants, FNV hashing | ~650 |
| `aether-hub-protocol` | `SchemaType` / `KindShape` / `KindLabels` / `InputsRecord` / canonical bytes **and** `EngineToHub` / `HubToEngine` / framing helpers | ~3250 |
| `aether-mail` | `Kind` / `Schema` / `CastEligible` traits, `Mail<'_>`, `Sink<K>`, `mailboxes` table, descriptor inventory | ~900 |
| `aether-kinds` | concrete substrate kinds (Tick, Key, DrawTriangle, audio, IO, control) | ~2250 |
| `aether-params-codec` | schema-driven JSON ↔ wire bytes | ~2700 |

Each crate has a real reason to exist — `aether-id` was extracted to break a `aether-mail → aether-hub-protocol → aether-mail` cycle (issue 469); `aether-hub-protocol` is `no_std` so guests can read the schema vocabulary; `aether-params-codec` was factored out of the hub so the scenario runner could share the JSON encoder without taking a hub dep — but the role each crate plays from a consumer's vantage point is muddled:

- `aether-hub-protocol` carries two unrelated populations: the **universal schema vocabulary** (`SchemaType`, `LabelNode`, `KindShape`, canonical bytes) that every guest needs to describe a kind, and the **hub channel wire** (`EngineToHub` / `HubToEngine` / framing) that only the substrate ↔ hub TCP loop touches. They share a crate by historical accident, and the name describes only the second half.
- `aether-mail` mixes the **data-shape vocabulary** (`Kind`, `Schema`, `CastEligible` traits and `decode_from_bytes` / `encode_into_bytes` wire-shape autodetect) with the **transport envelope** (`Mail<'_>`, `Sink<K>`, the reserved `mailboxes` table). The first half describes how a typed Rust value relates to bytes; the second half describes how envelopes are addressed and dispatched.
- `aether-params-codec` is named after its first consumer (mail params from MCP tool calls), but its actual subject is universal: given a `SchemaType`, encode or decode bytes from JSON. Production code in the crate reaches into `aether-mail` only for `tagged_id::encode` and `tag_for_type_id` — both id-handling helpers, not mail concepts.

The seam runs orthogonal to the current crate boundaries: **universal data format** (typed ids, schema vocab, traits that bind a Rust type to bytes, codec) versus **mail transport** (envelope shape, addressing, hub frames). The current layout cuts perpendicular to that seam, leaving the universal half smeared across `aether-id`, `aether-hub-protocol`, and `aether-mail`.

### Forcing function

The next planned subsystem is the prompt-system save format (durable on-disk storage of LLM dialogue trees, fact graphs, and generation history). It needs schema-described records on disk — the same `SchemaType` vocabulary the hub already uses for mail kinds, and the same JSON↔bytes codec already running in `aether-params-codec`. With the current layout, a "save record" type would either:

1. Pull `aether-mail` to use `Schema` and `Kind` (forcing every save format to depend on the mail envelope crate it has nothing to do with), or
2. Re-implement a parallel schema vocabulary in a new crate (forking the substrate's hard-won canonical-bytes encoder), or
3. Land its types in `aether-kinds` even though they're not substrate kinds.

None of those are good. The save format is the second concrete consumer of "universal data format" after mail dispatch, and it's the forcing function that makes the seam load-bearing rather than aesthetic.

## Decision

Re-cut the five-crate cluster along the universal-data-vs-transport seam:

```
aether-data            (no_std + alloc; foundation for everything that describes typed bytes)
   ▲
   ├── aether-codec    (universal: schema-driven encode/decode; future home for save-format adapters)
   │
   ├── aether-mail     (transport envelope only)
   │      ▲
   │      └── aether-hub-protocol  (one specific mail transport)
   │
   └── aether-kinds    (concrete substrate kinds)
```

### Crate-by-crate

**`aether-data`** *(new — absorbs all of `aether-id`, the schema half of `aether-hub-protocol`, and the data-shape half of `aether-mail`)*

Contents:

- `MailboxId`, `KindId`, `HandleId` newtypes; `Tag`; `tag_bits` constants; `tag_for_type_id`; tagged-string JSON encoding (from `aether-id`).
- `MAILBOX_DOMAIN`, `KIND_DOMAIN`, `TYPE_DOMAIN`; `fnv1a_64_*`; `mailbox_id_from_name` (from `aether-id`).
- `SchemaType`, `SchemaCell`, `NamedField`, `EnumVariant`, `Primitive`, `SchemaShape`, `VariantShape`, `KindShape`, `LabelNode`, `LabelCell`, `KindLabels`, `InputsRecord`, `INPUTS_SECTION{,_VERSION}` (from `aether-hub-protocol/types.rs`).
- All of `aether-hub-protocol/canonical/` (canonical bytes encoders + label sidecars).
- `Kind`, `Schema`, `CastEligible` traits; `decode_from_bytes` / `encode_into_bytes` wire-shape autodetect (from `aether-mail`).
- Native descriptor `inventory` machinery (from `aether-mail`).

`no_std` + `alloc`. Deps: `bytemuck`, `serde`, `postcard`, `aether-mail-derive` (optional, re-exported as `aether-data` derives), `inventory` (native target).

**`aether-codec`** *(rename of `aether-params-codec`)*

Same source files, deps re-pointed: `aether-data` (for `SchemaType` and id helpers) instead of `aether-hub-protocol` + `aether-mail`. Today's `encode_schema` / `decode_schema` keep their JSON ↔ bytes contract. Save-format adapters land here as siblings (`encode_record`, `decode_record`, etc.) — they reuse the same `SchemaType`-walking core.

**`aether-mail`** *(slimmed to the transport envelope)*

Contents after migration:

- `Mail<'_>` (the wire-form receive struct: sender + kind id + payload bytes lifetime).
- `Sink<K>` (an addressed sender, parameterised by kind).
- `mailboxes` table (`DIAGNOSTICS`, etc. — reserved mailbox names that only matter when transporting).

Deps: `aether-data`. Maybe ~200 LOC.

**`aether-hub-protocol`** *(slimmed to the hub channel wire)*

Contents after migration:

- Hub frames: `Hello`, `Welcome`, `Goodbye`, `LogEntry`, `LogLevel`, `MailFrame`, `EngineMailFrame`, `ClaudeAddress`, `EngineMailToHubSubstrateFrame`, `MailToEngineMailboxFrame`, `MailByIdFrame`, `EngineId`, `SessionToken`.
- Direction-typed enums: `EngineToHub`, `HubToEngine`.
- Framing helpers: `encode_frame`, `read_frame`, `write_frame`, `FrameError`, `MAX_FRAME_SIZE`.

Std-only (no more `default-features = false` gymnastics — this crate is unambiguously host-side). Deps: `aether-mail`, `serde`, `postcard`, `uuid`. Roughly half its current size.

**`aether-kinds`** *(unchanged in shape; deps re-pointed)*

Drops `aether-hub-protocol` (no longer needed for `Schema`); depends on `aether-data`. Source files unchanged.

### Naming

`aether-data` is the new foundation crate. The alternative names considered were `aether-schema` and `aether-format`; "data" wins because the crate's job is broader than schemas (it owns ids, traits, and the descriptor inventory in addition to the schema vocabulary), and "format" overstates its scope (codec is `aether-codec`).

Crate names that do not change: `aether-mail`, `aether-hub-protocol`, `aether-kinds`. The first two are slimmed but keep their names because their narrowed responsibilities still match.

### Migration shape

Mechanical search-and-replace at every consumer site:

- `aether_hub_protocol::SchemaType` → `aether_data::SchemaType`
- `aether_hub_protocol::canonical::*` → `aether_data::canonical::*`
- `aether_hub_protocol::{KindShape, KindLabels, InputsRecord, INPUTS_SECTION{,_VERSION}}` → `aether_data::*`
- `aether_hub_protocol::tag_bits` → `aether_data::tag_bits`
- `aether_mail::{Kind, Schema, CastEligible}` → `aether_data::*`
- `aether_mail::{MailboxId, KindId, HandleId, Tag, tagged_id, tag_for_type_id, mailbox_id_from_name}` → `aether_data::*`
- `aether_mail::{Mail, Sink, mailboxes}` → unchanged
- `aether_params_codec::*` → `aether_codec::*`
- `aether_mail_derive` macro emissions: `::aether_mail::SchemaType` → `::aether_data::SchemaType`, `::aether_hub_protocol::*` → `::aether_data::*`. (The proc-macro crate itself is not renamed; `#[derive(Kind)]` keeps importing from `aether_data` whose root re-exports the derive.)

The `aether-id` crate is deleted. Its directory disappears from `crates/`. `aether-params-codec` is renamed to `aether-codec` (directory move + Cargo.toml `name` change + every consumer's `Cargo.toml`).

Wire bytes do not change. Postcard's binary representation of `MailboxId` / `KindId` / `HandleId` (newtypes over `u64`) is byte-identical regardless of which crate the type lives in. The `aether.kinds` and `aether.kinds.labels` wasm custom sections keep their identical byte layout. Existing component wasms continue to load against a substrate built from the new layout.

Schema-hashed kind ids (ADR-0030) round-trip identically: the canonical-bytes encoder is the same code, just relocated.

## Consequences

**Positive**

- Crate names finally describe their roles: `aether-data` is the universal layer; `aether-mail` is a transport envelope; `aether-hub-protocol` is one specific mail transport; `aether-codec` is universal data ↔ format.
- The prompt-system save format depends on `aether-data` (for `Schema`) and `aether-codec` (for bytes ↔ on-disk encoding) — neither of which has any mail-shaped baggage. The forcing function lands cleanly.
- A future second mail transport (peer-to-peer, in-process bridge) is a sibling of `aether-hub-protocol` under `aether-mail`, not a new fork of the schema vocabulary.
- A future second codec (msgpack, CBOR, custom binary) is a sibling of `aether-codec` or an additional module within it.
- `aether-mail` shrinks to ~200 LOC and matches its name — Mail, Sink, mailboxes. Nothing else.
- `aether-hub-protocol` drops the `default-features = false` no_std gate; it is unambiguously std now.
- The `aether-mail → aether-hub-protocol → aether-mail` historical cycle (the reason `aether-id` exists) is structurally impossible in the new layout — there is no edge from the foundation crate to anything above it.
- One fewer crate. `aether-id` collapses into `aether-data`; that's a real reduction in directory and Cargo.toml count, not just a relabel.

**Negative**

- Significant one-time churn. Every consumer's `use` statements rewrite (`aether_mail::Kind`, `aether_mail::MailboxId`, `aether_hub_protocol::SchemaType`, `aether_params_codec::*`, etc.) — hundreds of call sites across `aether-substrate-core`, `aether-substrate-hub`, `aether-component`, `aether-kinds`, `aether-mail-derive`, the scenario crates, every component cdylib, and every test fixture. Search-and-replace is mechanical but the diff is large.
- The proc-macro crate (`aether-mail-derive`) emits `::aether_data::SchemaType` and friends. Its consumers must re-export the derive at `aether_data::Kind` so existing `use aether_mail::Kind;` callers move cleanly to `use aether_data::Kind;` without learning that the derive lives in a separate crate. The macro keeps its name to avoid a third crate-name change.
- Rust-analyzer caches across the workspace; consumers may need an explicit "Reload workspace" after pulling the change.
- The crate name `aether-data` is more generic than the names it replaces. The README of every crate in the cluster needs a one-line "what this is for" pointer; without that, future readers may file new universal-data work into `aether-mail` again because that's where they're used to seeing `Kind`.
- `aether-hub-protocol` becomes a thin crate (~1500 LOC). Some workspace conventions prefer larger crates over many thin ones; this ADR accepts the thinness because the role boundary is real.

**Neutral**

- The wire is unchanged. Hub TCP framing, `aether.kinds` custom sections, mail dispatch bytes — every byte boundary holds. This is a source-layout decision; the network and on-disk shapes are untouched.
- ADR-0064 / ADR-0065 (tagged-id newtypes) are unaffected — the types move crates but keep their semantics.
- ADR-0066 (per-component trunk rlibs) is orthogonal; component-owned trunk crates depend on `aether-data` instead of `aether-mail` for their kind types, but the per-component split itself is unchanged.
- `aether-mail-derive` is renamed in spirit (it now emits `aether_data` paths) but keeps its crate name — renaming a proc-macro crate is more disruptive than renaming a target crate, and the macro's *callers* see `aether_data::{Kind, Schema}` derives anyway via re-export.

**Follow-on work**

- Land `aether-data` as a new workspace member; absorb `aether-id` and the schema half of `aether-hub-protocol` and the data-shape half of `aether-mail`. Delete `aether-id`.
- Rename `aether-params-codec` to `aether-codec` (directory + Cargo.toml + every consumer's manifest).
- Slim `aether-mail` to the transport envelope; slim `aether-hub-protocol` to the hub channel wire.
- Update `aether-mail-derive` to emit `::aether_data::*` paths in its expansions.
- Sweep every consumer's `use` statements (`aether-substrate-core`, `aether-substrate-hub`, `aether-component`, `aether-kinds`, `aether-mail-derive` tests, scenario crates, every component cdylib).
- Update `CLAUDE.md`'s crate-roster paragraph and the memory note covering hub-protocol shape.
- Document the convention in the workspace `README.md`: "data is universal; mail is transport; codec is data↔format; hub-protocol is one transport; kinds is content."

## Alternatives considered

- **Status quo + documentation only.** Rejected — the seam is real, the prompt-system save format is on the critical path, and routing it through `aether-mail` (or forking schema) has worse long-term cost than the one-time churn here.
- **Collapse `aether-id` back into `aether-hub-protocol`.** Rejected — drops one crate but worsens the role mismatch (hub-protocol becomes the foundation crate that's named after a transport).
- **Cut `aether-hub-protocol` along the schema-vs-frames seam without folding `aether-mail`.** Considered as the original Option A in the design conversation. Rejected — addresses the schema-vocabulary-trapped-in-a-transport-name problem but leaves `Kind` / `Schema` filed under `aether-mail`, so a save-format crate still pulls the mail crate to describe data shapes. The forcing function makes the deeper cut load-bearing.
- **Move `Kind` and `Schema` into `aether-data` but keep `aether-id` as a separate leaf.** Rejected — `aether-id` was a workaround for the dep cycle, not a load-bearing role boundary. Once `aether-data` owns the foundation, the cycle is gone and the leaf has no remaining justification.
- **Wait for the prompt-system save format to land before refactoring.** Rejected — the save format is the load-bearing consumer that justifies `aether-data`, and landing it inside the wrong layout (under `aether-mail` or as a fork of `aether-hub-protocol`'s schema vocab) creates a second migration. Cutting the seam first means the save format's first PR uses the right shape from the start.

## References

- ADR-0005 — mail typing system; original `Kind` / payload-tier model.
- ADR-0006 — wire + topology; hub frames.
- ADR-0028 — `aether.kinds` custom section.
- ADR-0029 — name-derived `MailboxId`s.
- ADR-0030 — schema-hashed `KindId`s.
- ADR-0032 — canonical schema bytes + labels sidecar; the no_std move that today still gates `aether-hub-protocol`'s `std` feature.
- ADR-0064 — tagged opaque ids on the MCP wire.
- ADR-0065 — typed-id newtypes (`MailboxId` / `KindId` / `HandleId`).
- ADR-0066 — per-component trunk rlibs (orthogonal; trunk crates depend on `aether-data` after this ADR).
- Issue 469 — typed wire fields in `aether-hub-protocol`; the work that surfaced the dep cycle and led to `aether-id`'s extraction.
