# ADR-0220: The Journal Is an Append-Only Log of Typed Events

- **Status:** Proposed
- **Date:** 2026-09-16

## Context

A consumer whose state is a pure fold over a log needs a durable sequence of
typed events. The fold never reads wall clocks; it reads order. Large payloads
(transcripts, gate logs, diffs, prompts) must not ride in those events by
value.

ADR-0059 is why events are `Storage` kinds: TLV records with content-hashed
field tags, so adding, removing, reordering, or declared-renaming fields
decodes without a migration. ADR-0118 owns the wire format used inside a
storage leaf; the journal does not invent a second codec.

The journal knows nothing about the system that will use it.

## Decision

- The journal is an append-only, single-writer SQLite log. It never deletes,
  rewrites, reorders, compacts, migrates, folds views, or runs reactors.
- Every event payload is an `aether_data::Storage` kind. The only append path
  is `Draft::of<K: Storage>(...)`. There is no public path from raw bytes into
  the log. The journal records the kind name, not the kind's shape.
- Kinds are append-only by discipline: a breaking shape change is a new kind
  name; names are never reused; old types stay in the tree.
- The entry envelope is table columns (`seq`, `kind`, `cause`,
  `recorded_at_millis`, `bytes`). Adding a forgotten column later is a SQL
  default, not a migration. `seq` is dense, starts at 1, and is identity and
  fence. `recorded_at_millis` is for people and consoles; a fold never reads it.
- `open*` creates the `entries` table and `kind` / `cause` indexes if absent,
  sets `journal_mode = WAL` on file-backed databases only, and sets
  `synchronous = FULL`.
- `append` is one `BEGIN IMMEDIATE` transaction that reads the head, returns
  `HeadMoved { actual }` if the fence is stale (nothing written), otherwise
  inserts every draft with dense `seq` values and commits. Durable before
  return. All-or-nothing. An empty batch writes nothing.
- `read(since, limit)` returns entries with `seq > since`, ascending, at most
  `limit`. A backend failure is `Err`, never a short result. `since` past the
  head is `Ok([])`.
- There is no writer lease, no wake/notify (watching is a polling loop over
  `read`), no verification, no export, no deletion.
- Artifacts are content-addressed rows in the same database (`digest`,
  `size_bytes`, `recorded_at_millis`, `bytes`), referenced from events by
  digest. `put_artifact` is idempotent (`INSERT OR IGNORE`); a put whose event
  never lands leaves a harmless row that a retry re-uses. Artifacts are never
  deleted. No atomic event-plus-artifact write.

## Consequences

- A fold is a pure function of `read` plus `decode`. Wall clocks and artifact
  bytes stay off that path unless a consumer asks for them by digest.
- Schema evolution of events is ADR-0059's problem, not a SQL migration.
- Single-writer is a caller convention; the fence (`expect_head`) is the
  only concurrency check.
- The crate is a native rlib over bundled SQLite. It is not a wasm guest.

## Alternatives considered

- **Hash chain over entries (digest / prev / verify)** — deferred; no consumer
  asks to verify the log yet.
- **Kind-registration events recording schemas** — deferred; the journal does
  not know shapes, and ADR-0059 already carries field tags in the bytes.
- **Writer lease** — deferred; a single process owns the file for now.
- **In-memory second backend behind a Store trait** — deferred; tests use
  SQLite `:memory:`.
- **Idempotency keys** — deferred; land with the driver that retries.
- **Genesis event** — deferred; `Seq(0)` as the empty head is enough.
