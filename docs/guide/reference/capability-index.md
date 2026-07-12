# Capability and service index

This is a routing index, not a frozen chassis manifest. Use
`describe_handlers` and `describe_kinds` to confirm the selected live engine.
Marker types can exist even when a chassis installs only an unsupported
fallback or omits the runtime.

## Native capability families

| Mailbox / family | Responsibility | Public source | Guide |
|---|---|---|---|
| `aether.component` | load, replace, drop, describe wasm actor instances | `aether-capabilities/src/component` | [Components](../systems/components.md) |
| `aether.lifecycle` | frame stages, subscriptions, advance, shutdown | `aether-capabilities/src/lifecycle` | [Frame lifecycle](../systems/lifecycle.md) |
| `aether.render` | draw queues, textures/materials, view/projection, capture | `aether-capabilities/src/render` | [Rendering](../systems/rendering.md) |
| `aether.text` | font load, layout, batched text drawing, metrics | `aether-capabilities/src/text` | [Text](../systems/text.md) |
| `aether.audio` | instruments, notes, tracks, scheduling, gain/reverb | `aether-capabilities/src/audio` | [Audio](../systems/audio.md) |
| `aether.input` | subscription ownership for keyboard/mouse/text/IME streams | `aether-capabilities/src/input` | [Input](../systems/input.md) |
| `aether.window` | mode, title, focus and headless replies | `aether-capabilities/src/window` | [Window](../systems/window.md) |
| `aether.clipboard` | text get/set through system or in-memory backend | `aether-capabilities/src/clipboard` | [Clipboard](../systems/clipboard.md) |
| `aether.fs` | namespaced read/write/copy/delete/list/fetch-fold | `aether-capabilities/src/fs` | [File I/O](../systems/file-io.md) |
| `aether.http` | outbound HTTP fetch | `aether-capabilities/src/http/client` | [HTTP egress](../systems/http.md) |
| `aether.http.server` | inbound HTTP, routes, streams, websocket | `aether-capabilities/src/http/server` | [HTTP server](../systems/http-server.md) |
| `aether.tcp` | listener/connect control and session actors | `aether-capabilities/src/tcp` | [TCP](../systems/tcp.md) |
| `aether.rpc.server` | framed internal process RPC | `aether-capabilities/src/rpc` | [RPC](../systems/rpc.md) |
| `aether.inventory` | live names, kinds, handlers, transforms | `aether-capabilities/src/inventory` | [Inventory](../systems/inventory-and-transforms.md) |
| `aether.trace` | causal-tree and settlement evidence | `aether-capabilities/src/trace` | [Tracing](../systems/tracing-and-settlement.md) |
| `aether.engine` | hub fleet and artifact control | `aether-capabilities/src/engine` | [Engine fleet](../operating/engine-fleet.md) |
| `aether.anthropic` | Messages API and CLI text generation | `aether-capabilities/src/anthropic` | [Content generation](../systems/content-generation.md) |
| `aether.gemini` | image and music generation | `aether-capabilities/src/gemini` | [Content generation](../systems/content-generation.md) |
| `aether.game.gateway` | trusted player/session-to-sim binding | `aether-capabilities/src/game` | [Player sessions](../systems/player-sessions.md) |
| `aether.test_bench` | deterministic test-chassis advance/control | `aether-capabilities/src/test_bench` | [TestBench](../testing/testbench-and-fleetbench.md) |

Instanced families such as `aether.tcp.listener`, `aether.tcp.session`,
`aether.engine.proxy`, `aether.http.server.shard`, and guest trampolines gain
lineage suffixes. Resolve them from results/inventory rather than constructing
ids by hand.

## Shared/substrate kind families

Some contracts remain in `aether-kinds` because they are consumed above the
capability crate boundary or are genuinely cross-cutting:

| Family | Why it is shared |
|---|---|
| engine/component control | MCP and hub control need the same selectors/results |
| inventory queries | `aether-mcp` must query without depending on native capability implementation |
| lifecycle/input/window control bridges | substrate/chassis-wide stage or compatibility vocabulary |
| trace/log/cost tails | common evidence projected across processes |
| utility/diagnostics | ping/pong, unresolved-mail and monitor notices |

Capability-local public kinds belong next to their capability under
`aether-capabilities/src/<cap>/kinds.rs` (ADR-0121). Check ownership before
adding to the shared crate.

## Guest/product systems

| Family | Actor layer | Guide |
|---|---|---|
| `aether.kit.camera*` | camera/controller actors | [Rendering and camera](../systems/rendering.md) |
| `aether.kit.widget*` | widgets, focus, scrolling, panel/editor composition | [Widgets](../systems/widgets.md) |
| `aether.kit.workbench*` | editor viewport/panels | [World and terrain](../systems/world-and-terrain.md) |
| `aether.kit.world*`, `terra`, `mark` | terrain data, proposals, overlay/picking | [World and terrain](../systems/world-and-terrain.md) |
| `aether.kit.mesh*` | DSL/OBJ loading and display | [Mesh authoring](../systems/mesh-authoring.md) |
| `aether.sim*` | tick-native intent/fact reference sim | [Player sessions](../systems/player-sessions.md) |
| `aether.kit.client*` | reference player client | [Player sessions](../systems/player-sessions.md) |
| `aether.behavior*` | behavior host config and live script swap | [Behaviors](../systems/behaviors.md) |

These are hosted actor APIs. Their presence depends on which component export
is built and loaded, not merely the native chassis.

## Operator discovery

| Question | Tool |
|---|---|
| Which engines are alive/dead? | `list_engines` |
| Which component artifacts are stored? | `list_components` |
| What does a live component lineage expose? | `load_component` result, then `describe_component` by lineage name |
| Which kinds and schemas exist here? | `describe_kinds` |
| What native handlers/replies exist? | `describe_handlers` |
| What can this loaded component receive? | `describe_component` |
| Which native transforms are linked into this MCP build? | `describe_transforms` |
| What did one actor log? | `actor_logs` |
| Which handlers are expensive? | `actor_cost` |

The live tool schema is authoritative for arguments. The [operating chapter](../operating/index.md)
explains ownership and bounded use.
