# ADR-0238: Engine Blob Store of Immutable Checked-In Bytes Shared as Blob Values

- **Status:** Proposed
- **Date:** 2026-09-25
- **Amended:** 2026-09-25 — decisions 9, 10 and 11: a closure member is read only through `ClosureArtifact::load(expected)`, which verifies the member's claimed digest before returning any byte and is `blob_read_p32`'s production consumer; `Env::open` and its `VerifyingReader` are deferred until a program streams a large opaque input. This supersedes the `Env::open` clause of the amendment below.
- **Amended:** 2026-09-25 — decisions 3 and 11, wording: an empty attachments field is two words, not one; `Env::open` returns a `VerifyingReader` that streams through a `BlobReader` and checks the member's claimed digest at end of stream.
- **Amended:** 2026-09-25 — decision 3: a tag-1 field is valid only beside a matching attachment; one carried by a blob-free sender or a wire `Call` is refused at its recipient's decode; intra-cluster guest mail keeps each named value alive until the child's dispatch; a guest's reply to a session or engine mailbox is an egress path.
- **Amended:** 2026-09-25 — decisions 3, 4 and 12: a send resolves the tag-1 hashes already in its payload against the sender's own blobs and attaches their entries, so a raw forward stays shared and a guest shares a held value by hash on send; an unresolved hash refuses the send at the sender. Supersedes the guest share-on-send follow-on below.
- **Amended:** 2026-09-25 — decisions 2, 3, 4, 9 and 12: a tag-1 field carries the blob's hash; a guest `Blob` is the same value with an FFI-wrapper backing that takes a hold when built (`blob_hold_p32`, replacing `blob_len_p32`), and delivery pins attached entries only for the receive call, so `receive_p32` is unchanged; the egress rewrite keys on attachments, not a per-kind flag; file and journal writes use the plain encoder; the engine recovers a store entry by downcast; rehydrate re-grant and guest share-on-send are follow-ons.
- **Amended:** 2026-09-25 — decisions 2, 3, 9 and 12: `BlobReader` lives in `aether-data`; `is_empty` is added; the `read_at` and `blob_drop_p32` contracts are stated; teardown releases a guest's holds without a leak warning; envelope attachments hold store entries.
- **Amended:** 2026-09-25 — decisions 2–6 and 9–12 rewritten: `Blob` is a value of immutable bytes, not a handle. In-process mail shares it through the store; every other path writes its bytes. Supersedes the reference designs in the amendments below, which stay as history.
- **Amended:** 2026-09-25 — decision 6: persisting a handle is a documented misuse, not enforced; later directions recorded.
- **Amended:** 2026-09-25 — decision 10: `ReadArtifact` replies carry handles too; `aether.fs` recorded as a blob consumer.
- **Amended:** 2026-09-25 — decisions 2, 3, 9, 11: `BlobRef` is opaque (identity plus an engine-supplied keep-alive, no methods); reading goes through `ctx.read_blob`; guest imports are `blob_len_p32`, `blob_read_p32` and `blob_drop_p32`; the guest SDK's `std` feature is new; a general resource-type pattern is recorded.
- **Amended:** 2026-09-25 — decisions 2, 3, 4, 9: a handle is the blob's hash (`BlobRef`, cloned and dropped like an `Arc`), not a per-actor index; `BlobRef` lives in `aether-data`; there is no release verb.

Builds on [ADR-0038](0038-actor-per-component-dispatch.md) and
[ADR-0087](0087-blob-unit-of-dispatch.md) (one handler at a time per actor,
enforced by the actor `Mutex` in `DispatcherSlot`),
[ADR-0230](0230-proven-actor-references.md) (mints only in the reference
module; a type that cannot keep its invariant across serialization is not a
kind field), and [ADR-0233](0233-engine-only-mail.md) (per-kind refusal at the
process boundary, `is_engine_only`). Learns from the retired handle store of
[ADR-0045](0045-computation-dag-and-typed-handles.md),
[ADR-0048](0048-transforms-and-content-addressed-handles.md) and
[ADR-0049](0049-persistent-handle-store.md), the overturned
[ADR-0120](0120-sharded-filesystem-actors-as-the-engine-byte-store.md), and
[ADR-0133](0133-reply-based-stream-handles-for-the-http-server-data-phase.md).
Motivated by [ADR-0226](0226-native-bundle-driver.md) decision 10
(`ReadClosure`, `ClosureLimit::MAX_BYTES`) and issue #6719.

## Context

