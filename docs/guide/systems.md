# Subsystem map

This page routes a task to the system that owns it. The map reflects current
source families, not only the original engine core.

## Core runtime

| System | Owns |
|---|---|
| [Mail and kinds](systems/mail-and-kinds.md) | addressing, payload identity, delivery and replies |
| [Concurrency](systems/concurrency.md) | actor seriality and safe blocking boundaries |
| [Scheduler](systems/scheduler.md) | blobs, ready work, worker policy and cost-aware dispatch |
| [Frame lifecycle](systems/lifecycle.md) | ordered stages, subscriptions, advance and shutdown |
| [Tracing and settlement](systems/tracing-and-settlement.md) | causal trees, holds and exact completion |
| [Logging](systems/logging.md) | per-actor rings and out-of-actor diagnostics |
| [Configuration](systems/configuration.md) | argv/env/file layering and resolved dumps |

Start with [Core runtime systems](systems/core-runtime.md) before editing a
scheduler or settlement invariant.

## Hosted code and discovery

For the hosted-code overview and replacement boundary, start with
[Hosted code and live replacement](systems/hosted-code.md).

| System | Owns |
|---|---|
| [Components](systems/components.md) | wasm load/drop/replace, exports, config and state transfer |
| [Behaviors](systems/behaviors.md) | fail-open in-cluster script interposition |
| [Inventory and transforms](systems/inventory-and-transforms.md) | live names, kind schemas, native handlers, value transforms |

The hub's stored artifact and an engine's loaded instance are different things.
The [operating chapter](operating/component-registry.md) covers that lifecycle.

## Platform and network I/O

| System | Owns |
|---|---|
| [File I/O](systems/file-io.md) | namespaced host files and transform-folded fetch |
| [HTTP egress](systems/http.md) | bounded outbound requests |
| [HTTP server](systems/http-server.md) | inbound routes, streaming and websockets |
| [TCP](systems/tcp.md) | framed listeners and session actors |
| [RPC](systems/rpc.md) | hub/engine process transport |
| [Clipboard](systems/clipboard.md) | text clipboard with deterministic/headless backends |
| [Content generation](systems/content-generation.md) | Anthropic/Gemini provider queues, the `aether.process` CLI edge, and staged media |
| [Player sessions](systems/player-sessions.md) | trusted session/gateway tier over TCP and tick-native sim |

These all cross trust or blocking boundaries. Read [Platform and network I/O](systems/platform-io.md)
for the common rules.

## Media, interaction, and product tools

For the media and product-tools overview, start with
[Media, interaction, and product tools](systems/media-and-tools.md).

| System | Owns |
|---|---|
| [Rendering and camera](systems/rendering.md) | GPU draw queues, textures, materials, capture and matrices |
| [Render programs](systems/render-programs.md) | authored GPU programs, bindings, transients and passes |
| [Text](systems/text.md) | font atlas, layout, batches and metrics |
| [Mesh authoring](systems/mesh-authoring.md) | DSL, parser, tessellation and viewer load |
| [Audio](systems/audio.md) | realtime events, scheduling, instruments, tracks and effects |
| [Input](systems/input.md) | key, pointer, text, IME and subscription streams |
| [Window](systems/window.md) | mode, title, focus and unsupported replies |
| [Widgets](systems/widgets.md) | controls, focus, scroll, panel/editor composition |
| [World and terrain](systems/world-and-terrain.md) | chunk data, proposals, picking, mesh and workbench |
| [Puppet controls](systems/puppet.md) | articulated character pose, gaze, expression and turntable control |

Native capabilities own devices; kit actors compose them into product behavior.

## Fleet and operation

The hub/engine control plane spans [process topology](architecture/process-topology.md),
[engine fleet and stores](operating/engine-fleet.md), and the
[MCP harness](mcp-harness.md). It is separated from engine-local systems because
spawn/upload/selector semantics live outside a child registry.

## Find the current contract

Use the [capability index](reference/capability-index.md) for mailbox/source
routing and engine-scoped `describe_kinds`, `describe_handlers`, and
`describe_component` queries. The guide explains meaning; code and live schemas
answer exact signatures.
