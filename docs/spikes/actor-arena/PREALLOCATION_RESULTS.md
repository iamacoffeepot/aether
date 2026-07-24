# Actor arena preallocation results

Measured revision: `a907975fce9d19f2ce957b13abf4b646c86b6f4a`

Machine: Apple M4 Pro, 12 logical CPUs, aarch64 macOS 26 / Darwin 25.5

Toolchain: rustc 1.96.0, Wasmtime 44

Primary protocol: seven interleaved fresh-process samples per cell

## Outcome

An actor-count estimate is useful, but it does not need to be exact.

For 65,536 64-byte bullets, forecast error did not produce a measurable hot
update penalty when the sweep walked a hierarchical live-page bitmap. Native
hot medians across 50% through 400% hints were 1.116–1.158 ns/update; Wasm
medians were 1.279–1.295 ns/update. Spare capacity stayed out of the hot path.

The costs appeared where expected:

- underestimation introduced bounded incremental growth pauses;
- large native overestimates committed proportional heap memory;
- untouched Wasm overestimates remained lazily physically backed; and
- random retirement spread live actors across more pages, materially reducing
  cohort-iteration density.

The production implication is not “predict the exact actor count.” It is:
accept an advisory namespace/kind capacity hint, grow in byte-bounded stable
chunks, maintain live-page and live-slot bitmaps separately from availability,
and make active-page density observable.

## Forecast accuracy

Native state uses one stable heap slab per growth chunk. These cells use
16-page chunks: 1,024 actors or 64 KiB of 64-byte state.

| Hint / actual | Cold ns/actor | Cold IQR | Growth p99 | Hot ns/update | Unused state | Peak RSS |
|---:|---:|---:|---:|---:|---:|---:|
| 50% | 16.14 | 3.02 | 10.33 µs | 1.158 | 0 | 7.88 MiB |
| 75% | 20.48 | 4.17 | 6.46 µs | 1.123 | 0 | 7.86 MiB |
| 100% | 17.59 | 4.21 | — | 1.132 | 0 | 7.86 MiB |
| 125% | 17.33 | 2.77 | — | 1.116 | 1 MiB | 8.97 MiB |
| 200% | 19.69 | 3.91 | — | 1.132 | 4 MiB | 12.22 MiB |
| 400% | 29.56 | 1.68 | — | 1.135 | 12 MiB | 20.97 MiB |

Cold native results below 400% are not monotonic and their IQRs overlap. They
do not establish an exact-hint throughput optimum. The clear signal is that a
4× heap-backed reserve pays for and commits the extra 12 MiB, while none of the
reserves affect live-bitmap update throughput.

The Wasm arm starts with one real Wasmtime memory page. Reserve performs one
host `memory.grow`; underestimated cells grow again at arena-chunk boundaries.

| Hint / actual | Cold ns/actor | Cold IQR | Growth p99 | `memory.grow` calls | Hot ns/update | Unused state | Peak RSS |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 50% | 7.87 | 0.10 | 2.88 µs | 33 | 1.291 | 0 | 13.16 MiB |
| 75% | 8.06 | 0.52 | 1.96 µs | 17 | 1.285 | 0 | 13.17 MiB |
| 100% | 7.20 | 0.24 | — | 1 | 1.281 | 0 | 13.17 MiB |
| 125% | 7.54 | 0.99 | — | 1 | 1.279 | 1 MiB | 13.17 MiB |
| 200% | 7.20 | 0.39 | — | 1 | 1.295 | 4 MiB | 13.17 MiB |
| 400% | 7.46 | 2.24 | — | 1 | 1.281 | 12 MiB | 13.17 MiB |

One untouched Wasm pre-growth took roughly 2.5 µs regardless of the logical
reserve in this range. Underestimation raised cold cost by repeatedly crossing
linear-memory boundaries, but remained below 3 µs p99 per 16-page arena growth
in the primary run.

## Growth chunk tradeoff

At a 75% native hint, every cell eventually reaches the same 65,536 actors and
performs identical work.

| Pages/chunk | Actors/chunk | Incremental chunks | Cold ns/actor | Growth p99 | Maximum growth |
|---:|---:|---:|---:|---:|---:|
| 1 | 64 | 256 | 17.08 | 1.75 µs | 3.08 µs |
| 4 | 256 | 64 | 18.79 | 4.83 µs | 4.83 µs |
| 16 | 1,024 | 16 | 16.81 | 5.67 µs | 5.67 µs |
| 64 | 4,096 | 4 | 16.19 | 18.17 µs | 18.17 µs |

The allocation diagnostic counted the complete native cold phase:

| Pages/chunk | Allocation calls | Allocated bytes |
|---:|---:|---:|
| 1 | 4,109 | 5,750,592 |
| 16 | 265 | 5,550,912 |
| 64 | 71 | 5,540,928 |

Larger chunks did not materially improve total cold throughput; they exchanged
many small allocations for fewer, longer pauses. Sixteen pages is a reasonable
64-byte-bullet starting point—64 KiB of state and about a 6 µs observed growth
ceiling—but production policy should target bytes, not a fixed actor count.
State size and acceptable worker stall should determine pages per chunk.

The exact boundary behaved as designed:

| Actual actors | Initial hint | Final capacity | Incremental chunks | Growth p99 |
|---:|---:|---:|---:|---:|
| 65,535 | 65,536 | 65,536 | 0 | — |
| 65,536 | 65,536 | 65,536 | 0 | — |
| 65,537 | 65,536 | 66,560 | 1 | 5.21 µs |

