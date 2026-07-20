# Repository map

The workspace is layered. Most changes should move down this list only as far
as their responsibility requires.

```text
product actors and reusable UI/gameplay pieces       aether-kit
native services and their public mail contracts      aether-capabilities
guest actor and behavior authoring SDKs               aether-actor, aether-behavior
process profiles, binaries, test bench, packaging     aether-substrate-bundle
mail runtime, wasm host, scheduler, chassis traits    aether-substrate
wire/schema/identity/math foundations                 aether-data, aether-codec, aether-math
operator bridge                                       aether-mcp
procedural macros                                     *-derive crates
```

## Foundation crates

| Crate | Owns | Reach for it when… |
|---|---|---|
| `aether-data` | typed ids, schemas, canonical kind identity, wire-facing data traits | a value must cross an actor or process boundary |
| `aether-codec` | schema-driven JSON/wire conversion and framed streams | translating public values or carrying frames over a stream |
| `aether-math` | vectors, matrices, quaternions, bounds | sharing math between native and wasm code |
| `aether-kinds` | cross-cutting substrate vocabulary and shared descriptors | the kind is genuinely substrate-wide; capability-local kinds belong with their capability |

The capability-local ownership rule matters. Render, audio, filesystem, HTTP,
and other capability messages live under
`aether-capabilities/src/<capability>/kinds.rs`; do not put every new native
message into `aether-kinds`.

## Actor authoring crates

| Crate | Owns |
|---|---|
| `aether-actor` | `Actor`/`WasmActor`, typed mailboxes, contexts, request/reply correlation, wasm exports |
| `aether-actor-derive` | actor and handler code generation |
| `aether-behavior` | compact behavior ABI, filter envelope, verdicts and effects |
| `aether-behavior-derive` | behavior authoring macros |

Use a component when code needs actor state, typed handlers, replies, or a
first-class mailbox. Use a behavior when a small replaceable filter over mail
is the right boundary. The [extension-point guide](../building/extension-points.md)
compares these with native capabilities.

## Runtime and process crates

| Crate | Owns |
|---|---|
| `aether-substrate` | registry, rings, dispatch, scheduler, native/wasm actor hosts, settlement, chassis traits |
| `aether-capabilities` | native actors for render, audio, HTTP, filesystem, lifecycle, component hosting, engine fleet, and other chassis services |
| `aether-substrate-bundle` | desktop, headless, hub, and test-bench chassis; autoload; bundle packing; performance binaries |
| `aether-mcp` | MCP tools, JSON/schema adaptation, hub RPC session, live-name caches |

The substrate is mechanism. A capability is policy and I/O represented as an
actor. A chassis chooses which capabilities and drivers form a process. The hub
supervises engine processes; it is not the engine runtime folded into a tool
server.

## Product and geometry crates

| Crate | Owns |
|---|---|
| `aether-kit` | reference/product actors: camera, widgets, workbench, world/terrain, console, movement, simulation client pieces |
| `aether-mesh` | mesh DSL, parsing/serialization, cleanup, polygon tessellation, surface nets |

These crates are valuable examples, but “in tree” does not mean “native.”
`aether-kit` is actor code hosted by the same component machinery available to
other guest modules.

## Derive, fixture, and tooling crates

`aether-http-derive`, `aether-data-derive`, and `aether-derive` hold
code generation shared across the workspace. When a source annotation appears
to do more than its local file explains, inspect its derive implementation and
expanded tests.

`aether-test-fixtures-*` crates are deliberately small wasm/native artifacts
for integration contracts: typed and reshaped replacement, split capability
surfaces, defaultless multi-actor modules, and behaviors. They are often a
better executable example than an old prose snippet.

`xtask` owns repository automation such as distribution and bundle assembly.
The standalone `fuzz/` crate is excluded from the stable workspace because it
uses the nightly fuzzing toolchain.

## Other load-bearing directories

| Path | Purpose |
|---|---|
| `docs/adr/` | numbered architecture decisions and their status |
| `docs/guide/` | this mdBook source |
| `.agents/skills/` | current Codex repository workflows |
| `.codex/` | Codex MCP configuration and local guardrail hooks |
| `.github/workflows/` | hosted CI, review, dogfood, reconciliation, and release jobs |
| `scripts/` | developer/operator helpers, including the MCP tunnel |
| `fuzz/` | isolated nightly fuzz targets |

## Route a change before editing

| Change | Likely starting point | Also inspect |
|---|---|---|
| Add a message to a native capability | `aether-capabilities/src/<cap>/kinds.rs` | runtime handler, descriptors, wasm-facing feature gates |
| Change delivery or settlement | `aether-substrate/src/mail` or `scheduler` | actor contexts, trace/lifecycle tests, ADRs |
| Add an MCP operation | `aether-mcp/src/tools` and `args.rs` | underlying capability kinds and hub RPC behavior |
| Add a reusable guest actor | `aether-kit` or a new component crate | `aether-actor`, export/cardinality rules |
| Change a process profile | `aether-substrate-bundle/src/<chassis>` | config layers, linked capabilities, packaging |
| Change a wire shape | owning kind plus `aether-data`/`aether-codec` | compatibility fixtures and any RPC framing |

Start with `rg` across callers and tests. Crate boundaries communicate intent,
but the contract may cross a macro, registry, chassis, and tool adapter.
