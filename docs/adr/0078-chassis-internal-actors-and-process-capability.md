# ADR-0078: Chassis-internal actors, with `ProcessCapability` as the first

- **Status:** Proposed
- **Date:** 2026-05-06

## Context

ADR-0074 collapsed components and capabilities into a single actor primitive. The issue 552 stage tree (PRs 553–566) executed that for the chassis-policy mailboxes — Log, Handle, Io, Net, Audio, Render, Broadcast all live in `aether-capabilities` as `#[actor]` blocks today, each owning one `aether.<name>` mailbox and one mailbox of state. Mail in, handler runs, mail out. The substrate-side runtime around them is thin.

That model didn't reach the chassis driver itself yet. Three ungrazed surfaces survive as bespoke chassis-internal code:

1. **Hub process supervision.** `spawn_substrate` / `terminate_substrate` in `crates/aether-substrate-bundle/src/hub/spawn.rs` is a pair of async functions over `tokio::process::Child` plus an `EngineRegistry` and a `PendingSpawns` table. The MCP coordinator calls these directly and writes the result back into MCP tool replies. Process exit (SIGCHLD, waitpid completion) is detected by the engine connection task closing and the registry tearing the child down — no central event in the mail world.
2. **Server sockets.** Each chassis (hub TCP, future user-space app sockets) has its own listener loop wired by hand. Connection accept, byte read, byte write, close — none of it is mail today.
3. **Stream-shaped IO generally.** Anything that exposes a byte stream to a wasm component (cloud asset streaming, future MIDI input, future TCP component sockets) bypasses the mail model because mail is discrete envelopes and streams are not.

The cost of leaving these as bespoke chassis-internal code is the same cost the cap migration paid down: every new chassis-internal concern is one more piece of code that doesn't share testability, shape, or composition with the rest of the system. A new MCP tool that wants to react to "process exited" today threads a callback through the registry; in the actor model it would be a `#[handler] fn on_process_exited(...)` on whatever cap cares.

The alignment with ADR-0074 is straightforward — *chassis-internal* actors aren't a new model, they're the existing model applied to surfaces the issue 552 tree didn't reach. The structural question this ADR exists to answer is what comes with that decision: how stream-shaped IO fits the discrete-envelope mail surface, and what the v1 backpressure story looks like for chassis caps that wrap OS event sources.

## Decision

Chassis-internal capabilities follow the same actor pattern as the chassis-policy capabilities. The first concrete instance is `ProcessCapability` in the hub chassis, replacing the bespoke `spawn_substrate` / `terminate_substrate` plumbing. Subsequent instances (`ServerSocketCapability`, framing actors, etc.) are deferred to follow-up ADRs but the v1 stance below holds for them.

### 1. Phase 1 scope: `ProcessCapability` only

`ProcessCapability` lives in the hub chassis (provisional location: `aether-substrate-bundle::hub::process_capability`, with the cap struct + `#[actor]` block in the same `#[bridge] mod native { ... }` shape ADR-0076 settled on). It owns:

- The `EngineRegistry` of live children (today held in `hub::registry`).
- The `PendingSpawns` table (today in `hub::spawn`).
- A `tokio::process::Child` per spawned substrate.
- A blocking-task or signalfd-based reaper that converts SIGCHLD / `Child::wait` completion into actor mail.

Mail surface:

- `aether.process.spawn { binary_path, args, env, handshake_timeout_ms, ... } -> SpawnResult { engine_id | error }` — request/reply.
- `aether.process.terminate { engine_id, grace_ms } -> TerminateResult { ok | error }` — request/reply.
- `aether.process.exited { engine_id, exit_code, reason }` — broadcast emitted when the reaper observes a child's termination (whether via `terminate` mail or external).

The MCP coordinator's `spawn_substrate` / `terminate_substrate` tool handlers become thin wrappers that mail the cap and await the reply via the existing `pending-replies` mechanism the hub already uses for `capture_frame` / `load_component`. No coordinator-side process knowledge.

The cap's spawn handler still does the same `tokio::process::Command::spawn` + `Hello`-handshake-correlation work; it's the *call shape* that changes, not the underlying mechanism. PID-based correlation, handshake timeouts, SIGTERM-then-SIGKILL grace windows — all retain ADR-0009's behavior.

### 2. Stream-shaped IO is composed via actor chains (deferred phases)

For the future `ServerSocketCapability` and any other byte-stream wrapping cap, the model is a layered actor chain rather than a single fat cap:

```
TcpListenerActor  (accept → emits Connected { stream_id })
       │
       ▼
TcpStreamActor    (per connection — raw bytes in, raw bytes out)
       │
       ▼
FramingActor<P>   (consumes raw bytes, emits framed mail per protocol P)
       │
       ▼
ProtocolActor     (consumes framed mail, produces protocol behavior)
```

