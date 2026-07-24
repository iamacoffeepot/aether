# Namespace actor arena spike

Status: disposable measurement spike; not a runtime design decision.

[Measured results and recommendation](RESULTS.md) are recorded from revision
`fade4b593993644b87694c332dedb7d8047cf749`.

This spike asks whether namespace-owned actor arenas are valuable enough to
justify a production vertical slice. It deliberately leaves the substrate
unchanged and measures separable approximations of the mechanisms under
discussion:

1. typed state locality;
2. route endpoint simplification;
3. page-granularity scheduling;
4. persistent Wasm linear-memory state; and
5. packed host-to-guest delivery; and
6. reserve–initialize–retire lifecycle churn.

The result can establish a performance ceiling or reject an idea. It cannot,
by itself, prove the end-to-end runtime gain.

## Grounding in current code

The native baseline mirrors these current shapes:

- `actor/native/spawn.rs` allocates `Box<A::State>`;
- `actor/native/dispatcher_slot.rs` stores
  `Mutex<Option<Box<A::State>>>`;
- the registry route owns an `Arc<dyn InboxHandler>`;
- the scheduler queues `Arc<dyn Drainable>` actor slots and grants one actor
  run token per activation.

The Wasm baseline mirrors these shapes:

- a detached actor owns a Wasmtime `Store` and `Memory`;
- `component/dispatch.rs` copies each delivered payload into guest memory and
  enters `receive_p32`;
- inline children share an instance but live behind a
  `BTreeMap<MailboxId, InlineSlot>` and boxed erased actor state.

The fixture does not call those private runtime implementations. Instead it
duplicates only the relevant ownership and call shapes in a CPU-only harness,
keeping GPU/chassis startup, mail lineage, tracing, lifecycle hooks, and codec
work out of the signal.

## Experimental arms

| Arm | State | Route target | Run token / host entry |
|---|---|---|---|
| `boxed-current` | one boxed record per actor | deterministic map → `Arc<dyn handler>` | actor mutex per activation |
| `arena-state` | contiguous records in locked pages | same dynamic handler shape | actor activation locks its page |
| `arena-endpoint` | same arena | map → `{page, slot, generation}` | actor activation locks its page |
| `arena-page` | same arena | same concrete endpoint | one page lock per scheduling window |
| `wasm-detached` | one real Wasmtime memory per actor | host chooses store | one memory write + guest entry per mail |
| `wasm-inline` | one real Wasmtime memory; shuffled records behind a pointer table | guest pointer lookup | one write + entry per mail |
| `wasm-arena` | one real Wasmtime memory; directly indexed contiguous records | guest slot arithmetic | one write + entry per mail |
| `wasm-batch` | same contiguous guest arena | packed `{actor, value}` records | one write + entry per packed batch |

All arms execute the same deterministic state transition. Warmup is followed
by a state reset. The timed interval contains delivery only; setup, compilation,
allocation, reset, checksum, and serialization are excluded. Every trial
reports the exact completed mail count and a full-state checksum.

`--workload lifecycle-churn` changes the work unit from mail delivery to one
retire/replacement cycle. `boxed-current` drops and reallocates a boxed state;
`arena-state` releases a generation-stamped coordinate, reserves a replacement,
and initializes the in-page state without allocating. This is a single-thread
heap-versus-bitmap mechanism ceiling. The concurrency test establishes bitmap
correctness, but a production vertical slice must still measure contended
allocation across real namespace shards.

`--workload scene-sweep` is the high-population ECS-shaped cell. It requires
sequential access and one update per activation, then compares:

- a direct list of `Arc<dyn handler>` actors, with one actor mutex per update;
- the same dynamic call shape pointing into arena state;
- concrete generation-stamped coordinates with one page lock per entity; and
- a direct arena page walk with one lock/run token for the contiguous live
  run.

The fixture uses 65,536 same-kind, 64-byte bullet states and five million
updates. Each update advances three position words by three velocity words,
decrements lifetime, and folds in a frame stamp. It intentionally excludes
collision broadphase and rendering, so it remains a ceiling for lightweight
projectiles rather than a complete game-frame claim. Mailbox hashing is absent:
the workload asks what a scene system can gain once it already has the
namespace/kind cohort and wants to advance every bullet. Namespace-local bitmap
shards allow the fixture to exceed one 4,096-slot two-level root without adding
an arena id to the actor coordinate.

## Allocator mechanism

`HierarchicalBitmap` is the proposed small-form allocator:

