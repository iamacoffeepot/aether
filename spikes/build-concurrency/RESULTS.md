# Spike: concurrent-build capacity on the fleet host

2026-08-13. Question: how many workspace builds can the fleet host (32 cores,
31 GiB RAM, single NVMe) run concurrently, and which resource binds first?
Workload: this workspace, `cargo build --workspace --all-targets --locked`,
sccache disabled, rustc 1.97.1, source at db737451f. Every cell ran inside a
systemd scope (`MemoryMax=26G`, `MemorySwapMax=0`) so an overrun kills the
cell, not the coordinator. `driver.sh` is the harness; `driver.log.txt` the
raw cell log.

## Measurements

| Cell | Config                          | Wall  | Largest process RSS | Aggregate RAM |
|------|---------------------------------|-------|---------------------|---------------|
| B    | 1 x cold, `-j8`                 | 3:43  | 3.2 GiB             | 5.1 GiB       |
| C    | 2 x cold, `-j8`, concurrent     | 5:40  | 3.2 GiB each        | 9.8 GiB       |
| D    | 2 x warm (root touch), `-j8`    | 3:08  | 3.2 GiB             | 8.0 GiB       |
| A    | 1 x cold, uncapped `-j32`       | 3:03  | 3.4 GiB             | 14.9 GiB      |

Aggregate RAM is `max(MemAvailable) - min(MemAvailable)` sampled at 2 s
during the cell. Cell C was planned at 4 concurrent builds; the driver's
disk gate cut it to 2 (see finding 1).

## Findings

1. **Disk binds first, by a wide margin.** One cold all-targets target dir
   is 100 GB — the test binaries carry full debuginfo and dominate. Four
   concurrent cold builds would want ~400 GB. This also explains the
   unbounded growth of a long-lived shared target dir (#4912's 246 GB).
2. **RAM was overestimated.** The uncapped `-j32` cold build peaks at
   ~15 GiB aggregate, largest single rustc/link at 3.4 GiB. The 31 GiB box
   supports ~5 concurrent `-j8` builds (~5 GiB each) before RAM binds.
3. **Job caps are nearly free.** `-j32` beat `-j8` by only 18% on a solo
   cold build (3:03 vs 3:43) — the crate graph's critical path dominates.
   Capping per-build jobs to share the box costs little.
4. **Concurrency pays despite NVMe contention.** Two concurrent colds ran
   1.5x slower each (5:40 vs 3:43) but net throughput still improved: one
   build per ~2.8 min vs 3.7 solo.

## Consequences for the pipeline

- Max concurrent builds today: **2, disk-bound.** With `line-tables-only`
  debuginfo shrinking target dirs, the projected ceiling is **~5,
  RAM-bound** — the debuginfo trim is the unlock, and belongs with #4912's
  per-slot target dirs (100 GB per slot is unaffordable untrimmed).
- Per-lane `CARGO_BUILD_JOBS=8` (or a shared cargo jobserver) is the right
  global compile budget; lane count itself need not be the throttle.
- Verify lanes should run `CARGO_INCREMENTAL=0` + sccache (CI parity, and
  sccache does not cache incremental compilations — the reason hosted-CI
  hit rates look perfect while lane hit rates do not). Construct lanes keep
  incremental for the edit loop and ignore their sccache hit rate.
