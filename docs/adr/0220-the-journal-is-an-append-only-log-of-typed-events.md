# ADR-0220: The Journal Is an Append-Only Log of Typed Events

- **Status:** Proposed
- **Date:** 2026-09-16
- **Amended:** 2026-09-24 — tree entry names are unique byte for byte; the NFC-plus-case-folding collision rule is dropped, because no tree is written to a host directory (ADR-0237).

## Context

A consumer whose state is a pure fold over a log needs a durable sequence of
typed events. The fold never reads wall clocks; it reads order. Large payloads
(transcripts, gate logs, diffs, prompts) must not ride in those events by
value.

ADR-0059 is why events are `Storage` kinds: TLV records with content-hashed
field tags, so adding, removing, reordering, or declared-renaming fields
decodes without a migration. ADR-0118 owns the wire format used inside a
storage leaf; the journal does not invent a second codec.

The journal does not know the system that will fold it, and it does not
register a kind vocabulary. A generic head is a mutable slot that points at
immutable content of some runtime kind. That move cannot store a typed
`Ref<K>`: the kind is not fixed on the event type, so the destination is a
bare `Digest` plus a `KindId`. Derived `Cites` does not see a `Digest`. This
revision records that exception (one recognized event kind, checked inside
`append`) before implementation. Status remains Proposed.

Collecting an event's citations without knowing its shape needed a new
`Cites` trait in `aether-data`, emitted by `#[derive(Storage)]`. Two
alternatives in the code cannot carry it. A record-sink walker cannot see
the kind: `Ref<K>` is tag-identical to `[u8; 32]` because `terminate_field_hash`
folds the path carry and the canonical schema bytes only, and a `Ref` inside a
container never reaches a `RecordWriter` at all (`contribute_container` writes
elements into a plain `Vec<u8>`). A label-tree walk has no leaf name to hang
the kind on. A missing `Cites` impl is a compile error, never a silently
unwalked field. The recognized head-move check is not that walk: it decodes
`bloomery.head_moved` and looks up `to`.

## Decision

- The journal is an append-only, single-writer SQLite log. It never deletes,
  rewrites, reorders, compacts, migrates, folds views, or runs reactors.
  Current head bindings stay out of SQLite; they are an index over events.
- Every event payload is an `aether_data::Storage` kind. The only append path
  is `Draft::of<K: Storage + Cites>(...)`. There is no public path from raw bytes into
  the log. The journal records the storage `KindId` (`K::ID`), not the kind's shape, except
  that it decodes the one recognized kind `bloomery.head_moved` so it can
  validate that event's destination.
- Kinds are append-only by discipline: a breaking shape change is a new kind
  name; names are never reused; old types stay in the tree.
- The entry envelope is table columns (`seq`, `kind`, `cause`,
  `recorded_at_millis`, `bytes`). Adding a forgotten column later is a SQL
  default, not a migration. `seq` is dense, starts at 1, and is identity and
  fence. New `kind` values are exact eight-byte little-endian `KindId` BLOBs.
  Old file-backed journals with TEXT kind names reopen without a migration:
  reads hash those names with `storage_kind_id_from_name`, including names the
  current code does not recognize. The old TEXT-affinity column also accepts
  new bound BLOB values. Malformed BLOBs, invalid UTF-8 names, and other
  storage classes are corruption. `recorded_at_millis` is for people and
  consoles; a fold never reads it.
- `open*` creates the `entries` table and `kind` / `cause` indexes if absent,
  sets `journal_mode = WAL` on file-backed databases only, and sets
  `synchronous = FULL`.
- `append` is one `BEGIN IMMEDIATE` transaction that reads the log head,
  returns `AppendError::HeadMoved { actual }` if the fence is stale (nothing
  written), otherwise inserts staged blobs, verifies every citation, decodes
  each `bloomery.head_moved` event as `RecordedHeadMove` and validates `to`
  against the recorded head kind,
  inserts every event with dense `seq` values, and commits. Durable before
  return. All-or-nothing. An empty batch writes nothing. The recognized-event
  check runs after staged artifacts are inserted and before events commit.
  It applies to both `Batch::push_event` and `Batch::push_draft`; a missing
  `Cites` walk cannot stand in for it. Validation is existence plus the
  eight-byte kind prefix. No target-kind registry and no target-payload
  decode. `Journal::head()` remains the log sequence (`MAX(seq)`, or `Seq(0)`
  when empty). A named-head move is a different concept from
  `AppendError::HeadMoved` and does not rename that error.
