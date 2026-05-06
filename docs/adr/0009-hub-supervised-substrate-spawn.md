# ADR-0009: Hub-supervised substrate spawn

- **Status:** Superseded in part by ADR-0078
- **Date:** 2026-04-14

> **Note (2026-05-06):** §3 (the substrate-spawn mechanism — bespoke
> async `spawn_substrate` / `terminate_substrate` helpers in
> `crate::hub::spawn`, `EngineRegistry`-owned `Child` side-map, the
> hub coordinator's `terminate_all_children` shutdown sweep) is
> superseded by ADR-0078. Phase 1 of ADR-0078 lifted child-process
> supervision into `ProcessCapability` — a `#[bridge] mod native` cap
> in `aether-substrate-bundle::hub::process_capability` that owns
> every spawned `Child`, runs a per-child reaper task converting
> `Child::wait` into `aether.process.exited` broadcast mail, and
> exposes `aether.process.{spawn, terminate}` request/reply kinds the
> MCP coordinator routes through.
>
> §1 (the MCP-tool surface — `spawn_substrate`, `terminate_substrate`,
> `list_engines.spawned`), §2 (process lifecycle: `AETHER_HUB_URL`
> injection, `Hello`-handshake correlation by PID, SIGTERM → grace →
> SIGKILL escalation, externally-connected vs spawned distinction),
> and §4 (failure modes) all stay normative. The user-visible MCP
> wire shape is unchanged across the migration; only the substrate-
> internal plumbing moved into the actor model. Read this ADR for the
> behavior contract; read ADR-0078 for the post-actor-model
> implementation that ships today.

## Context

ADRs 0006-0008 made the hub a fully bidirectional broker between Claude sessions and connected engines. Engines connect to a passive hub: a human starts `cargo run -p aether-substrate` in a terminal, the substrate dials the hub, and Claude can drive it from there.

That's a hard floor on what Claude-in-harness can do unsupervised. Every "spin up an isolated engine, test a thing, tear it down" workflow needs a human to start the substrate. The whole point of the harness — Claude iterates on engine code without a human in the loop — runs out of road at "I need an engine."

Forces at play:

- **Process supervision belongs somewhere.** Either the hub becomes a supervisor or we add a separate launcher process. A separate launcher just relocates the problem and forces a third actor into the MCP picture (Claude → launcher → substrate → hub → Claude). The hub is the natural owner: it already has the lifecycle and already exposes MCP.
- **Multi-substrate is the long-term shape.** Test-in-isolation implies "one substrate per scenario," not "the substrate." The supervisor must support N concurrent children from day one.
- **The engine binary is build-system-specific; the hub isn't.** The hub doesn't know about cargo or workspace layouts. It executes a binary path the agent supplies.
- **Async startup is the truth, but synchronous-feeling APIs are friendlier.** The substrate dials the hub, handshakes, then is ready. Returning a spawn handle and making the agent poll for the engine id is honest but tedious. Blocking the spawn tool until handshake (with a timeout) is friendlier for the agent, at the cost of some MCP-side latency.
- **Orphan processes are a real failure mode.** A hub crash today leaves substrates connected to nothing harmful; once the hub spawns them, a hub crash leaves orphan children. Process death has to be coupled — child dies when parent dies.

## Decision

The hub gains a process-supervision surface, exposed via MCP, that spawns and terminates substrates. The hub does not know how to *build* a substrate — it just executes a binary path the caller provides, with `AETHER_HUB_URL` injected so the spawned substrate dials back automatically. Component selection is **not** a spawn-time concern; spawn produces an empty substrate, and ADR-0010 covers what happens after.

### 1. MCP-tool surface

- `spawn_substrate(binary_path: String, args?: Vec<String>, env?: Map<String, String>, timeout_ms?: u32) → SpawnResult`
  - Spawns the binary as a child process with `AETHER_HUB_URL` injected into `env`.
  - Blocks until either: (a) the substrate completes its `Hello` handshake → returns `engine_id` and the child PID, or (b) the spawn-handshake timeout fires → terminates the child and returns a spawn error.
  - Default timeout is generous (a few seconds) and overridable per call — slow CI machines need headroom.
- `terminate_substrate(engine_id: EngineId) → ()`
  - SIGTERM, then SIGKILL after a short grace period. Returns when the child is reaped.
  - Errors if the engine id refers to an externally connected substrate the hub didn't spawn.
- `list_engines` (existing) gains a `spawned: bool` flag distinguishing engines the hub spawned from those that connected externally.

### 2. Process lifecycle

- The hub owns the `Child` handle for every spawned substrate. The engine record (registry entry) holds it alongside the existing `mail_tx`.
- The child's stdin is closed; stdout/stderr are captured to per-engine ring buffers. A future MCP tool (`engine_logs(engine_id, since?)`) drains them; out of scope for this ADR.
- Externally connected substrates have no `Child` handle. The hub treats them exactly as today.

### 3. Process death coupling

- **Parent → child** (hub dies, kill substrate): tokio `Command::kill_on_drop(true)`, plus deliberate teardown on hub shutdown signal so children get SIGTERM rather than SIGKILL when shutdown is graceful.
- **Child → parent** (substrate dies, hub notices): the existing socket-disconnect path detects it. The child-handle reap surfaces an exit code for diagnostics.

### 4. What the hub deliberately doesn't do

- It doesn't know about cargo, workspace layouts, or how the binary was built. The agent passes a path; the hub executes it.
- It doesn't sandbox the child beyond OS process isolation. No cgroup limits, no syscall filtering.
- It doesn't multiplex hosts. All spawning is on the hub's own machine.
- It doesn't bake in a "default substrate" binary. If we want one, that's an additive registration step.

## Consequences

### Positive

- **Closes the autonomy gap.** Claude can spin up a substrate, drive it, tear it down — no human in the terminal. The "test a module in isolation" workflow becomes a sequence of MCP calls.
- **Multi-substrate scenarios are cheap.** Parallel test runs, side-by-side comparison, A/B harness configurations — all just N spawns.
- **Externally connected substrates still work.** Backward-compatible with the dev workflow where you `cargo run` the substrate by hand and want Claude to attach.
- **Spawn and component lifecycle stay separate concerns.** ADR-0010's runtime component loading composes cleanly: spawn empty, load whatever, terminate.

### Negative

- **Hub becomes a process supervisor.** It now owns child handles, exit codes, and shutdown teardown. New failure modes appear: zombie children, log-buffer growth, shutdown ordering.
- **Spawn is synchronous-feeling but masks async startup.** A flaky substrate that takes 4s to handshake will trip a 2s timeout; tuning the default is real work and the override has to be plumbed through MCP.
- **Binary-path-as-string couples the agent to filesystem layout.** Claude has to know where the substrate binary is. A "default substrate" registered with the hub at startup would soften this; deferred.

### Neutral

- **No sandboxing.** A spawned substrate has the same FS/network access as the hub. Fine for single-tenant V0; a multi-tenant future needs a real sandbox layer.
- **Stdout/stderr capture surface is deferred.** Buffers are filled, but the read tool is its own ADR.
- **Builds remain external.** Building the substrate before spawning is the agent's job (or a wrapper script's). The hub doesn't drive cargo.

