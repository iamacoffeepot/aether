# registry-view spike

Prices the registry redesign discussed in the shared-registry contention
investigation: production's `RwLock<FxHashMap>` (readers take a guard and
clone the entry out; spawners take the write guard per mutation) against
snapshot views (`ArcSwap` load on the read side, all mutations applied by
a single-writer owner draining mail batches and republishing per drained
cycle). Two publish strategies are priced: clone-per-batch over
`FxHashMap` and per-operation structural sharing over `im::HashMap`.

Run: `cargo run --release` (self-contained nested workspace).

## Results (2026-07-23, Apple Silicon, 12 logical cores, table size 10k)

### A. Read scaling, no churn (ns/lookup)

| table | 1 thr | 4 thr | 8 thr |
|---|---|---|---|
| lock/clone-out | 31.1 | 114.3 | 255.2 |
| swap/clone-out | 24.1 | 11.6 | 8.6 |
| swap/in-place | 5.5 | 1.5 | 0.8 |
| im/clone-out | 39.4 | 15.8 | 10.0 |

The lock *degrades* 8× as readers are added — every read-guard
acquisition is a read-modify-write on the shared reader count, so
parallel readers bounce one cache line. The snapshot load scales the
opposite direction (per-core cache residency, wait-free): 30× faster at
8 threads, 300× in the in-place mode the snapshot design newly permits
(production's clone-out exists only because holding an `RwLock` guard
across a handler is unacceptable).

### B. 4 readers vs one flat-out writer, 800 millis window

| config | read ns/op | writer ops/s |
|---|---|---|
| lock/write-per-op | 804.2 | 2,044,395 |
| swap/publish-per-batch-64 | 58.2 | 75,520 |
| im/publish-per-op | 78.7 | 201,772 |

Under adversarial churn the lock's readers collapse to 804 ns — a 26×
degradation against their own uncontended figure, the convoy the primer
hypothesized, now measured. Snapshot readers are unmoved by the writer
(58 ns tracks scenario A's contention-free profile). The write-side
ceilings (75k–200k mutations/s vs the lock's 2M) are the price, and they
sit two orders of magnitude above any plausible spawn rate.

### C. Write path, 200k inserts from 4 producers, end-to-end

| config | ns/update | publishes | drained/cycle |
|---|---|---|---|
| lock/direct | 138.5 | — | — |
| mail+swap, flush batch 1 | 307.1 | 5 | 40,000 |
| mail+swap, flush batch 32 | 118.4 | 2 | 100,000 |
| mail+swap, flush batch 256 | 138.6 | 2 | 100,000 |
| mail+im, flush batch 1 | 2,314.1 | 200,000 | 66,667 |
| mail+im, flush batch 32 | 2,346.5 | 200,000 | 100,000 |
| mail+im, flush batch 256 | 2,325.0 | 200,000 | 100,000 |

The mail-batched owner with clone-per-batch publishing is at parity with
direct locking (118–307 ns/update end-to-end) because self-batching does
what the design predicted: the owner drains everything queued per cycle,
so 200k updates cost 2–5 whole-map publishes. Per-operation structural
publishing (`im`) loses by 16× — the per-op `ArcSwap` store plus HAMT
insert dominates — which settles the open publish-strategy question in
favor of plain `FxHashMap` clone-per-batch.

## Caveats

- Throughput only; the one-scheduling-hop latency a staged spawn commit
  pays is not measured here (it is bounded by pool wake latency, the
  same hop every mail already pays).
- The mail path is a bare `std::sync::mpsc` — no envelope allocation,
  lineage stamping, or trace-ring cost. Those are per-batch in the
  proposed design (one envelope per flush), so the amortized deltas
  stay small, but the constant is understated here.
- Snapshot readers observe one-publish-stale state by design; scenario B
  quantifies staleness pressure only indirectly (publish rate).