- `read(since, limit)` returns entries with `seq > since`, ascending, at most
  `limit`. A backend failure is `Err`, never a short result. `since` past the
  head is `Ok([])`.
- `JournalIdentity` is minted by each constructor, compared by allocation,
  and stable when the journal moves. It is not persisted and is not a SQL
  column.
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
  construction, so the typed-`Ref` check does not depend on the writer. The
  store's only kind check is an eight-byte comparison. The persisted
  `bloomery.head_moved` destination is a bare `Digest`; the derived walk
  pushes nothing for it. The recognized-event lookup is the citation check
  for that field. Typed `HeadMoved<K>` holds a `Ref<K>` in memory; that
  still does not cite through `Cites`.

## Consequences

- A fold is a pure function of `read` plus `decode`. Wall clocks and artifact
  bytes stay off that path unless a consumer asks for them by digest.
  Typed binding lookup is `Heads`, a fold over `bloomery.head_moved` (and
  historical `bloomery.program.head_moved`) in seq order, not a second store.
- Schema evolution of events is ADR-0059's problem, not a SQL migration.
- Single-writer is a caller convention; the fence (`expect_head`) is the
  only concurrency check. Existing cause-envelope behavior is unchanged.
  A later reactor owns move intents, cause assignment, and re-derivation on
  fence failure. Writer authentication, cause-policy enforcement, and
  implicit retry of move batches are not this brick.
- The crate is a native rlib over bundled SQLite. It is not a wasm guest.
- An artifact's digest is not the raw sha256 of its payload. This is the
  trade Git makes with its object header, and it is the price of the kind
  being covered by the hash rather than sitting beside it.
- The prefix is an id, not a name: a reader holding a blob learns which kind
  produced it only if it can map the id back.
- The store's only kind check is an eight-byte comparison. The journal now
  also decodes one event kind; it still does not host a kind registry.

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

*(Amended 2026-09-24: two entries collide only when their names are equal
byte for byte, as Linux, tar, and Git enforce. The case-folding rule served
writing a tree onto a case-insensitive filesystem; since ADR-0237's
amendment no tree is written to a host directory, and a Debian userland
ships names that fold together (`xt_CONNMARK.h` beside `xt_connmark.h`).
Each name is still NFC on its own, so canonically equivalent spellings stay
byte-equal and still collide. Relaxing only admits more trees, so every
existing tree keeps its digest.)*

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

A `ProgramName` is an immutable label stored inside the declaration. It
is not a head. Binding a mutable name to a program is a head move,
identified by `(Program::ID, name)`. `ProgramName` and `ExecutorName`
keep their current dotted-ASCII rules and persisted encoding. Existing
`ProgramHeadMoved` (`bloomery.program.head_moved`) history stays
decodable under that name and encoding (`name: ProgramName`,
`program: Ref<Program>`). The `Heads` fold treats each as a move of
`RecordedHead::new(Program::ID, old.name)`. New writes use
`bloomery.head_moved`.

`Transition.input` and `Transition.result` are `Digest`, not `Ref<K>`,
because their kinds are known only from the cited `Program` at runtime.
A `Digest` field is a citation the store cannot type-check. `Digest`
therefore has the same leaf traits as `Ref<K>`, delegating to
`[u8; 32]`, plus a `Cites` impl that pushes nothing. Only the driver may
write a kind that carries one, and the driver checks both prefixes
against the declaration before append. That is a deliberate bend in the
typed-citation rule, judged by the driver because the expected prefixes
live on the cited `Program`, not on the event. A head-move destination is
the other bend: its expected prefix is the recorded head kind on the
event itself, so the journal judges it. Derived `Cites` sees neither bare
digest.

Fault rules:

1. A fault is an attempt that ended without an `Execution<P>`. If `finish` ran, it is a result; if not, a fault.
2. Whatever the wrapped thing did (tests failed, compiler errored) is a result, expressed in the result kind.
3. Faults are about the attempt, never the subject. The reason set is closed.
4. An executor may return only `Refused`, `InputMissing`, `InputDecode`. The driver assigns the rest from outside. Executors never write events.
5. A fault carries no blobs. Bounded inline detail only; text is a `Detail`. Truncation happens in `Detail::new` (cut at the last char boundary at or before 4096 bytes); decode of a stored blob past the cap refuses.
6. One fault per attempt. A retry is a new event.
7. Faults are never memoized and say nothing about purity.
8. If a fault seems to need structure, the declaration's result kind is wrong. Faults are never widened.

## Heads

A head is a mutable slot identified by `(target KindId, name)`. Different
target kinds may use the same name independently. The typed handle is
`Head<K>`; its current value is a `Ref<K>`; a recorded change is
`HeadMoved<K>`. The first move establishes the slot; later moves replace
its current value. There is no registration, deletion, rename, or
null-target operation. A move to the value the slot already holds is still
a recorded move. Several moves to one head in a batch take effect in event
order; last wins. Pointing back at an older target is legal.

Application source declares a typed head with the ordinary constructor:

```rust
const MAIN: Head<Tree> = Head::new("main");
static BUILD: Head<Program> = Head::new("build");
```

`Head<K>` owns a private `Cow<'static, str>` and `PhantomData<fn() -> K>`.
`Head::new` is a `const fn` that borrows the literal and panics on an
invalid name; in a `const` or `static` initializer that fails compilation.
Decoding owns the string. There is no macro, registry, or unchecked
constructor. `as_str`, `MAX_BYTES`, and `kind()` (`K::ID`) are the rest of
the identity surface. Clone, Eq, Ord, Hash, and Debug do not require those
traits on `K`. A static `Head<K>` is `Sync` even when `K` is not.

`MAIN.move_to(tree_ref)` produces `HeadMoved<Tree>` directly. Accessors
return the head and `Ref<K>` target infallibly after a successful
decode. A head is not an immutable content reference and contributes no
citation to its current target.

Journal and view decoding use explicit runtime forms: `RecordedHead`
(kind plus name, checked construction) and `RecordedHeadMove` (recorded
head plus destination digest). Typed and recorded heads encode the
same complete kind-plus-name identity. Typed decoding rejects a recorded
kind different from `K::ID` with a structured codec error, never a panic.
Journal validation uses the recorded head kind; no registry or runtime
target-type dispatch is needed.

The persisted event keeps the existing `bloomery.head_moved` `KindId` and
the flat leaves `target_kind`, string `head`, and digest `to`. Standalone
head storage includes kind and name; the event codec flattens that
identity into those fields. Typed `HeadMoved<K>` and `RecordedHeadMove`
share `Kind::ID` / `NAME` and that schema. A private codec-only DTO
keeps the flat derived shape. Schema labels are `Head` / `RecordedHead`;
`StorageError::Invariant` kind is `"Head"`. Every event decode path,
including containers and wire, validates names.

`to` is a runtime citation: a digest whose expected prefix is the recorded
head kind. The ordinary `Cites` walk does not see it. `append` decodes
`RecordedHeadMove` from the draft and judges `to` the same way it judges a
typed `Ref`: the blob exists, and its eight-byte prefix equals the
recorded kind. A typed `Ref<K>` used to build the event does not prove
that content exists; the journal always rechecks the persisted event.

`Journal::head()` is the log sequence. Current bindings are not SQLite
state. `Heads` is the typed index: last move per recorded identity in seq
order, looked up as `heads.get(&MAIN) -> Option<Ref<Tree>>`. It is not
another source of truth.

A head name is validated text, not a path and not a program label.
It accepts 1–128 UTF-8 bytes, rejects characters for which
`char::is_whitespace()` or `char::is_control()` is true, and otherwise
preserves bytes exactly. Length is checked first; a character that is both
control and whitespace is control. Equality, hashing, and ordering are
case-sensitive and normalization-free. Punctuation has no special
semantics. There are no filesystem restrictions and no extra
Unicode-format-character blacklist. A head name is independent of any
label stored inside its target. `ProgramName` and `ExecutorName` keep
their current rules and encoding; tree `Name` keeps its materialization
rules.