## Alternatives considered

- **External launcher.** A separate process supervises substrates; hub stays passive. Rejected: relocates the problem and adds a third hop in the MCP picture. The hub already has the right shape.
- **Hub-baked substrate binary.** Hub embeds a default substrate and spawns it on demand. Rejected: couples the hub release cycle to substrate code, and the multi-component-per-substrate direction (ADR-0010) means a "default" doesn't really make sense.
- **Async spawn with separate readiness query.** Spawn returns immediately with a spawn id; agent polls until the engine appears. Rejected for V0: synchronous-feeling spawn is friendlier and the timeout is the cost. Async shape can be added later without breaking the API.
- **Spawn-with-component arguments.** `spawn_substrate(component_path, ...)` bakes in component selection at spawn time. Rejected: ADR-0010 makes component loading a runtime mail; conflating spawn and load forecloses dynamic add/swap. Spawning produces an empty substrate; loading is a separate op.
- **Resource limits at spawn time.** cgroup quotas, memory caps. Rejected: YAGNI; nothing in V0 needs it.

## Follow-up work

- `spawn_substrate` / `terminate_substrate` MCP tools wired through `aether-hub`.
- `Child` ownership in the engine registry; teardown on hub shutdown.
- `AETHER_HUB_URL` injection into the spawn environment.
- Stdout/stderr capture buffers (read API is its own follow-on).
- `spawned: bool` field on `list_engines` output.
- Substrate-side: tolerate "no component baked in" startup (today the binary `include_bytes!`s a component; ADR-0010 lifts that).
- **Parked, not committed:** default-substrate registration, `engine_logs` MCP tool, sandboxing / cgroup limits, multi-host spawn, hub-driven build.