Large bytes cross mail as inline `Vec<u8>` fields. Every send copies them
into a mail buffer. Every wasm delivery copies them again into linear memory
(`Component::deliver` in
`crates/aether-substrate/src/actor/wasm/component/dispatch.rs:87-120`), and
the guest decode copies them a third time. Nothing lets two actors share one
resident copy.

### Where large bytes cross today

| Payload | Kind / field | Anchor |
|---|---|---|
| File contents | `aether.fs.read_result` `Ok.bytes` | `crates/aether-fs/src/kinds.rs:92` |
| Journal artifact | `read_artifact_result` `Found.bytes` | `crates/aether-bloomery-kinds/src/journal/mod.rs:132` |
| Program closure | `read_closure_result` `Found.artifacts`, `ClosureArtifact.bytes` | `crates/aether-bloomery-kinds/src/journal/closure.rs:119`, `crates/aether-bloomery-kinds/src/program/invoke.rs:16` |
| Program input | `aether.bloomery.program.invoke` `closure` | `crates/aether-bloomery-kinds/src/program/invoke.rs:57` |
| Program outputs | `EncodedArtifact.bytes` | `crates/aether-bloomery-kinds/src/journal/artifact.rs:44` |
| Process output | `aether.process.run_result` `stdout` / `stderr` | `crates/aether-process/src/kinds.rs:97`, `:99` |
| Texture pixels | `create_texture` / `update_texture` `pixels` | `crates/aether-render/src/kinds.rs:220`, `:256` |
| Geometry | `create_geometry` `vertices` / `indices` | `crates/aether-render/src/kinds.rs:347`, `:349` |
| Component wasm | `aether.component.load` / `replace` `wasm` | `crates/aether-kinds/src/lib.rs:644`, `:823` |
| Stored wasm | `resolve_component_result` `Ok.wasm` | `crates/aether-kinds/src/lib.rs:550` |
| Frame capture | `capture_frame_result` `Ok.png` | `crates/aether-kinds/src/lib.rs:1013` |
| HTTP bodies | `fetch_result` `Ok.body`, server request / response `body` | `crates/aether-http/src/kinds.rs:141`, `:170`, `:183` |
| Guest state | `save_state_p32` (FFI, 1 MiB cap) | `crates/aether-substrate/src/actor/wasm/host_fns.rs:40`, `:539` |

The caps that bound these today are the frame cap (`MAX_FRAME_SIZE`, 64 MiB,
`crates/aether-codec/src/frame.rs:42`), the guest delivery cap
(`MAX_DELIVERABLE_MAIL_BYTES`, 64 MiB,
`crates/aether-substrate/src/actor/wasm/component/mod.rs:47`), and the closure
ceiling (`ClosureLimit::MAX_BYTES`, 16 MiB,
`crates/aether-bloomery-kinds/src/journal/closure.rs:26`). aether-mcp spills
large reply leaves to host files (`crates/aether-mcp/src/tools/bytes.rs:279-316`).
The only indirections are path-addressed: component upload (`staged_path`)
and caps that fetch through `aether.fs` themselves (audio, text `load_font`,
`aether.kit.mesh.load`).

### One handler at a time per actor

An actor-local table with no lock is sound only if one handler touches it at
a time. The code holds that, with one placement condition.

- Every native cap and every wasm trampoline runs through one
  `DispatcherSlot`. Its state is `actor: Mutex<Option<Box<A::State>>>`
  (`crates/aether-substrate/src/actor/native/slot/dispatcher.rs:124`), held
  for the whole drain. That `Mutex` is the exclusion. The `SlotState`
  run-token is only a scheduling filter: after `mark_idle` a second worker
  can seize the slot and then block on the same `Mutex` (`dispatcher.rs:64-75`).
- ADR-0087 workers run in parallel only across different recipients.
- `#[router(shared)]` (ADR-0136) routes to N replicas, and each replica is a
  separate load with its own slot and `Store`. Replicas are separate table
  owners.
- ADR-0169 handler sets add a delegation tail to the same dispatch. They are
  not a concurrency axis.
- Wasm inline children (ADR-0114) share their parent's instance and slot.
  Parent and children are one table owner.
- Off-thread work (`spawn_inherit`, `dispatch_blocking*`,
  `crates/aether-substrate/src/actor/native/ctx/offload.rs:46`, `:115-194`)
  takes `Send + 'static` closures that cannot borrow actor state. Results
  come back through the binding and are picked up on the next handler turn.
- No host import blocks waiting for a reply, so no handler re-enters.