The typed write path uses the existing batch and append traits:

```rust
const MAIN: Head<Tree> = Head::new("main");
let event: HeadMoved<Tree> = MAIN.move_to(tree_ref);
batch.push_event(&event, cause)?;
journal.append(expected_seq, &batch)?;
```

Existing `ProgramHeadMoved` history stays decodable under
`bloomery.program.head_moved` and its current fields. The `Heads` fold
treats each as a move of `RecordedHead::new(Program::ID, old.name)`.
Current non-compatibility examples and callers write `bloomery.head_moved`.

Current heads can later serve as retention roots. Neither pruning nor
garbage collection is in this work.

## Views

A view is a fold over a contiguous journal prefix. It is not stored in
SQLite and it is not a second source of truth. The journal still does
not fold views; `aether-bloomery-view` consumes supplied `Entry` values
and does not own a `Journal`.

```rust
pub trait View: 'static {
    type Error: core::error::Error + 'static;
    fn empty() -> Self;
    fn cursor(&self) -> Seq;
    fn advance(&mut self, entries: &[Entry]) -> Result<(), Self::Error>;
}
```

`empty` starts at `Seq(0)`. A successful `advance` consumes every contiguous
entry in the batch, including kinds the view ignores. Views read only those
entries and their own state. There is no `Clone` / `Send` / `Sync` bound.
`Heads` keeps `apply` / `get` / `new` / `cursor` and implements `View` by
folding each entry through `apply`.

The former native `ViewRegistry` owned a `Journal` and cached one instance
per `TypeId`. Authors selected views with `views().at().with()`, and the
registry caught each fold up to an exact prefix, poisoned failed slots, and
refused a replaced journal via `JournalIdentity`. That author-facing
registry is retired: it had no remaining production consumer, and generated
reactor actors maintain the views they need. Cursor checks, poisoning, and
identity binding are the owner's responsibility. `aether-bloomery-view`
depends on kinds and data only, not the journal or program crates.

Historical views, persisted checkpoints, eviction, background updates,
Causes, reactor scheduling, and performance issue #6111 are deferred.
Batch input is an interface contract, not a performance claim.

## Alternatives considered

- **Hash chain over entries (digest / prev / verify)** — deferred; no consumer
  asks to verify the log yet.
- **Kind-registration events recording schemas** — deferred; recognizing
  `bloomery.head_moved` is not a schema registry, and ADR-0059 already
  carries field tags in the bytes.
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
- **Current-head table in SQLite** — rejected for this brick; the log is
  the source of truth and `Heads` is an index over it.
- **Typed `Ref<K>` on the persisted head-move event** — rejected; the
  target kind is runtime, the same reason `Transition` stores `Digest`.
- **Letting derived `Cites` stand in for the destination check** —
  rejected; `Digest` pushes nothing, and `Batch::push_draft` can carry the
  event without a typed walk.
- **Renaming `AppendError::HeadMoved`** — rejected in this work; the
  sequence fence and a named-head move are distinct.
- **Filesystem-safe or NFC-normalized head names** — rejected; a head
  name is not a path and is independent of tree `Name` and `ProgramName`.
- **Permanently borrowing `Journal`** — rejected with the retired
  `ViewRegistry`; a later owner of a fold is not this brick.
- **Clear caches on every mutable journal access** — rejected with the
  retired `ViewRegistry`; it defeated incremental reuse.
- **Separate file reader or shared backend redesign** — rejected; it
  excludes current in-memory consumers or expands this change.
- **Snapshot cloning and implicit rewind** — rejected with the retired
  `ViewRegistry`; unnecessary cost and undefined historical semantics.
- **Automatic rebuild after fold failure** — rejected with the retired
  `ViewRegistry`; it hides assumption failures and risks repeated
  corruption.
- **Keeping or relocating `ViewRegistry`** — rejected; no remaining
  production consumer, and generated reactor actors maintain their own
  views.
