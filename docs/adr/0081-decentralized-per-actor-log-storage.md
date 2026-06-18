# ADR-0081: Decentralized per-actor log storage

- **Status:** Accepted
- **Date:** 2026-05-19
- **Supersedes parts of:** ADR-0023, ADR-0077

## Context

ADR-0023 shipped substrate log capture as a single substrate-global ring buffer fed by a `tracing_subscriber::Layer`. ADR-0077 then moved per-handler *buffering* into a per-actor `LogBuffer`, which drains a `LogBatch` mail to `LogCapability` at handler exit (or eagerly on WARN/ERROR). The cap forwarded each batch to the hub via `egress_log_batch`.

Two follow-on changes since then make the centralized-storage half of ADR-0077 awkward:

1. **Issue 776 amendment to ADR-0077.** The hub no longer pulls a stream of `LogEntry` frames over the wire; the cap holds a 2,000-entry ring locally and serves `aether.log.read` mail on demand. With egress retired, `LogCapability`'s only remaining job is to *hold* a ring that every other actor flushes into.
2. **Issue 825 (parented on issue 776).** Crash-time post-mortem retention — ADR-0023 §3's "the buffer survives engine exit" property — went silently with the hub-side store when issue 763 retired it. Restoring that property on a centralized ring means giving the panic hook cross-thread access to a structure owned by a different actor's dispatcher thread, which adds either an `Arc<Mutex<VecDeque>>` on the hot logging path or a separate panic-shared mirror ring.

Two structural costs the centralized model still pays even on the steady-state path:

- **Per-tick mail traffic.** Every actor at every handler exit sends a `LogBatch` envelope. At N actors × 60Hz that's `N×60` envelopes/sec of overhead carrying no payload state — pure flush hop.
- **Asymmetry with ADR-0077's own move.** Per-actor buffering already pushed the *write* path out to each actor. The flush hop is the only thing that re-centralizes. The model is half-decentralized and half-centralized; finishing the move is the symmetric completion.

Pre-issue-776 the central ring earned its cost as the single egress point to the hub. Post-issue-776 that property is gone. What's left is inertia.

## Decision

Each actor owns its own bounded persistent log ring. Emits land in the ring directly — no per-handler `LogBuffer`, no `LogBatch` mail, no flush hop. `LogCapability` retires. `engine_logs` becomes a query coordinator that walks the actor registry and merge-sorts per-actor tails. Crash dump is per-actor file write on the panicking actor's own thread.

### 1. Per-actor ring replaces `LogBuffer` + central store

`ActorLogRing` is a bounded `VecDeque<LogEvent>` held in each actor's `ActorSlots` (same `Local` primitive ADR-0077 §2 used for `LogBuffer`). Default capacity is 1024 entries; `AETHER_ACTOR_LOG_RING_SIZE` overrides it for the whole substrate, resolved at chassis boot through the `ActorRingConfig` derive-`Config` knob (argv > env > default, ADR-0090) and seeded into each actor's ring as it spawns. The trace-side sibling (ADR-0086) carries the matching `AETHER_ACTOR_TRACE_RING_SIZE` knob. Overflow drops oldest (FIFO eviction); ADR-0023's `truncated_before` cursor surfaces the gap to the query side.

The `ActorAwareLayer` from ADR-0077 §2 still differentiates in-actor vs host branch. The in-actor branch now pushes directly into `ActorLogRing` instead of staging into `LogBuffer` for later drain. The priority-flush-at-WARN path retires — there's no flush to fast-track, the entry is already where queries will find it.

`LogBuffer` and the `drain_buffer()` family retire. The `ACTOR_DISPATCH` TLS slot that stamped each handler with its `MailTransport` for drain-side egress is no longer needed for logging (it may stay for other purposes if any handler-side egress survives).

### 2. `LogCapability` retires; `LogBatch` kind retires

