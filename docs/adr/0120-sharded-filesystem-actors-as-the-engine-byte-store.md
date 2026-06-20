# ADR-0120: Sharded Filesystem Actors as the Engine Byte Store

- **Status:** Proposed
- **Date:** 2026-06-20

## Context

Engine bytes have two homes today, and a value that lives in both gets copied into both.

- **Filesystem (ADR-0041).** Path-addressed files behind namespace adapters (`save`, `assets`, `config`), reached by mail on the `aether.fs` mailbox. Durable user data.
- **Handle store (ADR-0045, ADR-0048, ADR-0049).** A substrate-global, content-addressed cache of typed kind values, refcounted and LRU-evicted, with on-disk persistence under its own tree. The currency is `Ref<K>` (a handle id + kind id).

Loading a file and turning it into a handle crosses these twice: `aether.fs.read` returns `bytes`, the caller sends `aether.handle.publish { bytes }`, and the same bytes now sit in a file *and* in the handle store under a separate id. The store is also exposed two ways — as a raw `Arc<HandleStore>` the mailer hands to any native actor (`Mailer::handle_store()`), and as the `aether.handle` capability that wraps the same store for mail-addressable callers.

Three further forces compound this:

- **The mailer embeds handle bytes into mail.** To keep `Ref<K>` transparent (ADR-0045 §4), `route_mail` calls `walk_and_resolve` (the one production caller) and splices a handle's bytes inline into the payload before delivery, so the recipient decodes a complete value and never knows a handle was involved. Every ref-carrying mail ferries bytes between actors, invisibly, on the hot routing path. Embedding is a fossil of the pre-handle contract where mail always carried whole values; handles were added underneath it as a hidden cache rather than as the currency.
- **The DAG holds its own byte store.** The executor (ADR-0047) reads and writes the same handle store directly (`mailer.handle_store()`), making the compute layer a second claimant on where bytes live.
- **Persistence is duplicated.** The handle store hand-rolls atomic tmp+rename writes to its own directory tree (ADR-0049 §3), the same mechanism `LocalFileAdapter` already implements for the filesystem.

The goal: **one owner of bytes, handles as the only currency between actors, and no implicit byte transfers.** A handle becomes a token the owning actor recognizes; whether its bytes are resident in memory or on disk is that actor's private business. Reading the bytes out, or moving them, becomes an explicit, deliberate act, never the default cost of normal mail flow.

The consumer-side shape this enables is **load by transform**: to get a value in the form you want, you do not pull its bytes and parse them locally — you submit a transform pipeline to the owner of the handle, the owner runs it where the data lives, and the shaped result replies to you. Loading a config is submitting its parse and receiving the `Config` value back; loading a model is submitting the parse and receiving a *handle* to the parsed model, against which further transforms (inference) are submitted in turn. The raw bytes and every intermediate stay with the owner; only the final shaped result crosses — embedded in mail when small, replied as a handle when large. A consumer that genuinely needs raw bytes in its own memory still has `resolve`, but that is the exception, not how data is normally consumed.

A load-bearing simplification makes this tractable: **the engine owns these files, and external writes are undefined behaviour.** A buffer the engine owns cannot change unless the engine rewrites it, so a cached copy never has to revalidate against disk — the whole staleness/mtime-check problem is defined out of existence.

This is a large re-cut. It is built **adjacent** — the new sharded actors stand up alongside the existing filesystem, handle store, and DAG; nothing is ripped out on day one; migration follows once the floor is proven. This ADR commits to that floor and records the trajectory above it so the floor is built to admit the later layers without rework. It will, on completion of the migration, supersede the substrate-global handle store of ADR-0045 / ADR-0048 / ADR-0049 and amend ADR-0041.

## Decision

### The floor: sharded filesystem actors (this ADR)

