# Actor arena spike results

Core measured revision: `fade4b593993644b87694c332dedb7d8047cf749`

Preallocation measured revision: `a907975fce9d19f2ce957b13abf4b646c86b6f4a`

Machine: Apple M4 Pro, 12 logical CPUs, aarch64 macOS 26 / Darwin 25.5

Toolchain: rustc 1.96.0, Wasmtime 44

Primary protocol: nine alternating fresh-process AB/BA pairs

## Outcome

The arena is worth pursuing for the high-entity-count use case, but the reason
is narrower and more useful than “contiguous state is faster.”

Merely moving actor state from per-actor boxes into pages did not help. It was
neutral-to-slower in ordinary mail delivery, and 19% slower in the 65,536
bullet sweep. The repeatable gain appeared when the page became the
iteration/run-token unit: a tight 64-bullet page walk was 2.52× faster than
generation-checked per-actor endpoints, and the full current-shaped dynamic
actor loop → arena page loop was 2.18× faster.

That full-scene gain was stable from 4,096 through 65,536 bullets. This supports
a production-shaped vertical slice for namespace/kind cohort iteration. It
does not support an arena migration that only replaces `Box<A::State>`.

The preallocation follow-up shows that the slice does not require an exact
population forecast. Approximate hints can reserve stable chunks and grow
transparently without changing hot throughput, provided cohort iteration walks
live pages rather than reserved capacity. The consequential variable is active
page density, not estimate precision.

## Primary results

| Mechanism isolated | Base → candidate | Base | Candidate | Result |
|---|---|---:|---:|---|
| Native state placement, random mail | `boxed-current` → `arena-state` | 2.594 ns/mail | 2.690 ns/mail | +3.7%, inconclusive |
| Native route endpoint, random mail | `arena-state` → `arena-endpoint` | 2.674 ns/mail | 2.271 ns/mail | 1.174×, improvement |
| Page scheduling, random mail | `arena-endpoint` → `arena-page` | 2.353 ns/mail | 2.269 ns/mail | 1.027×, inconclusive |
| Bullet state placement, 65,536 | `boxed-current` → `arena-state` | 6.756 ns/update | 8.045 ns/update | +19.1%, regression |
| Bullet dynamic endpoint, 65,536 | `arena-state` → `arena-endpoint` | 8.041 ns/update | 7.555 ns/update | 1.063×, inconclusive |
| Bullet page iteration, 65,536 | `arena-endpoint` → `arena-page` | 7.572 ns/update | 3.020 ns/update | 2.521×, improvement |
| Full bullet path, 65,536 | `boxed-current` → `arena-page` | 6.686 ns/update | 3.073 ns/update | 2.182×, improvement |
| Reserve–initialize–retire | boxed heap → arena bitmap | 37.335 ns/op | 19.448 ns/op | 1.915×, improvement |
| Wasm instance/storage ceiling | detached → persistent arena | 20.000 ns/mail | 17.468 ns/mail | 1.139×, improvement |
| Wasm state placement | pointer table → direct arena | 17.788 ns/mail | 17.678 ns/mail | neutral |
| Wasm host boundary | per-mail → packed batch | 17.461 ns/mail | 3.995 ns/mail | 4.385×, improvement |

Adjacent rows are independent paired comparisons, so their base medians need
not be identical. Classification uses the predeclared ADR-0085-style noise
floor and 75% direction requirement.

## Preallocation follow-up

The follow-up tested 50% through 400% population hints, 1–64 pages per growth
chunk, exact chunk boundaries, 25%–100% occupancy, packed versus random holes,
and real Wasmtime pre-growth.

The useful findings are:

- hot native updates remained 1.116–1.158 ns/update across every capacity hint
  when a hierarchical live-page bitmap excluded spare pages;
- a 75% native hint with 16-page/1,024-actor chunks incurred sixteen
  incremental growths with a 5.67 µs p99 pause;
- one actor beyond an exact 65,536 hint allocated one 64-KiB state chunk in
  5.21 µs;
- at 25% packed occupancy, scanning the 2× reserved capacity was 49% slower
  than walking live pages;
- at the same occupancy, random holes kept four times as many pages active and
  made the live-page sweep 74% slower than packed state;
- exact Wasm pre-growth used one `memory.grow`; 50% and 75% hints used 33 and
  17 calls, respectively, without changing hot throughput; and
- a 2× untouched Wasm reserve left RSS flat, while forcing the spare 4 MiB
  resident raised RSS by approximately 4 MiB.

Chunk size is therefore a frequency-versus-pause choice, and should be
byte-bounded per actor kind. Exact population prediction is not a prerequisite.
Page density and live-page traversal are.

See the [complete preallocation result](PREALLOCATION_RESULTS.md) and
[machine-readable primary matrix](results/a907975f/preallocation/primary/matrix.json).

## Bullet scene

The scene cell contains same-kind 64-byte projectile states. One update:

- advances three position words by three velocity words;
- decrements lifetime; and
- folds in a frame stamp.

It excludes mailbox lookup, collision broadphase, rendering, and allocation.
That makes it a focused ceiling for the “many lightweight entities update
together” mechanism, not a complete frame-time claim.

| Bullets | Dynamic boxed actors | Arena page sweep | Speedup |
|---:|---:|---:|---:|
| 4,096 | 6.453 ns/update | 2.896 ns/update | 2.222× |
| 16,384 | 6.749 ns/update | 2.936 ns/update | 2.281× |
| 65,536 | 6.686 ns/update | 3.073 ns/update | 2.182× |

The 65,536-bullet trial performs five million updates, about 76 complete
sweeps. The current-shaped arm acquires five million actor locks/run tokens;
the arena arm acquires 78,125 page locks/run tokens. That 64:1 amortization,
plus monomorphic contiguous iteration, is the material result.