The `aether.log` mailbox and the `LogCapability` actor both retire. Their substrate registration is removed at boot. The `LogBatch` and `LogEvent` types are no longer wire kinds — `LogEvent` (or whatever shape carries `level + target + message + timestamp + sequence + origin`) survives only as a substrate-internal type backing `ActorLogRing` and the new query reply shape.

The `aether.log.read` / `aether.log.read_result` mail surface that the issue 776 amendment introduced retires alongside the cap.

### 3. Per-actor queries; client-side merge in the MCP

Each actor exposes its ring via the framework-built-in `aether.log.tail { max, level, since }` dispatch arm in `aether-substrate::actor::native::dispatch` — every actor responds, the author writes no handler. The reply (`aether.log.tail_result`) carries the slice matching the filter plus a `next_since` cursor and a `truncated_before` signal when the ring evicted entries the caller hadn't seen yet.

The MCP exposes this as the `actor_logs(engine_id, mailbox_name, max?, level?, since?)` tool: one query per actor, one round-trip per call. Agents that want a cross-actor view call the tool once per mailbox and merge in their own context. There is no substrate-side aggregator and no `aether.log` mailbox — the centralized cap retired with `LogCapability`.

*This is a revision of the original ADR text.* The first draft had a substrate-side `LogAggregator` actor at `aether.log` doing the fan-out via mail + `wait_reply`. That implementation hit several friction points in the existing cap API (no shared `WaitError` impl, no `wait_reply_any_of` for parallel reply collection, the actor model's stated norm against caps walking the actor registry, manual `Envelope` construction footguns). The cleanest path was to leave aggregation out of the substrate entirely. Filed as iamacoffeepot/aether#960 for the missing fan-out primitives that would let a substrate-side aggregator land cleanly if one ever becomes worthwhile.

The per-actor roundtrip cost is one mpsc round-trip per query; `actor_logs` is human-paced (MCP polling at most a few Hz), so this is not a hot path even if a caller iterates over 20 actors. A future ADR may introduce a query-coordinator capability if aggregation grows policy (per-namespace dedup, level normalization, format coercion); for v1 callers compose at the client.

### 4. Crash dump = panicking actor only

The substrate's `panic_hook` (ADR-0023 §3 lineage) extends: on a panic in an actor thread, the hook accesses the panicking actor's `ActorLogRing` via the thread-local `ActorSlots`, serializes ring contents + a panic header (payload, location, thread name, timestamp) as JSONL, and writes `<crash-dir>/<actor>.jsonl`. Crash dir defaults to `$XDG_DATA_DIR/aether/crash/<unix_ms>/`; `AETHER_CRASH_LOG_DIR` overrides; `AETHER_CRASH_LOG_DISABLE=1` skips the write entirely. Per-crash subdirectory (`<unix_ms>/`) preserves history across crashes without unbounded growth in any single file; operator cleanup is `rm -rf` against old subdirs.

Each actor's `unwire` hook (clean-shutdown path) also writes its ring to the same crash dir under the *current* `<unix_ms>` subdir, so clean shutdowns produce a complete per-actor log set for the same run if `AETHER_CRASH_LOG_DIR` is set. The default-disabled behavior on clean shutdown is parked — only the panic-hook path writes by default in v1.

A future `aether-crash-splice` tool (or similar) can merge `<crash-dir>/<unix_ms>/*.jsonl` by sequence to recover a unified timeline. That tool is not part of this ADR's scope.

### 5. Host events stay stderr-only

Substrate-host events (substrate boot, scheduler, panic hook itself, anything outside an actor's dispatch — ADR-0077 §3's "host branch") continue to go to stderr only via the `tsfmt::Layer` registered at `init_subscriber()`. They do not enter any ring and do not appear in `engine_logs` responses. This matches today's behavior post-issue-#601. Routing host events into a queryable construct (pseudo-actor or dedicated chassis-host ring) is a separate ADR if it becomes load-bearing.

### 6. `AETHER_LOG_FILTER` and stderr surface unchanged

`EnvFilter` (reads `AETHER_LOG_FILTER`, default `info`) and `tsfmt::Layer` to stderr stay in the subscriber stack as ADR-0077 §3 specified. Operators running a substrate from a terminal see logs locally regardless of any of this ADR's changes.

### 7. Wasm trampolines need no special path

Each loaded wasm component is a `WasmTrampoline` registered as a `NativeActor` at `aether.component.trampoline:NAME` (ADR-0074, issue 634 Phase 4). Under this ADR, a trampoline owns an `ActorLogRing` like any other actor. Guest emits cross the FFI synchronously inside `receive_p32` (or similar exports) on the trampoline's dispatcher thread, so the host-side `ActorAwareLayer` sees the trampoline's `ActorSlots` and lands the entry in the trampoline's ring through the same in-actor branch native actors take.

The `cfg(target_arch)` split ADR-0077 §2 introduced for the drain path — process-global `WASM_TRANSPORT` for the wasm side, per-handler `ACTOR_DISPATCH` TLS for the native side — retires. Both were artifacts of needing to stamp the destination mailbox for the `LogBatch` flush hop. With no flush hop, no stamp is needed.

The fan-out query side picks up trampolines naturally: `engine_logs` walks the actor registry, which already includes every loaded trampoline; each trampoline's *host-side* handler answers `aether.log.tail` with its ring slice. The guest doesn't implement the tail handler — it lives on the trampoline actor next to the host-side load/replace/drop trampoline logic, same way other built-in maintenance kinds are handled.

## Consequences

### Positive

- **Mail bandwidth reduction.** `LogBatch` envelopes retire; no per-handler flush hop. At a baseline 20 actors × 60Hz that's 1200 envelopes/sec eliminated — pure overhead before this ADR.
- **Crash dump becomes structurally free.** The panicking actor's ring is owned by the same thread the panic hook runs on. No cross-thread access required, no shared mutex on the logging hot path. Issue 825 is closed by construction for the load-bearing "what was the crasher doing" case.
- **Architectural symmetry.** ADR-0077 already decentralized the buffer; this ADR completes the move. Both write *and* storage are per-actor.
- **Per-actor query is the natural surface.** "What did actor X log?" no longer requires filtering a merged stream — it's the actor's own ring. Aggregation is a query-time merge, not a steady-state cost.
- **Cap-side complexity drops.** `LogCapability` retires entirely. The TLS re-entry guard from ADR-0077 §4 (`IN_LOG_PIPELINE`) may also retire if no other site emits `tracing::*` events from the push path.
- **Wasm asymmetry collapses.** ADR-0077 §2's `cfg(target_arch)` split for the drain path retires (`WASM_TRANSPORT` process-global, `ACTOR_DISPATCH` per-handler TLS). Wasm trampolines route logs the same way as native actors — §7 above.

### Negative

- **Memory grows linearly with actor count.** Today's centralized 2000-entry ring is replaced by N × 1024 per-actor rings. At N=20 that's ~4MB (similar order, distributed); at N=200 it's ~40MB. The default ring size or the per-actor cap may want revisiting if substrate-wide actor counts grow large.
- **`actor_logs` queries one actor at a time.** Replaces the pre-ADR `engine_logs` aggregated tool. Each call is single-roundtrip and queries a named mailbox; cross-actor views require N calls from the client. Acceptable for human-paced polling; high-frequency log consumers (none today) would want either client-side parallelism (`futures::join_all`) or a substrate-side aggregator (deferred — see iamacoffeepot/aether#960 for the missing primitives).
- **Multi-actor crash splice is not solved in v1.** When actor X panics, only X's ring is dumped. The cross-actor "what were other actors doing right before X died" property requires either pre-flushed-to-disk per-actor logs (continuous IO cost) or cross-thread coordination (locking cost). Deferred until forensic value is established.
- **Wire kind retirement.** `LogBatch` was an exported `Kind` in `aether-kinds`. Removing it is a wire-shape break. Hub-side and MCP-side consumers must drop any leftover references; the `engine_logs` MCP tool's response shape stays the same (no consumer-visible change there), but the *internal* `aether.log.read` mail surface that issue 776's amendment introduced retires.

### Neutral

- **MCP tool renamed: `engine_logs` → `actor_logs`.** Per-call shape is now `(engine_id, mailbox_name, max?, level?, since?)` → `{entries, next_since, truncated_before}`. Each call returns one actor's logs; the previously-aggregated wire response retires. Consumers polling for cross-actor merges call the tool N times.
- **`AETHER_LOG_FILTER`, `tsfmt::Layer` to stderr, panic-hook chain retain their existing semantics.**
- **Per-buffer length cap (ADR-0077 follow-up).** Replaced by the per-actor ring's bounded capacity and FIFO eviction. The unbounded-growth concern from ADR-0077's negatives section retires.

## Alternatives considered

- **Centralized ring + `Arc<Mutex<VecDeque>>` for crash-dump access.** Keep ADR-0077's centralized store, lift its ring out of `LogCapability` into a substrate-shared `Arc<Mutex<...>>` accessible from the panic hook. Rejected: adds lock-acquire on every log push (hot path), and doesn't address the steady-state mail-bandwidth cost. The single-vs-distributed memory tradeoff is real but the crash-dump motivation alone doesn't justify lock contention.
- **Snapshot mechanism.** `LogCapability` periodically publishes a copy of its ring to a panic-shared structure; panic hook reads the snapshot. No hot-path lock cost. Rejected: snapshot staleness undermines the "what happened right before the crash" property — the events the panicking handler emitted in its current dispatch are the most relevant context and the most likely to be missing from a periodic snapshot.
- **Per-actor file flush continuously (the splice idea).** Each actor's `LogBuffer` flushes to its own on-disk file at handler exit; crash dump is `<crash-dir>/*.jsonl` merged by sequence. Property-wise this recovers full multi-actor history. Rejected for v1: per-handler fsync at `N × 60Hz` is real IO cost (~600 flushes/sec at N=10), and accepting non-fsynced buffered writes loses data on crash. The pattern stays as a future option if multi-actor crash forensics becomes load-bearing.
- **Chassis-host pseudo-actor.** Register a fake "chassis-host" actor at substrate boot that owns a ring like any other actor; route substrate-boot / scheduler / panic-hook-chain events to it. Rejected for v1: host events have been stderr-only since issue #601 and no consumer needs them in `engine_logs` today. The pseudo-actor is the right shape *if* host-event queryability becomes load-bearing; deferred to that ADR.
- **`Arc<RwLock<VecDeque>>` per-actor ring exposed via registry.** Skip the `aether.log.tail` mail roundtrip; readers borrow the ring directly via the registry. Faster per query, but the cross-thread read path is a lock on the steady-state write side (writes wait on outstanding reads). Rejected: mail-based query keeps symmetry with the rest of the actor model and the lock-free write path is more valuable than `engine_logs` latency.

## Follow-up work

- **Multi-actor crash splice.** If forensic value is established, ship per-actor file flush + a `aether-crash-splice` merge tool. Cost and shape sketched in the splice alternative above.
- **Eviction-to-disk (write-behind ring).** In-memory ring as bounded tail of an unbounded on-disk log. Lets `engine_logs` reach back further than the ring at the cost of continuous IO. Pattern is well-known; file when "history beyond the ring" is load-bearing.
- **Host-event queryability.** Pseudo-actor or dedicated chassis-host ring for substrate-boot / scheduler / panic-hook events. Separate ADR.
- **Query-coordinator capability.** If `engine_logs` aggregation grows policy (dedup, namespace filtering, format coercion), promote the merge logic from the RPC handler to a dedicated capability. v1 keeps it inline.
- **Per-actor ring size as actor-declared.** Today's design is a substrate-wide default. If specific actors emit pathologically more than others, an actor-level override (compile-time or boot-time) becomes valuable.
