# ADR-0080: Substrate-wide mail tracing with settlement detection as the primary consumer

- **Status:** Proposed
- **Date:** 2026-05-09

## Context

The substrate has no causal-closure detection for mail. Today every lifecycle-or-frame gate that wants to wait for downstream effects either races (`probe.wire` mails `SubscribeInput` fire-and-forget; `LoadResult` returns before `InputCapability` processes it; the first few `Tick`s miss the probe) or papers over the race with a heuristic (`wait_instanced_quiesce` polls every spawned actor's `instanced_pending` counter against a 5 s deadline; the test-side `settle_observations` runs an extra no-tick `capture()` to drain in-flight broadcasts; `await_tick_subscribed` mails a redundant `SubscribeInput` to ride the cap's mpsc-FIFO ordering).

These workarounds compose poorly. The flake `test_bench_scenario::replace_component_preserves_mailbox_identity` fails ~10 % of the time on a busy machine because `wait_instanced_quiesce` exits before the trampoline emits its last `tick_observed` broadcast. The substrate doesn't *know* the broadcast is coming — it just guesses with a deadline.

Issue #707 was filed to address settlement detection for lifecycle gating. The design conversation expanded the scope: tracking every mail's causal lineage gives us settlement as one consumer of a much more general piece of infrastructure. Tying every `Sent` / `Received` / `Finished` event to a `root` MailId and a `parent_mail` MailId reconstructs the full causal graph of substrate work — the same data structure that powers distributed-tracing flame graphs, queue-latency analysis, handler-duration histograms, and the future MCP `describe_tree` / debugger surfaces.

The shape borrows lessons from ADR-0023 (substrate text-log capture) and ADR-0077 (actor-aware logging via per-actor `LogBuffer` + handler-exit drain) but cannot reuse the per-actor flush-on-handler-exit pattern: settlement correctness requires every `Sent` and `Finished` to reach the consumer, including across actor panics. A trace event lost to a missed flush corrupts the counter and either deadlocks the gate (lost `Finished`) or fires `Settled` prematurely (lost `Sent`).

ADR-0038 retired worker-pool dispatch in favour of one mpsc-fed OS thread per actor, deliberately eliminating cross-actor shared mutable state. A natural-fit settlement design would put per-tree `Arc<AtomicU32>` counters on every envelope, but that reintroduces the contention shape ADR-0038 just removed. The decision below routes the firehose through a chassis-wide queue + drainer + observer-as-actor instead — actor-pure, eventually-consistent, and big enough to land tracing as a first-class substrate surface.

## Decision

The substrate emits a structured trace event for every mail's send / receive / completion, batches them on a dedicated chassis thread, ships the batches to a regular `#[actor]` observer cap, and exposes settlement as the v1 consumer. Tracing infrastructure is always-on; gating is one query against the trace graph.

### 1. `TraceEvent` is a typed enum on a chassis-wide MPSC

```rust
pub enum TraceEvent {
    Sent {
        mail_id: MailId,
        root: MailId,
        parent_mail: Option<MailId>,
        sender: MailboxId,
        recipient: MailboxId,
        kind: KindId,
        t: Nanos,
    },
    Received { mail_id: MailId, t: Nanos },
    Finished { mail_id: MailId, t: Nanos },
}
```

`MailId` is a 128-bit composite: `MailId { sender: MailboxId, correlation_id: u64 }`. The `correlation_id` is the existing envelope field, generalised: where today it's set per-call-site for reply-expecting sends, post-ADR-0080 it's always-present and allocated from a per-actor `AtomicU64` (no cross-thread contention — each actor's counter is touched only by that actor's send paths). Reply-slot lookups key off `(sender_mailbox, correlation_id)` instead of the bare `correlation_id` they use today; the test bench and hub minters move from per-call to per-actor allocators. The reply-correlation semantic doesn't change — every mail has a `correlation_id` now, and replies cite the originating mail's value, same as before.