Evidence:

- [full 65,536-bullet report](results/fade4b59/scene-full-65536/report.md)
- [full comparison JSON](results/fade4b59/scene-full-65536/comparison.json)
- [page-iteration cut](results/fade4b59/scene-page-65536/report.md)
- [paired-delta plot](results/fade4b59/scene-full-65536/paired-deltas.svg)

## What state locality alone did

The random-mail storage arm did not show a general locality win. Across
64-byte, 256-byte, and 4-KiB states plus random, sequential, and hot/cold
access, `arena-state` was either inconclusive or slower. The 4-KiB cell leaned
6% faster but was noisy and did not qualify.

There are two likely reasons:

1. bulk-created boxes are already fairly allocator-local on this machine; and
2. an arena endpoint that still validates a generation and acquires a page
   mutex per actor adds work without amortizing anything.

This is exactly why the scene result matters: the architectural payoff requires
an API and scheduler path that can consume the cohort as pages. Storage layout
without cohort execution is not enough.

## Lifecycle churn

The single-thread reserve–initialize–retire cell was 1.915× faster in the arena.
In the separate allocation-instrumented pass, 250,000 boxed replacements
performed 250,000 allocations and deallocations, moving 64 MB through the
system allocator. The arena arm performed zero timed heap allocations.

The bitmap's concurrent test proves eight reservers cannot duplicate a
coordinate across 4,096 slots. This run does not measure contended allocator
throughput; the production vertical slice still needs multiworker namespace
shards.

Evidence:

- [lifecycle report](results/fade4b59/lifecycle-churn/report.md)
- [allocation diagnostic](results/fade4b59/diagnostic-alloc-lifecycle-churn/comparison.json)

## Wasm

Directly indexing contiguous state instead of a shuffled pointer table was
neutral at both 256-byte and 4-KiB states. The strong Wasm signals came from
instance consolidation and host-boundary batching:

- detached → persistent arena was 1.139× faster at 256 actors;
- one packed guest entry per 1,024 mails was 4.385× faster than one entry per
  mail;
- the 4-KiB-state batching sensitivity cell remained 4.388× faster.

The resident-memory population curve also strongly favors persistent arenas:

| Actors | Detached median RSS | Arena median RSS | Detached guest memory | Arena guest memory |
|---:|---:|---:|---:|---:|
| 64 | 12.0 MB | 10.6 MB | 4.0 MB | 64 KiB |
| 128 | 13.5 MB | 10.6 MB | 8.0 MB | 64 KiB |
| 256 | 16.9 MB | 10.9 MB | 16.0 MB | 128 KiB |
| 512 | 22.6 MB | 10.7 MB | 32.0 MB | 192 KiB |

Detached RSS grew by roughly 23.5 KiB per additional actor over this range;
arena RSS was effectively flat. Guest logical memory exposes the underlying
minimum-page cost more directly. RSS is lower because untouched guest pages
remain lazily backed.

Evidence:

- [detached versus arena report](results/fade4b59/wasm-detached-arena/report.md)
- [packed batch report](results/fade4b59/wasm-batch/report.md)
- [512-actor memory point](results/fade4b59/memory-wasm-512/comparison.json)

## Measurement limits

- Native fixtures mirror Aether's box, dynamic handler, mutex, coordinate, and
  run-token shapes, but do not execute the complete envelope, lineage, tracing,
  panic, or lifecycle wrappers.
- The bullet cell is single-threaded. It establishes per-core page-iteration
  throughput; it does not prove worker scaling, fairness, or an acceptable
  page-shard size.
- The preallocation follow-up uses 64-byte state and heap-backed native chunks.
  Larger actor states, native virtual-memory reservation, and multiworker
  growth remain production-slice measurements.
- The scene baseline already has the namespace cohort and therefore omits
  mailbox routing. That is intentional: it isolates dynamic per-actor update
  from ECS-like cohort iteration.
- Collision and rendering work would reduce the percentage attributable to
  dispatch/locking. Different bullet complexity must be added as a sensitivity
  in the production vertical slice.
- Wasm arms execute real Wasmtime code and memory writes, but use a core-Wasm
  fixture rather than Aether's complete component ABI.
- Hardware instruction, cache, and TLB counters were unavailable in the
  portable runner. Wall time cannot prove that a gain came from L2 behavior.
- All primary reports use exact completion counts and full-state checksums,
  warm/reset before timing, identical traces, fresh processes, alternating
  order, paired medians, paired-delta IQR, and raw trial retention.

## Recommended next slice

Build one production-shaped bullet namespace behind an experimental feature,
not a broad actor migration:

1. namespace owns typed, stable `ActorArena<A::State>` pages;
2. an advisory per-kind count reserves byte-bounded chunks but never becomes a
   hard actor limit;
3. mailbox routes resolve to `{page, slot, generation}`;
4. availability and live-page/live-slot bitmap hierarchies remain distinct;
5. per-actor mail remains valid for ordinary work;
6. an explicit same-kind cohort update can acquire one page run token and call
   a monomorphized handler across its live slots;
7. ready/live bitmaps let pages be split across workers rather than
   serializing the namespace;
8. allocation reuses holes, reclaims empty chunks, and reports actors/page so
   random retirement cannot silently destroy density;
9. Wasm pre-grows persistent cells/shards lazily and uses a packed delivery ABI
   as a separate mechanism; and
10. the real-runtime comparison repeats this scene, random mail, churn,
    density, and worker-scaling matrix before an ADR commits the architecture.

The key decision is therefore: pursue arenas together with typed cohort/page
execution. Do not pursue “arena allocation” as an isolated box replacement.
