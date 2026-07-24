# Wasm actor state-transfer results

Measured revision: `051de9931f1580d8e883113f574e50ba6fc2c4ad`

Machine: Apple M4 Pro, 12 logical CPUs, aarch64 macOS 26 / Darwin 25.5

Toolchain: rustc 1.96.0, Wasmtime 44, release profile

Primary protocol: nine alternating fresh-process AB/BA pairs

## Outcome

Copying the complete actor-state arena into Wasm and back after every scene
update is a viable compatibility bridge for very small records, but it is not
the better default storage model.

In the primary 65,536-actor, 64-byte bullet cell, resident guest state took
1.676 ns per actor update and the best-case bulk round trip took 3.200 ns. The
copy added 1.523 ns per actor, or 99.8 µs to one complete scene sweep. That is
a 90.9% regression in the focused update loop, although the absolute cost is
only about 0.10 ms per 65,536 actors on this machine.

A post-hoc duration confirmation increased the timed work tenfold. It measured
1.691 ns resident versus 3.213 ns copied, a 90.1% regression and 99.2 µs of
extra work per sweep. All eighteen primary and confirmation pairs favored
resident state.

The result becomes much less forgiving when an actor has cold bytes. The guest
function continued to touch only the first 64 bytes, but copying complete
256-byte, 1-KiB, and 4-KiB records made the loop 4.08×, 9.33×, and 28.1× as
slow as resident state. Runtime storage should therefore remain resident in
the execution domain. `Kind` can still be the unified serialization format for
mail, files, snapshots, migration, and explicit host projections without
becoming a mandatory per-update round trip.

## What was compared

Both arms use one real Wasmtime instance, one contiguous guest linear-memory
arena, the same 64-byte hot state transition, one guest `run_sweep` entry per
complete actor population, and the same full-state checksum:

- `wasm-arena` leaves the authoritative state resident in guest linear memory;
- `wasm-copy-roundtrip` keeps an authoritative host `Vec<u8>`, performs one
  bulk `Memory::write` of the complete arena, invokes the identical guest
  function, then performs one bulk `Memory::read` of the complete arena.

Compilation, instance construction, allocation, initial-state construction,
warmup, reset, and final checksum are outside the timer. There is no `Kind`
encoding, per-actor copy call, allocation, component ABI lifting/lowering, or
mail dispatch in the copied arm. This is intentionally a favorable lower bound
for copy-in/copy-out, not a simulation of a costly serialization path.

## Primary bullet result

The primary cell contains 65,536 actors × 64 bytes, 80 complete timed sweeps,
eight warmup sweeps, and nine process pairs.

| Metric | Resident guest arena | Host↔guest round trip |
|---|---:|---:|
| Median time | 1.676 ns/update | 3.200 ns/update |
| Complete 65,536-actor sweep | 109.8 µs | 209.7 µs |
| Host entries per trial | 80 | 80 |
| Host→guest bytes per trial | 0 | 320 MiB |
| Guest→host bytes per trial | 0 | 320 MiB |
| Full state round trips | 0 | 80 |

The median paired penalty was 1.523 ns/update, with a 0.078 ns paired-delta
IQR and 100% directional consistency. Dividing transferred bytes by that
incremental time gives an effective 84.1 GB/s. This is not a standalone memory
bandwidth claim: it includes the changed cache behavior around the identical
guest update.

One base observation in the predeclared primary was slower than the other
eight. It reduced that pair's apparent copy penalty rather than creating the
result. The median remained stable, and the ten-times-longer confirmation
reduced the paired-delta IQR to 0.029 ns/update:

| Confirmation metric | Result |
|---|---:|
| Resident | 1.691 ns/update |
| Round trip | 3.213 ns/update |
| Paired penalty | 1.514 ns/update |
| Relative change | +90.1% |
| Complete-sweep penalty | 99.2 µs |
| Directional consistency | 100% |

## Population sensitivity

The 64-byte campaign held total updates and transferred bytes approximately
constant while changing population and the number of full sweeps.

| Actors | Resident | Round trip | Paired penalty | Penalty per sweep | Relative change |
|---:|---:|---:|---:|---:|---:|
| 4,096 | 1.568 ns/update | 3.116 ns/update | 1.551 ns/update | 6.4 µs | +98.7% |
| 16,384 | 1.551 ns/update | 3.157 ns/update | 1.641 ns/update | 26.9 µs | +103.6% |
| 65,536 | 1.676 ns/update | 3.200 ns/update | 1.523 ns/update | 99.8 µs | +90.9% |
| 100,000 | 1.665 ns/update | 3.194 ns/update | 1.536 ns/update | 153.6 µs | +91.8% |
| 131,072 | 1.679 ns/update | 3.186 ns/update | 1.510 ns/update | 197.9 µs | +89.8% |

