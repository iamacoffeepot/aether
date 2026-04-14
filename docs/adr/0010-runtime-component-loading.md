# ADR-0010: Runtime component loading and replacement

- **Status:** Proposed
- **Date:** 2026-04-14

## Context

The substrate today bakes in a single component at compile time (`HELLO_WASM` via `include_bytes!`). To run a different component, you rebuild the substrate. To run two components in one substrate, you can't — the registry assumes one.

ADR-0009 gives the hub the ability to spawn substrates. By itself, that doesn't help much: every test scenario would still need its own substrate binary, which means rebuilding the substrate every iteration. The "test in isolation" workflow only becomes real when components themselves are mobile — when the substrate is a generic host that loads whatever bytes it's handed.

Forces at play:

- **Components shouldn't be a build-time concept.** The substrate is the GPU/I/O host. Components are the engine code being iterated. Coupling the two at compile time means every component change is a substrate rebuild and a GPU re-init.
- **The mail abstraction already covers control flow.** ADR-0008 made hub→engine and engine→hub both mail. "Load this component" is just another mail with a kind. No new transport, no new vocabulary outside the kind namespace.
- **Several substrate internals are init-only today.** The component table, mailbox allocator, and kind registry are all populated before any component runs. Loading at runtime mutates each — the read paths (especially kind dispatch) need to be safe under concurrent mutation.
- **Late-loaded components may need new kinds.** If the substrate doesn't already know `aether.physics.contact_event`, a physics component arriving at runtime can't register or send it. The load operation has to carry kind descriptors alongside the WASM bytes.
- **"Swap" decomposes into load + drop.** Once you can load a new instance and drop an old one, replacing is a small composition: load new, atomically rebind the old mailbox id, drop old. The genuinely hard part — preserving instance state across the swap — stays out of scope for this ADR.
- **In-flight mail to a dropped component is a real edge case.** Mail addressed to mailbox N when N is dropped or replaced has to go somewhere: drained by the old, dropped, or rerouted to the new. Drop is the simplest honest answer for V0.

## Decision

Components are loaded into a substrate at runtime via mail. The substrate exposes a small control vocabulary on a reserved `aether.control.*` kind namespace; the hub (driven by an MCP agent) is the typical sender. Replacement is identity-preserving load + drop. State migration is explicitly out of scope.

### 1. Control kinds

Reserved namespace `aether.control.*`. The substrate handles these kinds itself rather than dispatching to a component.

- `aether.control.load_component` — payload: WASM bytes, kind descriptors, optional human-readable name. Result: reply-to-sender mail `aether.control.load_result` carrying the new mailbox id, or an error.
- `aether.control.replace_component` — payload: target mailbox id, WASM bytes, kind descriptors. Atomically rebinds the mailbox; old instance is dropped.
- `aether.control.drop_component` — payload: target mailbox id. Removes the component; the mailbox id becomes invalid.

Results return as reply-to-sender mail using the ADR-0008 path. The agent issuing the load gets the result targeted at its session.

### 2. Component bytes inline on the wire

WASM bytes are carried in the load mail's payload. Two reasons:
- Path-by-reference would couple hub and substrate to a shared filesystem. They might not share one (separate machines, sandboxed processes, or just clean separation of concerns).
- The "everything is mail" ethos. Special-casing WASM as not-mail invites every other artifact to want its own channel.

The ADR-0006 `MAX_FRAME_SIZE` (1 MiB) applies. Real WASM modules fit comfortably; if they ever stop fitting, raising the cap is cheaper than building a side channel.

### 3. Identity allocation

The substrate allocates the mailbox id. The agent doesn't pick it — that risks collision and conflates naming with identity. The result mail carries the assigned id; subsequent mail addresses that id directly.

A `name` field on the load mail is metadata for human consumption (logs, `list_engines` enrichment). It does not affect addressing.

### 4. Late kind registration

Load mail carries kind descriptors. The substrate registers them at load time:

- New name → fresh kind id, descriptor recorded for ADR-0007 encoding on the hub.
- Existing name with identical encoding → no-op.
- Existing name with different encoding → load fails with a conflict error; the agent has to resolve (rename, restart substrate).

The hub's per-engine descriptor cache (ADR-0007) needs to refresh when an engine's kind set changes. Either the substrate sends a `KindsChanged` frame post-load, or the hub re-pulls on next describe — implementation choice deferred.

### 5. Replacement semantics

`replace_component(target, bytes)` is:
1. Instantiate the new WASM component.
2. Atomically rebind mailbox `target` from old → new instance.
3. Drop the old instance. Any mail in the old instance's queue at the moment of swap is dropped.