The `(MailboxId, u64)` shape is exact by construction — no central minter to contend on, no hash to collide. The 8 extra bytes per envelope vs a 64-bit MailId are worth the absence of birthday-paradox collision risk (a 64-bit hash of `(sender, correlation_id)` collides with ~1 % probability after ~5 hours of busy-substrate uptime; correctness-breaking for the observer's `HashMap<MailId, MailNode>`).

**A causal chain is identified by the `MailId` of its root — the mail that started it.** No separate id type for the chain, no separate allocator, no separate id space. When a `send_mail` call has no in-flight handler context to inherit from, the mail being sent is itself a root, and that mail's `MailId` is what every descendant carries in its `root` field. Children inherit `root = parent.root`. The framing is "you didn't have a trigger, so this mail is the root of its own causal chain." Field names (`root: MailId` vs `mail_id: MailId` vs `parent_mail: Option<MailId>`) carry the role at every site where it matters; the type system only knows them as `MailId`s.

Originator information (chassis sentinel for chassis-dispatched mail; owning cap's mailbox for cap-spawned worker threads such as `TcpCapability`'s per-connection workers) reads off `root.sender` directly — no separate originator field needed.

The chassis-wide MPSC is `crossbeam::queue::SegQueue<TraceEvent>` for v1 — unbounded, lock-free MPSC, per-producer FIFO.

`Nanos` is a `u64` representing nanoseconds since a `SUBSTRATE_START: Instant` reference captured at boot. Producers compute `Instant::now().duration_since(SUBSTRATE_START).as_nanos() as u64` at each event push. The reference + subtraction lets timestamps be `Copy` / `Serialize` (raw `Instant` is platform-opaque) so events cross the mail boundary in `BatchedTraceEvents` without a wire-vs-in-memory split. A u64 of nanoseconds-since-boot accommodates ~584 years of substrate uptime — adequate.

### 2. Producer side: send and dispatch entry/exit emit to the queue

Three hook sites in `aether-substrate`:

- **`Sender::send_to_named` (and the typed wrappers)**: after resolving the recipient mailbox and before enqueueing the mail to the recipient's mpsc, push a `Sent` event. `mail_id` is freshly allocated; `root` is inherited from the sender's in-flight context (§5); `parent_mail` is the in-flight mail id at the sender (or `None` for chassis-root sends); `t = Instant::now()`.
- **Native dispatcher loop (`actor::native::dispatcher_slot`) and the wasm trampoline (`WasmTrampoline`)**: at handler entry, push `Received { mail_id, t }`. At handler exit (including the panic / unwind path that already brackets `#321` panic legibility), push `Finished { mail_id, t }`.

`std::time::Instant` is monotonic since Rust 1.59 (the stdlib clamps backward jumps internally) and reads cost ~10–20 ns on Linux/macOS via VDSO (`clock_gettime(CLOCK_MONOTONIC)` / `mach_absolute_time`); the `duration_since(SUBSTRATE_START)` subtraction adds ~1–2 ns. CLOCK_MONOTONIC is process-global on Linux and `mach_absolute_time` is system-wide on macOS, so different actor threads on the same substrate share a clock source — no inter-actor skew. Per-mail producer overhead is three timestamp reads + three queue pushes ≈ 30–60 ns. At a busy-scene baseline of ~33 k mails/sec this is ~1 ms/sec of extra CPU per active actor — negligible.

### 3. Chassis drainer thread batches events into mail

The chassis spawns one drainer thread alongside its other infrastructure threads (peer to the scheduler, the hub client, the audio thread). It loop-drains the trace queue:

```rust
loop {
    let batch = drain_up_to(&trace_queue, BATCH_MAX);
    if !batch.is_empty() {
        sender.send_detached(
            TRACE_OBSERVER_MAILBOX,
            &BatchedTraceEvents { events: batch },
        );
    }
    park_timeout(BATCH_INTERVAL); // also signaled when queue exceeds high-water mark
}
```

Defaults: `BATCH_MAX = 256`, `BATCH_INTERVAL = 1 ms`. At baseline this is ~1 k observer-mails/sec, two orders of magnitude reduction from the per-event mail count.

### 4. `TraceObserver` is a regular `#[actor]` cap

Lives in `aether-capabilities` next to `BroadcastCapability` / `LogCapability`, registered at substrate boot under `aether.trace`. Handlers:

- `on_batched_trace_events(BatchedTraceEvents)` — fold each event into the per-root counter map and the parent / mail / kind graph used by query consumers.
- `on_subscribe_settlement(SubscribeSettlement { root, reply_to })` — register interest in a causal chain's settlement; emits `Settled { root }` to `reply_to` when `counter[root]` hits zero (per §6, possibly multiple times).
- Future: `on_describe_tree`, `on_export_trace`, etc. — additional consumers slot in here without further infrastructure changes.

The observer maintains:

- `HashMap<MailId, RootState>` keyed by the root mail id, where `RootState { counter: u32, in_flight: HashSet<MailId>, lifecycle: RootLifecycle }` and `RootLifecycle = Tick(frame_no) | Wire(actor) | Init(actor) | Drop(actor) | Replace(actor) | McpRequest(...) | HubBridge(...) | External(MailboxId)` labels the chain's *cause* for query output. The originator (chassis sentinel, owning cap, etc.) is read directly off the root `MailId.sender` — no separate field needed.
- `HashMap<MailId, MailNode>` where `MailNode { root, parent, sender, recipient, kind, t_sent: Nanos, t_received: Option<Nanos>, t_finished: Option<Nanos> }` for graph queries (`Option` on the latter two — a node is created at `Sent` arrival and patched as `Received` / `Finished` land later).
- `HashMap<MailId, Vec<ReplyTo>>` of pending settlement subscribers, keyed by root.

### 5. Roots originate at "no in-flight mail" sites; everything else inherits

Each actor's per-handler context (`NativeCtx` / `WasmCtx`) carries the in-flight mail's `MailId` and `root: MailId` (the root of the causal chain it belongs to). `Sender::send_to_named` reads them from the calling context:

- **In a handler** — child inherits the in-flight `root` and stamps `parent_mail` to the in-flight mail id.
- **Outside any handler context** (chassis dispatching `Tick`, lifecycle (`init` / `wire` / `drop` / `replace`), externally-bridged mail from the hub or MCP, or a cap-owned worker thread reacting to an external event such as a TCP connection's inbound bytes) — the new mail's `MailId` *is* its own root. The send path detects the absence of an inherited root, stamps `root = self.MailId` and `parent_mail = None` on the outgoing envelope, and the observer's `on_batched_trace_events` allocates a `RootState` keyed by that `MailId` on first `Sent`-with-no-parent.

Per-actor `correlation_id` allocators (§1) cover the entire `MailId` space — root mails and child mails draw from the same counter, no separate root allocator, no substrate-wide central counter, no cross-actor contention on the mail-throughput hot path. The id space is reset on substrate boot; ids are unique within a substrate run, not across runs (consistent with today's `MailboxId` / `KindId` per-run uniqueness).

The chassis is not an actor but is an addressable mail endpoint at `MailboxId(0)`, the existing `MailboxId::NONE` sentinel (`crates/aether-data/src/ids.rs:153`). The "no origin" semantic generalises naturally to "chassis-originated, no actor sender": chassis-dispatched mail (Tick, lifecycle, hub-bridged, MCP-bridged) has no actor sender, so one sentinel covers both cases. The mailbox-name registration guard already rejects names whose FNV-1a hash collides with 0 (collision probability ~2⁻⁶⁴), so the sentinel never collides with a real cap mailbox. The symbolic `CHASSIS_MAILBOX_ID` constant aliases `MailboxId::NONE` for code that wants the chassis-specific framing at the call site. The dispatcher loop has a small switch on `recipient == CHASSIS_MAILBOX_ID` ahead of the registry lookup; settlement reply mail (`Settled { root }`) routes through that switch into the chassis's gate-site notification logic. The chassis-as-sentinel framing also gives the causal graph a labelled root — every chain whose root mail's `sender == CHASSIS_MAILBOX_ID` is chassis-originated, and `RootState.lifecycle` (§4) names the specific cause (`Tick(frame_no) | Wire(actor) | Init(actor) | Drop(actor) | Replace(actor) | McpRequest(...) | HubBridge(...)`) so query output names the cause of every causal chain.

### 6. Settlement is a hint, not a guarantee — consumers are idempotent

The trace queue and the recipient mpsc are independent paths. Cross-producer event ordering at the observer is therefore not strictly preserved (per-producer FIFO holds for any reasonable MPSC, but B's `Finished` for a child can in principle reach the observer before A's `Sent` for that same child). A naive counter would briefly hit zero, fire `Settled`, then bounce back up.

Rather than enforce a producer-side "Sent before enqueue" ordering invariant (which would couple the trace push to the mail enqueue at every send site), the design treats `Settled` as a hint:

- Observer fires `Settled { root }` to subscribers whenever `counter[root]` transitions to zero.
- If a late `Sent` arrives, the counter increments back up and a subsequent transition to zero re-fires `Settled`.
- Gate consumers (chassis Tick gate, lifecycle gates, `replace_component` drain) are written to be idempotent: first `Settled` unblocks the gate; duplicate `Settled` is a no-op. None of them destroy state on first `Settled` — they only unblock waiters. Late events landing under the new chain (which has a different root MailId) cannot mix in.

Optional follow-ons if telemetry shows spurious fires are common enough to matter: a one-batch quiescence window at the observer (fire only after `counter[root]` has been zero for one batch interval), or generation numbers on `Settled` so consumers can reason about replay. Both deferred past v1.

### 7. Trace events are detached — the tracing layer is meta

The drainer's outbound `BatchedTraceEvents` mail goes through `Sender::send_detached`, a new send variant that bypasses the trace-event push. Without it the observer's own emissions would generate observer events and recurse. The detach API is also the explicit escape hatch for any future "fire-and-forget, do not gate my parent" send sites (logs, hub broadcasts of observation mail) — most code uses `send` and inherits; detach is opt-in and rare.

**`send_detached` is fire-and-forget only.** Detached sends MUST NOT be used for reply-expecting requests: a detached send mints no parent linkage, so the receiver processes it as the root of a new tree, and any reply the receiver issues inherits the *receiver's* tree (not the sender's). The reply still routes back to the sender's reply slot via `(sender, correlation_id)` — that part is unchanged — but the causal-graph linkage between request and reply is broken by design. Reply-correlated sends always go through the inherited (`send`) path. The `send_detached` API documents this constraint at the call site; misuse is a logic bug, not a runtime error.

### 8. Names resolve at query time, not in events

`MailboxId` and `KindId` ride events as 64-bit ids (per ADR-0029 / ADR-0030). The observer holds an id → name lookup populated from the same `describe_kinds` / `describe_component` info the hub already sees, and resolves names when a query consumer asks. Keeps event size tight; readable output stays cheap.

### 9. Backpressure: unbounded for v1 with a drainer-lag metric

v1 ships the trace queue as `crossbeam::queue::SegQueue` (unbounded MPSC). A pathological scenario where actors emit faster than the drainer can ship would grow memory unbounded; the chassis exports a `trace_queue_depth` metric so operators can see drainer lag. Switching to a bounded structure with either producer-block or lossy-overflow semantics is a knob change, not an architecture change, and is deferred until measured.

### 10. Implementation phasing

Three landable PRs:

1. **Tracing infrastructure + settlement gate.** Trace queue, drainer, `TraceObserver` cap, `TraceEvent` spec, per-actor `correlation_id` allocator (drives `MailId` allocation; root ids are just the originating mail's MailId, no separate counter), in-flight-context plumbing on `NativeCtx` / `WasmCtx`, ctx-mediated thread-spawn primitives (`spawn_inherit` / `spawn_detached` / `spawn_root_worker` per §12), `CHASSIS_MAILBOX_ID` sentinel + dispatcher switch. Replace `wait_instanced_quiesce` callers with settlement subscriptions; retire `await_tick_subscribed` and `settle_observations` from the #648 tests; close the `replace_component_preserves_mailbox_identity` flake.
2. **MCP `describe_tree(root)`.** Read the observer's graph, return the structured causal subgraph rooted at the given mail. Lights up live tracing in the agent harness. (Tool name keeps `tree` because what's *returned* is a tree-shaped subgraph — the parameter is a `root: MailId`.)
3. **Flame-graph export.** `mcp__aether-hub__export_trace(root, format = "chrome" | "folded")` — Chrome-trace JSON is the de-facto standard (Perfetto / chrome://tracing / speedscope). Direct mapping from `MailNode` to a Chrome-trace span; trivial transform from the existing data.

Phases 2 and 3 don't change the substrate — they're pure additions to the observer cap and the hub.

### 11. Eviction policy: in-memory only, time + count cap, discard on evict

The observer's `RootState` and `MailNode` maps grow with every observed mail. Without a bound, an hour at busy load is ~120 M nodes. Two-tier eviction caps the footprint:

```
RETENTION_MS  = 600_000    // env: AETHER_TRACE_RETENTION_MS (10 min)
MAX_ROOTS     = 100_000    // env: AETHER_TRACE_MAX_ROOTS    (~50 MB at baseline)
```

- A root is **eligible for eviction** once `Settled` has fired and `now_nanos - root_state.t_settled_nanos >= RETENTION_MS * 1_000_000`.
- A root is **forced for eviction** when the observer holds more than `MAX_ROOTS` total roots and this is the oldest-by-`t_settled_nanos`.
- **In-flight roots are never evicted regardless of age** — they're load-bearing for gating.
- Eviction runs at the tail of `on_batched_trace_events` (no separate timer thread, no separate scheduler tick). When a root is evicted, drop its `RootState` and every `MailNode` whose `root` field matches the evicted MailId.

Per-root memory ≈ 80 B `RootState` + 5 × 80 B `MailNode` ≈ 480 B at the baseline ~5-mails-per-chain estimate. Defaults sized to give the time cap room to actually matter at typical loads while keeping memory bounded:

| Throughput | Roots/sec | Time at `MAX_ROOTS=100k` | Operative limit |
|---|---|---|---|
| Quiet (idle/menu) | ~100 | ~17 min | `RETENTION_MS` (10 min) wins |
| Baseline (busy scene) | ~6.6k | ~15 s | `MAX_ROOTS` wins |
| Pathological synthetic | ~30k+ | ~3 s | `MAX_ROOTS` wins, lossy |

10 min retention covers "I noticed something a couple minutes ago" use cases and most agent-harness session lengths; 100k roots × ~480 B ≈ ~50 MB is small enough not to be a memory-budget concern in dev/debug builds. Operators tune both downward via env vars for production builds.

A future refinement worth flagging but parked: **differential retention by `RootLifecycle`** (e.g., `Tick` chains evict after 30 s, `McpRequest` after 1 hr, `Wire`/`Init`/`Drop` after 10 min). At 60 Hz the substrate produces 60 root chains/sec just from Ticks, most uninteresting; differential retention would let the operator-facing trees survive longer for the same memory. Defer until the flat-retention defaults prove inadequate.

**Discard, not persist, in v1.** Disk persistence is real complexity (format choice, rotation, syscall cost on the drainer hot path, crash-recovery semantics) that the v1 consumers don't need. Settlement gating only cares about in-flight roots; `engine_logs` causal grouping captures the in-flight root MailId *into the log entry* at emission time so the observer can drop the root afterwards; MCP `describe_tree` and Chrome-trace export are for "show me what just happened," seconds-to-minutes window. Two future opt-ins if usage justifies:

- **Aggregated histograms** (always-on, separate retention budget). Per-bucket counters for handler duration per kind, queue latency per recipient — tiny memory, retained indefinitely, survives root eviction. Feeds the performance-tuning use case (#687, scheduler tuning).
- **Operator-opt-in streaming export** via `AETHER_TRACE_OUT=/path/to/trace.jsonl`. Observer appends each settled chain as one Chrome-trace JSON line. Operator-opt-in (not default), no rotation in v1 — just append. Use case: long-running profiling sessions, crash forensics for non-panic-path failures.

Neither is in scope for Phase 1 above.

### 12. Actor-spawned threads: ctx-mediated send authority

Some actors own threads driven by external events outside the actor's mail dispatch loop — `TcpCapability` per-connection workers, future drivers like `WebSocketCapability` or pollers, occasional CPU-offload workers. The trace-aware send path needs to know how each spawned thread relates to the in-flight causal graph; without a forcing function this becomes a "uninformed dev forgets to plumb the context" gap. The ctx pattern already established for handler dispatch (`NativeCtx` / `WasmCtx`) extends naturally: **threads receive a ctx that grants send authority and carries the inheritance choice in its type**. There is no way to send mail without holding one.

Two ctx flavors:

- **`InheritCtx<A>`** — captures the spawning handler's in-flight `(mail_id, root)` and a `Sender<A>` clone. Sends from the spawned thread inherit `tree = self.in_flight.root` and stamp `parent_mail = self.in_flight.mail_id`. The spawning handler's tree does not settle until every spawned-thread send completes. Correct shape for short-burst CPU offload that is *part of* the current handler's causal closure.
- **`RootCtx<A>`** — carries a `Sender<A>` clone with no in-flight context. Each send mints a fresh root with `sender = A.mailbox` (per §1 / §5). Correct shape for long-lived workers that respond to external events with no caller context — the canonical case being `TcpCapability`'s per-connection worker minting one root per inbound network packet.

Spawn primitives:

```rust
// Inside a handler — captures the handler's in-flight context:
impl NativeCtx<'_, A> {
    fn spawn_inherit<F>(&self, f: F) -> JoinHandle
    where F: FnOnce(InheritCtx<A>) + Send + 'static;

    fn spawn_detached<F>(&self, f: F) -> JoinHandle
    where F: FnOnce(RootCtx<A>) + Send + 'static;
}

// On the actor handle (for boot-time / wire-time spawn of long-lived workers,
// before any handler has been entered):
impl ActorRef<A> {
    fn spawn_root_worker<F>(&self, f: F) -> JoinHandle
    where F: FnOnce(RootCtx<A>) + Send + 'static;
}
```

The closures *receive* the appropriate ctx — they cannot construct one or smuggle a `Sender<A>` in another way. The type system enforces "every send goes through a ctx with the right inheritance shape."

For the no-actor-context case (`Drop` impls, signal handlers, panic hooks) where there is no actor to send-from-as, the only path is `send_detached` (the same primitive §7 establishes for the drainer's recursion break) — bypasses tree tracking entirely, no parent / no root.

**Shared thread pools and async runtimes are deferred to v2.** Pool workers don't statically belong to one actor; each submitted work item carries its own ctx (`InheritCtx<A>` or `RootCtx<A>`), and the pool worker function invokes the work-item closure with that ctx in scope. Same shape, just per-task instead of per-thread. v1 ships only the per-thread spawn primitives above; if shared pools become common the per-task variant slots in without re-shaping the contract.

## Consequences

### Positive

- **Settlement gating without deadlines.** `wait_instanced_quiesce` retires. Per-frame Tick gating, lifecycle gates, and `replace_component` drain all become "subscribe to root R's `Settled`". The `replace_component_preserves_mailbox_identity` flake closes structurally. The #648 test helpers (`await_tick_subscribed`, `settle_observations`) retire.
- **Causal graph for the agent harness.** MCP `describe_tree(root)` returns the structured cause-and-effect subgraph of any in-flight or recent work. Future debugger / introspection tools build on the same graph.
- **Performance instrumentation for free.** Inbox queue latency (`t_received - t_sent`), handler duration (`t_finished - t_received`), critical-path analysis (longest sequential chain in a causal subgraph), parallelism observation (overlapping `[t_sent, t_finished]` windows in the same chain). Used directly during scheduler tuning and the eventual lifecycle-barrier-graph work (issue #687).
- **Flame graphs.** Chrome-trace export via the observer; Perfetto / chrome://tracing / speedscope read it natively.
- **`engine_logs` causal grouping.** ADR-0023 text-log entries carry the in-flight root MailId of the actor that emitted them, so log lines group by causal chain. Light add to ADR-0023's `LogEntry`.
- **Foundation for the future debugger.** Repeated user direction toward "show me what's happening inside the engine" lands here.
- **Inheritance contract is type-enforced for actor-spawned threads.** §12's ctx-mediated spawn primitives (`spawn_inherit` / `spawn_detached`) make "this worker thread inherits the in-flight tree" vs "this worker thread mints fresh roots" a compile-time choice that survives across the FFI / wasm boundary. No way to send mail from a thread without explicitly stating the inheritance shape; new caps that spawn workers can't accidentally drop into an undefined-context mode.

### Negative

- **Always-on hot-path cost.** ~30–60 ns per mail in the producer path (three `Instant::now()` reads + three SegQueue pushes). Negligible at baseline; measurable under absolute-throughput stress. No knob to turn it off — settlement gating depends on it.
- **One always-running drainer thread per chassis.** Plus the `TraceObserver` cap's dispatcher thread. Two more OS threads per substrate, joining the existing chassis infrastructure threads.
- **Observer memory grows with in-flight + recent roots.** `RootState` and `MailNode` retained until the chain settles + a retention window (per §11). At baseline ~10 k retained nodes; bounded by load.
- **Spurious `Settled` fires are possible.** Consumers must be idempotent. None of the v1 consumers destroy state on first `Settled`, but future consumers must respect the contract.
- **Unbounded trace queue under pathological load.** Memory grows if drainer falls behind. v1 ships with a `trace_queue_depth` metric and no policy; bounded variants deferred.

### Neutral

- **Sits alongside ADR-0023 / ADR-0077, not on top of them.** Text logging (`tracing::*` events → `LogBuffer` → `LogCapability` → hub) and mail tracing (substrate `send` / dispatch hooks → trace queue → `TraceObserver`) are independent pipelines with similar shape. Cross-link only at consumer side: `engine_logs` reads the in-flight root MailId from a thread-local that the dispatcher stamps when entering a handler.
- **No wire changes for existing consumers.** The substrate → hub wire is untouched in v1. New MCP tools (`describe_tree`, `export_trace`) are additions; no existing MCP tool's response changes.
- **`correlation_id` rolls together with `MailId`.** The existing per-call `correlation_id` field becomes always-present and per-actor-allocated; reply-slot lookups key off `(sender, correlation_id)` (= `MailId`) instead of bare `correlation_id`. No new field on the envelope (`correlation_id` already exists), but its allocator moves from the test bench / hub to per-actor `AtomicU64`s.

## Alternatives considered

- **Per-tree `Arc<TreeCounter>` threaded through every envelope.** Settlement detection in the producer path: send increments, dispatch-exit decrements, waker fires at zero. Sub-µs settlement detection. Rejected: reintroduces the cross-actor shared-mutable-state shape that ADR-0038 deliberately eliminated, even if technically lock-free. The Arc on every envelope also costs 8 bytes per mail and couples actor lifetimes through the counter Arc.
- **Function-call interface from chassis to observer (chassis is special, not an actor).** Chassis owns the observer struct, calls `mint_root` / `await_settled` directly; only actors emit observer events as mail. Rejected: carve-out for the dispatcher when the mail interface works fine. Mail round-trip latency is in the same order as a Tick fanout (sub-ms), which the chassis already does every frame. The "chassis isn't an actor but is a mail endpoint at a sentinel id" framing keeps the model uniform without making the chassis a `NativeActor`.
- **Per-actor `LogBuffer`-style per-handler buffer flushed at handler exit (mirror ADR-0077).** Each actor accumulates trace events in a thread-local during dispatch, flushes at exit. Rejected: settlement correctness requires every `Sent` and `Finished` to reach the observer, including across actor panics. The flush-on-exit path is best-effort by construction; ADR-0077 tolerates dropped log events on panic, settlement does not. The shared-queue + drainer pattern decouples emission from delivery so panic-path bracketing alone is enough to ensure events ship.
- **Producer-side "`Sent` before mail enqueue" FIFO invariant.** Eliminate spurious `Settled` fires by ordering the trace push before the mail enqueue at every send site. Considered: zero runtime cost. Rejected for v1: couples the trace push to the mail enqueue at every producer, including all the indirect send paths (cap handlers, trampoline forwards, drainer self-emissions). Pushing complexity to the consumer side (idempotent gates) keeps producer paths fully decoupled. The invariant remains a free optimization to add if spurious fires turn out to matter.
- **`u32` `correlation_id` with eviction-window recycling.** Saves 4 bytes per `MailId` × ~4 `MailId`s per `Sent` event = 16 B/event ≈ ~530 KB/sec at baseline (~5 % of observer mail bandwidth). Wraparound at u32 max (4.29 B values) is per-actor: a 1k/sec actor wraps in ~50 days, a 60k/sec render-ish actor wraps in ~20 hours, a pathological 1M/sec actor wraps in ~71 minutes. Recycling is *safe* if the wrap time exceeds `RETENTION_MS` plus max-in-flight lifetime — at the new defaults (10 min retention) all realistic loads clear that bar. Rejected anyway: the safety condition couples a wire-format choice to an operator-tuned retention setting, and silently goes from "safe" to "incorrect" if either retention is bumped or a single actor's load spikes. Today's `correlation_id` field is already `u64` (test bench `next_correlation_id`, hub session counters); going to u32 is a regression in the existing field, not just a new constraint. The "no wraparound in any realistic scenario" property is worth more than ~5 % wire bandwidth for an always-on infrastructure system. If wire bytes do become the bottleneck (they won't at these rates), better levers are dropping the redundant `Sent.sender` (= `mail_id.sender`, ~265 KB/sec), per-batch dictionary encoding for repeated MailboxId/KindId values, or cap-side compression on `BatchedTraceEvents` — all preserve the safety properties.

### Clock alternatives

`std::time::Instant` was picked over four alternatives. Recorded for future reconsideration if a measured bottleneck or platform requirement surfaces:

- **`quanta` crate.** TSC-based with calibration against `Instant`, ~7–10 ns per read on x86_64. Saves ~5–10 ns per timestamp × 99 k events/sec ≈ 1 ms/sec of CPU per active substrate. Mature, used by other tracing crates. Rejected for v1: marginal win, added dep, and Linux's VDSO `CLOCK_MONOTONIC` already exposes a calibrated TSC on supported hardware so the gap narrows further. Drop-in swap if profiling shows reads as a measurable bottleneck.
- **`minstant` crate.** Same shape as `quanta` (TSC + calibration), ~10 ns. Rejected for the same reason; `quanta` is the more conventional pick if we ever swap.
- **`coarsetime` / `CLOCK_MONOTONIC_COARSE`.** ~1–2 ns per read but precision is jiffies (~1–10 ms typical). Rejected: handler durations are typically microseconds, so coarse precision destroys the queue-latency / handler-duration / critical-path consumers.
- **Raw `rdtsc` (x86 / `cntvct_el0` ARM64).** Fastest possible (~10–25 cycles, ~3–7 ns) but raw TSC values aren't comparable across cores without invariant-TSC + calibration, and the platform-specific paths multiply. Rejected: `quanta` already wraps this with the necessary guards, so if we ever go this direction we go through `quanta`.

`std::time::Instant`'s monotonicity guarantee (Rust 1.59+) plus VDSO-backed reads make it the simplest correct choice. The `Nanos` newtype around `u64 nanoseconds since SUBSTRATE_START` (§1) decouples the storage / wire representation from the clock source, so swapping `Instant::now()` for `quanta::Instant::now()` is a single-call-site change later if needed.