Every pair in every population cell favored resident state. The nearly flat
per-actor penalty and linearly growing sweep penalty show no population
threshold at which copying becomes free; population matters through total
bytes.

## Cold-state sensitivity

Only the first 64 bytes are read and mutated in these cells. The remaining
bytes are deliberately cold but must still cross the boundary when the entire
actor record is copied. Actor counts and sweep counts were adjusted so each
copied trial transferred approximately 1 GiB across both directions.

| State bytes | Actors | Resident | Round trip | Total slowdown | Sweep penalty | Effective transfer rate |
|---:|---:|---:|---:|---:|---:|---:|
| 64 | 65,536 | 1.676 ns/update | 3.200 ns/update | 1.91× | 99.8 µs | 84.1 GB/s |
| 256 | 65,536 | 2.347 ns/update | 9.564 ns/update | 4.08× | 471.3 µs | 71.2 GB/s |
| 1,024 | 32,768 | 3.832 ns/update | 35.742 ns/update | 9.33× | 1.048 ms | 64.0 GB/s |
| 4,096 | 16,384 | 5.524 ns/update | 155.285 ns/update | 28.1× | 2.454 ms | 54.7 GB/s |

The declining effective rate is consistent with larger working sets and cache
effects, but wall time alone cannot attribute the mechanism. The actionable
observation is simpler: copying cost follows the whole stored record, not the
hot portion used by the update.

## Error controls

- Measurement code and the predeclared matrix were committed before sampling.
- Each side of each pair ran in a fresh process, and pair order alternated
  AB/BA.
- Both sides used the same deterministic seed, actor count, state layout,
  guest instructions, host-entry count, warmup, and complete-sweep count.
- State was reset after warmup and before timing.
- Every pair completed the exact requested updates and produced identical
  resident-versus-copied full-state checksums.
- The primary uses nine pairs; sensitivities use seven, except the 4-KiB cell's
  predeclared five. Reports retain every raw process result.
- Classification uses paired medians, paired-delta IQR, a noise floor, and
  directional consistency rather than treating millions of actor updates as
  independent samples.
- The post-hoc ten-times-longer confirmation tests whether short timed
  intervals or the primary outlier changed the conclusion. It closely
  reproduced the primary result.

Important limits remain:

- This measures safe contiguous Wasmtime `Memory::write` and `Memory::read`
  operations, not a custom host-memory implementation or unsafe alias into
  linear memory.
- It is a core-Wasm fixture, not the full Aether component ABI.
- No serialization is timed. Real `Kind` lifting/lowering can only add work to
  this complete-buffer design.
- The update is single-threaded and the host does not concurrently inspect
  state.
- No hardware cache, memory-controller, or TLB counters were collected. The
  reported effective transfer rate is derived from paired wall time.
- The copy immediately touches the full guest arena and can change cache
  warmth before the guest scan. That cache behavior is part of a real
  copy-in/copy-out design, not an independently controlled memcpy result.

## Recommendation

Use the same logical actor-state model in both execution domains, but do not
require the same physical owner:

1. native actor kinds live in typed native namespace arenas;
2. Wasm actor kinds live in persistent, contiguous guest arenas owned by their
   Wasm cell or shard;
3. updates enter once per cohort/page range rather than once per actor;
4. the host keeps compact routing, liveness, generation, query-index, and
   scheduling metadata;
5. state crosses the boundary for explicit snapshots, persistence, migration,
   debugging, or requested query projections; and
6. if an early implementation needs copy-in/copy-out, copy only a flat hot
   buffer once per cohort and enforce a byte budget.

For a 64-byte temporary bridge, the measured cost—about 0.10 ms per 65,536
actors per sweep—may be acceptable. It should be treated as a budgeted adapter,
not the storage architecture. Resident Wasm state preserves the
actor-as-state-buffer model while avoiding duplicated state and a penalty that
grows with cold fields.

## Evidence

- [primary report](results/051de993/wasm-state-transfer/primary-65536-state64/report.md)
- [primary aggregate](results/051de993/wasm-state-transfer/primary-65536-state64/comparison.json)
- [long-duration confirmation](results/051de993/wasm-state-transfer/confirmation-65536-state64-long/report.md)
- [256-byte sensitivity](results/051de993/wasm-state-transfer/state-256/report.md)
- [1-KiB sensitivity](results/051de993/wasm-state-transfer/state-1024/report.md)
- [4-KiB sensitivity](results/051de993/wasm-state-transfer/state-4096/report.md)
- [reproduction script](../../../scripts/actor-arena-wasm-state-transfer-measure.sh)

Each cell directory also contains its environment record, exact reproduction
command, paired-delta SVG, aggregate JSON, and unaggregated pair reports.
