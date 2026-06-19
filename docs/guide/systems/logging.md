# Logging

> **Governing ADR:** [ADR-0081](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0081-decentralized-per-actor-log-storage.md)
> (decentralized per-actor log storage). The model — a bounded ring per actor,
> queried by mailbox name, fed only by in-actor `tracing::*` events — is
> **stable**.

Logs in aether are **decentralized**: each actor owns its own bounded log ring,
and you read one actor's logs by querying that actor's mailbox by name. There is
no central log store. Log storage scales with the number of actors and an actor's
ring dies with the actor, so the question a log query answers is narrow and
precise — "what did *this* actor say" — not "what happened somewhere in the
engine."

The one thing to know before anything else: **only `tracing::*` events emitted
inside an actor's handler land in a ring.** Anything the host emits outside a
dispatched handler — substrate boot, the scheduler thread, the panic hook — goes
to stderr and never enters any ring. So when a log line you expected is missing
from `actor_logs`, the first thing to check is whether it was emitted from inside
an actor at all.

## Why it exists

A central log buffer makes every actor pay a flush hop — at N actors times the
tick rate, that's a steady stream of envelopes carrying nothing but log entries
to one shared ring. Giving each actor its own ring removes that hop entirely: an
event lands directly in the ring of the actor that emitted it, where a query will
find it, with no mail in between. Storage follows the actors — a short-lived actor
carries a small ring and frees it on teardown, and no single structure grows
without bound as actors come and go.

Reading stays decentralized to match. Rather than a substrate-side aggregator
walking the actor registry, each actor answers a query against its own ring and a
caller composes a cross-actor view client-side if it wants one. One query, one
actor, one round-trip — and the actor model's boundary stays intact, because no
capability reaches across actors to read another's state.

## What it does

**Each actor owns an `ActorLogRing`.** It's a bounded FIFO of log entries living
in the actor's own slots. An entry carries its level, target, message, a
millisecond timestamp, and a per-actor `sequence` number that starts at 1 and
increases monotonically. The actor's own dispatcher thread is the sole writer, so
the ring needs no lock.

**In-actor events land in the ring; host events go to stderr.** The tracing layer
the substrate installs routes each event by where it fired:

- A `tracing::*` event emitted **inside a dispatched handler** lands in that
  actor's ring. A loaded wasm component is no exception — its guest `tracing::*`
  events cross the FFI on the component's dispatcher thread, so the same layer
  catches them and they land in the component's ring.
- An event emitted **outside any actor** — substrate boot, the scheduler, the
  panic hook — hits **stderr only** through the registered formatting layer. It
  enters no ring and surfaces in no query.

That split is the page's main gotcha and the first thing to reach for when a line
is missing: a host-side log is real, it's on stderr, and it was never going to
appear in a ring.

**`AETHER_LOG_FILTER` filters at emit time.** The subscriber reads an `EnvFilter`
from `AETHER_LOG_FILTER` (the same `target=level` directive syntax `RUST_LOG`
uses), defaulting to `info`. An event the filter rejects is never recorded — it
reaches neither a ring nor stderr. That's the emit-side gate; the query-side
`level` filter below narrows what an already-recorded ring returns.

**The ring is a bounded window, not a durable log.** Once the ring is at
capacity, the oldest entry is evicted to make room — under sustained load, or
simply with enough elapsed time, an actor's earliest entries are gone. The same
trace-ring caveat applies that [Tracing & settlement](tracing-and-settlement.md)
covers for its rings: read promptly, and don't count on reconstructing something
from a high-volume burst minutes ago. A query reports a `truncated_before` cursor
when eviction dropped entries the caller hadn't yet seen, so a gap is visible
rather than silent.

**The query is `aether.log.tail`, inherited by every actor.** Every actor
answers `aether.log.tail { max, min_level, since }` through a framework-built-in
dispatch arm — no author writes a handler for it. The reply,
`aether.log.tail_result`, carries the matching entries oldest-to-newest, a
`next_since` cursor (the highest `sequence` returned), and the `truncated_before`
signal. `min_level` filters by level, `since` returns only entries past a cursor,
and `max` caps the count (`0` resolves to a default of 100, and anything above
1000 clamps to 1000).

## How to use it

**Over MCP — `actor_logs`.** The `actor_logs(engine_id, mailbox_name, max?, level?, since?)`
tool sends `aether.log.tail` to the named mailbox and returns its ring slice. The
surface:

- **`mailbox_name`** — any actor's mailbox is queryable, addressed by name. That
  includes chassis mailboxes (`"aether.audio"`, `"aether.render"`) and a loaded
  component by its full lineage name (`"aether.component/aether.embedded:camera"`).
- **`max`** — caps returned entries; defaults to 100, clamps to 1000.
- **`level`** — `trace` / `debug` / `info` / `warn` / `error`; filters
  server-side so a noisy ring returns only what you asked for.
- **`since`** — the prior reply's `next_since`. Thread it back to page forward
  past entries you've already read without re-fetching them.

Paging is the move you'll reach for on a busy actor: an `actor_logs` call returns
`next_since`, and passing it as the next call's `since` walks the ring forward in
chunks. A query against an unregistered mailbox name comes back as an error, not
an empty success — so a typo'd mailbox name reads as a failure you can see.

**From a component — emit with `tracing`.** An author writes `tracing::info!`,
`tracing::warn!`, and the like inside handlers; those calls are exactly what
populates the actor's ring. Nothing else is required — there's no log capability
to address, no batch to flush:

```rust
#[handler]
fn on_tick(&mut self, ctx: &mut FfiCtx<'_>, _t: Tick) {
    tracing::debug!(frame = self.frame, "advancing");
}
```

A `tracing::*` call from outside a handler — at module load, on a thread the
actor spawned — is a host-side event by the rule above, so it won't appear in the
ring. Keep diagnostic emits inside the handler if you want them queryable.

## How to extend or reuse it

The surface is intentionally fixed: every actor inherits the `aether.log.tail`
query arm, so a new actor — native capability or loaded component — is queryable
the moment it exists, with no wiring. What an author controls is what reaches the
ring: the `tracing::*` calls inside handlers, gated by `AETHER_LOG_FILTER` at the
substrate. Cross-actor aggregation, level normalization, or per-namespace dedup
isn't substrate policy — a caller composes a multi-actor view client-side by
querying each mailbox and merging, which is exactly what an agent does when it
walks several mailboxes over MCP.

The log rings have a sibling: the **trace rings**. Both are bounded per-actor
storage queried the same way — `aether.log.tail` for what an actor *said*,
`aether.trace.tail` for what a mail *caused*. They differ under saturation: a log
ring wraps immediately, dropping its oldest line; a trace ring grows toward a
configured ceiling first, because dropping a single trace event leaves a hole in
the tree it belongs to. They answer different questions and stay separate;
[Tracing & settlement](tracing-and-settlement.md) is the page for the trace side.

## Where to read more

- The decentralized model, the per-actor ring, and the crash-dump path —
  [ADR-0081](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0081-decentralized-per-actor-log-storage.md).
- The actor-aware tracing layer that routes in-actor versus host events —
  [ADR-0077](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0077-actor-aware-logging.md).
- The trace rings that sit beside the log rings, and the shared
  `tail` query shape —
  [Tracing & settlement](tracing-and-settlement.md) and
  [ADR-0086](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0086-decouple-settlement-from-trace.md).
- The `actor_logs` tool in the agent-facing surface — [The MCP harness](../mcp-harness.md).
