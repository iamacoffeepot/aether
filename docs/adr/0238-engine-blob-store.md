# ADR-0238: Engine Blob Store of Immutable Checked-In Bytes Passed as Per-Actor Handles

- **Status:** Proposed
- **Date:** 2026-09-25
- **Amended:** 2026-09-25 — decision 6: persisting a handle is a documented misuse, not enforced; later directions recorded.

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
| 0133 | Reply-based stream handles for HTTP | A per-instance handle table already exists: the wasm `ReplyTable` (`crates/aether-substrate/src/actor/wasm/reply_table.rs`), a generation-tagged slab that grows and never drops a held handle, moved across `replace` as `PendingReplies`. That is the model for the blob table. Its limit (instance-local, not transferable) is solved here by carrying the bytes' `Arc` in the envelope, not the index. |
| 0165 | Published views over `arc-swap`, staged effects | Readers of immutable published data never take the writer's lock. Blob bytes are the simplest case: immutable from check-in, so an `Arc` read needs no synchronization at all. |

## Decision

### 1. A native store, not an actor

```rust
// crates/aether-substrate/src/store/ (native only; never a mailbox)
pub struct BlobStore { /* hash -> Weak<BlobEntry>, resident byte gauge, reclaim sender */ }

pub struct BlobEntry {
    hash: BlobHash,     // dedup key; never a capability
    bytes: Box<[u8]>,   // immutable from check-in
}

impl BlobStore {
    pub(crate) fn check_in(&self, bytes: Box<[u8]>) -> Arc<BlobEntry>;
}
```

- In memory only. Never backed by files. A restart forgets every blob.
- Check-in only. Bytes are immutable once checked in. A change is a new
  check-in and a new handle.
- Check-in hashes the bytes with BLAKE3. The hash is only a dedup key, and
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

### 2. A handle is an index into the actor's own table

| Side | Handle | Table home |
|---|---|---|
| Native actor | `Blob` (non-`Copy`, generation-tagged `u32`) | beside `A::State`, under the slot's actor `Mutex`; lent to `NativeCtx` for the turn |
| Wasm guest | `Blob` (same `u32` shape) | `ComponentCtx`, beside `reply_table`; moved across `replace` like `PendingReplies` |

- The table is a generation-tagged slab like `ReplyTable`: index in the low
  bits, generation in the high bits, FIFO free queue, grows and never drops a
  held entry.
- Only the owning actor's handler touches its table. There is no shared table
  and no `RwLock`.
- An entry leaves the table when the actor releases it (`ctx.release(blob)`,
  consuming) or when the actor dies.
- A native table is never stored on `NativeBinding` (see Context).

### 3. Handles move only inside mail

| Step | What happens |
|---|---|
| Send | The payload carries the sender's table index. The send path looks up each blob field in the sender's table and clones its `Arc` into the envelope. An index that is not live refuses the send. The sender keeps its own entry. |
| Carry | The envelope carries the `Arc`s beside the payload: `attachments: Option<Box<[(u32, Arc<BlobEntry>)]>>` on `Mail`, one null word when empty. |
| Deliver | The substrate installs each `Arc` in the recipient's table and rewrites each payload index to the recipient's index before the handler runs. Fan-out installs per recipient. |

A grant is a side effect of delivering mail. An actor cannot see a new blob
until mail carrying it is delivered. Prior art: Unix `SCM_RIGHTS`, Fuchsia
VMO handles over channels, Mach out-of-line memory, and wasm component-model
resources.

Blob fields are a new schema variant, `SchemaType::Blob`. Each kind
descriptor records whether its schema contains one. The send-side lookup and
the delivery rewrite walk only those kinds, so blob-free mail pays nothing.
The mail layer stays byte-transparent for everything else.

### 4. The hash grants nothing

