# ADR-0165: Handlers Read Views, Emit Effects

- **Status:** Proposed
- **Date:** 2026-07-23

## Context

Actor birth writes shared, globally-locked structures, and the hot one — the mailbox
routing table in `aether-substrate`'s `Registry` (`mail/registry/mailbox.rs`) — is read
on every mail dispatch. One `std::sync::RwLock` guards three maps (`mailboxes`, `kinds`,
`name_index`); `route_lookup` takes a read guard per mail and clones the entry out, and
every spawn takes the write guard mid-handler through `Spawner::spawn_actor`. ADR-0087
parallelized dispatch across the worker pool, but creation still funnels through this
lock, so spawn-heavy workloads (a `TcpSessionActor` per accepted connection; any future
entity swarm) serialize against all routing.

The investigation that produced this ADR started from the adjacent problem: injecting
shared values (the registry among them) into handlers without handing out lock-bearing
aggregates. The `spike/registry-view-contention` branch carries the two artifacts this
decision rests on:

- **A benchmark** (`spikes/registry-view/README.md`). Reads through the `RwLock`
  degrade ~15× as reader threads are added (guard acquisition is a read-modify-write on
  a shared count; parallel readers bounce one cache line) and collapse ~24× under
  writer churn — the convoy, measured. Snapshot reads through an `arc-swap` load are
  wait-free, unmoved by writers, and 30× faster at 8 threads. On the write side, a
  single-threaded owner draining batched mutations reaches and passes direct-lock
  throughput (99 ns/update vs 140 ns contended) because batches self-size under load,
  while a whole-map clone per publish is prohibitive past ~100k entries (84 ms at 1M).
- **A consumer-contract audit** (`spikes/registry-view/AUDIT.md`). Every production
  call site of the `Registry` surface classified against this ADR's contract: 12 hold
  unchanged, 6 adapt mechanically, and 3 clusters need the specific resolutions
  §Decision incorporates. Two findings corrected the working assumptions: the
  `set_on_mailbox_change` hook has zero production installers, and
  `Registry::drop_mailbox` has zero production callers.

Under concurrency, a synchronously-read value is stale by the time the reader acts on
it — `route_lookup` already concedes this by cloning the entry and dropping the guard
before dispatch, so dispatch has always operated on a per-mail micro-snapshot. The only
atomicity point that actually exists is the moment a mutation applies. The existing
lifetime policy (borrow-bound `'a` handles for point-in-time properties of the
executing context; ownable handles only for values that re-resolve per use) already
encodes the read half of this; the input capability's local subscriber table (ADR-0021)
already encodes the ownership half; per-frame draw accumulation and `CostCells` local
aggregation already encode the write half at the engine's public surfaces.

## Decision

A handler's relationship to state is classified by ownership, and each class has
exactly one access shape:

1. **Own state** — the actor's `&mut self`. Mutated directly; the inbox already
   serializes it. Unchanged.
2. **Read-dominated shared state** — owned by a single writer; read everywhere through
   **views**; mutated only by **staged effects**.
3. **Write-hot shared state** — never takes a per-write synchronization hop. Local
   accumulation with periodic aggregation (the existing `CostCells` shape).

The classification is a design gate: an owner actor running hot, or a commit-latency
requirement attaching to a staged mutation, means the structure was misclassified —
the response is reclassification, not engineering around the shape.

### Views

A `View<T>` is the ownable read handle to a single-writer structure's published state:
a cheap-to-clone wrapper over an `arc-swap` slot, threaded to actors through the
ADR-0156 params channel. Loading it yields a point-in-time snapshot guard scoped to
the handler that loaded it. The head snapshot is pinned by the slot; a superseded
snapshot frees when its last reader guard drops. `ViewPublisher<T>` is the non-`Clone`
writer half, held by exactly one owner.

The view API is **keyed from day one**: hot-path consumers resolve
per-id (`entry_for(id)`-shaped), and whole-table consumers (descriptor listings,
egress) use an explicit enumeration surface. This keeps a later sharding of the
publisher a publisher-side change with zero consumer diffs (§Consequences).

### Effects

Handlers emit mutations of shared state the way they emit mail: staged into the
handler's work buffer (the ADR-0087 blob generalizes from a mail buffer to an effect
buffer), flushed at handler end as **one batch envelope** to the owning actor. The
owner drains its inbox, applies updates in inbox order — mutual exclusion and ordering
come from the actor model itself, not a lock — and republishes once per drained batch.
A rejected update settles into the emitting handler's chain (ADR-0080); asynchronous
invariant failure is the norm of this contract, not a special case. The unit crossing
the mail system is always the batch, never a single update.

### The registry conversion (first application)

The `Registry`'s `mailboxes` table becomes the first converted structure:

- **Publish structure: double-buffer with operation replay.** Two `FxHashMap` buffers
  alternate as the published head; each drained batch applies to the standby plus the
  previous batch's replay lag, then the buffers swap via the `arc-swap` slot.
  Publishing is O(1) at any table size; every update costs two plain inserts
  amortized; reads keep raw `FxHashMap` speed. `Arc::make_mut` is the straggler
  valve: a reader still pinning the two-publishes-old snapshot forces one real clone
  for that cycle. Lag replay preserves batch order, and the implementation carries a
  test pinning in-order semantics for register-then-drop sequences. The alternatives
  are priced in the spike: whole-map clone-per-publish dies past ~100k entries, and
  per-operation structural sharing loses 16× on the write path and 8× on hot-path
  reads at 1M entries.
