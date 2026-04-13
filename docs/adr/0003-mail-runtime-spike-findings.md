# ADR-0003: Mail-runtime spike findings

- **Status:** Accepted
- **Date:** 2026-04-13

## Context

ADR-0002 set the architecture as a thin native substrate hosting a WASM runtime, with engine components communicating exclusively via mail. That ADR was filed as **Proposed** because the load-bearing risk — *can a pure-mail boundary meet per-frame budgets at game-engine workloads?* — could not be answered from first principles. Issue #7 specified a measurement spike against four representative workloads; PRs #10, #11, and #12 implemented it.

This ADR records what the spike found and the resulting status of ADR-0002.

## Decision

**ADR-0002 is elevated from Proposed to Accepted as written.** The pure-mail boundary clears the 16.67ms 60Hz frame budget at the gate cell with several orders of magnitude of headroom, scales linearly across the matrix, and exhibits the batching amortization the ADR predicted. No deferred lever from ADR-0002's "Optimization paths" section is being pulled at this stage.

## Evidence

Numbers below are from a single developer macOS machine (Apple Silicon, release builds of both host and guest, wasmtime 30, single-threaded). Absolute values will vary by hardware; ratios and scaling shapes are what carry across.

### Gate cell (issue #7's success criterion)

| Workload | n_actors | work_per_actor | mean | p99 | Headroom vs 16.67ms |
| --- | --- | --- | --- | --- | --- |
| broadcast | 8 | 10,000 | 20.7µs | 25.5µs | ~800× |
| mixed | 8 | 10,000 | 21.0µs | 29.1µs | ~800× |

Both clear the gate by roughly three orders of magnitude.

### Scaling

- **Broadcast** scales linearly in both `n_actors` (1–32) and `work_per_actor` (100–100,000). The most extreme cell (n=32 × work=100k) sits at 830µs mean — still ~20× under budget.
- **Mixed** is approximately 2× broadcast at the same cell, as expected from the doubled mail count per frame; same scaling shape.
- **Chain** scales linearly in depth: ~270ns per link, consistent from D=2 through D=16.

### Batching amortization

The bulk workload directly tests ADR-0002's batching prediction by sweeping batch size *K* from 1 to 4096 with fixed per-item work:

| K | mean per-mail | amortized per-item |
| --- | --- | --- |
| 1 | ~75ns | ~75ns/item |
| 16 | ~400ns | ~25ns/item |
| 256 | ~6.0µs | ~23ns/item |
| 4096 | ~97µs | ~24ns/item |

Per-item amortized cost drops ~3× from K=1 to K=4096, exactly the shape the ADR predicted: at small K the boundary cost dominates, at large K the work cost dominates and boundary becomes negligible. Above K≈16 the amortization curve flattens — that's where the boundary cost is fully absorbed.

### Per-mail boundary cost

Roughly 75ns per host↔guest mail crossing on this machine. As a fraction of a per-actor frame budget of ~1ms (16.67ms ÷ 16 subsystem actors), that is ~0.0075% — negligible relative to any plausible per-actor work cost.

## Caveats — what this spike does NOT prove

These are limits on how far the numbers should be read, not failures of the result. They're recorded so future readers don't over-interpret.

- **Work cost is not representative of real subsystem code.** `do_work` is a register-bound ALU loop with no memory access, no branches, no cache pressure, no allocator activity, and no WASM bounds-check load/store overhead. Real subsystem ticks (physics, scene, AI) are dominated by memory bandwidth, branch behaviour, and cache effects that this spike does not exercise. The spike measures **boundary cost honestly**; it does not predict real-game throughput.
- **Sub-microsecond cells hit the timing-resolution floor.** Per-iteration `Instant::now()` overhead is ~20–50ns on these platforms; cells with mean < ~100ns (notably bulk K=1 reported as `0.0µs`) are at the limit of what individual-iteration timing resolves. The verdict-relevant cells (gate and above) are measured with timing-overhead noise < 0.1%.
- **Single-threaded execution.** Internal parallelism inside actors (declared as future work in ADR-0002) is not measured. The chain workload's serial-dependency contrast with broadcast is muted as a result; it materializes once parallel execution lands.
- **wasmtime only.** No comparison to other WASM runtimes.
- **Single hardware data point.** No matrix run on Linux, Windows, or older hardware. Ratios should hold; absolute numbers won't.

## Consequences

- **The substrate-and-mail architecture moves forward as ADR-0002 describes.** No re-architecture; no deferred-lever PRs in the immediate roadmap.
- **The deferred-levers section of ADR-0002 stays deferred.** Hierarchical shared memory, substrate-hosted fast paths, read-only caches, and mail compaction are not pre-emptively built. They get pulled when a specific concrete workload demands one of them.
- **Real-workload characterization is identified as future work.** When real subsystem code starts existing — physics integrators, scene traversal, asset streaming — the spike's measurement harness should be re-pointed at workloads that exercise memory access, branches, and cache. That's a separate effort, not an immediate blocker.
- **The spike crates (`aether-mail-spike-host`, `aether-mail-spike-guest`) become reference artefacts.** They live in the workspace as throwaway/educational code. When the real substrate begins, it can borrow shape from them but should not depend on them.
- **Boundary cost (~75ns/mail) is the operating baseline** that future engine-design discussions can reason against.

## Alternatives considered

- **Run the matrix on more hardware before declaring.** Rejected as scope creep — the headroom is large enough on a single machine that the verdict is unlikely to flip on slower hardware. If we ever see a host machine where this matters, it's cheap to re-run the spike crate.
- **Pull a deferred lever now (e.g., add hierarchical shared memory) "while we're here."** Rejected: violates the "tighten only when measurement demands" principle ADR-0002 is built on. Each lever should be justified by a workload that fails without it.
- **Defer Acceptance until real workloads characterize per-subsystem cost.** Rejected: the spike answered the question ADR-0002 actually posed (is the boundary the bottleneck?). Per-subsystem characterization is a different, later question.
