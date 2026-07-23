# Choose the owning extension point

Most expensive Aether mistakes start one layer too low. A new feature does not
automatically need a native capability, and a new message does not automatically
belong in the substrate. Choose the boundary by the responsibility that must be
owned.

## Decision table

| Need | Prefer | Why |
|---|---|---|
| Stateful application/gameplay logic | Wasm actor component | isolated mailbox, typed handlers, hot load/replace |
| Small replaceable mail filter | Behavior | narrow ABI and effect vocabulary |
| Host I/O, device access, secrets, or privileged policy | Native capability | chassis-owned resources behind mail |
| Pure bounded value conversion | Native transform | discoverable value-to-value operation without actor state |
| Reusable product/editor actor | `aether-kit-*` actor | shared guest layer, not substrate policy |
| New process composition | Chassis profile or bundle | selects drivers and capabilities at boot |
| Agent/operator convenience | MCP tool over an existing contract | adapts JSON and evidence; should not invent engine semantics |
| Shared portable identity/schema primitive | Foundation crate | only when multiple owning layers truly need it |

## Component versus capability

Choose a component when the feature can live within the actor contract:

- it consumes and emits typed mail;
- it can use existing capability mailboxes for I/O;
- its state should be loadable, replaceable, or replicated;
- it should run with the same isolation model as third-party code.

Choose a native capability when the host must own something wasm cannot or
should not own:

- OS handles, sockets, windows, GPU/audio devices, host files, subprocesses;
- credentials or egress policy;
- fleet supervision or wasm runtime control;
- a thread/callback boundary with native resource lifetimes.

Native is not a performance escape hatch by itself. Moving ordinary product
logic into a capability expands the trusted surface and chassis matrix.

## Actor versus behavior

A component actor owns a mailbox identity, lifecycle, state, and typed handler
set. It can initiate mail and participate in request/reply chains.

A behavior receives a compact envelope and returns a verdict/effect list. It is
suited to interception or policy whose inputs and outputs fit that vocabulary.
If the design needs open-ended actor interaction, durable state ownership, or a
new public mailbox, use an actor.

## Handler versus transform

Use a handler when work involves state, I/O, scheduling, replies, or failure
that belongs to an actor. Use a transform when all of these are true:

- the operation is deterministic for its inputs;
- runtime and memory cost are bounded;
- there is no persistent owner;
- the result is a value, not an ongoing resource;
- build-static discovery through `describe_transforms` is useful.

Filesystem fetch folding is an example of a capability request that can invoke
a registered transform after trusted file access. It does not make arbitrary
host code callable from a guest.

## Kind ownership

Put a kind next to the actor/capability that owns the contract. Capability
kinds belong in that capability's own crate, `aether-<capability>/src/kinds.rs`. Component kinds
belong in the component's public `rlib` surface. Promote vocabulary to
`aether-kinds` only when it is genuinely substrate-wide or is an explicit
upstream bridge.

Before adding a kind, check whether the existing contract already represents
the operation as a variant or typed route. Kind proliferation increases schema,
descriptor, MCP, fixture, and compatibility surface.

## MCP is an adapter, not a second engine API

Add an MCP tool when an operator needs a task-shaped operation, bounded
projection, or evidence that is awkward to express as raw `send_mail`. The tool
should still route through the same capability and mail contracts used by other
clients.

Do not put product semantics only in `aether-mcp`. A headless client, substrate harness,
or future UI should be able to exercise the underlying engine operation without
pretending to be an MCP client.

## Chassis and distribution choices

A capability module makes code available; a chassis installs a runtime actor;
a bundle chooses a deployable composition. If a feature is optional by process
profile, answer separately:

1. Are its marker/kind types available to guest code?
2. Is its native runtime linked by this binary?
3. Does the chassis register the actor or an explicit fallback?
4. Does configuration enable the resource successfully?

This prevents “the crate has a module” from being confused with “every engine
has this mailbox.”

## Change checklist

Before implementation:

1. Identify the owner of state and external resources.
2. Identify the public mail/schema boundary.
3. List affected chassis and feature sets.
4. Decide whether hot replacement or replication applies.
5. Find the accepted ADR that owns the invariant; draft one if the decision is
   load-bearing and new.
6. Choose a test level that crosses the changed boundary.
7. Decide whether MCP/docs need an adapter or only live discovery.

Then follow the focused guide:

- [Writing guest code](../writing-guest-code.md)
- [Capability module anatomy](../capability-anatomy.md)
- [Wiring an MCP tool](../recipes/wiring-an-mcp-tool.md)
- [Configuration](../systems/configuration.md)
- [Architecture decisions](../contributing/architecture-decisions.md)