- **Boot carve-out.** Chassis boot is single-threaded and precedes the worker pool; it
  branches on synchronous registration results to abort (`BootError`) and does
  read-your-writes against earlier boot phases. Boot therefore builds the table
  directly and seals it to the owner at pool start. The staged-mutation contract
  activates at steady state.
- **Spawn.** `Spawner::spawn_actor` splits into lock-free build (construction and
  `init` already precede every shared write) and a commit that becomes one staged
  effect fusing registration, seize-handle install, and rollback — three operations
  with read-your-write dependencies become one atomic apply. Name-conflict detection
  for lineage spawns (ADR-0099) is parent-local — the folded id can only collide
  within one parent's children, so the check runs synchronously against live ∪ staged
  children; a cross-parent collision at 64 bits remains fail-fast. `SpawnError` for
  apply-time failures settles into the spawning chain; the deterministic id returns
  immediately, and mail staged to the child in the same flush orders after its
  registration, so birth-happens-before-announcement is a property of flush order.
- **Component load.** The load path builds `LoadResult` and the hub egress snapshots
  from the staged effect's own payload — the handler knows exactly what it registered;
  reading it back from the live table was a detour that becomes a stale read under
  this contract. This also gives the input/http/window subscribe-validation reads a
  sound ordering they lack today (the audit found them racy against concurrent drops
  already).
- **Change notification.** The `set_on_mailbox_change` hook mechanism is deleted
  (zero production installers). Consumers that track registry change — hub inventory
  egress, future monitors — subscribe to an empty change-event mail and load the view
  themselves; a generation counter stamped on each publication makes every consumer
  idempotent and coalescing (latest-wins, the `ConnectionReady` wake-token idiom).

`kinds` / `name_index` (append-only after boot and component load, mutated together)
convert as a second, simpler view behind the same owner. The remaining spawn-path
structures (`actor_registry`, `instanced_slots`) classify under the same taxonomy in
follow-on work; the cost table is already class 3 and does not change.

## Consequences

- Dispatch reads become wait-free and faster at every measured thread count; the
  convoy between spawn and routing is gone by construction rather than mitigated.
- Writes trade nanosecond commit latency for microseconds. The latency is masked for
  all intra-engine consumers by flush ordering, and failure feedback rides settlement.
  The one observable widening: a by-name `lookup` from the wire path racing a
  just-staged registration sees not-found for one publish cycle longer than today — a
  race that already exists, with a narrower window.
- The owner consumes worker capacity under churn. Its measured ceiling (~10M
  updates/s) sits two orders of magnitude above any nameable workload, and batches
  self-size under load (busier means cheaper per update).
- Resident memory for the routing table doubles (two buffers). At 1M actors this is
  the real scale pressure; the lever is name interning, orthogonal to this decision.
- **Sharding is deferred, deliberately.** Reads don't need it (wait-free already),
  the owner doesn't need it (ceiling), and the publish doesn't need it (O(1)).
  Splitting flush batches K ways would fragment the batch amortization the write-path
  numbers depend on. The keyed view API and the single internal effect-submission
  chokepoint keep it a contained later change. Revisit triggers, written here so the
  decision is a measurement: sustained churn above ~5% of the measured single-owner
  ceiling, a commit-latency requirement attaching to spawns, or the appearance of a
  consumer requiring an atomic cross-shard snapshot (none exists; any future one must
  justify itself against the sharding cost it imposes).
- Handler ctxs move toward vending narrow view handles and effect emitters instead of
  the `Mailer` aggregate — the concrete resolution path for the god-object finding,
  carried as its own follow-on arc.
- Tests that poke the registry's synchronous mutators adapt mechanically (the audit
  enumerates them); none encodes a contract production must preserve.
- The stale `Registry::drop_mailbox` doc comment (claiming a production caller that
  does not exist) is corrected in passing.

## Alternatives considered

- **Shard the existing `RwLock` by id range.** Preserves synchronous semantics and
  divides contention, but leaves reads lock-bound (still degrading with reader count),
  keeps mutation outside the actor model, and solves neither the view-injection
  problem nor the god-object handout. Retained as the fallback shape inside the
  publisher if a revisit trigger fires.
- **Fleet-wide staged registration zipped at a barrier (RCU).** Needs a cross-batch
  merge-conflict policy and two-tier visibility machinery; per-handler effect buffers
  flushed to a single owner get the same batching with none of that, because
  cross-buffer ordering reduces to inbox order.
- **Persistent (structural-sharing) map as the published table.** O(log n) publish
  without cloning, but 16× slower on the write path per-op-published and, decisively,
  8× slower on the per-mail hot path at scale. The double-buffer dominates it on
  every axis except resident memory.
- **Cap-local tables for ephemeral actors (no global registration).** Dissolves the
  contention architecturally, but sessions and entities are real actors: it forfeits
  universal addressability, per-actor logs/cost queries, and settlement lineage, and
  the accept path would still need a hand-distributed sender-handle scheme. Rejected
  for actors; remains the correct shape for non-actor state (ADR-0021 subscriber
  tables).
- **Synchronous concurrent map (sharded locks or lock-free).** Fixes contention but
  keeps mutation-as-ambient-side-effect, so every consumer contract stays implicit;
  none of the audit's read-your-write couplings would have surfaced, and the
  view-injection problem that motivated the investigation would remain unsolved.