The condition: `NativeBinding`
(`crates/aether-substrate/src/actor/native/binding/mod.rs:84`) is reached from
other threads and guards its fields with `Mutex`es. A table stored there
would need its own lock. The table must live in state the slot's actor
`Mutex` guards. Pumped slots (desktop window and render) own their actor on
one thread with no `Mutex`; they are serial, so they qualify too. Inline
mailboxes (`RouteEndpoint::Inline`, `crates/aether-substrate/src/mail/mailer.rs:859-869`)
run on the sender's thread and may run concurrently, but they have no actor
state and so own no table.

### What the prior attempts teach

| ADR | What it did | Lesson |
|---|---|---|
| 0045 | Substrate-global refcounted store, `Ref<K>` = `{id, kind_id}` on the wire, LRU eviction, pins, parked mail | A global id is ambient: whoever learns it can read. Mail-based refcounts (`aether.handle.release`) and LRU at refcount zero cost a sink round trip per handle and still needed pins to avoid evicting live work. |
| 0048 | Content-addressed ids for native transform outputs | Content addressing is right for dedup. It stays the dedup key here, not the capability. |
| 0049 | Persisted the store to disk | Persistence duplicated `aether.fs` and was removed with its only producer. This store is in memory only. |
| 0120 | Handles as `{owner, token}` resolved by mailing a sharded fs actor | Resolving by mail puts a round trip and an actor on every read. Kept: no implicit byte transfers, bytes immutable once owned. |
| 0133 | Reply-based stream handles for HTTP | A per-instance handle table already exists: the wasm `ReplyTable` (`crates/aether-substrate/src/actor/wasm/reply_table.rs`), a generation-tagged slab that grows and never drops a held handle, moved across `replace` as `PendingReplies`. Its growth rule (grow, never drop a held entry) is the model for the guest's blob table, which is keyed by hash instead of an index and, unlike `PendingReplies`, starts empty in each instance (section 2). Its limit (instance-local, not transferable) is solved here by carrying the bytes' `Arc` in the envelope. |
| 0165 | Published views over `arc-swap`, staged effects | Readers of immutable published data never take the writer's lock. Blob bytes are the simplest case: immutable from check-in, so an `Arc` read needs no synchronization at all. |

## Decision

### 1. A native store, not an actor

```rust
// crates/aether-substrate/src/store/ (native only; never a mailbox)
pub struct BlobStore { /* hash -> Weak<BlobEntry>, resident byte gauge, reclaim sender */ }

pub struct BlobEntry {
    hash: BlobHash,     // the blob's identity and dedup key; knowing it grants nothing
    bytes: Box<[u8]>,   // immutable from check-in
}

impl BlobStore {
    pub(crate) fn check_in(&self, bytes: Box<[u8]>) -> Arc<BlobEntry>;
}
```

- In memory only. Never backed by files. A restart forgets every blob.
- Check-in only. Bytes are immutable once checked in. A change is a new
  check-in and a new value.
- Check-in hashes the bytes with BLAKE3. The hash is the blob's identity and dedup key, and
  it never matches the Bloomery journal's digests, which cover a kind prefix
  plus the payload, so it need not share their sha256. If the hash is already resident, check-in
  returns the existing entry and drops the new bytes (dedup).
- An entry is shared as an `Arc`. Reading its bytes takes no lock.
- The dedup index takes a short lock on check-in only. Reads, sends and
  delivery never touch it.

The entry is `Arc<BlobEntry>` holding a `Box<[u8]>`, not a bare `Arc<[u8]>`.
The index must hold weak references so that it does not keep bytes alive.
A `Weak<[u8]>` keeps the whole `Arc<[u8]>` allocation (header and bytes) until
the last weak reference drops, so a weak index over bare `Arc<[u8]>` would free
nothing. `Weak<BlobEntry>` pins only the small `BlobEntry` header, and the
boxed bytes are freed when the last strong reference drops.

### 2. A `Blob` is a value, not a handle

```rust
// crates/aether-data: one definition for every target; no store, no cfg, no FFI
pub struct Blob(Repr);                       // immutable bytes; Clone is cheap
enum Repr {
    Owned(Arc<[u8]>),                        // bytes in hand
    Shared(Arc<dyn BlobBacking>),            // an engine store entry, reached through the trait
}
pub trait BlobBacking: Send + Sync + 'static {
    fn len(&self) -> u64;
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> usize;   // at most buf.len(); 0 only at or past the end
    fn is_empty(&self) -> bool { self.len() == 0 }
}
impl From<Vec<u8>> for Blob { /* Owned */ }
```

