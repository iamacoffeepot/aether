# ADR-0086: Decouple settlement from the trace pipeline; decentralize trace to per-actor rings

- **Status:** Proposed
- **Date:** 2026-05-23

## Context

Mail tracing (ADR-0080) and lifecycle settlement (ADR-0082) currently share one pipeline. The producer hooks (`record_sent` / `record_received` / `record_finished`, plus `HoldOpen` / `Release` for the settlement hold contract) push `TraceEvent`s onto a per-root-sharded queue (`ShardedTraceQueue`, #1063). A drainer thread parks `BATCH_INTERVAL = 1ms` between drains and ships `BatchedTraceEvents` to the `TraceObserverCapability`. The observer folds events into per-root state — `RootState { in_flight, held_open }` in a fixed ring (#1054) — and fires `Settled { root }` (mail to the chassis mailbox) on the zero-transition `(in_flight == 0 && held_open == 0)`. The lifecycle driver gates each frame advance on `Settled` for the advance's root (ADR-0082).

This fuses two concerns with opposite requirements:

- **Settlement** is control-plane: *exact*, on the frame's critical path, latency-sensitive.
- **Tracing** is observability: *best-effort*, off the critical path, tolerant of loss.

Three problems follow from the coupling:

1. **Settlement detection is drainer-gated.** The frame cannot observe `in_flight == 0` until the drainer ships (≤1ms park) and the observer folds. So frame-advance carries up to ~1ms of latency *after the work actually finished* — a fixed cost that grows as a fraction of the frame budget at higher refresh: roughly 6% at 60Hz (16.6ms), 14% at 144Hz (6.9ms), 24% at 240Hz (4.2ms).

2. **Observability is on the critical path** — the failure class #1048 named. #1048 was an O(n) eviction in the observer that, under sustained 60Hz load, made the observer fall behind, ballooned settlement latency past the frame tick, and *permanently wedged the lifecycle*. It has since been patched twice — the #1054 fixed-ring (O(1)-on-overwrite eviction) and a lifecycle advance-timeout that force-completes a stuck advance — but #1048's own prevention section flagged the class-level fix as an open, ADR-level question: *"decide deliberately whether frame-pacing should depend on the trace pipeline at all."* **This ADR is that decision.**

3. **The per-root sharding (#1063) is a contention band-aid.** A single `SegQueue`'s `push` contends on its tail cache line; many workers pushing concurrently ping-pong it (~50% of worker CPU under saturation, the `mail_saturation_profile`). Sharding by root spreads concurrent pushers across 64 tails. It works, but it keeps a contended shared structure in the producer hot path, and it exists *only* to preserve per-root FIFO ordering — ordering that is needed *only because* settlement is folded from the stream in order.

The root cause of all three is that **settlement rides the trace event stream.** Decouple them and each takes its proper shape.

## Decision

Split the pipeline into two independent layers fed from the same producer hooks.

### 1. Settlement layer — control-plane, exact, emit-time

Per-root accounting moves to emit time. When a `Sent` / `Finished` / `HoldOpen` / `Release` is produced, the producing thread updates a shared per-root counter directly and fires `Settled { root }` **synchronously** on the zero-transition. No drainer, no fold, no ≤1ms lag.

- **Structure:** a *striped* concurrent map keyed by root `MailId`; each cell a packed `u64` holding `in_flight: u32 | held_open: u32`. The decrement-and-test is a CAS loop, so the joint `(in_flight == 0 && held_open == 0)` test is a single atomic read and a `Finished`'s decrement-to-zero cannot race a concurrent re-opening `Sent` (a root re-opens 0→1 when a late `Sent` arrives under the same root). Insert-on-first-event, drop-on-settle.
- **Why atomics, not mail-back-to-root:** settlement is on the frame's critical path. Routing accounting as mail to the root actor reintroduces the mail-hop latency we are removing and serializes a wide fan-out's accounting through one actor. Atomics are also the runtime layer's existing grain — `SlotState` (`AtomicU8`), `SpinPark` (`AtomicUsize`), and the correlation counter are all runtime atomics. (The "actor state is plain fields, no locks/atomics" rule governs component/cap state, not runtime plumbing; settlement accounting is plumbing the observer already owns centrally.)
- **Convergence:** this is the same produce-before-consume accounting the run-token scheduler direction is heading toward (hand in-flight credit to an emitted message before releasing the token). Built standalone now, the counter can later migrate onto that release path rather than fight it.

### 2. Observation layer — best-effort, off the critical path, decentralized

Trace events move to **per-actor rings** — the same per-actor storage ADR-0081 established for logs (the `ActorLogRing` / `Local<T>` mechanism, queried via `aether.log.tail`). A sibling `aether.trace.tail` surfaces each actor's trace ring. The trace tree is reconstructed on demand by a query coordinator that fans out `tail` queries and stitches events by their lineage keys (`mail_id` / `parent_mail` / `root`, ADR-0083). Reconstruction is already purely lineage-keyed today — the central `mails_by_root` / `by_mail` indices are query optimizations, not semantics — so it survives decentralization unchanged.

**Completeness is self-reporting.** Per-actor rings carry the same `truncated_before` cursor the log ring already returns, so a reconstructed tree distinguishes *present* / *known-evicted-at-actor-A-before-T* / *genuinely-absent* nodes, and can flag itself known-incomplete by comparing reconstructed node count against the (authoritative) settlement layer. This is strictly more honest than the central ring, which loses nodes silently on wrap.

### 3. Retire

Once the above lands: the per-root `ShardedTraceQueue` (#1063), the central drainer thread, and the `TraceObserverCapability`'s role as settlement authority (it reduces to the query coordinator, or is removed in favour of per-actor rings + a coordinator).

## Consequences

**Positive**

- **Settlement latency:** frame advance fires synchronously on the zero-transition instead of waiting up to a drainer interval (~1ms). The win scales with refresh rate. (Sized in Phase 0; not asserted as fact until measured.)
- **Robustness:** observability leaves the engine's critical path entirely — the #1048 prevention §1 decision. A slow or lossy trace subsystem can no longer affect frame pacing. The advance-timeout net stays as defense-in-depth.
- **Contention:** the bulk observational volume moves to per-actor rings (single-writer, contention-free by the run-token's one-thread-per-actor guarantee), so the sharded queue and drainer disappear. The only shared structure left is the settlement counter map — cheap per-root cells, not an ordered FIFO.
- **Legible completeness:** trace trees self-report truncation and cross-check against an exact oracle, instead of failing silently on ring wrap.

**Negative / costs**

- The settlement counter is a shared concurrent structure with a CAS zero-transition — a new correctness kernel that must be proven race-free. This is the riskiest part; Phase 1 lands it in *shadow mode*, cross-checked against the incumbent fold, before anything gates on it.
- Trace-tree queries become N fan-out calls + a coordinator/client merge instead of one central query (the latency harness's single `DescribeWindow` becomes per-actor queries + merge). Mechanical, but more moving parts at query time.
- Per-actor rings evict independently, so deep/old trees are incomplete at finer granularity than the central ring — mitigated by self-reporting truncation.

**Neutral / out of scope**

- The dominant dispatch cost — the parked-worker wakeup (~4.3µs) — is untouched. This is a trace/settlement change; it does not move headline dispatch latency on a wakeup-bound workload.
- #1073 (batch a handler's fire-and-forget sends) is re-scoped by this: per-actor rings remove the contention #1073 batched around, leaving settlement-counter coalescing (`in_flight.fetch_add(N)` for wide fan-out) + clock-read coalescing as the surviving value. Re-evaluate #1073 after Phase 3.

## Phasing

Each phase lands independently and is measurable on its own.

- **Phase 0 — De-risk.** Instrument settlement-detection latency (advance `Sent` → `Settled`) on a real workload to size the win. Microbench the packed-`u64` counter + race-free zero-transition in isolation. Go/no-go gate.
- **Phase 1 — Emit-time counters, shadow mode.** Add the per-root atomic accounting at the producer hooks *alongside* the existing observer fold; fire a shadow `Settled` and assert it agrees with the observer's. Risky kernel landed dormant + cross-checked against the incumbent.
- **Phase 2 — Flip frame-gating.** Lifecycle subscribes to the emit-time `Settled`; frame advances synchronously. Keep the advance-timeout net. Observer is no longer the settlement authority. Measure latency before/after.
- **Phase 3 — Decentralize trace.** Per-actor trace rings (extend ADR-0081); `aether.trace.tail`; `DescribeTree` / `DescribeWindow` become a fan-out-and-stitch coordinator; the harness queries per-actor. Retire `ShardedTraceQueue` (#1063) + drainer.
- **Phase 4 — Cleanup.** Remove the sharded queue / drainer / observer settlement role; supersede the affected ADR-0080 sections.

## Alternatives considered

- **Mail settlement events back to the root actor** (actor-pure accounting). Rejected for the critical path: reintroduces mail-hop latency (the cost being removed) and serializes a wide fan-out's accounting through one actor. Fine for an off-critical-path subsystem; not for frame gating.
- **Weighted reference counting / credit-passing** (credit rides on each mail, splits on send, returns on finish; root settles on full reclaim). Reduces updates to the shared cell but still needs a convergence point, and is more machinery than our scale (one frame root, modest fan-out) warrants. Noted as a future optimization if counter contention ever bites.
- **Keep per-root sharding, tune it.** Rejected: a band-aid that keeps a contended shared structure in the hot path and exists only to preserve the stream ordering the decouple makes unnecessary. The decision is to install permanent infrastructure, not a better band-aid.
- **Leave settlement on the trace pipeline; only shorten/eagerly-wake the drainer.** Rejected: trades latency for drain overhead and does nothing about the critical-path coupling #1048 flagged — the same band-aid posture.