- one `AtomicU64` leaf owns the free bits for up to 64 slots;
- one `AtomicU64` summary indexes up to 64 leaves;
- the leaf CAS is authoritative;
- a summary bit is only a hint and is repaired after a racing clear;
- every coordinate contains `{page, slot, generation}`;
- release increments the generation before publishing the free bit.

Tests fill non-power-of-two capacities, reject stale/double release, verify
generation change on reuse, and race eight reservers across all 4,096 slots.
The production design would extend the same rule to more summary levels or
shards rather than treating this two-level spike as a final allocator.

## Predeclared primary comparisons

The committed measurement run uses the following comparisons before looking
at results:

1. `boxed-current` → `arena-state`: state placement only, retaining dynamic
   routing.
2. `arena-state` → `arena-endpoint`: remove dynamic handler dispatch.
3. `arena-endpoint` → `arena-page`: change the scheduling/run-token unit.
4. `wasm-detached` → `wasm-arena`: instance and state-storage ceiling.
5. `wasm-inline` → `wasm-arena`: scattered pointer-addressed versus directly
   indexed linear-memory state.
6. `wasm-arena` → `wasm-batch`: host-boundary batching independently of state
   placement.
7. `boxed-current` → `arena-state` under `lifecycle-churn`: heap
   drop/allocation versus bitmap release/reserve, with identical state
   initialization.
8. Three `scene-sweep` cuts at 65,536 bullets:
   `boxed-current` → `arena-state` → `arena-endpoint` → `arena-page`.
   These isolate state placement, dynamic dispatch, and tight page iteration
   for the high-entity-count case.

Native primary cell: 4,096 actors, 256 bytes/state, random activations,
16 mails/activation, 64 slots/page.

Wasm primary cell: 256 actors for the detached comparison and 1,024 actors for
the shared-memory comparisons, 256 bytes/state, random activations. Detached
population is intentionally bounded because each arm really creates that many
stores and minimum Wasm memories.

Sensitivity cells use sequential and hot/cold access plus 64-byte and
4-KiB states. A production decision should not be based on a single favorable
cell.

## Noise controls

The comparison binary follows ADR-0085:

- every sample is a fresh trial process;
- pair order alternates AB/BA;
- both sides use the same seed and precomputed access trace;
- each process warms and then restores state;
- results are paired before aggregation;
- the report records medians, paired-delta IQR, direction consistency, and a
  noise floor of `max(1.5 × delta IQR, 10% of base, 0.3 ns/mail)`;
- raw reports remain available, rather than pooling operation samples.

Allocation atomics perturb timing, so `--instrument-allocations` is a separate
diagnostic pass. Peak RSS includes process and Wasmtime setup and is useful as
a population slope, not as a precise object-size measurement. Hardware cache,
instruction, and TLB counters are intentionally a separate platform-specific
pass; the portable macOS runner does not pretend wall time identifies cache
misses.

## Build and run

```text
cargo build --release -p aether-harness-actor-arena --bins

scripts/actor-arena-measure.sh /tmp/actor-arena-results

target/release/aether-actor-arena-compare \
  --base boxed-current \
  --candidate arena-state \
  --workload dispatch \
  --pairs 9 \
  --artifact-dir /tmp/actor-arena/native-storage \
  --actors 4096 \
  --mails 5000000 \
  --mails-per-activation 16 \
  --page-slots 64 \
  --state-bytes 256 \
  --pattern random \
  --warmup-mails 250000
```

Each artifact directory contains:

- `raw/pair-NN.json`: unaggregated paired trial reports;
- `comparison.json`: machine-readable aggregate;
- `environment.json`: commit, toolchain, OS, CPU, and executable;
- `report.md`: result, mechanism counters, and interpretation limits;
- `paired-deltas.svg`: pair drift and direction;
- `reproduce.txt`: the exact comparison arguments.

## Gate after the spike

The next step is a thin production-runtime vertical slice only if the measured
mechanism is repeatable and its benefit survives sensitivity cells. That slice
must use a real namespace arena, registry endpoint, scheduler, lifecycle, and
component ABI before an ADR commits the architecture.

In particular:

- a detached-Wasm memory reduction is expected and does not prove native arena
  locality;
- a `wasm-batch` win proves boundary amortization, not arena storage;
- a page-scheduling win must later be checked for fairness and parallelism
  loss;
- a storage win must later survive real envelope, tracing, and lifecycle costs;
- generation failure, panic containment, replacement, teardown, and namespace
  sharding remain correctness work, not benchmark details.
