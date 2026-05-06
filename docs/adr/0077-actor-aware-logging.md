# ADR-0077: Actor-aware logging through `LogCapability` and per-actor buffer-and-drain

- **Status:** Accepted
- **Date:** 2026-05-06
- **Supersedes parts of:** ADR-0023

## Context

ADR-0023 shipped substrate log capture as a substrate-global ring buffer fed by a `tracing_subscriber::Layer`, drained on a 100-entries-or-250ms timer by a dedicated flush task that posted `EngineToHub::LogBatch` frames over the hub channel directly. A `flush_now` synchronous entry point existed for the trap-and-abort path so a dying substrate's last words still reached `engine_logs`. The hub-side ring + `engine_logs` MCP tool sat downstream of all of that, unchanged.

The capture path predates the unified actor model. ADR-0074 collapsed components and capabilities into a single actor primitive sharing one SDK over two transports (FFI / mpsc), and the issue 552 stage tree (PRs 553–566) executed that — every chassis cap is now a `#[actor]` block, every chassis-owned mailbox addresses as `aether.<name>`, and the hub egress for chassis state happens through capability mail handlers (`LogCapability`, `BroadcastCapability`, etc.). The `log_capture` ring sat outside that model: it was substrate infrastructure that shipped log entries directly over the hub wire without ever being mail.

Three concrete frictions accumulated:

1. **Sender attribution.** Each `LogEntry` should carry the originating actor's `MailboxId` so an operator polling `engine_logs` can tell *which* component logged a given line. The ring captures events at the `tracing_subscriber::Layer` callback, which runs in whichever thread happens to be emitting — there's no ergonomic way to tag the entry with "this came from actor X" because the layer can't see actor identity from a tracing event alone. A separate per-thread shim would have to be installed by every actor, and the ring would need a wider entry shape.
2. **`flush_now` correctness on trap.** Trap containment (ADR-0015) called `log_capture::flush_now` from the abort path to push pending entries before `process::exit`. That worked, but it was an out-of-band synchronous reach into the capture machinery, parallel to the normal async drain. Two paths into the same buffer with subtly different correctness properties.
3. **Architectural drift from ADR-0074.** The hub's egress story for everything else became "a chassis cap forwards mail to outbound." Logs were the conspicuous exception — a separate static ring + flush task that read `tracing` events and pushed frames without going through the actor model at all. Future per-actor policy (rate limits, level overrides per component, structured field passthrough) would have to reach into substrate-internal infrastructure rather than landing as cap-side handler logic.

Issue 583 (PR 586) was a transitional step — it added a direct-emit path that bypassed `tracing` macros for code already inside `LogCapability`-adjacent surfaces, removing one source of dispatcher reentrancy. That work was retired in full by issue 581.

## Decision

Logging follows the actor model end-to-end. `tracing::*` events are buffered into a per-actor [`LogBuffer`] by an actor-aware tracing layer; the chassis dispatcher drains the buffer at handler exit (or eagerly on `WARN`/`ERROR`) by shipping a `LogBatch` mail to the well-known `aether.log` mailbox; `LogCapability` (in `aether-capabilities`) handles that mail and forwards each entry as a `LogEntry` through `HubOutbound::egress_log_batch` to the hub. The substrate-global ring + flush task retire entirely.

### 1. `LogBatch` is the only wire kind for log content

