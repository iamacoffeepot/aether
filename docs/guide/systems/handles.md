# Handles

> **Governing ADRs:** [ADR-0045](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0045-computation-dag-and-typed-handles.md)
> (handles as a primitive, the founding decision),
> [ADR-0048](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0048-transforms-and-content-addressed-handles.md)
> (content-addressed ids), [ADR-0049](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0049-persistent-handle-store.md)
> (the persistent store, its layout and eviction). The `Ref<K>` **wire type** and
> the **store** are **stable**. The store is wired on every chassis and covered by
> tests, but it's **lightly exercised** in practice: its heaviest consumer, the
> content-generation pipeline ([ADR-0084](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0084-plato-content-generation-on-the-dag.md),
> a 0.5 target), isn't built yet, and today almost all handle traffic comes from
> the [computation DAG](dag.md). When in doubt, the ADRs are
> authoritative.

A handle is a typed reference to a value the substrate is holding for you. Instead
of putting a value on the wire, you put its id, and the value stays in the
substrate's **handle store** until someone dereferences it. The point is that a
value can move through a chain of work without its bytes moving through every
actor on the way: an actor that only forwards a result never has to load it.

You meet handles two ways. Authoring a kind or capability, a handle is the
`Ref<K>` wire type — a field that carries either an inline value or a reference
the substrate resolves before your handler runs. Driving the engine over MCP, the
store is something you inspect with `describe_handles` to see what the substrate
is holding and how close it is to its disk budget.

## Why it exists

Without a stored reference, three costs pile up wherever work composes.

Bytes flow through an actor that never needs them. A texture fetched by one actor
and consumed by the GPU passes through that actor's wasm linear memory on the way
in, again decoded, and again on the way out — three copies of a value the actor
only hands along.

Pure work gets recomputed. Two callers that embed the same prompt, or fetch and
decode the same asset, each pay full price, with no way to say "if someone already
produced this value, give me theirs."

And expensive results vanish at exit. A generated image — real money, seconds of
latency — is gone on the next boot, even though its inputs were identical and the
result was perfectly reproducible.

A handle answers all three. The value lives in the substrate under an id; a mail
carries the id, so nothing flows through an actor that only passes it on. The id
of a pure result is derived from its inputs, so the same computation lands on the
same id and the store serves it once. And the store spills to disk, so a result
outlives the process that made it.

## What it does

**`Ref<K>` is the wire type, inline-or-reference.** Anywhere a kind value travels,
a field typed `Ref<K>` carries one of two forms:

```rust
pub enum Ref<K> {
    Inline(K),                          // the whole value, on the wire
    Handle { id: u64, kind_id: u64 },   // a reference into the handle store
}
```

A sender with the value in hand passes `Ref::inline(v)`; a sender holding a
reference passes `Ref::handle(id)`, which stamps `kind_id` from `K::ID` so the
reference can't disagree with the field's type. The substrate resolves a `Handle`
to its stored value *before* the recipient's handler runs — checking the stored
entry's kind matches — so a handler decodes a plain `K` and never knows which form
arrived. A handle is typed over a kind, usually a reply kind like `ReadResult`, so
a failed upstream resolves to that kind's own `Err` value and rides the same
`Result` shape every handler already matches on. There's no separate error channel
and no liveness check. (The `Ref<K>` schema arm itself is on
[The type system](../foundations/type-system.md); this page is what the store does
with it.)

**The store is substrate-global and two-tiered.** One store per engine, shared
across every actor — which is what lets two callers hit the same cached value.
It's a hot in-memory cache over a persistent on-disk tree:

- **In-memory** holds resolved bytes keyed by id, bounded by
  `AETHER_HANDLE_STORE_MAX_BYTES` (default 256 MB). Under pressure it evicts by
  least-recent-use, but only entries that are both unreferenced and unpinned —
  anything in use or explicitly kept stays.
- **On-disk** is a content-addressed file tree at `AETHER_HANDLE_STORE_DIR`
  (defaulting under the platform data directory), bounded by
  `AETHER_HANDLE_STORE_DISK_BUDGET_BYTES` (default 16 GB) and evicted on a slow
  background tick. A restart doesn't read it into memory; it builds a sparse index
  over the on-disk entries and materializes an entry's bytes on first access. Boot
  cost tracks how many entries are stored, not their total size, so a store of a few
  large handles boots about as fast as an empty one while many small entries cost
  more. A disk hit then looks exactly like an in-memory miss to the caller.

