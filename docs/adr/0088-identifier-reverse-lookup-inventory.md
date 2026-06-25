# ADR-0088: Reverse-lookup identifier inventory with name templates

- **Status:** Accepted
- **Date:** 2026-05-24

## Context

Every substrate identifier is a **one-way hash**. `MailboxId` (ADR-0029) is `fnv1a_64_prefixed(MAILBOX_DOMAIN, name)`; `KindId` (ADR-0030) is `fnv1a_64_prefixed(KIND_DOMAIN, canonical_schema)`; ADR-0064 stamps a 4-bit type tag into the high nibble so the id is `[tag:4][hash:60]`. The hash is computed as a compile-time const at both the substrate and the guest SDK, so ids round-trip across the FFI verbatim with no host-fn resolve (`crates/aether-data/src/hash.rs`). By construction, **you cannot recover the origin name from the id**.

ADR-0064 gives every id a printable string form (`mbx-q3lr-bv2x-mtdr`), but that encodes the *hash*, not the name. MCP renders these hex-ish tags because it has no way to get back to `"aether.audio"` (`crates/aether-mcp/src/tools.rs` — `mailbox_id_to_tagged`, `kind_id_to_tagged`). This is a binary-wide gap: anything that wants to show a human a name from an id is stuck.

What already exists, partially: the `Kind` link-time inventory (#243). `#[derive(Kind)]` emits a `cfg(not(wasm32))` `inventory::submit! { DescriptorEntry { name, schema } }` (`crates/aether-data/src/lib.rs` `__inventory`), and `aether-kinds::descriptors::all()` materializes the list. So a `KindId → name` reverse map *is* already buildable — and MCP ships a compiled-in copy of it for `describe_kinds`. But:

- **Mailbox names** (cap NAMESPACE consts, `aether.audio` / `aether.render` / …) and **thread names** (`aether-worker-N`, `aether-root-<NAMESPACE>`, `aether-instanced-<full_name>`) have no inventory at all.
- **Instanced / dynamic names** can't live in a static inventory by nature — `aether-instanced-player:42` and `aether.component.trampoline:cam` are minted at runtime from a pattern plus a runtime parameter. A flat link-time name list can't enumerate them.

**The forcing function** is Phase 4 dispatch-latency work (#1059 / #1101 follow-on). Profiling the warm dispatch hop (1-worker saturation, uncontended) attributes ~25% of it to a single per-mail `String` allocation: `TraceEvent::Received.thread_name`, built every hop via `thread::current().name().map(str::to_owned)` (`crates/aether-substrate/src/actor/native/dispatcher_slot.rs`, `…/dispatch.rs`). The name is the constant `aether-worker-N` for a given worker, so allocating it fresh per hop is pure waste. The fix is to store the name's `u64` hash (Copy, zero alloc) in the trace event and reverse it to a display name on the cold render path — which is exactly the reverse-lookup capability the codebase lacks. Rather than special-case thread names, we build the general construct.

## Decision

Layer an **additive reverse-lookup inventory** over the existing flat-hash ids. The ids do not change; reversal is a side table, never encoded in the id bits.

### 1. Ids stay flat hashes; reversal is a side table

ADR-0029/0030 are unchanged; ADR-0064's `Tag` enum gains one additive `Thread` variant (a reserved tag value, exactly as `Dag`/`Transform` were added later — see Decision 7). An id is still `[tag:4][hash:60]`, computed as a compile-time const, round-tripping across the FFI verbatim. Reversal is a separate registry keyed on the id; the id bits never encode the name. This keeps the hot path (const id derivation, no lookup) and the wire encoding untouched; reverse-lookup is a cold-path, observability-time concern.

### 2. The reverse-lookup chain

Given an id, recover its origin name by walking, in order:

1. **Static name inventory** (declared names) → exact name.
2. **Template prehash** (bounded / declared params) → reconstruct from template + parameter.
3. **Runtime registry** (dynamic instances) → name registered when the instance was minted.
4. **Miss → ADR-0064 hex tag** (`mbx-XXXX`), unchanged.

Reversal is therefore a strict upgrade: a hit shows the real name, a miss shows exactly what we show today. Nothing regresses.

### 3. Static name inventory — generalize #243

A `NameEntry { domain, name: &'static str }` link-time inventory, sibling to `DescriptorEntry`, submitted for every declared name: mailbox NAMESPACE consts, declared transform names (the ADR-0048 §1 link-time transform inventory), and (reusing the existing `DescriptorEntry.name`) kinds. At boot the inventory is folded into a `hash → name` map. The hash for each entry is recomputed with the entry's `domain` so it matches the id space exactly.

### 4. Name templates for instanced families

`TemplateEntry { domain, template: &'static str, param: ParamKind }` declares a family whose instances are not statically enumerable. `template` is a pattern with one hole (`"aether-worker-{N}"`, `"aether.component.trampoline:{name}"`), and `param` is one of:

- **`Bounded { lo, hi }`** — finite integer range (`aether-worker-{0..=63}`). Enumerated and pre-hashed at boot → exact reverse, zero runtime cost. This is the "embed the common/expected hashes" case.
- **`Declared { domain }`** — the parameter ranges over another inventory's names (`aether-root-{NAMESPACE}`, where `NAMESPACE` is a declared actor namespace).
- **`Dynamic`** — instances minted at runtime from an unbounded parameter (`aether-instanced-{full_name}`). The template declares the family's *existence and shape*; individual instances reverse via the runtime registry (step 3), not the template.

A template thus declares "ids in this family exist and look like *this*" even when it cannot say *which* exist — the distinction that a flat name list can't express.

### 4 (v2). Cardinality — the how-many axis

> **Removed (issue #2335).** The cardinality axis recorded in this section was plumbed end-to-end but read by no production consumer — the only reader was the manifest producer itself, so the served `aether.inventory.manifest` reply carried metadata nothing on the client consumed. It was removed in full: the `Cardinality` enum and the `cardinality` field on `TemplateEntry`, the `aether.inventory.cardinality` wire kind (`CardinalityWire`) and the `cardinality` field on `TemplateEntryWire`, and the `#[actor]` / `#[bridge]` `one_per` argument. The `ParamKind` shape axis (§4 v1) and the `singleton` / `instanced` addressing axis (ADR-0079 / ADR-0119) are unaffected — they are the live machinery the removal deliberately left in place. The text below is retained as the historical record of the v2 decision and no longer describes the live wire format. If an instance-cardinality observability consumer is ever genuinely identified, it gets designed against that concrete need rather than reinstating this speculative shape.

*Amendment, issue #1132.* §4's `ParamKind` conflated two questions: *how* a template's hole is filled (its **shape** — integer range / declared name / opaque string) and *how many* instances the family can have. v1 punted on the second: every instanced actor emitted `ParamKind::Dynamic`, so a manifest consumer saw four identical opaque families (trampoline, tcp session, tcp listener, engine proxy) and learned nothing about what each instance corresponds to.

v2 split the axes. `ParamKind` kept the shape role (and drove the reverse-map enumeration unchanged). A `Cardinality`, stated explicitly on **every** `TemplateEntry`, carried the how-many:

- **`Bounded(u64)`** — a compile-time-known finite instance bound (`aether-worker-{N}` prehashes a ceiling).
- **`OnePer(<entity>)`** — one instance per live entity of a named kind. This is the relationship all four instanced actors actually have: not "N instances" but "as many as there are components / connections / listeners / engines."
- **`Unbounded`** — open-ended, runtime-minted, no fixed relationship (`aether-instanced-{full_name}`; the old `Dynamic`-only semantics, now a cardinality in its own right).

`OnePer` was the load-bearing addition — the intent was to make the manifest self-describing about instance shape. The axes were orthogonal: the same shape paired with different cardinalities (every instanced actor is `ParamKind::Dynamic`, but the trampoline was `OnePer("component")` while the instanced thread-name fallback was `Unbounded`). The reverse-map builder read only `ParamKind`; `Cardinality` was manifest metadata. Cardinality was declared at the `#[bridge(instanced, one_per = "component")]` / `#[actor(instanced, one_per = "…")]` site (absent ⇒ `Unbounded`) and rode through `TemplateEntryWire` to the served manifest. (All of this is removed per #2335 — see the note at the head of this section.)

**Deferred (over-reach for v2):** typed-id holes — e.g. `aether.engine.proxy:{engine: EngineId}` where the hole is a tagged-id type rather than a bare string, enabling *chained* reversal (reverse the embedded `EngineId` to its engine name as part of reversing the instance name). That is a meaningfully larger change to the inventory's hole model and isn't needed to make the manifest expose cardinality shape; `OnePer(<entity>)` over a string hole delivers the self-describing-manifest win on its own.

### 5. Runtime registry for dynamic instances

A process-global registry (substrate-side, `std` `RwLock<HashMap<u64, Box<str>>>`) maps id → name for dynamically-minted names. It is populated at the moment a name is minted — the thread-spawn name builders (`alloc_instanced_thread_name`, the `aether-worker-N` formatter), the trampoline mailbox registration, etc. Registration is lazy-safe: the same code path that builds the name and derives its hash also registers it, so any id that can appear in a trace or diagnostic is registered before it can be observed. Writes are rare (instance creation), reads are cold (render time), so the lock is uncontended in practice and off the dispatch hot path entirely.

### 6. The inventory actor — manifest + resolve

A chassis-owned `aether.inventory` mailbox serves reverse-lookup data over mail, so the out-of-process MCP (and any future observer) reads the **authoritative, per-build** inventory from the running substrate instead of a drift-prone compiled-in copy:

- **`aether.inventory.manifest`** → the compile-time manifest: every `NameEntry` + `TemplateEntry` (bounded templates carry their range so the client can expand or prehash locally). MCP fetches this once at connect and builds its local reverse map for statics + templates.
- **`aether.inventory.resolve { ids: [...] }` → `[Option<String>]`** — on-demand reversal of dynamic-instance ids that the client can't resolve locally (a runtime-registry lookup). MCP calls this only on a local miss and caches the result.

This is why the manifest is *served*, not shipped: MCP's reverse map is always the running substrate's own, and the dynamic instances it can't know at compile time are one cheap query away.

### 7. First consumer — thread-name reverse-lookup (Phase 4)

`TraceEvent::Received.thread_name: Option<String>` (`crates/aether-kinds/src/trace.rs`) becomes `Option<ThreadId>` — a **first-class tagged id**. A new `THREAD_DOMAIN` prefix (`hash.rs`) and a `Tag::Thread` variant (string prefix `thr`, an additive use of one of ADR-0064's reserved tag values, `tagged_id.rs`) make a thread id `[Tag::Thread:4][fnv1a_64(THREAD_DOMAIN ++ name):60]` — uniform with mailbox / kind ids, encodable to `thr-XXXX-XXXX-XXXX`, and reversible through the same chain and the same `aether.inventory.resolve` API. The dispatch hot path reads a **thread-local cached `ThreadId`** (computed once per worker via `thread::current().name()`, which also registers name→hash in the runtime registry on first compute), eliminating both the per-hop `str::to_owned` and the `thread::current()` Arc bump. Display-name resolution happens on the cold path (`trace_walk::fold_nodes` → `MailNodeWire.thread_name: Option<String>`, or MCP render) via the inventory, so the renderer is unchanged. Worker names declare a `Bounded` template; root names a `Declared` template; instanced names register dynamically.

### 8. v1 scope — MCP renders real names

v1 wires MCP to reverse mailbox / kind / thread ids to real names everywhere it currently shows hex tags (falling back to the ADR-0064 tag on miss), via the inventory actor. `Handle` / `Dag` ids stay hex — they are counter-backed, with no origin name to recover. Transform reverse-lookup folds in if its link-time name inventory is cheap to expose.

## Consequences

**Positive.** MCP and diagnostics show human names (mailbox / kind / thread) instead of hex — a binary-wide observability win for the agent-facing surface. The Phase 4 thread-name perf shave (~25% of the warm hop) drops out as the first consumer. One reverse-lookup model replaces ad-hoc per-id handling. The served manifest is drift-free — always the running build's own inventory.

**Cost.** A new chassis mailbox + inventory actor. A process-global runtime registry (a lock taken only when a dynamic name is minted — cold, rare, off the dispatch path). MCP gains a connect-time manifest fetch and a cached `resolve` round-trip for dynamic misses. ADR-0064's `Tag` enum gains a `Thread` variant (additive — a reserved value). The trace wire type changes (`TraceEvent.thread_name` `String → ThreadId`) — in-tree only, both sides rebuild; the `KindId` schema hash of `TraceEvent` shifts accordingly.

**Follow-on / foreclosed.** Could subsume the existing compiled-in `describe_kinds` inventory into the served manifest (consolidation) — deferred, not v1. Structured-id migration is explicitly foreclosed (see Alternatives). `Handle` / `Dag` naming stays out of scope; if a named-handle need appears it slots into the same chain as a new domain + template.

## Alternatives considered

- **Structured ids** (encode `(template_id, param)` in the id bits so it reverses by construction) — would break the flat-hash model, the base32 wire encoding (ADR-0064), and the const FFI round-trip (`mailbox_id_from_name` is a compile-time const guests compute identically), re-opening ADR-0029/0030/0064 for a reversal feature a side registry delivers additively. Rejected.
- **`Arc<str>` thread-name** (thread-local cached `Arc<str>`, clone per event) — kills the malloc but keeps per-hop atomic refcount traffic and a pointer-sized event, couples the wire type to serde's `rc` feature, and doesn't generalize to the binary-wide reverse-lookup need. Rejected in favor of the tagged `ThreadId` + inventory, which is `Copy` and reusable.
- **MCP ships a compiled-in inventory copy** (extend the `describe_kinds` pattern) — works for statics but drifts from the substrate's actual build and can never see runtime-minted instances. Rejected for the runtime-served manifest actor.
- **Per-event hex tag only (status quo)** — no reverse-lookup; MCP shows `mbx-XXXX`. The thing this ADR fixes.
- **Drop / reduce the `thread_name` field** — recovers the alloc but loses per-thread trace attribution (#734). Rejected; the inventory makes the field cheap (a `u64`) instead of removing it.

## References

- ADR-0029 (name-derived `MailboxId`), ADR-0030 (schema-hashed `KindId`), ADR-0064 (type-tagged opaque ids), ADR-0065 (typed-id newtypes + first-class type ids), ADR-0048 (native transforms + link-time inventory), ADR-0080/0081 (trace + per-actor rings).
- Issues: #243 (`Kind` link-time inventory), #734 (per-thread trace attribution), #1059 / #1101 (dispatch-latency umbrella + blob redesign — the forcing function).