`LogEvent { level, target, message }` becomes a non-mailable `Schema` struct (no `Kind` derive). `LogBatch { entries: Vec<LogEvent> }` is the lone `Kind`-derived envelope on the wire. Single-entry batches are how host-branch events ship; multi-entry batches are what an actor's `LogBuffer` produces at drain time. Sender attribution rides on the mail envelope's `ReplyTarget::Component(id)` (read via `NativeCtx::origin()` in the cap's handler), not on the payload — the same pattern every other reply-aware cap uses.

### 2. Per-actor `LogBuffer` via the `Local` primitive

`LogBuffer(Vec<LogEvent>)` impls the `Local` trait (issue 582 / PR 585), so each actor's per-thread `ActorSlots` carries one. The actor-aware tracing layer (`aether-substrate::log_install::ActorAwareLayer`) reads `LogBuffer::try_with_mut` on each event:

- **In-actor branch** (slot present) — push the event into the buffer. If level ≥ WARN, call `drain_buffer()` immediately so high-priority events don't wait for the handler to return.
- **Host branch** (no slot — substrate boot, scheduler, panic hook, anything outside an actor's dispatch) — call `ship_host_event()` which sends a single-entry `LogBatch` directly through a process-global host dispatch.

`drain_buffer()` walks the current actor's `LogBuffer`, encodes the contents as a `LogBatch` payload, and sends it via the per-handler `ACTOR_DISPATCH` TLS slot (the actor's own `MailTransport`, stamped by the chassis dispatcher when it enters a handler). On wasm guests there's no per-handler stamp — a single process-global `WASM_TRANSPORT` covers the whole instance — so the drain path branches on `cfg(target_arch)`.

### 3. Two-phase install

`init_subscriber()` runs once at substrate boot in `SubstrateBoot::build`, before any cap loads. It installs the global tracing subscriber stack: `EnvFilter` (reads `AETHER_LOG_FILTER`, default `info`) → `tsfmt::Layer` to stderr → `ActorAwareLayer`. With no log mailbox registered yet, the host branch silently drops mail-egress events; `tsfmt::Layer` keeps stderr live so early-boot diagnostics aren't lost.

`install_log_target_if_registered(mailer, registry)` runs at the end of `Builder::build`, after every cap has been booted. It looks up `"aether.log"` in the registry; if present, it `Box::leak`s a `MailerHostDispatch { mailer }` and registers it through `aether_actor::log::install_log_target`. From that point forward, host-branch `tracing::*` events flow as single-entry `LogBatch` mail to the cap. Chassis that intentionally skip `LogCapability` get a no-op — the host branch keeps dropping mail-egress events while stderr still receives them.

### 4. TLS re-entry guard

`drain_buffer` and `ship_host_event` route through `Mailer::push`, which itself emits `tracing::*` events on certain failure modes (a dead capability mailbox sender shows up as `tracing::warn!`). Without a guard, those events re-enter the `ActorAwareLayer`, push the actor's buffer, priority-flush at WARN, and recurse — observable as a stack overflow during chassis shutdown. A TLS `IN_LOG_PIPELINE` flag (managed by an RAII `PipelineGuard`) wraps both drain and host-ship paths; `ActorAwareLayer::on_event` and `WasmSubscriber::event` short-circuit when the flag is set. Events that hit the guard still reach the registered `tsfmt::Layer`, so stderr observers keep seeing them.

### 5. `LogCapability` as pure forwarder

`LogCapability` (in `aether-capabilities`, behind the `#[bridge]` mod pattern from ADR-0076) holds an `Option<Arc<HubOutbound>>` and an `AtomicU64 sequence`. Its single handler receives `LogBatch` mail, reads `ctx.origin()` for the originating actor's `MailboxId`, stamps each entry with `now_unix_ms()` + monotonic `sequence`, maps the wire `level: u8` onto the substrate-side `LogLevel` enum, and forwards via `outbound.egress_log_batch(entries)`. The hub-side ring + `engine_logs` MCP tool stay exactly as ADR-0023 specified — what changed is everything *upstream* of `egress_log_batch`.

### 6. `LogEntry` gains `origin: Option<MailboxId>`

The substrate→hub wire `LogEntry` grows one field. `Some(id)` for cap-attributed entries (every batch from an actor's drain), `None` for host-branch events (boot, scheduler, panic hook). The hub's `engine_logs` MCP tool surfaces it on the response so an operator polling logs sees which actor logged each line.

### 7. Trap-time drain

`fatal_abort` in `aether-substrate::lifecycle` calls `aether_actor::log::drain_buffer()` once before `process::exit(2)`, so the dying actor's last events ship before the broadcast goes out and the process tears down. This replaces ADR-0023's `flush_now` reach into the capture ring; the new path is the same `drain_buffer` every handler-exit drain calls, just one final time. Hub-egress is still fire-and-forget against the exit window, same correctness properties as before.

## Consequences

### Positive

- **Sender attribution surfaces in `engine_logs`** without per-actor capture shims. The mail envelope already carried it; reading `ctx.origin()` in `LogCapability::on_log_batch` is the natural place to extract it.
- **One drain path.** Handler-exit drain, priority-flush-at-WARN drain, and trap-time drain all call `drain_buffer()`. ADR-0023's parallel `flush_now` retires.
- **Cap-side policy is uniform.** Rate limits, level overrides, structured-field passthrough — anything future logging policy wants — lands as cap handler logic, not substrate infrastructure. `LogCapability` can be replaced by a thicker variant per-chassis without touching `aether-actor` or `aether-substrate`.
- **Architectural symmetry.** The hub egress story is now: every chassis-owned mailbox is a cap; every cap forwards through `HubOutbound`. Logs no longer carve out a special path.
- **Net code reduction.** Issue 581's PR removed 695 lines of substrate-internal `log_capture` plumbing and 255 lines of wasm-side log SDK shim, replaced with ~580 lines of unified `aether-actor::log` + ~117 lines of `log_install` glue + ~198 lines of `LogCapability`. -733 LOC net after retiring the ring.

### Negative

- **More TLS than ADR-0023.** Per-handler `ACTOR_DISPATCH`, the `IN_LOG_PIPELINE` re-entry guard, and the lifetime-erasure transmute inside `with_actor_dispatch` are real complexity that the ring didn't need. Mitigated by isolating each to one function with an RAII restorer; the surface is `with_actor_dispatch(&dispatch, || { ... })` for the chassis dispatcher and `is_in_pipeline()` for the layer.
- **Two transport-flavoured drain paths.** Native (per-handler stamped TLS) and wasm (process-global `WASM_TRANSPORT`) reach `drain_buffer` through different cfg branches. The old ring was target-symmetric.
- **Drain isn't immediately bounded.** A pathological actor that emits many `INFO+` events between handler exits builds an unbounded `LogBuffer`. Priority flush at WARN/ERROR caps the worst case for high-severity, but a tight `info!` loop in a long-running handler can grow memory until the handler returns. Out of scope to fix here; if it bites, a per-buffer length cap with drop-and-record-loss semantics matches the rest of the system.
- **Wire shape changed.** `LogEntry` grew an `origin` field. Existing hub-side consumers handle this transparently (postcard tolerates field addition), but the wire isn't byte-identical to ADR-0023.

### Neutral

- **Hub-side ring + `engine_logs` MCP tool unchanged.** `truncated_before`, cursor-based polling, level filter, server-side filtering — all the same. Only `origin` is added to the response.
- **`AETHER_LOG_FILTER` still works.** `EnvFilter` is part of the new subscriber stack; the env-var contract carries over.
- **`tsfmt::Layer` to stderr remains.** Operators running a substrate from a terminal still see logs locally regardless of hub state.

## Alternatives considered

- **Keep ADR-0023's ring, add an actor-id field to its entries.** Each actor's tracing-layer event would consult a thread-local "current actor id" stamped by the dispatcher, and the layer would write that into the ring entry. Rejected: keeps logging outside the actor model, doesn't solve the cap-policy-uniformity problem, and the thread-local already exists in this design — but it's load-bearing for *dispatch* (`ACTOR_DISPATCH`), not for capture stamping. Less plumbing to drain through dispatch than to add a parallel stamping channel.
- **Per-actor `tracing::Subscriber` rather than per-actor buffer.** Each actor's dispatcher installs its own subscriber with an actor-bound mail egress. Rejected: tracing's subscriber model is process-wide; nesting subscribers per call is doable but expensive (every event traverses N subscribers), and the layer-with-thread-local-actor-context pattern is the established way to attribute events to a logical scope.
- **Direct-emit from `LogCapability` only (issue 583's intermediate).** Code inside the cap's adjacency calls a non-`tracing` emit function that writes to the ring directly. Rejected as the destination — works for cap-internal calls, doesn't help substrate-internal or guest-side `tracing::*` calls. Shipped as a stepping stone in PR 586 and retired by PR 588.

## Follow-up work

- **Pull-based log model (issue 587).** Today the substrate pushes batches to the hub via `egress_log_batch` regardless of whether anyone's polling `engine_logs`. A pull model — hub queries `LogCapability` on demand — would localize retention policy inside the cap. Issue 587 is queued; not part of this ADR.
- **Per-buffer length cap.** Drop-and-record-loss when an actor's `LogBuffer` exceeds a threshold between drains. Match the bounded-channel discipline of the rest of the system. File when the unbounded growth becomes observable.
- **Structured field passthrough.** Tracing's structured fields (`?value`, `%value`) still flatten into the message string at the layer's `Visit` impl, same as ADR-0023. Threading them through `LogEvent` as a `Vec<(String, String)>` is mechanical; parked until a consumer asks.
