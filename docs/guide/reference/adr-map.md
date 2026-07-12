# ADR map by topic

The ADR directory is chronological; this page is a reading map. Always open the
record and read its status/amendments. A number in source comments is a route to
rationale, not proof that every paragraph remains current.

## How to interpret a record

| Status | Meaning |
|---|---|
| Proposed | design under consideration; not current policy by status alone |
| Accepted | intended architecture unless amended or superseded |
| Superseded | historical context; follow the named replacement |

Implementation is separate. Code may realize part of a Proposed record, and old
types may remain after an architecture was superseded. Verify both facts.

Search by number/title/status:

```sh
rg -n '^# ADR-|^[-*] \*\*Status:' docs/adr
rg -l 'HTTP server|settlement|component' docs/adr
```

## Core actor, mail, and wire model

| Topic | Start with | Continue with |
|---|---|---|
| Mail-only engine boundary | ADR-0002 | ADR-0013, ADR-0017 |
| Typed identity/schema | ADR-0003, ADR-0004 | ADR-0064, ADR-0118 |
| Components and wasm hosting | ADR-0022 (**superseded by ADR-0038**), ADR-0033 | ADR-0096, ADR-0099 |
| Instanced actors | ADR-0079 | ADR-0114, ADR-0138 |
| Reply classes | ADR-0112 | ADR-0134 |
| Behavior scripting | ADR-0137 | current `aether-behavior` code |
| Capability-owned kinds | ADR-0121 | ADR-0122 marker/runtime split |

## Scheduling, lifecycle, and evidence

| Topic | Records |
|---|---|
| Causal tracing and settlement | ADR-0080, ADR-0106 |
| Lifecycle actor and stages | ADR-0082 |
| Blocking dispatch with holds | ADR-0093 |
| Scheduler/runtime concurrency | ADR-0087, then its in-record shipped-state corrections |
| Per-actor logging | ADR-0077, ADR-0081 |
| Per-handler execution cost | ADR-0036's supersession note, then current `actor_cost` source |
| Live name/kind/handler inventory | ADR-0088, ADR-0091, ADR-0109 |

## Chassis, fleet, and tooling

| Topic | Records |
|---|---|
| Chassis composition | ADR-0070, ADR-0071; ADR-0034 and ADR-0073 for hub/consolidation history |
| Hub/RPC control | ADR-0072 (amended), ADR-0074, ADR-0118 |
| Stable MCP tunnel | ADR-0089 |
| Layered configuration | ADR-0090 |
| Binary/component artifact stores | ADR-0115, ADR-0116 |
| Autonomous pipeline | ADR-0146 (**Proposed**) |
| Release branches | ADR-0092 (**Proposed; not current policy**) |

## Platform I/O

| Topic | Records |
|---|---|
| Filesystem namespaces | ADR-0041 |
| HTTP egress | ADR-0043 |
| TCP instancing | ADR-0079; wire updated by ADR-0118 |
| HTTP server baseline | ADR-0108 |
| HTTP streaming | ADR-0128 |
| Websocket and stream ids | ADR-0129, amended by ADR-0132 and ADR-0133 |
| Route registration/typed routes | ADR-0130, ADR-0131 |
| Explicit handler classes | ADR-0134 |
| HTTP sharding/shared target sets | ADR-0135, ADR-0136 |

Read the HTTP sequence in order: later records deliberately amend addressing
and topology while preserving earlier public concepts.

## Rendering, media, and authoring

| Topic | Records |
|---|---|
| Rendering/capture foundations | ADRs cited from [Rendering](../systems/rendering.md) |
| Text capability | ADR-0105 |
| Mesh DSL and meshing | ADR-0026, ADR-0051–ADR-0053, ADR-0056–ADR-0057, ADR-0062 |
| Audio baseline/scheduling/samples | ADR-0039, ADR-0103, ADR-0104, ADR-0126, ADR-0127 |
| Widgets/composition/editor | ADR-0117, ADR-0140, ADR-0141 |
| World/terrain workbench | ADR-0140–ADR-0143 |

The old DAG and handle-store designs (ADRs 0045, 0047, 0049) are superseded.
Residual id types are not evidence that those native subsystems still ship.

## Content and gameplay

| Topic | Records |
|---|---|
| Provider content generation | ADR-0050, interpreted against shipped provider code |
| Tick-native simulation | ADR-0144 (Accepted) |
| Player sessions over TCP | ADR-0145 (**Proposed**, partially realized in code) |

ADR-0050 contains deferred/historical provider discussion. The current
`anthropic`, `gemini`, and shared content-generation modules define the shipped
set.

## Testing and performance

| Topic | Records |
|---|---|
| Test-bench chassis | ADR-0067 |
| Performance comparison posture | ADR-0085 |
| Component replacement fixtures | follow ADRs 0113, 0114, 0138 and fixture docs |

## When to write a new ADR

Write one when a change fixes a durable boundary, invariant, compatibility
policy, trust model, or rejected alternative that future contributors would
otherwise re-litigate. Do not write one for a local refactor whose rationale is
fully captured by code/tests.

Use `docs/adr/TEMPLATE.md`, allocate the next number without renumbering, link
records it amends/supersedes, and keep status honest. The
[architecture-decision workflow](../contributing/architecture-decisions.md)
explains the repository process.
