# ADR-0034: Hub-as-substrate

- **Status:** Proposed
- **Date:** 2026-04-20

## Context

The current engine has two native binaries with distinct shapes:

- **`aether-substrate`** is the rich runtime: wasmtime host, mail scheduler, per-mailbox component table, kind registry, control plane, plus chassis-specific peripherals (winit window, wgpu device, input dispatch, frame capture). Components are WASM, swappable at runtime via `load_component` / `replace_component`, and observed through the kind manifest.
- **`aether-hub`** is the coordination layer: TCP listener, engine registry keyed by UUID, session tracking (per MCP connection), schema-driven mail encoding on the Claude-facing edge, and the `rmcp`-backed MCP tool surface. It is ~2000 lines of native rust. Its routing logic today is trivial — it looks up `engine_id` from the mail envelope and forwards.

These two roles accreted independently. The hub predates runtime component loading (ADR-0010) and the kind manifest (ADR-0028, ADR-0032); when those shipped, the substrate gained most of the "smart runtime" capabilities while the hub stayed an RPC relay.

Two recent design discussions pressure this split:

1. **Logical-address routing and sharding.** Real hosted game worlds want routing by logical name (`world.combat` → whichever substrate currently runs shard east-3) rather than caller-specified `engine_id`. Implementing that in the hub's native rust means every policy change is a binary rebuild + redeploy — and the hub is supposed to be always-on infrastructure.

2. **Operator Claudes.** The Claude-in-harness vision treats Claude as assistant/engineer/designer. A natural extension is Claude as server-operator / region-admin / dungeon-master: Claude sessions that make live decisions about sharding, player management, world state, narrative progression. The hub is the natural home for that role. If the hub is native rust, operator Claude surfaces through a hardcoded MCP API and can only work with what the hub binary already exposes — no runtime introspection, no component loading, none of the substrate's Claude-in-harness affordances apply to hub-level work.

Stated plainly: **the substrate already is the thing the hub would need to become.** It has WASM hosting, typed mail, runtime kind registration, hot-swap, observation broadcast, MCP integration through the hub-socket protocol. The hub is a small amount of functionality (TCP listen, child process spawn for spawned substrates, the MCP tool surface) wrapped around what is effectively an engine registry and a mail relay.

This ADR asks: what if there were only one runtime — the substrate — and the hub became a specialized deployment of it?

## Decision

**The hub collapses into a specialized substrate.** The target state is:

- One runtime primitive: `aether-substrate-core`. Contains wasmtime hosting, mail scheduler, kind manifest, component table, control plane, hub-socket protocol (as a *client* of another hub-substrate, not as a server).
- A chassis layer abstracts the native peripherals each deployment needs. Current deployments:
  - **Desktop chassis** (`aether-substrate-desktop`): winit window, wgpu device, input dispatch, frame capture. What the game-server and game-client substrates run today.
  - **Headless chassis** (`aether-substrate-headless`): no window, no GPU, console logging, tick driver. For dedicated game servers and processing workloads.
  - **Hub chassis** (`aether-substrate-hub`): TCP listener accepting inbound substrate + MCP connections, child-process supervisor for hub-spawned substrates, session tracking, the `rmcp` tool-router binding. No window, no GPU.