`read_at` copies at most `buf.len()` bytes and may copy fewer: a guest backing
copies at most `MAX_READ_BYTES` per call. It returns `0` only at or past the
end, so a caller loops until it sees `0`.

A `Blob` means its bytes wherever it goes: in an actor, over the wire, in a
file, in a tool. Only its backing changes, and the backing is the engine's
business. Aether's one communication model treats every kind as data that can
go anywhere; a value type keeps `Blob` inside that model, where a handle that
names one process's memory would sit outside it. Erlang's reference-counted
binaries are the prior art: a binary is a value, shared by pointer inside a
node and sent as bytes between nodes, and no program ever holds a handle.

| Backing | Where it comes from | What reads do | What the last drop does |
|---|---|---|---|
| `Owned` | decoded from the wire, a file or JSON; built with `Blob::from` | read the slice | frees the buffer |
| `Shared`, native | the store: an in-process send interns an `Owned` value (section 3) | read the store's `BlobEntry` | frees or reclaims the entry (section 7) |
| `Shared`, guest | a guest's decode of a delivered tag-1 field: an FFI wrapper over the hash, whose construction takes one hold (`blob_hold_p32`) | `blob_read_p32` against the guest's table | `GuestHold::drop` gives the hold back (`blob_drop_p32`) |

- Retrieval never looks anything up. A `Blob` carries its bytes or a strong
  reference to them, so a read cannot find them missing.
- `aether-data` holds only the value and the trait. The store stays in the
  substrate, and the guest backing lives in `aether-actor`.
- A guest's table on `ComponentCtx` maps hash to (entry `Arc`, count of live
  `GuestHold`s). Delivery pins each attached entry there for the length of one
  receive call. Building a guest `Blob` takes a hold, which the pin or an
  existing hold admits, and dropping it gives the hold back. When the call
  returns the pins go, and only holds keep an entry. Counts equal live values
  however many times a guest decodes a mail, and a field it never decodes
  holds nothing past the call.
- The table is scoped to one instance: a `replace` starts the new instance
  with an empty table. Values carried in the saved state are written as bytes
  and come back `Owned`, within the state bundle's 1 MiB cap; granting them
  again on rehydrate waits for a consumer (Follow-on). References that died
  with the old instance's memory take their counts with them, so a `replace`
  cannot leak.
- Only the owning actor's handler touches its table. The table grows and never
  drops a held entry. Teardown releases every entry still counted and logs one
  debug-level count. The engine frees a guest's memory without running its
  `Drop`s, so a counted entry at teardown is the normal case for a guest that
  keeps blobs in its state, not a leak.

### 3. In-process mail shares; every other path writes bytes

The encoder decides whether a `Blob` field is shared or materialized. The
representation only says where the bytes live now.

```rust
trait Encoder {
    // default: write the bytes. Every codec gets this for free.
    fn blob(&mut self, value: &Blob) { /* tag 0, then length-prefixed bytes */ }
}
impl Encoder for EnvelopeEncoder {          // in-process mail: the only override
    fn blob(&mut self, value: &Blob) { /* intern if Owned, attach, write tag 1 + hash */ }
}
```

The `Blob` leaf's encode calls `blob`, and the derive and the container impls
pass the encoder through, so each `Blob` field reaches `blob` exactly once
wherever it nests. The field's binary form carries a one-byte tag:

```text
tag 0 → [len][bytes]          inline: every path out of the process, and every decode source
tag 1 → [32-byte hash]        in-process mail only; never leaves the process
```

