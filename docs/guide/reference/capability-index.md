# Capability and service index

This is a routing index, not a frozen chassis manifest. Use
`describe_handlers` and `describe_kinds` to confirm the selected live engine.
Marker types can exist even when a chassis installs only an unsupported
fallback or omits the runtime.

## Native capability families

| Mailbox / family | Responsibility | Public source | Guide |
|---|---|---|---|
| `aether.component` | load, replace, drop, describe wasm actor instances | `aether-component/src/component` | [Components](../systems/components.md) |
| `aether.lifecycle` | frame stages, subscriptions, advance, shutdown | `aether-lifecycle/src` | [Frame lifecycle](../systems/lifecycle.md) |
| `aether.render` | draw queues, textures/materials, view/projection, capture | `aether-render/src` | [Rendering](../systems/rendering.md) |
| `aether.text` | font load, layout, batched text drawing, metrics | `aether-text/src` | [Text](../systems/text.md) |
| `aether.audio` | instruments, notes, tracks, scheduling, gain/reverb | `aether-audio/src` | [Audio](../systems/audio.md) |
| `aether.window` | multi-window lifecycle/control plus selector-aware keyboard, mouse, resize, text, and IME publication | `aether-window/src` | [Window](../systems/window.md), [Input](../systems/input.md) |
| `aether.clipboard` | text get/set through system or in-memory backend | `aether-clipboard/src` | [Clipboard](../systems/clipboard.md) |
| `aether.fs` | namespaced read/write/copy/delete/list/fetch-fold | `aether-fs/src` | [File I/O](../systems/file-io.md) |
| `aether.http` | outbound HTTP fetch | `aether-http/src/client` | [HTTP egress](../systems/http.md) |
| `aether.http.server` | inbound HTTP, routes, streams, websocket | `aether-http/src/server` | [HTTP server](../systems/http-server.md) |
| `aether.tcp` | listener/connect control and session actors | `aether-tcp/src` | [TCP](../systems/tcp.md) |
| `aether.rpc.server` | framed internal process RPC | `aether-rpc/src` | [RPC](../systems/rpc.md) |
| `aether.process` | allowlisted one-shot subprocess execution with captured output, timeout, and reap discipline | `aether-process/src` | [Content generation](../systems/content-generation.md) |
| `aether.inventory` | live names, kinds, handlers, transforms | `aether-inventory/src` | [Inventory](../systems/inventory-and-transforms.md) |
| `aether.trace` | causal-tree and settlement evidence | `aether-trace/src` | [Tracing](../systems/tracing-and-settlement.md) |
| `aether.fleet` | hub fleet and artifact control | `aether-fleet/src` | [Engine fleet](../operating/engine-fleet.md) |
| `aether.substrate_harness` | deterministic test-chassis advance/control | `aether-substrate-harness-cap/src` | [SubstrateHarness](../testing/substrateharness-and-fleetharness.md) |

Instanced families such as `aether.tcp.listener`, `aether.tcp.session`,
`aether.fleet.proxy`, `aether.http.server.shard`, and guest trampolines gain
lineage suffixes. Resolve them from results/inventory rather than constructing
ids by hand.

`aether.process` is grounded in Accepted ADR-0157. Its marker and kinds can be
linked without the native runtime, and a configured runtime is deny-by-default;
neither source presence nor this index proves that a selected chassis permits a
requested binary.

## Loadable provider components

Content-generation providers are wasm guest components a substrate loads on
demand rather than native chassis capabilities (ADR-0159). The default
composition carries none; a workload uploads and loads the one it needs, and
the loaded component answers at `aether.component/aether.embedded:<namespace>`.
No provider component currently ships in the workspace; see
[Content generation](../systems/content-generation.md) for the pattern.

## Shared/substrate kind families

Some contracts remain in `aether-kinds` because they are consumed above the
capability crate boundary or are genuinely cross-cutting:

| Family | Why it is shared |
|---|---|
| engine/component control | MCP and hub control need the same selectors/results |
| inventory queries | `aether-mcp` must query without depending on native capability implementation |
| lifecycle/window control and window-originated events | substrate/chassis-wide stage, identity, or compatibility vocabulary |
| trace/log/cost tails | common evidence projected across processes |
| utility/diagnostics | ping/pong, unresolved-mail and monitor notices |

Capability-local public kinds belong next to their capability, in that
capability's own crate under `aether-<cap>/src/kinds.rs` (ADR-0121). Check
ownership before adding to the shared crate.

## Guest/product systems

| Family | Actor layer | Guide |
|---|---|---|
| `aether.kit.camera*` | camera/controller actors | [Rendering and camera](../systems/rendering.md) |
| `aether.kit.mesh*` | DSL/OBJ loading and display | [Mesh authoring](../systems/mesh-authoring.md) |
| `aether.widget*` | widgets, focus, scrolling, panel/editor composition | [Widgets](../systems/widgets.md) |
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
