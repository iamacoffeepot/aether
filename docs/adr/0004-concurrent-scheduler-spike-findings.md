# ADR-0004: Concurrent-scheduler spike findings

- **Status:** Accepted
- **Date:** 2026-04-13

## Context

ADR-0003 validated the per-mail boundary cost of the pure-mail architecture single-threaded, with several orders of magnitude of headroom against the 60Hz frame budget. The remaining open scaling question — *can the substrate dispatch N actors' ticks concurrently across a worker threadpool, and how does it scale?* — was specified as issue #14 and implemented across PRs #15 and #16.

The model under test is the simplest honest baseline: K worker threads sharing a single `Mutex<VecDeque<Tick>>` + `Condvar` queue, with a `Mutex<usize>` + `Condvar` frame barrier. No work-stealing, no per-worker deques, no batch compaction. Hand-rolled with `std` primitives only — if we are measuring a scheduler, we own it end to end.

This ADR records the verdict and the resulting engineering posture.

## Decision

**The worker-pool scheduler clears the 60Hz frame budget across the entire N×K matrix — N up to 4096 actors, K up to 12 workers — and proceeds as the scheduler baseline for the first real substrate.** Scaling is visibly sub-linear and the single shared queue is the limiting term; work-stealing per-worker deques are identified as the most-likely-next optimization lever but are **not** pulled now. No re-architecture.

## Evidence

Numbers below are from the same single developer macOS machine as ADR-0003 (Apple Silicon, 12 hardware threads, release build, wasmtime 30). Absolute values will vary by hardware; the scaling shapes and ratios are what carry across.

### Budget

| Workload | worst cell (mean) | worst cell (p99) | Budget vs 16.67ms |
| --- | --- | --- | --- |
| parallel_broadcast | N=4096, K=1 → 11.3ms | 12.1ms | fits |
| parallel_mixed | N=4096, K=1 → 11.9ms | 12.8ms | fits |
| churn | N=4096, K=12 → 1.13ms | 1.41ms | ~15× under |

Every cell in the measured matrix clears the 16.67ms budget. The tightest cell (parallel_mixed N=4096 K=1) still has ~5ms of headroom.

### Speedup

At the cells with enough exposed parallelism to matter (high N):

| Workload, N=4096 | K=1 | K=2 | K=4 | K=12 | K=12 speedup |
| --- | --- | --- | --- | --- | --- |
| parallel_broadcast | 11.3ms | 5.8ms | 3.0ms | 3.0ms | ~3.8× |
| parallel_mixed | 11.9ms | 6.7ms | 3.7ms | 4.3ms | ~2.8× |

Speedup saturates between K=2 and K=4 and then goes flat (or backward for parallel_mixed). On 12 hardware threads we never get closer than ~3.8× linear. The scheduler is leaving cores idle.

### Contention — the shape behind the sub-linearity

`churn` uses the same dispatch pattern as parallel_broadcast with `work_per_actor = 10` (nominal) — guest work is ~100× smaller than broadcast's, so the frame cost is dispatch-dominated. That makes scheduler overhead directly visible:

| N | K=1 | K=2 | K=4 | K=12 |
| --- | --- | --- | --- | --- |
| 64 | 7.7µs | 13.6µs | 18.7µs | 38.1µs |
| 512 | 47.3µs | 62.2µs | 74.9µs | 157.5µs |
| 4096 | 545.7µs | 436.4µs | 585.2µs | 1133.3µs |

Adding workers *actively hurts* dispatch-dominated workloads: at N=64 the frame cost grows 5× from K=1 to K=12; at N=4096 it grows ~2×. This is the single shared queue's mutex contention. The scheduler floor (churn mean ÷ N, at the best K per N) is ~**100–150ns per tick** — the same order of magnitude as ADR-0003's ~75ns per-mail boundary cost, consistent with *"the queue's lock is on the critical path of every dispatch."*

### Small-N regression

At low N the pattern inverts in a different way: parallel_broadcast at N=1 goes from 6.7µs at K=1 to 16.1µs at K=12. With only one actor there is no parallel work to absorb, and the worker-wakeup / signalling overhead just piles up. A real scheduler should not scale K blindly with hardware threads — the right K is bounded by exposed parallelism.