The first actor beyond the estimate acquired another complete 1,024-slot,
64-KiB chunk. It did not create a meaningful total cold-time cliff, but it did
create one visible per-spawn pause.

For Wasm, arena chunk size and physical memory growth are related but distinct.
One-, four-, and sixteen-page arena chunks all caused 17 `memory.grow` calls at
the 75% hint because 16 arena pages of this state equal one 64-KiB Wasm page.
A 64-page arena chunk caused five calls. Hot throughput was unchanged.

## Occupancy and hole shape

All occupancy cells reserve twice the peak actor population. `capacity-scan`
therefore visits 2,048 pages per sweep. `live-bitmap` visits only pages with at
least one actor.

| Live actors | Packed live pages | Packed bitmap | Packed capacity scan | Random live pages | Random bitmap | Random capacity scan |
|---:|---:|---:|---:|---:|---:|---:|
| 25% | 256 | 1.106 ns | 1.652 ns | 1,024 | 1.925 ns | 2.217 ns |
| 50% | 512 | 1.141 ns | 1.342 ns | 1,024 | 1.458 ns | 1.588 ns |
| 75% | 768 | 1.119 ns | 1.268 ns | 1,024 | 1.245 ns | 1.308 ns |
| 90% | 922 | 1.159 ns | 1.226 ns | 1,024 | 1.176 ns | 1.273 ns |
| 100% | 1,024 | 1.122 ns | 1.208 ns | — | — | — |

At 25% packed occupancy, scanning capacity was 49% slower by medians than
walking live pages. Random holes kept all originally populated pages live:
the live-bitmap arm was then 74% slower than the packed live-bitmap arm, even
though both updated the same number of actors.

This is the strongest new design constraint. A live bitmap prevents spare
capacity from becoming sweep work, but it cannot recover locality when a few
actors pin every page. Stable actor coordinates should therefore be paired
with:

- allocation that preferentially fills existing partially live pages;
- reclamation of completely empty chunks;
- live actors/page telemetry; and
- an explicit future decision about evacuation/compaction rather than an
  accidental promise that actors can move.

## Logical reserve versus physical commitment

The forced-touch pass writes once per 4-KiB host page before actor
initialization. It is a diagnostic, not a primary timing pass.

| Target | Hint | Logical unused state | Untouched peak RSS | Forced-touch peak RSS |
|---|---:|---:|---:|---:|
| Native heap slabs | 100% | 0 | 7.91 MiB | 7.89 MiB |
| Native heap slabs | 200% | 4 MiB | 12.27 MiB | 12.28 MiB |
| Wasm memory | 100% | 0 | 13.17 MiB | 13.20 MiB |
| Wasm memory | 200% | 4 MiB | 13.16 MiB | 17.17 MiB |

The native `Box<[u64]>` fixture already commits its zeroed spare slabs, so
forced touching changes nothing. Untouched Wasm growth reserves address space
without committing the extra state pages; touching the 4-MiB overestimate
raises RSS by approximately 4 MiB. A native `mmap`/`VirtualAlloc` design could
seek the same property, but this spike did not implement or measure it.

## Recommendation

Carry these requirements into the production-shaped namespace slice:

1. accept an optional advisory actor-count hint per namespace/kind;
2. round it to stable, byte-bounded chunks and allow transparent growth;
3. initialize actor state only when a slot becomes live;
4. keep availability and live-page/live-slot hierarchies distinct;
5. sweep live pages, never reserved capacity;
6. prefer hole reuse and reclaim empty chunks while preserving coordinate
   generations;
7. pre-grow Wasm memory once from the estimate without eagerly touching spare
   pages; and
8. repeat chunk-size and density measurements with real state sizes and
   multiworker contention before fixing production defaults.

## Limits and evidence

- The primary cells use 64-byte bullet state. Growth pauses will scale with
  state bytes and backing strategy.
- Native chunks use heap-backed zeroed slabs, not `mmap`, and create separate
  availability, lock, live-bit, and state allocations.
- The allocator and sweeps are single-threaded. CAS correctness is tested
  elsewhere in the spike, but this matrix does not measure contention.
- Sparse Wasm state is excluded because that would require choosing a
  production live-set ABI.
- Growth p95/p99 values describe the finite growth events within each fresh
  process. With only a few large chunks, p99 equals the maximum.
- Cold measurements are noisier than hot sweeps and remain descriptive.
  Non-monotonic differences inside their IQR are not treated as wins.
- Peak RSS includes runtime setup. Logical byte counts identify the exact
  reserve; RSS is used only for population slopes and the forced-touch
  diagnostic.
- Hardware cache, TLB, and instruction counters remain unavailable.

Artifacts:

- [primary report](results/a907975f/preallocation/primary/report.md)
- [primary matrix JSON](results/a907975f/preallocation/primary/matrix.json)
- [primary CSV](results/a907975f/preallocation/primary/matrix.csv)
- [allocation diagnostic](results/a907975f/preallocation/diagnostic-allocations/report.md)
- [forced-touch diagnostic](results/a907975f/preallocation/diagnostic-touched/report.md)
- [hot-update plot](results/a907975f/preallocation/primary/hot-update.svg)
- [growth-pause plot](results/a907975f/preallocation/primary/growth-pause.svg)

The artifact tree retains 343 primary and 54 diagnostic raw fresh-process
reports, their environment records, and exact reproduction commands.