**Two lifecycle dimensions, kept separate.** *Refcount* is in-process and
transient — who is holding this right now — and resets to zero on restart, where
nothing holds anything yet. *Pinned* is durable and user-declared — keep this
regardless of refcount or memory pressure — and survives restart. The split keeps
persistence honest: a pin records the caller's explicit "save this" directive, and
the store honors exactly that.

**How a value's id is derived decides whether it persists, and that turns on the
value's origin.** A source observation and a transform computation are addressed
differently:

- **Source values** — the reply from a sink op like a fetch or a read — get an
  **ephemeral, monotonic** id. A fetch today and the same fetch tomorrow are two
  distinct observations with two distinct ids, because the world changed between
  them. Source values stay in memory only; persisting one by id would let a caller
  "restore" something the next observation won't reproduce.
- **Transform outputs** — the result of a pure function over fixed inputs — get a
  **content-addressed** id derived from the transform's identity and its input
  ids. The same transform over the same inputs lands on the same id, so the store
  deduplicates automatically and the result is worth persisting. These are the ids
  that spill to disk. (Transforms run as part of a DAG — see
  [The computation DAG](dag.md).)

The exception to "sources don't persist" is a pinned one: pin a fetch result and
the bytes survive restart for any caller holding that id, even though the
observation itself can't be reproduced. Pinning is how you capture a one-time
observation as a durable input.

**Schema evolution invalidates stale bytes by itself.** Each on-disk entry records
the schema-hash of its kind at write time. Change a kind's schema and its hash
changes, so old entries no longer match and are treated as a miss — the producer
re-runs and writes fresh bytes under the new hash. Correct by construction, no
migration step; the cost is that schema churn mid-iteration discards cached work,
which is what pinning is for.

**The mail surface is the `aether.handle` mailbox.** Handle bookkeeping rides mail
rather than a privileged host call, so the traffic is observable and gated by the
same per-mailbox capability model as everything else:

| Kind | Does | Reply |
|---|---|---|
| `aether.handle.publish` | stash bytes under a fresh id | `HandlePublishResult` — the id |
| `aether.handle.release` | drop a reference | `HandleReleaseResult` |
| `aether.handle.pin` / `unpin` | toggle the durable keep-flag | `HandlePinResult` / `HandleUnpinResult` |
| `aether.handle.describe` | summarize the store | `HandleDescribeResult` |

## How to use it

**From a capability or component.** Most of the time you do nothing special: give
a kind a `Ref<K>` field and the substrate resolves any handle into a plain `K`
before your handler runs, so you write the handler exactly as you would for an
inline value. When you want to hold or keep a value yourself, the `aether.handle`
mailbox is the surface — publish bytes to get an id, pin one to keep it across
restart, release when you're done. A handler promises nothing about replies, so
treat a handle's resolution like any other mail: it arrives, or it resolves to its
kind's `Err`.

**From an agent over MCP.** `describe_handles(engine_id, max?)` is the window into
the store: total entry count, in-memory and on-disk bytes against the disk budget,
the pinned count, and the top entries by size and by recency. It's how you answer
"why is the store at its cap" without reaching into the machine. Handle ids come
back as tagged strings (the `hdl-XXXX-XXXX-XXXX` form) — opaque tokens you hand back
verbatim.

## How to extend or reuse it

The seam is the `Ref<K>` field. A new pipeline kind adopts `Ref<K>` on the fields
where a value should be able to travel by reference — large payloads, or results
produced a step earlier. A field typed `Ref<K>` becomes a slot the substrate
resolves transparently and, downstream, a slot a [DAG](dag.md) edge
can fill. Existing kinds that don't use `Ref` are untouched: the substrate's
field-resolution walk is a no-op on a kind with no reference fields, so adoption
is per kind, per field, with no migration.

The persistent half extends through pinning rather than new machinery. A workflow
that wants a value to outlive the process pins it; a content-addressed transform
output persists on its own because its id is reproducible. The store grows until
its disk budget, then evicts the unreferenced and unpinned — so "what survives" is
a property you set with pins.

## Where to read more

- Handles as a primitive and the store's refcount / LRU / pinned model —
  [ADR-0045](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0045-computation-dag-and-typed-handles.md);
  content-addressed ids —
  [ADR-0048](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0048-transforms-and-content-addressed-handles.md);
  the on-disk layout, eviction, and schema-mismatch invalidation —
  [ADR-0049](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0049-persistent-handle-store.md).
- `Ref<K>` as a schema arm, and where `HandleId` sits among the typed ids —
  [The type system](../foundations/type-system.md).
- The DAG that produces and consumes most handles —
  [The computation DAG](dag.md).
- The `describe_handles` tool in context — [The MCP harness](../mcp-harness.md).
