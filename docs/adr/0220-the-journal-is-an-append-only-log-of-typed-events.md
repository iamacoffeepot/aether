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

Collecting an event's citations without knowing its shape needed a new
`Cites` trait in `aether-data`, emitted by `#[derive(Storage)]`. Two
alternatives in the code cannot carry it. A record-sink walker cannot see
the kind: `Ref<K>` is tag-identical to `[u8; 32]` because `terminate_field_hash`
folds the path carry and the canonical schema bytes only, and a `Ref` inside a
container never reaches a `RecordWriter` at all (`contribute_container` writes
elements into a plain `Vec<u8>`). A label-tree walk has no leaf name to hang
the kind on. A missing `Cites` impl is a compile error, never a silently
unwalked field.

## Decision

- The journal is an append-only, single-writer SQLite log. It never deletes,
  rewrites, reorders, compacts, migrates, folds views, or runs reactors.
- Every event payload is an `aether_data::Storage` kind. The only append path
  is `Draft::of<K: Storage + Cites>(...)`. There is no public path from raw bytes into
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
  inserts staged blobs, verifies every citation, inserts every event with
  dense `seq` values, and commits. Durable before return. All-or-nothing.
  An empty batch writes nothing.
- `read(since, limit)` returns entries with `seq > since`, ascending, at most
  `limit`. A backend failure is `Err`, never a short result. `since` past the
  head is `Ok([])`.
- There is no writer lease, no wake/notify (watching is a polling loop over
  `read`), no verification, no export, no deletion.
- The artifact store stays raw and content-addressed with its landed schema
  (`digest`, `size_bytes`, `recorded_at_millis`, `bytes`) and knows nothing
  about kinds. A row is a blob. There is no kind column, no DDL change, and
  no migration: an existing journal file reopens, and blobs written before
  this change have no prefix and are simply blobs nobody can cite typed.
  The crate is pre-1.0 and there are no production journals.
- An artifact is an abstraction above the store: a blob whose bytes are an
  eight-byte `KindId` prefix followed by a payload. The digest is the store's
  ordinary `sha256` over the whole blob, so the digest covers the kind and a
  digest names one kind and one payload. The same payload under two kinds is
  two blobs with two digests, both stored.
- Two leaf kinds ship (opaque bytes, UTF-8 text) beside encoded kinds. Their
  ids are derived from their names like every storage kind, via
  `storage_kind_id_from_name`. Artifact kinds are append-only by discipline
  like entry kinds.
- `Ref<K>` is the only storable citation; `Digest` is a plain identity value.
  A batch of staged artifacts plus events is the only write. `append` is the
  only judge: inside the fence transaction it verifies that every cited blob
  exists and carries the expected prefix, and writes nothing on any refusal.
  Citations are collected by a derive-emitted `Cites` walk at draft
  construction, so the check does not depend on the writer. The store's only
  kind check is an eight-byte comparison.

## Consequences

- A fold is a pure function of `read` plus `decode`. Wall clocks and artifact
  bytes stay off that path unless a consumer asks for them by digest.
- Schema evolution of events is ADR-0059's problem, not a SQL migration.
- Single-writer is a caller convention; the fence (`expect_head`) is the
  only concurrency check.
- The crate is a native rlib over bundled SQLite. It is not a wasm guest.
- An artifact's digest is not the raw sha256 of its payload. This is the
  trade Git makes with its object header, and it is the price of the kind
  being covered by the hash rather than sitting beside it.
- The prefix is an id, not a name: a reader holding a blob learns which kind
  produced it only if it can map the id back.
- The store's only kind check is an eight-byte comparison, which is what
  keeps the kind vocabulary out of the journal.

## Tree

A tree is a directory: a map from name to entry. It is a `Storage` kind
(`bloomery.tree`) stored as an artifact. Its digest commits to every byte
beneath it, so repository state is one thirty-two byte value and two states
are equal when their roots are equal.

Four entry kinds:

- **File** — a citation of opaque bytes.
- **Executable** — the same citation with the one pure mode bit. Drop it
  and every script materializes non-runnable.
- **Symlink** — an inline path, not a blob. Git stores a link target as a
  blob because a Git tree entry can only hold a hash; our tree is an
  encoded kind, so the target sits in the node, costs no blob and no
  citation, and is still covered by the tree's digest.
- **Directory** — a citation of another tree. Forced by the citation rule:
  `Ref<Tree>` and `Ref<OpaqueBytes>` are different kinds and `append`
  checks the prefix.

A name is refused at construction and at decode unless it materializes
losslessly on Linux, default macOS, and Windows, and Git will accept it:
no `/` or `\`, no NUL, no controls or format/bidi characters, not `.` or
`..`, no trailing `.` or leading/trailing Unicode whitespace, no Windows
reserved characters or device stems, not `.git`, NFC only, 1..=255 bytes.
A path is a relative `/`-separated symlink target of at most 1024 bytes;
each segment is `.`, `..`, or a name. Two entries may not collide under
NFC plus `char::to_lowercase`. Over-refusal is safe; under-refusal is not.

Owner, timestamps, and the other permission bits are dropped on purpose,
as Git drops them. Build outputs are never entries in any tree; that is a
rule of the snapshot brick.

The vocabulary (`Digest`, `Ref`, leaf kinds, `Name`, `Path`, `Node`,
`Tree`) lives in `aether-bloomery-kinds` so the journal's SQLite store is
not on the cite path for the reactor, the Git projection, or WASM
programs.

## Programs

A program is a contract stored in the journal. An executor performs
executions of it. A transition records one execution. The three never
blur: the definition has no code, the executor is never stored, the
record is the only place they meet.

| Word | Meaning |
|---|---|
| program | a stored declaration: name, input kind, result kind, mode, intent. Identity is its digest. |
| execution | one performance of a program by an executor: staged blobs plus one result artifact |
| executor | a runtime type that can perform executions of the programs it claims; never stored |
| transition | the event recording an execution: program, input, result, executor |
| fault | the event recording an attempt that produced no execution |
| result | the one artifact an execution is rooted at; its kind is declared by the program. Programs may stage any blobs; all must be reachable from the result |

"Results describe the world, faults describe the attempt."

`Transition.input` and `Transition.result` are `Digest`, not `Ref<K>`,
because their kinds are known only from the cited `Program` at runtime.
A `Digest` field is a citation the store cannot type-check. `Digest`
therefore has the same leaf traits as `Ref<K>`, delegating to
`[u8; 32]`, plus a `Cites` impl that pushes nothing. Only the driver may
write a kind that carries one, and the driver checks both prefixes
against the declaration before append. This is the one deliberate bend
in the typed-citation rule.

Fault rules:

1. A fault is an attempt that ended without an `Execution<P>`. If `finish` ran, it is a result; if not, a fault.
2. Whatever the wrapped thing did (tests failed, compiler errored) is a result, expressed in the result kind.
3. Faults are about the attempt, never the subject. The reason set is closed.
4. An executor may return only `Refused`, `InputMissing`, `InputDecode`. The driver assigns the rest from outside. Executors never write events.
5. A fault carries no blobs. Bounded inline detail only; `String` fields are capped at 4096 bytes by a validated constructor on `FaultReason` (`FaultReason::refused(text)` truncates, never refuses).
6. One fault per attempt. A retry is a new event.
7. Faults are never memoized and say nothing about purity.
8. If a fault seems to need structure, the declaration's result kind is wrong. Faults are never widened.

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
- **Kind column on the artifacts table** — rejected; it puts the kind beside
  the bytes instead of inside them, and teaches the store a vocabulary.
- **Kind *name* prefix** — rejected; variable width and a second framing to
  pin when the id is already fixed width and already computed.
- **`Cites` derive on `#[derive(Schema)]` types** — rejected; the missing
  impl is a useful compile error.