- The hub's current logic — engine registry, routing (today: `engine_id` lookup; future: logical-name and policy-based), MCP tool implementations, observation fan-out — migrates to **components running on a hub-chassis substrate**. A `hub-router` component holds the registry in its state, receives inbound mail, decides the target, and emits routed mail back out. A `hub-session` component tracks MCP sessions and publishes to `hub.claude.broadcast`. MCP tool bodies become kind-shaped mail (`aether.mcp.tool.<name>`) the component handles.
- Protocol unchanged: substrate-to-substrate is the only wire protocol. A hub-chassis substrate accepts many of these connections (it's the listener); a desktop-chassis substrate opens exactly one as a client. Mail kinds, framing, and the Hello/Welcome handshake are identical.
- The prerequisite chassis split (substrate-core + chassis crates) is its own implementation ADR (ADR-0035, planned); this ADR commits to the direction that the hub will be one of the chassis targets.

This is a foundational restructuring, not an incremental feature. The rollout is explicitly phased (see Follow-up work).

## Consequences

### Positive

- **One mental model.** Every node in an aether deployment is a substrate. Differences are chassis + component set. A Claude session connecting to the hub, a game server simulating a world, a headless processor crunching data — all speak the same protocol and respond to the same tooling.
- **Hot-swappable routing and operator logic.** Change the sharding policy, adjust routing rules, rewrite the session-tracker — all via `replace_component` on the hub-substrate. No binary redeploy.
- **Sandboxed policy code.** The routing component runs in WASM. Today an operator error in hub routing risks the whole relay; post-change, the chassis stays up even if the routing component traps — it can be swapped for a previous version without dropping connections.
- **Hubs-of-hubs for free.** A hub-substrate can open a client connection to another hub-substrate. Federated routing, hierarchical regions, cross-datacenter relays fall out of the substrate-to-substrate protocol with no new primitives.
- **Operator Claude pattern unlocks.** Claude sessions can register as handlers for logical names on a hub-substrate the same way they already register as clients. The operator role becomes just another substrate-client role.
- **Uniform Claude-in-harness tooling.** The same MCP tools that let Claude introspect, load, replace, and observe a game substrate now work on the hub-substrate too: `describe_component` on `hub-router`, `engine_logs` for hub-level debugging, `capture_frame` is a no-op on hub-chassis but everything else applies.

### Negative

- **Performance: WASM hop per routed message.** Mail that currently traverses native-rust `HashMap::get(engine_id)` will instead traverse a WASM component's receive path. For a hub serving a handful of substrates and MCP sessions this is not measurable; for a 10k-player MMO gateway it matters. Mitigations (precompiled components, fast-path native routing for trivial cases, sharding across multiple hub-substrates) exist but are not in this ADR's scope.
- **Chassis framework complexity.** The chassis trait has to be rich enough to host a TCP listener + MCP server as first-class peripherals — not just GPU + window. The chassis split (ADR-0035) can no longer get away with a minimal trait covering desktop + headless; it has to anticipate hub-chassis shape.
- **Refactor scope.** Current hub: ~2000 lines of native rust. Most of it migrates (some to the hub-chassis, some to components), most paths get rebuilt. This is weeks of work, not days.
- **MCP surface becomes component-defined.** Today the MCP tool set is fixed at compile time in `aether-hub`. Post-change, tools live inside components and the chassis has to surface them to `rmcp`. The registration story — how does the hub-chassis know a loaded component publishes `aether.mcp.tool.describe_kinds`? — needs design. Candidate: the `aether.kinds.inputs` manifest (ADR-0033) extended with an MCP-tool variant, or a sibling custom section.

### Neutral

- **Trust model shifts.** The hub was previously trusted infrastructure; its logic is now sandboxed WASM. This is a security win for dynamic routing and a philosophical shift in who controls what the hub does. Both effects exist and neither dominates.
- **Observability path unchanged.** The substrate already emits `aether.observation.frame_stats` and routes it via `hub.claude.broadcast`. Hub-level observation mail (routing decisions, registry changes) uses the same path. Every substrate, regardless of chassis, speaks the observation protocol the same way.
- **Bootstrap is clean.** The hub-substrate doesn't need a parent hub. The hub-chassis binds the TCP listener and runs without an `AETHER_HUB_URL` — it is the hub. Other substrates connect to it as today.

## Alternatives considered

- **Keep hub as separate binary; add routing logic in native rust.** Simpler, faster, no chassis-framework growth required. Rejected because it hardcodes the split between hub and substrate concepts, locks routing policy behind binary deploys, rules out sandbox + hot-swap benefits, and forecloses the operator-Claude pattern. The operational cost of a native-rust hub for a project whose explicit goal is Claude-in-harness across every surface is high.
- **Hub-as-substrate but keep routing native inside the hub-chassis.** Half-measure: get one binary and one mental model, keep performance, lose hot-swap + sandbox + operator Claude. Not enough of a win over the status quo to justify the chassis-framework cost.
- **Multi-tier mesh without substrate unification: thin native gateway + heavy substrate world servers.** This is the conventional MMO backend shape (e.g., gateway → agent server → world server). Works fine but doesn't address the Claude-operator vision and doesn't unify the protocol. Stays on the table as the fallback if hub-as-substrate proves too costly to implement.
- **Defer indefinitely and grow the native hub with routing, session mgmt, sharding logic in rust.** Rejected as a path, accepted as a default if no concrete pressure materializes. The tic-tac-toe server demo does not need hub-as-substrate; an MMO-scale deployment likely does. If the project never reaches MMO scale, the current hub is sufficient and this ADR becomes Superseded-by-doing-nothing.

## Follow-up work

### Phased rollout

This ADR is direction-setting. Implementation is explicitly phased; each phase is a separate PR (or PR series) with its own design ADR as needed.

**Phase 0 — Prerequisite: chassis split (ADR-0035).**
Factor `aether-substrate` into `substrate-core` (wasmtime + scheduler + mail + control plane + kind manifest) and `substrate-desktop` (winit + wgpu + frame capture). Ship `substrate-headless` as the second concrete target to prove the `Chassis` trait surface. Hub-chassis is not implemented in this phase but the trait must accommodate its known shape (TCP listener, many concurrent inbound connections, no GPU, no window).

**Phase 1 — Hub-chassis, hub logic still native.**
New binary `aether-substrate-hub` built on `substrate-core` + a native hub-chassis that takes over the current hub's responsibilities: TCP listener, child-process supervisor, session tracking, MCP tool registry. No components yet — the hub-chassis implements hub behavior directly. Delivers the "one binary primitive, specialized by chassis" story and lets us retire `aether-hub` as a separate crate.

**Phase 2 — Routing component.**
Extract routing + registry logic from hub-chassis into a `hub-router` WASM component. Hub-chassis becomes thinner: it receives inbound mail, forwards to the bound routing component, emits the component's decisions. `replace_component` on the router enables hot-swap of routing policy. Session tracking may stay in the chassis in this phase.

**Phase 3 — Component-defined MCP surface.**
Move MCP tool bodies into components. Design the manifest extension that lets the hub-chassis discover which components publish which tools. Enables third-party components (or Claude-authored components) to extend the MCP tool set at runtime.

**Phase 4 — Operator Claude pattern.**
Document and smoke-test Claude sessions acting as server operators: registering for logical-name handling on a hub-substrate, making live routing decisions, responding to hub-observation mail. Likely its own ADR capturing the conventions.

### Deferred

- **Federation / hubs-of-hubs.** Technically possible after Phase 2 since substrate-to-substrate is the only protocol and a hub-substrate can be a client of another. Operational patterns for multi-region deployment (who owns what logical names, how routing tables sync, what failure modes look like) are their own ADR.
- **Performance optimization for high-throughput routing.** Precompiled routing components, fast-path native bypass for trivial routes, sharding across multiple hub-substrates. Revisit when a real workload measurably suffers.
- **Non-Claude MCP clients.** Today MCP is the only session surface and Claude Code is the primary consumer. If the hub-substrate opens up to non-Claude clients (game clients speaking the substrate protocol directly), ACL / per-session authorization becomes load-bearing.