External senders see no addressing change. Mail in flight on the wire that hasn't yet reached the substrate continues to address `target`; the new instance receives it.

In-flight mail policy is **drop**, not drain. Drain is more semantically clean but adds protocol — quiesce signal, flush ack, then swap. V0 picks drop; if a real workload needs drain, it's an additive `drain: bool` flag on the replace mail.

### 6. State migration is out of scope

The new component starts fresh. WASM linear memory from the old instance is not transferred. If a component needs persistent state across swaps, it's the component's responsibility to externalize it (e.g., write to a sidecar mailbox another component owns, or to a future persistence service). When state migration becomes a concrete need, it gets its own ADR.

## Consequences

### Positive

- **Iteration becomes cheap.** Edit a component, rebuild the WASM, send `replace_component`. No substrate restart, no GPU re-init thrash, no lost connection state.
- **Multi-component-per-substrate is unlocked.** The current "one component baked in" assumption falls away. Substrates host whatever set of components currently makes sense.
- **Test-in-isolation composes naturally with ADR-0009.** Spawn an empty substrate, load the component under test, drive it via mail, terminate. Each iteration is independent.
- **Swap reuses the load primitive.** Replacement isn't a separate code path — it's "load, rebind, drop" with the same lifecycle hooks.

### Negative

- **The substrate's init-only invariants are gone.** Registry, mailboxes, and kinds all mutate at runtime. Concurrency stories that assumed init-then-run need revisiting (the kind registry in particular is read on every dispatch).
- **WASM bytes on the wire eat frame budget.** A 500 KiB module against the 1 MiB cap leaves only 524 KiB for whatever shares the channel that round-trip. Real workloads will probably want a higher cap or eventually a side channel.
- **Drop-on-swap loses in-flight mail.** A `replace_component` issued while the old instance has a busy queue silently drops that work. Visible to the sender via undeliverable status, but still a sharp edge.
- **Late kind registration adds a coordination problem.** Two simultaneous loads that register `aether.foo.bar` with different encodings race. First wins; second fails. Agents have to handle it.

### Neutral

- **Component bytes are not signed or verified.** The substrate trusts the hub. Fine for single-tenant V0; a multi-tenant story needs signed bytes or a capability check.
- **Drop semantics for in-flight mail can be tightened later.** Adding `drain: bool` to `replace_component` is additive.
- **State migration stays a future concern.** No design hooks added for it; when a real use case appears, it'll be its own ADR.

## Alternatives considered

- **Components specified at spawn time only.** ADR-0009's spawn carries a component path; substrates are immutable after boot. Rejected: rebuilds the iteration loop at spawn granularity, defeats the point. Spawn an empty substrate, load components separately.
- **Path-by-reference instead of inline bytes.** Load mail names a file path; substrate reads it. Rejected: filesystem coupling between hub and substrate, breaks down for non-colocated processes.
- **Agent-supplied mailbox ids.** The load mail names the desired id; substrate accepts or errors on collision. Rejected: agent has to track ids, collisions are a source of bugs, no upside vs. substrate-allocates-and-reports.
- **Drain-on-swap by default.** Old instance finishes its queue before being dropped. Rejected for V0: requires a quiesce protocol, blocks the swap, complicates implementation. Drop is honest and correctable.
- **Explicit separate "swap" wire op without using load.** Replacement is its own primitive that doesn't share code with load. Rejected: re-implements load with extra constraints. Composing is cleaner.
- **Kind descriptors registered separately from component load.** Two-step: register kinds, then load. Rejected: introduces an ordering hazard (load before kinds = dispatch errors). Bundling them in one mail makes the load atomic.

## Follow-up work

- `aether.control.{load,replace,drop}_component` and `aether.control.load_result` kinds in `aether-substrate-mail`.
- Substrate-side handlers that gate on the reserved namespace and route to the runtime registry instead of dispatching to a component.
- Runtime-mutable `Registry`: mailbox add/remove, kind add, conflict detection. `kind_name` reverse lookup is already in place.
- `KindsChanged` frame (or per-handshake re-announcement) so the hub's ADR-0007 cache refreshes after a load.
- Result reply via ADR-0008 reply-to-sender. The substrate-side plumbing is in place; the WASM-component-facing host fn was deferred in ADR-0008's follow-up and is now needed.
- Substrate boot path: tolerate "no component" startup (depends on ADR-0009).
- **Parked, not committed:** drain-on-swap, signed component bytes, state migration, hot-reload primitives that survive substrate restart, component capability declarations / sandboxing, side channel for >1 MiB modules.