| Step | What happens |
|---|---|
| Send | A send encodes with the envelope encoder, since most mail stays local. Each `Blob` field is interned into the store if it is `Owned`, attached to the envelope as its store entry (`attachments: Option<Box<[Arc<BlobEntry>]>>` on `Mail`, two words with a null pointer when empty), and written as tag 1 with its hash. Interning already yields the entry, and a guest delivery needs the entry, which a `Blob` hides. A send borrows its payload, so the sender keeps its values. |
| Resolve on send | A payload can already hold tag-1 fields: a raw forward of received bytes, or a guest's encode of a `Blob` it holds, which writes tag 1 with the hash (a guest's `Owned` value is written as tag 0). Before the send leaves the sender, the engine walks the kind's schema, resolves each such hash against the sender's own blobs (a guest's table: its pins and holds; a native actor: the attachments of the mail it is handling) and attaches the entry. A hash that resolves nowhere refuses the send at the sender with an error naming it. A sender that holds and pins no blob cannot carry a valid tag-1 field, so the walk runs only for senders that hold blobs, and blob-free senders pay nothing. Intra-cluster guest mail never reaches the host: the guest keeps each held value it names alive until the inline child's dispatch. |
| Deliver | A native recipient's decode matches each tag-1 hash to its attached entry and yields a `Shared` `Blob` over it. A guest recipient's table pins every attached entry for the receive call; the guest's decode reads the hash and builds its `Blob` over the FFI wrapper, taking a hold (section 2). `receive_p32` is unchanged, and the guest needs no delivery context. Fan-out delivers per recipient. |
| Egress | Any path that leaves the process rewrites each tag-1 field to tag 0 by copying in the bytes of the attachment its hash names: RPC reply-out (`crates/aether-rpc/src/server/runtime.rs:934`) and unresolved-recipient egress (`crates/aether-substrate/src/mail/mailer.rs:920`), and a guest's reply to a session or engine mailbox (`reply_mail_p32`). File and journal writes encode typed values with the plain encoder, so they write tag 0 by construction and never take an envelope payload. |
| Ingress | Nothing. Tag 0 decodes to an `Owned` value, interned only if it is later sent in-process. |

A tag-1 field is valid only beside a matching attachment. One that arrives without it (stored bytes raw-sent by a sender that holds no blob, which resolve on send does not walk, or a tag-1 byte in a wire `Call`) is refused by its recipient's decode (`DetachedBlob`); if it leaves the process unrewritten, the far decoder refuses it. A hash grants nothing, so the refusal is the whole consequence: a guest whose table already holds that hash gains only another value for bytes it can already read.

Blob fields are a schema variant, `SchemaType::Blob`: its binary form differs
from plain bytes by the tag, so the schema must say so. Every outside codec
treats it as bytes: the JSON codec (`encode_schema` / `decode_schema`) reads
and writes plain bytes, so MCP never sees a tag. The egress rewrite runs only
for mail whose envelope carries attachments: a tag-1 field is written only
beside an attachment (resolve on send guarantees it), so blob-free mail pays
nothing and no per-kind record is kept.

### 4. No lookup by hash

The BLAKE3 hash is internal: the store's dedup key and the guest table's key.
There is no fetch by hash, no store lookup by hash, and no public way to build
a `Shared` value. The guest's host imports take a hash but resolve it only
against the caller's own table, and resolve on send (section 3) resolves a
payload's hashes only against the sender's own blobs, so a guessed or logged
hash reaches nothing the caller does not already hold. Only the engine constructs `Shared` values (the
store's interning and delivery), behind a hidden constructor that the existing
mint scanner (`scripts/check-reference-mint.py`) confines. Anyone may build an
`Owned` value: it is just bytes. When a native `Shared` value is sent on, the
envelope encoder recovers its store entry through a hidden accessor to the
backing and an `Any` downcast to `BlobEntry`. `BlobEntry` is private to the
substrate, so only the engine can take that door; the alternatives are a
lookup by hash, which this section forbids, or copying the bytes into the
store again on every send.

### 5. Crossing a process: bytes, bounded by the frame limit

A `Blob` that leaves the process is its bytes, so kinds with `Blob` fields are
ordinary wire kinds and aether-mcp sends and receives them like any other. A
materialized value must fit one frame (`AETHER_MAX_FRAME_SIZE`). A larger one
is refused with an error naming its size and the limit, until chunked transfer
has a consumer. Inside one engine, which is where Bloomery moves large bytes,
nothing is copied.

### 6. Persistence writes bytes

Writing a `Blob` to a file, the journal or a log writes its bytes, and reading
it back yields an `Owned` value. Nothing about a `Blob` names one engine's
memory, so there is no persistence misuse to document or enforce.

Directions recorded for a later trait rework, none chosen.

- **A general pattern for types with a local form.** `Blob` is the first type
  with a universal form that every codec speaks (bytes) and a local form one
  encoder may use (a shared entry). The encoder hook of section 3 generalizes
  to a trait for that pair, with materialize as the default. `ActorRef` is the
  natural second case: universal form an address description, re-proved on
  receipt.
- **A general resource-type pattern** for things that genuinely cannot move,
  such as sockets and GPU textures: an opaque identity plus an engine-supplied
  keep-alive whose type decides what the last drop does, process-local by
  construction, and operated only through APIs that take it. Prior art: Erlang
  NIF resource objects, wasm component-model resources (`own<T>` /
  `borrow<T>`), FIDL `resource` types, Lua userdata with `__gc`, Rust
  `bytes::Bytes`'s per-instance vtable, and C++ `shared_ptr` with a custom
  deleter.
- **Liveness classes and encoder accept sets.** Every kind keeps one schema
  and carries a liveness class, derived as the most restrictive class among
  its fields: `Universal` for plain data, `Process` for anything that names
  this engine's memory. Each encoder declares the classes it accepts, and a
  typed path refuses a mismatch at compile time
  (`const { assert!(K::LIVENESS <= E::ACCEPTS) }`). The classes form a partial
  order (Actor, Incarnation, Process, Session, Fleet, Store, Universal), so the
  check is `K::REQUIRES ⊆ E::PROVIDES`. Confidentiality is a second axis
  checked the same way: a secret (ADR-0235) is accepted by no encoder but the
  secrets store. Explicit cast maps bridge classes (`Ref<K>` to an inline `K`,
  `ActorRef<R>` to an address description), with authored mirrors such as
  `#[cast(from = Texture)] struct TextureFile { .., pixels: Blob }`. Prior art:
  FIDL's `resource` split, Cap'n Proto `save` / `restore`, serde's `remote`
  derive and `DeserializeSeed`.

### 7. Freeing by refcount, with a reclaim thread

- `Shared` `Blob`s, guest table entries and envelopes hold strong references.
  When the last one drops, no reference provably remains, and the bytes can go at any time.
- `BlobEntry::drop` removes its own index slot if the slot still points at
  it (a concurrent check-in of the same hash may already have replaced it).
- Large entries do not free on the dropping thread. `BlobEntry::drop` sends
  a `Box<[u8]>` at or above `RECLAIM_THRESHOLD_BYTES` to one reclaim thread,
  which frees it. Small entries free inline.
- The store never drops a referenced entry. Under memory pressure it grows
  and surfaces the pressure: a resident-byte gauge warns at each new
  high-water mark, like `ReplyTable`. Spilling referenced entries to
  persistent storage is not part of this design.

### 8. Allocation: system allocator plus off-thread reclaim

The store allocates with the system allocator. The concerns are churn and the
cost of freeing large blocks.

- No bump arena. Entries have independent lifetimes. An arena frees only as a
  whole, so one long-lived blob would pin every block around it.
- The system allocator serves large allocations as their own mappings. Freeing
  one returns it to the OS without fragmenting the small-object heap.
- The expensive part of freeing a large block (unmapping) moves off hot
  dispatch threads onto the reclaim thread (section 7).
- A custom allocator over an engine-held block can come later, behind
  `BlobStore`'s interface, if measurement shows churn. Nothing outside the
  store sees how bytes are allocated.
- Check-in from a producer that knows the length fills an
  `Arc::new_uninit_slice`-style buffer in place, so building an entry does not
  copy the bytes twice.

### 9. Reads stream on both sides

```rust
// aether-data (re-exported by aether-actor), both targets
impl<'a> BlobReader<'a> {
    pub fn open(blob: &'a Blob) -> BlobReader<'a>;
    pub fn len(&self) -> u64;
    pub fn is_empty(&self) -> bool;
    pub fn read(&mut self, buf: &mut [u8]) -> usize;                  // at most MAX_READ_BYTES per call
    pub fn seek(&mut self, to: u64);
    pub fn read_range(&self, offset: u64, buf: &mut [u8]) -> usize;   // does not move the cursor
}

// host imports (crates/aether-substrate/src/actor/wasm/host_fns.rs); hash_ptr points at the 32-byte hash
// "aether"."blob_hold_p32"(hash_ptr: u32) -> i64         // takes one hold; returns the length
// "aether"."blob_read_p32"(hash_ptr: u32, offset: u64, dst_ptr: u32, dst_len: u32) -> i64
// "aether"."blob_drop_p32"(hash_ptr: u32)                 // gives one hold back: GuestHold::drop
```

- A `Blob` has no whole-bytes accessor. Reading streams everywhere, through
  `BlobBacking::read_at`: the slice for `Owned`, the entry for native
  `Shared`, the imports for guest `Shared`.
- The caller supplies the buffer. The host copies at most `MAX_READ_BYTES` per
  call and returns the length (negative for a hash the guest does not hold).
  `blob_drop_p32` on a hash the guest does not hold logs a warning and does
  nothing. No blob import traps.
- The reader lives in `aether-data` beside `Blob`, whose representation is
  private there, so `Blob` needs no public `read_at` and reading has one door.
- Streaming is the paradigm on both sides: a caller has to ask why it would
  load something large into memory. A caller can still loop the reader into a
  `Vec`; nothing makes it the easy path, and the loop is visible in review.
- There is no zero-copy view yet. One arrives as its own API when a consumer
  needs contiguous bytes, such as the render cap uploading textures.
- `blob_hold_p32` refuses a hash the table neither pins nor holds.
  `blob_hold_p32` and `blob_drop_p32` are a matched pair: a guest `Blob`'s
  construction and its `GuestHold`'s `Drop`. `blob_read_p32` has a named
  production consumer, per the rule that
  every FFI import needs one: Bloomery's `ClosureArtifact::load`, which
  streams a closure member through a `BlobReader` while it hashes (section
  11).

### 10. Closures carry `Blob`s, and the closure ceiling rises

This amends ADR-0226 decision 10. `ReadArtifact` and `ReadClosure` answer
with `Blob`s, not inline byte vectors, and `Invoke` hands the program its
closure as `Blob`s, so no member is copied into the program's memory unless
the program reads it.

The ceiling now bounds resident bytes checked in for one closure, not a mail
frame. `ClosureLimit::MAX_BYTES` rises from 16 MiB to 4 GiB, which admits a
~950 MB environment plus a real source tree. The driver's configured limit
stays within `MIN_BYTES ..= MAX_BYTES`, and `ClosureTooLarge` keeps its
meaning.

### 11. Bloomery reads and the reader's shape

- A closure member's digest crosses mail as an unverified claim, so a member
  is read only through `ClosureArtifact::load(expected)`. It streams the
  member through a `BlobReader`, hashing every byte, and returns the bytes only
  when they hash to `expected`. It is the named production consumer of
  `blob_read_p32`. `Env::read::<K>` (`crates/aether-bloomery-program/src/env.rs`)
  keeps returning a decoded `K` and goes through `load`, so a mismatch refuses
  before any decode.
- `Env::open`, a streaming reader over an `OpaqueBytes` input, is deferred
  until a program streams a large opaque input it does not load whole, such
  as a follow-up program that takes a prior run's log as an input. Until
  then no program reads an `OpaqueBytes` member.
- `BlobReader` does not implement `std::io::Read`, whose provided
  `read_to_end` is exactly the whole-load shortcut. A separately named adapter
  (`BlobReadAdapter`, behind a new `std` feature of the guest SDK, since the
  SDK is `no_std`) provides `Read` for decoders that need it, so the whole-load path
  is always a named choice.

### 12. Files that change (implementation, not this ADR)

| Area | Files |
|---|---|
| Store | new `crates/aether-substrate/src/store/` |
| Envelope | `crates/aether-substrate/src/mail/mod.rs` (`Mail`), `mail/registry/dispatch.rs` (`OwnedDispatch`, `DispatchParts`, `MailDispatch`) |
| Native send | `crates/aether-substrate/src/actor/native/binding/{outbound,pending,flush,send}.rs` (ring entries are plain bytes, so attachments ride on `PendingMail` beside the ring entry) |
| Native deliver | `actor/native/slot/dispatcher.rs` (decoded `Blob` fields are the attachments), `actor/native/ctx/{mod,send}.rs` |
| Armed hand-offs | `actor/native/blob/work.rs:670`, `actor/native/spawn/activation.rs:564`, `mail/mailer.rs` (`route_tail`) |
| Wasm | `actor/wasm/host_fns.rs` (`send_mail_p32` resolve on send, `blob_hold_p32`, `blob_read_p32`, `blob_drop_p32`), `actor/wasm/component/{ctx,dispatch}.rs`, `actor/wasm/blob_table.rs` (the instance's table) |
| Guest SDK | `crates/aether-actor/src/wasm/{raw.rs,bridge/mail.rs}` |
| `Blob` and schema | `crates/aether-data/src/{blob/,schema.rs}` (`Blob`, `BlobBacking`, `BlobReader`, `SchemaType::Blob`), `crates/aether-codec/src/{encode,decode}.rs` (read and write plain bytes) |
| Egress rewrite | `crates/aether-substrate/src/mail/mailer.rs` (`route_tail` egress), `crates/aether-rpc/src/server/runtime.rs` (reply-out), `crates/aether-codec/src/inline/` |

## Consequences

- **Bloomery.** A program's input closure and `ReadArtifact` /
  `ReadClosure` replies carry `Blob`s (section 10). Inside the engine the
  closure no longer has to fit one mail frame or be copied into the program,
  which resolves #6719.
- **Memory is the limit.** The store is in memory only, so a `Shared` value's
  bytes are resident while anyone holds it, and a closure's members are
  resident while it runs. The raised ceiling (section 10) bounds that per
  closure.
- **One kind for every caller.** A kind with a `Blob` field works in-process
  and over the wire alike, so no reply needs a separate bytes twin for wire
  callers. aether-mcp sends and receives such fields as bytes, and its
  existing large-reply spill applies.
- **`aether.fs` moves to blobs.** `aether.fs.read` can answer a `Blob` and
  `aether.fs.write` can take one, for in-engine readers and MCP alike. The move
  needs its own issue and a named in-engine consumer.
- **Other consumers.** Process stdout, captures, HTTP bodies, and future
  mostly-static graphics data (meshes, textures) held as blobs and uploaded by
  the render cap, the likely first consumer of a zero-copy view.
- **Naming.** `actor/native/blob/` already means ADR-0087's unit of dispatch.
  The store's module is `store/`, with the internal types `BlobStore` /
  `BlobEntry`, and the value is `Blob`. ADR-0087's unit of dispatch is to be
  renamed separately.
- **Negative.**
  - Mail carrying attachments pays a schema walk at egress.
  - The first in-process send of an `Owned` value copies it into the store
    once.
  - A value larger than one wire frame is refused off-process until chunked
    transfer exists.
  - Building a guest `Blob` and dropping it each cost one host call, and a guest that
    leaks a `Blob` (for example with `mem::forget`) keeps its entry until that
    instance ends.
- **Follow-on.**
  - Chunked wire transfer, when a consumer sends large values off-process.
  - A zero-copy view, when a consumer needs contiguous bytes.
  - Guest-side creation needs no new mechanism: a guest builds an `Owned`
    value with `Blob::from`, and its first in-process send copies it to the
    host once.
  - Granting `Blob`s carried in saved state again on rehydrate, instead of
    carrying them as bytes, waits for a consumer.

## Alternatives considered

- **A global table of handles by id (ADR-0045).** Any holder of an id can
  read, so access is ambient. Rejected.
- **A process-local handle refused at every boundary** (this ADR's second
  version, `BlobRef`). It kept the store and the counting but made a class of
  values the one communication model could not carry: refused at the wire, a
  documented misuse when persisted, a separate bytes twin for wire callers,
  and reading as a capability of the handle. Replaced by value semantics, as
  Erlang's reference-counted binaries do.
- **Distributed liveness** (Cap'n Proto, E, Java RMI). References cross the
  wire and a protocol of releases or leases tracks them. Heavy, and the
  mail-based refcounts of ADR-0045 already showed the cost. Rejected.
- **A per-actor generation-tagged index as the handle** (this ADR's first
  version). The same blob gets a different number in each actor, delivery
  rewrites every payload index, generation bits are needed to catch stale
  reuse, and code needs a release verb that reads as freeing shared bytes.
  Replaced by the hash: holding the hash in your own table is what grants
  access, so knowing a hash grants nothing and it can serve as the handle.
- **Leaving raw forwards detached** (recipients refuse them, and they leave
  the process unrewritten), or refusing them at egress. The first leaves a
  tag-1 field outside the process; the second walks every egress payload.
  Rejected: resolving on send fixes the forward at its source, keeps it
  shared, and walks only for senders that hold blobs.
- **Carrying attachment hashes beside the payload** (extending `receive_p32`,
  a second receive export, or a pull import). Each adds ABI for what the
  payload can carry itself, and a per-decode mint breaks the hold count when a
  guest decodes one mail twice. Rejected: tag 1 carries the hash, and the
  delivery pin plus a hold per built value keep counts equal to live values.
- **A shared table under an `RwLock`.** Every read takes a lock, and ADR-0165
  measured `RwLock` reads degrading about 15 times under contention. Rejected;
  each actor's handler already runs alone.
- **An actor that owns the bytes, read by mail (ADR-0120).** A round trip per
  read. Rejected.
- **Files as backing.** Duplicates `aether.fs` and the journal's blob
  directory (ADR-0220), as ADR-0049 did. Rejected.
- **Mutable blobs.** Readers would need synchronization. Rejected; a change
  is a new check-in.
- **A bump arena.** Frees only as a whole, so one long-lived blob pins its
  block. Rejected (section 8).
- **A custom allocator now.** No measurement shows need. Deferred behind the
  store interface.
- **A decode that needs a delivery context.** The typed decode is
  context-free (`Kind::decode_from_bytes`). A tag-0 field decodes to an
  `Owned` value with no context; only the envelope decoder, which already has
  the attachments, produces `Shared` values.
- **Evicting unreferenced entries under an LRU (ADR-0045).** With refcounts,
  an unreferenced entry is already freed. Nothing is left to evict.