A guest names blobs only by indices into its own table. There is no
check-out by hash, no lookup by hash, and no host import that takes a hash.
Knowing a hash grants nothing. `Blob` handles are minted only in the
reference module: by check-in (into the caller's own table) and by delivery.

### 5. Handles never cross the wire

A handle cannot keep its invariant across serialization, so it is not
exportable. Any kind whose schema contains `SchemaType::Blob` is
process-local. The boundary refuses it both ways, like
`is_engine_only` (`crates/aether-substrate/src/mail/boundary.rs:127`) but
derived from the schema, not declared: RPC `Call` accept
(`crates/aether-rpc/src/server/runtime.rs:446`), RPC reply-out
(`runtime.rs:934`), unresolved-recipient egress
(`crates/aether-substrate/src/mail/mailer.rs:920`), and the JSON codec
(`encode_schema` / `decode_schema`). Crossing a process means sending bytes and
checking them in on the other side.

### 6. Persisting a handle: a documented misuse, not enforced

A handle names an entry in one engine's memory, so a persisted handle names
nothing after a restart. Writing a handle out is a misuse: code that needs a
blob's contents outside the engine writes the bytes. The engine documents
this and does not enforce it. Enforcement costs more than the correctness it
buys, and an enforced rule would get in the way of legitimate uses such as
logging a value that holds a handle. Passing a handle over the wire is
already refused (decision 5), and a program that wants to do it is doing
something odd.

An index resolves only against its holder's own table, so a replayed or
forged handle reaches only blobs the holder was already granted. A misuse can
cost correctness, never access.

Directions recorded for a later trait rework, none chosen. The leading one:

- **Liveness classes and encoder accept sets.** Every kind keeps one schema
  and carries a liveness class, derived as the most restrictive class among
  its fields: `Universal` for plain data, `Process` for anything holding a
  reference into this engine's memory, such as a `Blob`. Each encoder declares
  the classes it accepts: the mail envelope accepts both; wire, file and
  journal encoders accept only `Universal`; a log formatter accepts everything
  for display and never reads it back. A typed path refuses a mismatch at
  compile time (`const { assert!(K::LIVENESS <= E::ACCEPTS) }`). Decision 5's
  wire refusal and the persistence question become one rule, logging is just
  another destination, and further classes (for example `Session`) can be
  added later. Fuchsia FIDL's split of `resource` types from value types is
  the prior art.

  Refinements noted with it:

  - The classes form a partial order, not a ladder: `Process` and `Session`
    are incomparable. A kind requires the union of its fields' scopes and an
    encoder provides a set, so the check is `K::REQUIRES ⊆ E::PROVIDES`.
    Candidate scopes by reach: Actor (one table), Incarnation (survives
    `replace`, not a restart), Process (`Blob`, proven refs), Session (one
    connection), Fleet (one hub's lifetime), Store (journal `Ref`),
    Universal. Start with only the scopes a real type needs: Process, Store
    and Universal.
  - Confidentiality is a second, independent axis checked the same way: a
    secret (ADR-0235) is accepted by no encoder but the secrets store, and the
    log formatter redacts it.
  - Explicit cast maps bridge classes. A reference type declares a cast pair
    (`Blob` to `Bytes` and back by check-in; `Ref<K>` to an inline `K` and back
    by storing it; `ActorRef<R>` to an address description and back by
    re-proving). A kind that must cross declares an authored mirror, such as
    `#[cast(from = Texture)] struct TextureFile { .., pixels: Bytes }`; the
    derive verifies that every field has a cast and emits both conversions.
    Each type keeps one schema, a costly conversion is a visible `cast` call,
    and a kind with no declared mirror stays where its class allows. Prior
    art: Cap'n Proto `save` / `restore`, serde's `remote` derive, `From` /
    `TryFrom`.

Also recorded:

- a reference type that materializes its bytes when it leaves the engine and
  checks them back in when it returns;
- a derive-generated mirror type, with each `Blob` replaced by bytes;
- an encoder and decoder parameterized by destination, after serde's
  `Serializer` and `DeserializeSeed`;
- a schema-derived `HAS_BLOB` flag with compile-time asserts in typed
  persistence paths, and a random per-table incarnation in the encoded handle
  so a restored handle fails closed.

### 7. Freeing by refcount, with a reclaim thread

- Table entries and envelopes hold strong references. When the last one
  drops, no reference provably remains, and the bytes can go at any time.
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

### 9. Reads: native borrows, wasm streams

Native code borrows zero-copy: `ctx.blob_bytes(&blob) -> &[u8]`.

A wasm guest reads only through a streaming API. No host import and no guest
SDK call returns a blob's whole bytes.

```rust
// guest SDK (aether-actor, no_std)
pub struct BlobReader<'a> { blob: &'a Blob, cursor: u64, len: u64 }

impl<'a> BlobReader<'a> {
    pub fn open(blob: &'a Blob) -> BlobReader<'a>;
    pub fn len(&self) -> u64;
    pub fn read(&mut self, buf: &mut [u8]) -> usize;   // at most MAX_READ_BYTES per call
    pub fn seek(&mut self, to: u64);
    pub fn read_range(&mut self, offset: u64, buf: &mut [u8]) -> usize;
}

// host import (crates/aether-substrate/src/actor/wasm/host_fns.rs)
// "aether"."blob_read_p32"(handle: u32, offset: u64, dst_ptr: u32, dst_len: u32) -> i64
```

- The guest supplies the buffer. The host copies at most `MAX_READ_BYTES` per
  call into it and returns the length (negative for a stale handle).
- Streaming decoders run over the reader.
- This is a forced paradigm. A component author has to ask why they would load
  something large into memory.
- A guest can still loop the reader into a `Vec`. The API cannot prevent it.
  Nothing makes it the easy path, and the loop is visible in review.
- `blob_read_p32` lands with a named production consumer, per the rule that
  every FFI import needs one: Bloomery's `Env::open` over an `OpaqueBytes`
  input (section 11).

### 10. Closures carry handles, and the closure ceiling rises

This amends ADR-0226 decision 10. `ReadClosure` answers with handles, not
inline bytes, and `Invoke` hands the program its closure as handles. A
program reads an `OpaqueBytes` member through `BlobReader`, so no member is
copied into the program's memory unless it reads it.

The ceiling now bounds resident bytes checked in for one closure, not a mail
frame. `ClosureLimit::MAX_BYTES` rises from 16 MiB to 4 GiB, which admits a
~950 MB environment plus a real source tree. The driver's configured limit
stays within `MIN_BYTES ..= MAX_BYTES`, and `ClosureTooLarge` keeps its
meaning.

### 11. Bloomery reads and the reader's shape

- `Env::read::<K>` (`crates/aether-bloomery-program/src/env.rs:393`) keeps
  returning a decoded `K`, since a typed artifact must be decoded anyway. A
  new `Env::open` returns a `BlobReader` over an `OpaqueBytes` input. It is the
  named production consumer of `blob_read_p32`.
- `BlobReader` does not implement `std::io::Read`, whose provided
  `read_to_end` is exactly the whole-load shortcut. A separately named adapter
  (`BlobReadAdapter`, behind the guest SDK's `std` feature, since the SDK is
  `no_std`) provides `Read` for decoders that need it, so the whole-load path
  is always a named choice.

### 12. Files that change (implementation, not this ADR)

| Area | Files |
|---|---|
| Store | new `crates/aether-substrate/src/store/` |
| Envelope | `crates/aether-substrate/src/mail/mod.rs` (`Mail`), `mail/registry/dispatch.rs` (`OwnedDispatch`, `DispatchParts`, `MailDispatch`) |
| Native send | `crates/aether-substrate/src/actor/native/binding/{outbound,pending,flush,send}.rs` (ring entries are plain bytes, so attachments ride on `PendingMail` beside the ring entry) |
| Native deliver | `actor/native/slot/dispatcher.rs` (install and rewrite, table under the actor `Mutex`), `actor/native/ctx/{mod,send}.rs` |
| Armed hand-offs | `actor/native/blob/work.rs:670`, `actor/native/spawn/activation.rs:564`, `mail/mailer.rs` (`route_tail`) |
| Wasm | `actor/wasm/host_fns.rs` (`send_mail_p32` lookup, `blob_read_p32`), `actor/wasm/component/{ctx,dispatch}.rs`, `crates/aether-component/src/trampoline/runtime/mod.rs` (table across `replace`) |
| Guest SDK | `crates/aether-actor/src/wasm/{raw.rs,bridge/mail.rs}` |
| Schema | `crates/aether-data/src/schema.rs` (`SchemaType::Blob`), `crates/aether-codec/src/{encode,decode}.rs` (refuse) |
| Boundary | `crates/aether-substrate/src/mail/boundary.rs`, `crates/aether-rpc/src/server/runtime.rs` |

## Consequences

- **Bloomery.** A program's input closure and `ReadArtifact` /
  `ReadClosure` replies carry handles (section 10). The closure no longer has
  to fit one mail frame or be copied into the program, which resolves #6719.
- **Memory is the limit.** The store is in memory only, so a handle always
  names resident bytes, and a closure's members are resident while it runs.
  The raised ceiling (section 10) bounds that per closure.
- **Wire callers keep bytes.** aether-mcp is a wire client, so it can never
  receive a handle. A handle-bearing read is a new kind (for example
  `aether.fs.open` answering a `Blob`), and `aether.fs.read` keeps its bytes
  for wire callers. The same holds for process output, captures and HTTP.
- **Other consumers.** Process stdout, fs reads, and future mostly-static
  graphics data (meshes, textures) held as blobs and uploaded by the render
  cap from a borrowed slice.
- **Naming.** `actor/native/blob/` already means ADR-0087's unit of dispatch.
  The store's module is `store/`, with the types `BlobStore` / `BlobEntry` /
  `Blob`. ADR-0087's unit of dispatch is to be renamed separately.
- **Negative.** Blob-bearing kinds pay a schema walk on send and on delivery.
  A guest must release handles it no longer needs, or its table grows until
  the actor dies.
- **Follow-on.** Guest-side check-in (a chunked writer that finishes into a
  `Blob`) needs its own named consumer and is not decided here.

## Alternatives considered

- **A global table of handles by id (ADR-0045).** Any holder of an id can
  read, so access is ambient. Rejected.
- **The hash as the handle.** Knowing a hash would grant access. Rejected.
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
- **Rewriting indices by decode-time lookup instead of in bytes.** The typed
  decode is context-free (`Kind::decode_from_bytes`), so a lookup at decode
  would need an ambient delivery context. Rejected in favor of rewriting the
  payload before the handler sees it.
- **Evicting unreferenced entries under an LRU (ADR-0045).** With refcounts,
  an unreferenced entry is already freed. Nothing is left to evict.
