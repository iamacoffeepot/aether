# ADR-0002: Pure-mail actor model on a WASM-hosted substrate

- **Status:** Accepted (elevated from Proposed by ADR-0003)
- **Date:** 2026-04-13

## Context

Aether is a game engine designed around a Claude harness acting as assistant, engineer, designer, and architect — with capabilities that vary per build target (player, designer, engineer). Several forces shape the runtime design:

- **Hot-swap of components** during iteration is a first-class requirement. Dev-time reload without losing world state; mod/plugin support; and Claude-authored components that can be rebuilt and swapped at runtime.
- **Sandboxing** is required because Claude-authored code cannot be trusted to be safe. Capability gating must be enforceable per build target.
- **Claude must address and mutate the world** through some mechanism. That mechanism should not be a privileged side-channel; Claude should use the same interface other components use, so capability equals reachability.
- **Inputs** are hardware signals (keyboard, mouse, gamepad), disk, and network. **Outputs** are graphical and auditory. Everything else is simulation.
- A game loop has tight per-frame budgets (≤16ms at 60Hz), potentially across thousands of entities, and parts of the engine are genuinely hot code. The boundary model we pick has to withstand that workload — which is the load-bearing risk in what follows.

We are in a research phase. The intent is to start from the loosest abstraction that cleanly expresses the design we want, measure it against representative workloads, and tighten only where measurement demands.

## Decision

Aether is built as a **thin native substrate** hosting a **WASM runtime** (initial target: `wasmtime`). Most of the engine runs as WASM components above the substrate. Components communicate **exclusively via mail** — a pure message-passing actor model with no shared memory between components.

The vocabulary this ADR adopts: **substrate** is the native base layer that owns hardware and hosts the runtime; **components** are WASM modules running above it; **mail** is the protocol between components. Each word does one job; the whole system ("Aether" or "the engine") is the three together plus tooling.

### Substrate responsibilities (native Rust)

- GPU rendering (wgpu)
- Windowing and input event loop (winit)
- Audio output — substrate-owned because the hardware latency budget (~10ms) cannot tolerate boundary crossings regardless of the component model above it
- Disk and network I/O
- WASM runtime hosting, component lifecycle, and hot-reload
- The scheduler that ticks the mail graph and dispatches work across a threadpool
- The host-function surface exposed to components — adding one is a deliberate capability decision, not a convenience

### Mail as the only inter-component protocol

Every component is addressable by **mailbox**. Mail envelopes are typed and **batch-carrying by construction**: `{ recipient, type, payload_batch: Vec<T> }`. A "single event" is a batch of one. Hot paths send batches of N and amortize boundary cost across the batch.

There is no shared memory between components. Anything one component needs to know about another component's state arrives as mail. This is deliberately strict: it keeps the abstraction uniform, keeps capability reasoning simple (you can reach who you can mail), and lets us observe where — and whether — the strictness actually hurts us.

### Component granularity

**Components are subsystem-sized, not entity-sized.** A physics component owns its entire physics world. A scene component owns its entire scene graph. An AI component owns its entire AI state. Inside a component, state is plain data and iteration is a tight loop in native (WASM) code.

The mail boundary is between subsystems, not between the things a subsystem manages. "One actor per game entity" would turn every cross-entity interaction into per-frame mail and destroy performance; that is not what this decision authorizes. Granularity discipline is load-bearing and part of the decision, not a detail below it.

### Claude as a participant

Claude sends mail to components like any other participant. There is no privileged Claude API. Claude's capability in a given build is exactly determined by which mailboxes are registered and reachable for that build target. Transport is interchangeable: Claude may run as an in-process WASM component, or deliver mail over a wire (e.g., MCP); from the recipient's perspective the two are identical.

## Consequences

- **Capability gating falls out naturally.** A build target determines which mailboxes are registered and reachable. Claude in a Player build cannot address what isn't on its mail graph.
- **The "in-process SDK vs. out-of-process server" question dissolves** into a transport choice. Recipients don't care where mail came from.
- **Hot-swap is real.** WASM provides a stable ABI, so components can be unloaded and reloaded without the host-side contortions other mechanisms require.
- **Crash isolation.** A misbehaving component traps in its VM; the substrate survives.
- **Determinism** is available when desired — WASM is deterministic given stable host-function responses, which helps replay and networked simulation.
- **Performance is the open risk.** The design is deliberately loose and unoptimized. It is expected — not assumed — that measurement will expose specific places where pure mail cannot meet a requirement. The next concrete step is a spike that measures this on representative workloads before this ADR moves from Proposed to Accepted.
- **Granularity discipline matters more than the mechanism.** A pure-mail system with component-per-entity granularity fails. A pure-mail system with component-per-subsystem granularity has a real chance of meeting requirements. Project conventions and review must enforce the latter.
- **Adding host functions widens capability surface.** Each host function becomes available to every component that can reach it. Growth of the host-function surface is reviewed as deliberately as any other architectural change.

## Optimization paths (deferred)

These are levers we have identified but are explicitly *not* taking now. They exist so future-us can reach for them in a known order rather than reinventing:

- **Hierarchical shared memory.** Components could be organized into a tree; a parent could map a memory region into each child's linear memory, and siblings could declare shared-variable access for scheduler-coordinated concurrent reads. Adds structure but tightens per-frame data access for workloads that the spike shows pure mail cannot handle.
- **Substrate-hosted fast paths.** Specific high-volume message types could be handled by native code in the substrate rather than round-tripping through a component, when that component's logic is simple enough to live as a host function.
- **Read-only caches.** The substrate could expose immutable snapshots of specific state to components that only need to read it, avoiding per-frame mail for that state.
- **Batched mail compaction.** The mail dispatcher could coalesce equivalent mails to the same recipient within a tick.

None of these are binding. They are documented to prevent the reflex of inventing a new lever under pressure when one we already know about would do.

## Alternatives considered

- **Shared memory as the primary substrate, messages as a secondary layer.** Components share a memory space; a scheduler coordinates access via declared dependencies; messages are used only for coarse coordination. Higher potential per-frame throughput, but makes Claude's integration a side-channel: Claude must understand the memory layout and scheduling contract rather than simply knowing who to mail. Rejected as the primary model. Elements of it may be reintroduced selectively as optimization paths if measurement requires them.
- **An embedded scripting runtime** (a dynamic, interpreted language VM hosted natively) instead of WASM. Considered and deferred. WASM keeps the engine Rust-all-the-way-down, provides sandboxing and hot-reload on the same footing, and avoids introducing a second language surface for Claude to author in. May be revisited if the actor model turns out to want smaller, more dynamic component units than WASM modules naturally support.
- **A privileged Claude API separate from the component interface.** Rejected: creates two parallel mutation paths, undermines capability-by-reachability, and introduces a special-case surface to design, secure, and evolve.
