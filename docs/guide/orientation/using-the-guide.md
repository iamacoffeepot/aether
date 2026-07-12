# How to use this guide

Aether is easiest to understand from the outside in. Start with the operation
you need to perform, learn the contract at that boundary, and only then descend
into the runtime that implements it. The book is arranged to support that
order.

## Pick a path

| If you are trying to… | Start here | Then read |
|---|---|---|
| Drive a running engine | [First engine session](first-engine-session.md) | [Operating an engine](../operating/index.md) |
| Write a wasm actor | [Writing guest code](../writing-guest-code.md) | [Components](../systems/components.md) or [behaviors](../systems/behaviors.md) |
| Add a native service | [Choose an extension point](../building/extension-points.md) | [Capability anatomy](../capability-anatomy.md) |
| Change mail or scheduling | [Actor model](../foundations/actor-model.md) | [Mail](../systems/mail-and-kinds.md), [concurrency](../systems/concurrency.md), and [scheduler](../systems/scheduler.md) |
| Change a public capability | [Subsystem map](../systems.md) | The capability page and its owning ADRs |
| Work on the repository | [Repository map](repository-map.md) | [Agent and contributor workflow](../contributing/agent-workflow.md) |
| Diagnose a failure | [Inspection and debugging](../operating/inspect-and-debug.md) | [Recovery runbook](../operating/recovery.md) |

The foundational chapters explain durable concepts. System chapters explain
the public contract and route you to implementation. Recipes are worked
examples, not the canonical definition of an API. Runtime-internals pages such
as the scheduler are useful when changing the engine, but they are not required
to send mail or author an ordinary component.

## Match authority to the question

Documentation is a map of a moving pre-1.0 codebase. No universal ranking can
decide every disagreement because the sources own different questions:

- **The current user or repository owner request** owns intent and permission.
- **Current code and tests** own what the checked-out revision implements.
- **Accepted ADRs and their amendment chain** own architectural rationale. A
  Proposed ADR is a proposal, not proof that its design ships; a Superseded ADR
  is history.
- **The active surface contract** owns how an agent works in the repository.
  Codex reads `AGENTS.md`, `.agents/skills/`, and the live tool schema. Claude
  surfaces use `CLAUDE.md` and `.claude/`. Do not translate commands between
  harnesses by syntax alone.
- **Live introspection** owns the state of the selected running engine.
  `describe_kinds`, `describe_handlers`, `describe_component`, and
  bounded probes are better evidence than a static list when a component or
  chassis selection can change the answer. `describe_transforms` is different:
  it reports the static transform set linked into `aether-mcp`, with no engine
  selector.
- **This guide** explains and connects those sources. If it has drifted, fix it
  in the same change as the code when practical.

Classify the disagreement as intent, implementation, rationale, workflow, or
live state before choosing the authority. The full matrix is in
[Sources and live reference](../reference.md).

## Read maturity literally

Aether has implemented code behind some Proposed ADRs and old code described by
some Superseded ADRs. These facts are separate:

- **ADR status** says whether the decision is accepted.
- **Implementation status** says whether code exists today.
- **Chassis availability** says whether a particular binary links and enables
  the capability.
- **Runtime state** says whether a particular engine loaded or configured it.

This guide labels proposals where they matter and avoids presenting source
presence alone as a stable product contract.

## Use links as change routes

System pages end with implementation and decision routes. When making a change,
follow all three:

1. read the public kind/config definitions;
2. read the native or wasm runtime that handles them;
3. read the accepted ADRs that own the invariant.

Search the current tree before trusting an exact path printed in prose:

```sh
rg --files crates docs/adr | rg 'http|component|0138'
rg -n 'struct HttpServerConfig|export!' crates
```

For a behavioral change, also find tests and callers. A type can be public while
the useful contract is enforced in a derive macro, a chassis builder, an MCP
adapter, or an integration fixture.

## Keep operator and engine identities separate

Examples use several identifiers that are not interchangeable:

- an **engine id** selects one substrate supervised by the hub;
- a **mailbox name** selects an actor within that engine;
- a **kind name** selects the message contract;
- a **component selector** selects stored wasm bytes or an exported actor;
- a **binary selector** selects a stored chassis executable.

Use the typed or named surface accepted by the active tool. Do not substitute a
host path where a registry selector is required, or a component name where a
mailbox lineage name is required. The [glossary](../reference/glossary.md) and
[capability index](../reference/capability-index.md) are quick reminders.

## A documentation rule for agents

Do not merely make a page longer when a subsystem grows. Add a focused subpage
when readers need a different prerequisite, task, failure model, or source
route. Keep the parent page as a map. That makes the book usable both as a
human narrative and as bounded context for an agent.