**1. One actor type, instanced per namespace.** The filesystem becomes an instanced actor addressed `aether.fs:{namespace}` — `aether.fs:assets`, `aether.fs:save`, `aether.fs:config`, and a content-addressed `aether.fs:cache` for derived/anonymous data. Each instance owns its namespace's files plus an optional in-memory buffer cache, carries its own run-token (so `assets` reads do not queue behind `save` writes), and may choose its own cache policy (read-only namespaces cache aggressively; write-rarely namespaces can stay read-through). The boot-time `HashMap<String, Arc<dyn FileAdapter>>` of ADR-0041 §2 becomes a boot-time set of fs instances. All instances are native and co-process: a shard boundary is a concurrency and addressing boundary, not a memory boundary.

**2. Handles are first-class filesystem vocabulary.** A handle is `{ owner: MailboxId, token, kind_id }`. The `owner` is the mailbox of the instance that minted it — a handle is self-routing, so it can be handed to any actor and resolved by sending to its owner. The `token` is opaque: some identifier the owning instance knows how to resolve, with the representation deliberately unfixed at this stage. There is no separate `aether.handle` mailbox; minting, resolving, and lifecycle of handles are operations on the fs instances.

**3. Bytes never transit between actors by default; two explicit powers touch them.** Mail carries handles, never embedded bytes. An actor that genuinely needs raw bytes uses one of two deliberate, first-class operations, neither of which is the default:

- **`resolve`** — handle → bytes, delivered *out* to the asking actor. For a native actor this is a zero-copy read against the owning instance's buffer; across the wasm linear-memory boundary it necessarily copies into the guest's memory.
- **`copy`** — handle on instance X → handle on instance Y; the data stays inside the filesystem world and never lands in a consumer's memory. (Generalizes the existing host-path→namespace `copy` to instance→instance.)

**4. Engine-owned files, no external mutation.** Each instance owns its backing exclusively; external writes are undefined behaviour (backstopped by the single-substrate lock of ADR-0049 §7). A cached buffer is trusted until the engine rewrites it through the owning instance, and that write is the only thing that dirties it. No mtime checks, no revalidation.

**5. Identity is the caller's naming policy; the filesystem is identity-agnostic.** An instance maps a token to bytes; it does not impose what the token *means*. A file names itself by path; the compute cache names derived data by content-hash; a caller may hand its own id. Cross-caller dedup is the caller's responsibility — deterministic callers (the DAG, by content-hash) get it by construction; the filesystem does not dedup what it does not interpret.

### Trajectory above the floor (follow-on ADRs, recorded here for fit)

These are **not** decided by this ADR. They are the direction the floor is shaped to admit, each deserving its own ADR once the floor stands:

- **The mailer stops resolving handles.** `walk_and_resolve`'s byte-splice is deleted; mail carries the `Ref` untouched and the mailer becomes byte-blind and handle-blind, doing nothing but routing. Parking (a mail that references a not-yet-produced handle) leaves the mailer and becomes a resolve-time wait owned by the producing fs instance.
- **A pipeline verb on the compute instance.** "Apply these transforms to data that lives here, in the background (off the run-token, since the cache instance is the byte bottleneck), then reply with the result." Transforms stay pure and side-effect-free (so their outputs remain content-addressable and cacheable); each transform's output is a mail-embeddable kind (a value when small, a `Ref` when large). Submitting a pipeline is a **reply-required request**: by default the result — the last transform's output, or a pipeline error naming the failed stage — replies to the submitter; redirecting it to a different recipient is the opt-in (the generalized observer of ADR-0047, which shapes the result into that recipient's own kind and sends it). The pipeline is **atomic**: one submit, one reply, intermediates invisible, a failure at any stage aborting the whole with no partial result. Atomicity falls out of the pure-transform rule — there are no side effects to roll back, only a terminal reply that does not fire on error, which is also why a transform may not send mail mid-chain. `copy` is the degenerate empty pipeline; persisting a result is just building a `write` mail.
- **The DAG demotes to composition.** With pipelines terminating in mail, graphs emerge from mail flow between pipeline-running actors. An explicit DAG survives only for what mail-chaining does not give for free — fan-in/join, validation-before-run, and cancel/status — holding topology, never bytes.

## Consequences

### Positive

- **One owner of bytes.** A value lives in exactly one instance; loading a file and using it no longer forks the bytes into a second store.
- **Handles are the currency.** Mail between actors carries tokens; raw bytes move only through `resolve` or `copy`, both explicit.
- **The god-actor concern is answered by construction.** Sharding per namespace gives each instance its own run-token; the byte traffic parallelizes without copying, because the shards are co-process.
- **Staleness is defined away.** Engine-owned, no-external-write means caches never revalidate.
- **The compute cache is just another instance.** The content-addressed store becomes `aether.fs:cache` with content-hash as its token policy — no separate subsystem, and ADR-0048's cross-caller dedup is preserved by the caller choosing hash tokens.

### Negative

- **The `Ref` wire grows an owner.** A self-routing handle carries `owner: MailboxId`, abandoning the compact two-id form of ADR-0045 §1. Sharding buys parallelism at the cost of a fatter ref.
- **Two coexisting systems during migration.** Adjacent-first means the old handle store and the new instances both exist until callers move over; the transition surface is real.
- **Automatic cross-caller dedup is gone.** An identity-agnostic store cannot dedup bytes it does not interpret; callers that want shared cache entries must agree on a token.
- **Cross-shard moves are explicit.** Bringing a source into the compute instance is a deliberate `copy` (or a pipeline source step), not transparent — correct under the single-owner rule, but more verbose than a global store.

### Neutral

- **No new persistence mechanism.** Instances reuse the atomic tmp+rename write path; the handle store's hand-rolled copy of it retires with the migration.
- **Durability is a property of the backing.** A file-backed token survives restart (the token re-derives from its path); a memory-only token dies with its instance. The old source-vs-pinned split (ADR-0049 §1) re-expresses as "is there a file behind the token."

## Open questions

Deliberately unresolved; to be settled as the floor and the layers above it are drilled:

- **Token representation.** Left opaque on purpose. Path string, caller id, content-hash, or a single number per instance — decided when an instance's resolution path is built, not now.
- **Checkout / check-in borrow semantics.** A background transform reading an instance's bytes off the run-token needs the data pinned and conflicting writes excluded for the duration. Whether this is a first-class lease (with auto-release on borrower death and shared-read-default / fallible-exclusive rules) or something lighter is a pipeline-layer decision.
- **Native co-located vs guest transforms.** Guest wasm transforms get fault isolation and interruptibility on the bottleneck instance at the cost of a contained copy-in/out; native co-located transforms get literal zero-copy at the cost of running unsandboxed. Leaning guest-default, native as a future opt-in — but this is a pipeline-layer call.
- **Whether the standalone DAG survives.** It may thin to a join-and-lifecycle layer, or dissolve into mail-chained pipelines entirely.
- **`Ref::Inline`.** Whether the inline-a-whole-value variant of ADR-0045 §1 survives for small values, or everything becomes a handle.

## Alternatives considered

- **Merge the handle capability into the filesystem capability, both unsharded.** The original framing. Rejected — it leaves a single byte-bottleneck actor and does not address the two-owner problem; sharding plus single-ownership is the substantive change, not folding two mailboxes into one.
- **Keep the substrate-global handle store, just fold `aether.fs` into it.** Preserves automatic cross-component dedup (ADR-0045 §2, which explicitly rejected per-component stores for that reason). Rejected — a global store is exactly the second byte home this ADR removes; the dedup it bought is recovered by the compute cache being one shared instance whose callers agree on hash tokens.
- **Keep transparent `Ref` resolution in the mailer.** Convenient — handlers receive whole values. Rejected — it is the mechanism that ferries bytes between actors implicitly, the precise thing the single-owner / zero-default-transfer goal exists to remove. (Deferred to the mailer follow-on ADR, recorded here for completeness.)
- **Big-bang replacement.** Rip out the handle store and DAG, ship the new model in one cut. Rejected — too large to land safely; adjacent-first lets the floor prove out before callers migrate.
- **A shared memory arena under logical shards vs isolated per-instance stores.** Co-process instances *can* share one backing arena, making cross-shard `copy` a reference hand-off. Noted as an implementation latitude rather than decided here; the floor only commits that shards are co-process and that `copy` is the explicit cross-shard primitive.
