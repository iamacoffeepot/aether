# registry-view spike

Prices the registry redesign discussed in the shared-registry contention
investigation: production's `RwLock<FxHashMap>` (readers take a guard and
clone the entry out; spawners take the write guard per mutation) against
snapshot views (`ArcSwap` load on the read side, all mutations applied by
a single-writer owner draining mail batches and republishing per drained
cycle). Three publish strategies are priced: clone-per-cycle over
`FxHashMap`, per-operation and per-cycle structural sharing over
`im::HashMap`, and a double-buffer with operation replay (two `FxHashMap`
buffers alternate as head; each batch applies to the standby plus the
previous batch's lag, then the buffers swap; `Arc::make_mut` clones only
if a straggling reader still pins the two-publishes-old snapshot).

The consumer-contract audit that gates the design lives in `AUDIT.md`.

Run: `cargo run --release` (self-contained nested workspace).

## Results (2026-07-23, Apple Silicon, 12 logical cores, table size 10k)

### A. Read scaling, no churn (ns/lookup)

| table | 1 thr | 4 thr | 8 thr |
|---|---|---|---|
| lock/clone-out | 29.3 | 105.3 | 427.6 |
| swap/clone-out | 24.5 | 9.0 | 9.0 |
| swap/in-place | 5.4 | 1.5 | 0.8 |
| im/clone-out | 40.1 | 14.2 | 10.5 |

The lock *degrades* ~15× as readers are added — every read-guard
acquisition is a read-modify-write on the shared reader count, so
parallel readers bounce one cache line. The snapshot load scales the
opposite direction (per-core cache residency, wait-free), and the
in-place mode the snapshot design newly permits (production's clone-out
exists only because holding an `RwLock` guard across a handler is
unacceptable) reaches sub-nanosecond.

### B. 4 readers vs one flat-out writer, 800 millis window

| config | read ns/op | writer ops/s |
|---|---|---|
| lock/write-per-op | 699.1 | 1,655,002 |
| swap/publish-per-batch-64 | 58.1 | 77,520 |
| im/publish-per-op | 78.9 | 196,908 |

Under adversarial churn the lock's readers collapse to ~700 ns — a 24×
degradation against their own uncontended figure, the convoy the primer
hypothesized, now measured. Snapshot readers are unmoved by the writer.
The write-side ceilings (77k–197k mutations/s vs the lock's 1.7M) sit
two orders of magnitude above any plausible spawn rate.

### C. Write path, 200k inserts from 4 producers, end-to-end (ns/update)

| config | batch 1 | batch 32 | batch 256 |
|---|---|---|---|
| lock/direct | 140.5 | — | — |
| mail+swap (clone-per-cycle) | 238.5 | 126.6 | 143.5 |
| mail+im-op | 2,331.3 | 2,289.7 | 2,298.8 |
| mail+im-cycle | 493.3 | 325.5 | 401.5 |
| mail+double | 201.7 | **99.4** | 124.9 |

The mail-batched owner reaches parity with direct locking through
self-batching (clone-per-cycle drained up to 100k updates per publish),
and the double-buffer *beats the direct lock* while publishing far more
often (205 publishes at batch 1 vs clone-per-cycle's forced 7) — lower
staleness and lower cost simultaneously, because its publish is O(1)
regardless of table size. Per-operation structural publishing loses 16×.

### D. Publish-strategy scaling with table size (single-threaded costs)

| entries | fx clone µs | fx insert ns | im insert ns | fx read ns | im read ns |
|---|---|---|---|---|---|
| 10k | 214.8 | 34.9 | 152.0 | 1.9 | 17.1 |
| 100k | 2,584.4 | 33.2 | 167.1 | 2.3 | 26.8 |
| 1M | 84,403.2 | 51.9 | 495.7 | 10.2 | 78.9 |

The scale verdict. Clone-per-cycle's O(n) publish crosses from cheap
(215 µs at 10k) to prohibitive (84 ms at 1M — superlinear, allocator
pressure) and is only viable for small tables or behind sharding.
Structural sharing scales gently on the write side but taxes the
per-dispatch hot path 8–9× (79 ns vs 10 ns at 1M). The double-buffer
pays two plain inserts per update (~70–100 ns at 1M) and O(1) publish
with `FxHashMap` read speed at every size — dominant at scale on every
axis except memory (exactly two resident tables plus one batch of lag).

## Snapshot lifetime semantics (all `ArcSwap` variants)

The head snapshot is pinned by the `ArcSwap` itself; every reader load
holds a transient guard; a superseded snapshot is freed the moment its
last reader guard drops. Under structural sharing a straggler pins only
the delta nodes unique to its version, not a whole table; under the
double-buffer a straggler pins the two-old buffer, and the owner's
`Arc::make_mut` transparently falls back to a real clone for that one
cycle — the safety valve that makes buffer reuse sound.

## Caveats

- Throughput only; the one-scheduling-hop latency a staged spawn commit
  pays is not measured here (it is bounded by pool wake latency, the
  same hop every mail already pays).
- The mail path is a bare `std::sync::mpsc` — no envelope allocation,
  lineage stamping, or trace-ring cost. Those are per-batch in the
  proposed design (one envelope per flush), so the amortized deltas
  stay small, but the constant is understated.
- Snapshot readers observe one-publish-stale state by design; the
  double-buffer's cheap publish narrows the staleness window (it
  publishes per drained cycle without an O(n) disincentive).
- `Arc::make_mut` straggler clones are not counted separately; with
  nanosecond-scale reader guards they should be vanishingly rare.
