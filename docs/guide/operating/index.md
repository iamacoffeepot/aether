# Operating a live Aether engine

This part of the guide is the runbook for an agent—or a human at the same
controls—operating Aether through the MCP harness. It starts after code has
been built and answers four practical questions:

1. Which process owns the thing I am looking at?
2. Which identifier is safe to reuse?
3. What evidence can I collect without changing the engine?
4. What must I clean up when the task ends?

For the transport topology and connection setup, begin with
[The MCP harness](../mcp-harness.md). The pages here assume its tunnel,
`aether-mcp`, and hub are reachable.

## The operating model

The harness has four ownership layers. Confusing adjacent layers is the most
common source of bad recovery decisions.

| Layer | Owns | Survives a hub restart? | Operator handle |
|---|---|---:|---|
| MCP client session | tool calls and returned evidence | the tunnel normally does; an `aether-mcp` restart does not | MCP connection |
| Hub | supervised engine table, engine proxies, artifact store | fleet: no; stored artifacts: yes | hub-local tools |
| Substrate | one live engine, native caps, live kind and mailbox registries | no | `engine_id` |
| Loaded component | one wasm actor behind a substrate mailbox | no | lineage `name` and `mailbox_id` |

Keep three nouns separate:

- **Stored** means bytes and a manifest exist in the hub's content-addressed
  artifact store. `list_binaries` and registry `list_components` report this.
- **Running** means a substrate is in the hub's current supervised fleet.
  `list_engines` reports this.
- **Loaded** means a component actor is registered inside one running
  substrate. `load_component` creates it; `describe_component` inspects it.

A stored component is not loaded. A loaded component on engine A says nothing
about engine B. An old `engine_id` says nothing about a new hub process.

## The safe operating loop

Use this loop even for a short probe:

- [ ] Start with `list_engines` and decide whether to adopt an existing engine
      or create one.
- [ ] Before using any selector, prove the artifact is stored with
      `list_binaries` or registry `list_components`; otherwise upload it first.
- [ ] Record the exact `engine_id` returned by `spawn_substrate`.
- [ ] Record each explicit load's returned lineage `name` and `mailbox_id`;
      for boot loads, record the configured name used to derive its lineage.
- [ ] Inspect the live surface before guessing mail: `describe_kinds`,
      `describe_handlers`, or `describe_component`.
- [ ] Prefer settled `send_mail`; use `send_mail_traced` when causal timing or
      whole-chain proof matters.
- [ ] Collect logs, traces, costs, and frames before destructive recovery.
- [ ] Drop task-owned components and require their successful `drop_result`, or
      terminate task-owned engines when done.
- [ ] After termination, call `list_engines` again and verify the engine cleanup
      is visible. A component drop is verified by its result, not the fleet list.

The tool's live JSON schema is the parameter reference. This guide names the
current tools and their contracts, but does not duplicate every field because
that surface evolves.

## Choose the next page

| If you need to… | Read |
|---|---|
| adopt, spawn, identify, or terminate substrates | [Engine fleet](engine-fleet.md) |
| upload, select, load, replace, inspect, or drop wasm | [Component registry](component-registry.md) |
| learn the live schema or gather diagnostic evidence | [Inspect and debug](inspect-and-debug.md) |
| respond to a failure without making it worse | [Recovery](recovery.md) |

The subsystem explanations remain useful alongside these runbooks:

- [Components & lifecycle](../systems/components.md) explains the wasm host and
  trampoline model.
- [Mail, kinds & scheduling](../systems/mail-and-kinds.md) explains addresses
  and payload schemas.
- [Tracing & settlement](../systems/tracing-and-settlement.md) defines what
  “settled” proves.
- [Logging](../systems/logging.md) explains what reaches actor log rings.

## Ownership rules

Ownership is operational, not merely descriptive:

| Resource | Default owner | Cleanup rule |
|---|---|---|
| Engine you spawned | your task/session | call `terminate_substrate` on its exact `engine_id` |
| Engine you merely found | whoever created it | do not terminate without explicit authority |
| Component you loaded into a shared engine | your task/session | send the component drop mail when safe |
| Component loaded at boot into your engine | your engine | terminating the engine cleans it up |
| Named artifact in the hub store | hub operator | names persist and protect their current target from LRU eviction |
| Per-engine scratch/handle-store directory | hub operator | termination does not expose directory cleanup through MCP |
| Evidence file returned by a spill or capture | caller | preserve or remove it deliberately; it is host-side state |

There is no current MCP delete tool for stored binaries or components. Reusing
an upload name repoints that name to the new hash; it does not erase the old
bytes. If no other name points at the old hash, it becomes unnamed history and
can later be evicted under the store's disk budget. Do not promise registry
cleanup that the tool surface cannot perform.

Component drop is also narrower than engine termination: it unloads the guest
and clears the mailbox's capabilities, but leaves an empty trampoline at the
same lineage address. Only terminating the substrate tombstones that slot.

## Identifier discipline

- Treat `engine_id` as a hub-lifetime routing handle. The hub allocates it from
  process-local state and only guarantees uniqueness in its current fleet. A
  later hub can mint the same text for a different engine.
- Treat `rpc_port` as observation, not identity. Never route a tool call by
  substituting a port for an `engine_id`.
- Hand tagged ids such as `mbx-…` and `knd-…` back verbatim.
- Address a loaded component by the full lineage `name` returned by the load,
  normally `aether.component/aether.embedded:NAME`.
- Prefer a content hash when a rollout or rollback must select exact bytes.
  A human-readable registry name is a movable pointer.

## Mutation posture

Read-only discovery should precede mutation. `list_*`, `describe_*`,
`actor_logs`, and `actor_cost` are the normal first calls. `capture_frame` is
observational to the engine when used without mail bundles; its `mails` and
`after_mails` fields make it engine-mutating. Independently, `save_path` creates
parent directories and can overwrite a host file even when both bundles are
empty. `send_mail`, component lifecycle calls, spawn, replace, and terminate all
change live state.

Fire-and-forget is not a faster form of verification. It proves that the call
was written, not that the engine completed the work. Use it only when the
recipient intentionally has no useful reply or when a later observation is the
actual success criterion.

The current terrain-specific mutation tools are `terrain_marks`,
`terrain_editor`, `apply_terrain_brush`, `run_terrain_automaton`,
`propose_terrain_edit`, `commit_terrain_proposal`,
`discard_terrain_proposal`, and `set_terrain_proposal_preview`. They require
exact loaded component lineage names; they do not replace the fleet,
introspection, evidence, or cleanup loop above. See
[Authoring terrain](../recipes/authoring-terrain.md) for the domain workflow and
the live tool schemas for current parameters.

## Source routes

When prose and behavior disagree, inspect these routes in order:

- MCP tool names and user-facing contracts:
  `crates/aether-mcp/src/tools/mod.rs`
- Live JSON argument and response shapes: `crates/aether-mcp/src/args.rs`
- Fleet and component orchestration:
  `crates/aether-mcp/src/tools/engine.rs` and `components.rs`
- Hub fleet ownership and engine death handling:
  `crates/aether-capabilities/src/engine/server/runtime.rs` and
  `engine/proxy/runtime.rs`
- Stored artifacts: `crates/aether-capabilities/src/engine/store/`
- Live kind inventory: `crates/aether-capabilities/src/inventory/`
- Loaded component registry and lifecycle:
  `crates/aether-capabilities/src/component/`
