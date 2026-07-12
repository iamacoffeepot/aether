# Introduction

Aether is a pre-1.0 application engine built for games, tools, and other
interactive systems. A thin native substrate hosts wasm and native actors;
actors communicate by typed mail. An external operator can start engines, load
and replace code, send mail, inspect live contracts, capture frames, and gather
evidence without linking into the runtime.

The operator is often an agent, but the architecture is not a private dialect
for one model or harness. The same explicit, discoverable surfaces should make
sense to a human contributor, a test harness, a game client, and different
agent runtimes.

## What is unusual

Three choices explain most of the system:

1. **Everything important crosses a mail boundary.** A filesystem read, draw
   request, component load, audio note, and game intent are addressed typed
   messages rather than hidden host calls.
2. **Native services and wasm code share the actor model.** The substrate owns
   privileged resources; application/product logic stays replaceable and
   observable above it.
3. **Operation is out of process.** MCP and the hub make a running engine
   inspectable and mutable while preserving process isolation and restartability.

Read [Why Aether is shaped this way](philosophy.md) for the design pressure and
[Architecture overview](architecture.md) for the map.

## Start by task, not chronology

| Goal | Route |
|---|---|
| Run and inspect an engine | [First live-engine session](orientation/first-engine-session.md) |
| Understand the repository | [Repository map](orientation/repository-map.md) |
| Learn actors, kinds, and settlement | [Foundations](foundations/actor-model.md) then [core runtime](systems/core-runtime.md) |
| Write guest code | [Writing guest code](writing-guest-code.md) |
| Add a native or operator surface | [Choose an extension point](building/extension-points.md) |
| Find a subsystem | [Subsystem map](systems.md) |
| Diagnose a live failure | [Inspection and debugging](operating/inspect-and-debug.md) |
| Contribute planned work | [Agent and contributor workflow](contributing/agent-workflow.md) |

[How to use this guide](orientation/using-the-guide.md) explains source
authority, maturity labels, and how the chapters fit together.

## The documentation layers

No single prose file is authoritative for every question:

- **Current code and tests** define implemented behavior.
- **Accepted ADRs** preserve architectural intent and rejected alternatives.
- **The active surface contract** defines repository workflow: Codex uses
  `AGENTS.md` and `.agents/skills/`; Claude/headless surfaces use their own
  checked-in contracts.
- **The live MCP schema and engine inventory** define current tool arguments and
  one engine's actual kinds/handlers.
- **This guide** connects those facts into concepts, task paths, and change
  routes.

Proposed ADRs are proposals even when partial code exists. Superseded ADRs are
history even when an old type name remains. The [ADR map](reference/adr-map.md)
teaches that distinction.

## Written for agents, readable by people

Agent-first documentation needs bounded pages, explicit prerequisites, exact
source routes, failure models, and honest uncertainty. Human-readable
documentation needs motivation and a coherent narrative. This book aims for
both: prose explains why a boundary exists; tables and checklists make the next
action easy to locate.

System pages avoid duplicating volatile function signatures. Use them to find
the owner and invariant, then verify the exact type/tool against source or live
introspection.

## Tutorials are API pressure

A callable surface should support a short, truthful path from intent to result.
If a recipe requires private archaeology, fake parameters, or unexplained
special cases, either the recipe is stale or the API is too difficult to use.
Fixing documentation is therefore part of API review, not polish after the
design freezes.

The [recipes](recipes.md) are worked examples. The system chapters remain the
conceptual contract, and current code remains the implementation authority.