Each actor is independently testable and replaceable. A new framing protocol is a new `FramingActor` impl, not a new cap. This keeps the per-cap surface narrow (a TCP cap doesn't know about postcard-length-prefix) and matches the rest of the actor model's composition story.

Phase 1 doesn't ship any of this. ADR-0078 commits to the *direction*; the concrete socket cap lands in a follow-up.

### 3. Backpressure: bounded inbox + drop-to-log

The v1 stance for any chassis cap that wraps an event source is **bounded inbox + drop-to-log on overflow**.

- Each cap's mailbox uses a bounded mpsc channel with a per-cap default capacity (1024 entries proposed; tunable per cap when the default is wrong).
- When a producer (the reaper, an accept loop, an in-handler `ctx.actor::<R>().send(...)`) overflows, the producer:
  - records `tracing::warn!(target: "aether_substrate::backpressure", cap = %name, "mailbox full; dropping mail")` — once per drop event, not per actor's lifetime, to avoid log floods.
  - drops the mail.
- The cap-side handler doesn't know a drop happened. Higher-layer protocols that need delivery guarantees handle them at the protocol level (ack / sequence numbers).

This is intentionally the lazy-but-honest answer. It composes with the rest of the system (every other mailbox is currently effectively unbounded, so a cap that's bounded is a *strict* improvement on the worst-case memory profile), and it doesn't preclude richer schemes. Two known follow-ons that this ADR explicitly does **not** commit to:

- **Credit-based flow control.** Producer obtains credits from the consumer before sending; consumer replenishes credits as it drains. Right answer for high-throughput data planes; expensive to retrofit.
- **Per-class backpressure policy.** Frame-bound caps drop on overflow (frame still ticks); free-running caps could choose to block. ADR-0074 §Decision 5 already classifies caps; reusing the classification for backpressure policy is a small step but not part of v1.

Either of these belongs in its own ADR if a forcing function appears. The v1 stance is sufficient for `ProcessCapability` (low-rate control plane: spawn / terminate / exit events — drops would be alarming, but the inbox bound at 1024 is wildly over-provisioned for the actual rate).

### 4. Out of scope

- **Wasm components addressing chassis-internal caps.** A wasm component asking the hub to spawn another substrate is not a use case today. If it becomes one, the existing typed-sender path (`ctx.actor::<ProcessCapability>().send(&Spawn { ... })`) is reachable structurally — but the security implications (a guest spawning host processes) need their own ADR. Phase 1 keeps `ProcessCapability` reachable only from the hub's MCP coordinator and (eventually) other hub-internal actors.
- **Hot-path data plane.** Per-byte mail for high-throughput sockets is not part of the v1 stance. The framing actor chain absorbs framing cost, but the per-frame mail rate is still bounded by the protocol's frame rate. Streaming MB/s of raw bytes through mail is its own design problem; deferred.
- **`Capability`-trait reshaping.** Today every cap implements `NativeActor` and lives in `aether-capabilities` or (proposed) the chassis crate. There's no shared "I wrap an OS resource" trait. Adding one is premature — until we have ≥3 chassis-internal caps shipped, the right shape isn't clear and a placeholder trait is just bookkeeping.
- **ADR-0044 (capabilities-as-firewall).** ServerSocketCapability is the natural enforcement point for per-component network policy, but ADR-0044's design predates ADR-0074 and needs revisiting against the actor model regardless. Deferred to its own follow-up.

## Consequences

### Positive

- **Hub process supervision becomes uniformly testable.** `ProcessCapability` is a `#[actor]` block; its handlers are unit-testable with a mock transport the same way every other cap's handlers are. `spawn_substrate` / `terminate_substrate` today require `tokio::test` + a real binary to exercise.
- **Process-exit events surface as mail.** Any future cap or hub-internal actor that wants to react to "engine X exited" subscribes to `aether.process.exited` rather than threading callbacks through `EngineRegistry`. Useful for: log retention policy ("keep buffer for N seconds after exit"), automatic respawn under supervision policies, MCP tool authoring that needs lifecycle hooks.
- **MCP coordinator gets simpler.** `spawn_substrate` / `terminate_substrate` tool handlers become 5-line wrappers (mail, await reply, project to MCP shape) rather than direct callers into process-management functions.
- **Direction commits to symmetry across chassis.** Hub, desktop, headless, test-bench all plug capabilities into the same `Builder::with_actor` boot path. Anything chassis-internal that becomes an actor instantly works on every chassis that loads it.
- **Backpressure stance lands once, applies everywhere.** Bounded mailbox + drop-to-log is a per-cap default that any future chassis-internal actor inherits. Today every mailbox is effectively unbounded; the v1 stance is a strict improvement.

### Negative

- **One more layer between MCP tool and process state.** A spawn today is one async function call; post-Phase-1 it's a mail send + wait_reply. Latency cost is negligible (mail dispatch is in-process mpsc) but the path is longer.
- **Bounded inbox introduces drop semantics.** ADR-0023's pre-existing log capture had effectively-unbounded queues; the v1 stance for `ProcessCapability` introduces an explicit overflow case the hub didn't have before. Mitigated by over-provisioning (1024 entries vs an actual rate of <10/sec).
- **Reaper task is its own correctness problem.** Converting `Child::wait` into mail requires a per-child task or a centralised signalfd reaper. Both are workable; both have edge cases (zombie children if the reaper task panics; SIGCHLD coalescence on Linux). Phase 1 ships with the simplest shape (per-child `tokio::spawn`-ed wait task that emits `aether.process.exited` mail and exits) and tightens if it bites.
- **ADR-0009's mechanism description goes stale.** ADR-0009 §3 describes `spawn_substrate` as a hub method; the post-implementation reality is mail-to-cap. Plan: an editorial note on ADR-0009 once Phase 1 ships, similar to the ADR-0023 / ADR-0077 supersession.

### Neutral

- **`tokio::process::Child` stays.** The cap wraps it, but the underlying mechanism is unchanged. PID correlation, handshake timeouts, SIGTERM grace, child stdio inheritance — all retained.
- **MCP wire protocol unchanged.** `spawn_substrate` / `terminate_substrate` MCP tools accept the same args and return the same shapes. Internals change; user-visible surface doesn't.
- **Hub already has actor infrastructure.** ADR-0034 made the hub a chassis; the `Builder::with_actor` path already exists there. Adding `ProcessCapability` is a new cap registration, not a new chassis subsystem.

## Alternatives considered

- **Keep `spawn_substrate` / `terminate_substrate` as functions, expose process-exit as a mail event only.** Halves the work: process state stays in the hub's bespoke registry, but lifecycle events become subscribe-able. Rejected: leaves the hybrid model in place. A cap that owns process state but exposes only events is conceptually clean; what's proposed (full actor with all process state) is structurally simpler — one piece of state, one mailbox, one set of handlers.
- **One catch-all `ChassisCapability` per chassis with sub-handlers for process / sockets / etc.** Reduces cap count. Rejected: ADR-0074's per-cap discipline is load-bearing (one mailbox, one state, narrow handlers), and lumping unrelated OS resources into one cap defeats the per-cap testability story.
- **Block on the broader stream-IO design before shipping `ProcessCapability`.** Don't ship Phase 1 until the framing actor chain and backpressure scheme are fully designed. Rejected: process supervision is naturally event-shaped (no streams, no framing) so it's the cheapest validation of the chassis-internal-actors direction. Bundling it with the harder design questions delays a clear win for unclear reasons.
- **Skip the actor model for chassis-internal stuff entirely; let chassis driver code stay bespoke.** The "everything is an actor" model is fine for in-substrate work but isn't load-bearing for chassis driver internals. Rejected: same friction as the cap migration. Chassis-internal callbacks, registry-threading, and bespoke event plumbing are exactly the kind of ad-hoc code the actor model exists to retire.

## Follow-up work

- **Phase 1 implementation issue** (filed alongside this ADR): `ProcessCapability` boots in the hub chassis, MCP tool handlers route through it, `aether.process.exited` broadcast is wired, bounded mailbox + drop-to-log instrumented. Test plan: existing `spawn_substrate` / `terminate_substrate` integration tests pass unchanged; new test for `aether.process.exited` event emission on substrate exit; mailbox-overflow drop fires the warn log under a synthetic flood.
- **ADR-0009 editorial note** when Phase 1 ships, pointing readers at this ADR for the post-actor-model spawn/terminate path.
- **Phase 2 (parked, ADR-pending):** `ServerSocketCapability` + framing actor chain. Forcing function: a new MCP-tool-shaped capability that needs network IO, or revisiting ADR-0044's per-component network policy, or a user-space app component that wants TCP. None forcing today.
- **Per-class backpressure policy (parked):** reuse ADR-0074 §Decision 5's frame-bound vs free-running classification to pick drop vs block. ADR-pending if v1's drop-to-log proves too coarse.
- **Credit-based flow control (parked):** for high-throughput data planes the actor chain has to support eventually. ADR-pending; not load-bearing for any current cap.