## Caveats — what this spike does NOT prove

- **Single shared hardware.** Same caveat as ADR-0003. Ratios should carry; absolute numbers will not.
- **wasmtime only.** No comparison to other WASM runtimes.
- **ALU-bound guest work.** The `do_work` loop is still register-bound with no memory or cache pressure; a real subsystem's per-tick cost will be dominated by memory behaviour this spike does not exercise. The **scheduler** numbers here are honest; the **per-tick totals** are not predictive of real subsystem frames.
- **Memory footprint not measured programmatically.** N=4096 actors ran to completion on a 16GB developer laptop without visible pressure, which is a loose upper bound (<~64KB per actor is plausible for wasmtime's default linear memory) — not a real characterization. A proper measurement is future work if per-actor footprint becomes a binding constraint.
- **Scheduler design is deliberately the simplest honest baseline.** Work-stealing, per-worker deques, lock-free queues, NUMA-aware placement — all unimplemented here. The point was not to land the final scheduler, but to measure where the unoptimized baseline breaks.
- **Chain workload not re-measured.** Serial cross-actor dependencies (ADR-0003's chain) were not added to the concurrent matrix; they would not parallelize at all under this model by construction, and the spike focuses on the concurrent-throughput question. When a real workload exposes meaningful cross-actor serial critical paths, that's a separate measurement.

## Consequences

- **The worker-pool scheduler shape moves forward** into the first real substrate: shared `wasmtime::Engine`, per-actor `Mutex<Actor>`, shared queue, frame barrier. Implementation details (atomic counters, condvar signaling choice, actor registry shape) are free to change; the model isn't.
- **Work-stealing per-worker deques are the first-lever candidate** when real scaling requires it. The evidence is churn's K>1 regression at high N: any scheduler improvement that *reduces per-dispatch lock contention* would flatten that curve. Pre-work-stealing options (sharded queues, batched-dispatch-under-one-lock, lock-free queues) can be measured against the same harness when a specific workload asks for it.
- **K should not track hardware threads blindly.** Small-N regression shows that adding workers above exposed parallelism costs latency. A first real scheduler should either cap K based on current actor count, let workers park more aggressively, or adopt a stealing model that leaves idle workers passive rather than polling.
- **ADR-0002's deferred levers stay deferred** (hierarchical shared memory, substrate fast paths, read-only caches, mail compaction). Nothing in this spike motivated any of them over work-stealing, and none are being pre-emptively pulled.
- **The scheduler baseline (~100–150ns per tick under contention, ~75ns per mail, budget-clearing at N=4096)** is the operating baseline that future engine-design discussions can reason against — alongside ADR-0003's per-mail boundary number.
- **`bin/concurrent.rs`, `scheduler.rs`, and the concurrent-matrix CSV plumbing become reference artefacts** alongside the ADR-0003 spike crates. Throwaway/educational code. The real substrate can borrow shape from `scheduler.rs` but should not depend on it.

## Alternatives considered

- **Pull work-stealing now while the spike is fresh.** Rejected: same principle as ADR-0003 rejected pulling hierarchical shared memory. Every lever is justified by a workload that fails without it. Churn's contention signal is *expected* from the baseline and does not break any real requirement; pulling work-stealing pre-emptively violates the "tighten only when measurement demands" principle.
- **Replace `std::thread` + `Mutex` with `rayon` or `tokio`.** Rejected for the spike: we want to measure our scheduler, not a library's. A rayon-based implementation may be the right answer for the real substrate once measurement demands it; that's a separate, later decision.
- **Defer Acceptance until real subsystem code characterizes per-tick cost.** Rejected: the spike answered issue #14's actual question (does concurrent dispatch meet the budget, and where does it saturate?). Real-subsystem cost is a different, later question — the same partition ADR-0003 drew.
- **Add programmatic memory measurement before accepting.** Considered. Rejected as scope creep: N=4096 completed on a commodity laptop, no workload presently depends on a tight per-actor memory bound, and the measurement is cheap to add later if it starts mattering.
