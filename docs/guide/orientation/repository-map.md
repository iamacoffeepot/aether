# Repository map

The workspace is layered. Most changes should move down this list only as far
as their responsibility requires.

```text
product actors and reusable UI/gameplay pieces       aether-kit-*
first-party development control plane                 aether-bloomery*
native services and their public mail contracts      aether-<capability> crates
guest actor and behavior authoring SDKs               aether-actor, aether-behavior
process profiles, binaries, packaging     aether-chassis, aether-chassis-*
mail runtime, wasm host, scheduler, chassis traits    aether-substrate
wire/schema/identity/math foundations                 aether-data, aether-codec, aether-math
operator bridge                                       aether-mcp
test harnesses                                        aether-harness-*
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
the owning capability's own crate, `aether-<capability>/src/kinds.rs`; do not
put every new native message into `aether-kinds`.

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
| `aether-render`, `aether-text`, `aether-audio` | draw queues and the wgpu pipeline, font layout and the glyph atlas, the synth and instrument banks |
| `aether-fs`, `aether-clipboard`, `aether-window` | namespaced file I/O, text clipboard, multi-window lifecycle/control, and selector-aware window-event subscriptions |
| `aether-http`, `aether-http-derive`, `aether-tcp`, `aether-rpc` | HTTP egress and ingress with its typed route macros, TCP listeners and sessions, framed process RPC |
| `aether-process` | deny-by-default, allowlisted one-shot subprocess execution and captured typed replies (Accepted ADR-0157) |
| `aether-component`, `aether-lifecycle`, `aether-inventory`, `aether-trace` | wasm component hosting and the trampoline, frame stages, live name/kind lookup, causal-tree evidence |
| `aether-fleet` | hub fleet supervision and the content-addressed artifact store |
| `aether-anthropic` | the content-gen provider component (loaded on demand, not a chassis fixture), a self-contained guest carrying its own pure DTO/string helpers |
| `aether-chassis` | shared chassis composition: boot fragments, config registry, CLI roots, autoload, boot-manifest and package-depot formats |
| `aether-chassis-desktop` / `aether-chassis-headless` / `aether-chassis-hub` / `aether-chassis-harness` / `aether-chassis-bloomery` | the five checked-in chassis binaries; Bloomery is the dedicated application profile and can run standalone or through the hub launch path |
| `aether-harness-substrate` | composable in-process substrate harness with deterministic mail, lifecycle, and settlement control |
| `aether-harness-substrate-capture` | opt-in render/GPU capture and visual comparison support layered onto the core substrate harness |
| `aether-harness-fleet` | real-process hub/RPC/headless fleet scenarios over raw framed calls |
| `aether-harness-perf` | performance trial / compare / plot binaries |
| `aether-mcp` | MCP tools, JSON/schema adaptation, hub RPC session, live-name caches |

The substrate is mechanism. A capability is policy and I/O represented as an
actor. A chassis chooses which capabilities and drivers form a process. The hub
supervises engine processes; it is not the engine runtime folded into a tool
server.

## Bloomery application crates

| Crate | Owns |
|---|---|
| `aether-bloomery` | canonical Bloomery values, immutable work/bloom identities, the pure reducer, and control/source contracts |
| `aether-bloomery-github` | GitHub source and outward-projection adapter; GitHub objects shadow Bloomery identities rather than defining them |
| `aether-chassis-bloomery` | the dedicated Bloomery process, native control/store/artifacts/session/source/signing services, REST API, and reactors |

Bloomery is a first-party development control-plane application hosted on
Aether. Its dedicated binary can run standalone or be uploaded, selected, and
forked through the hub's binary/fleet path, while `FleetServer` remains owned by
the hub and generic hub/headless chassis do not become build servers. ADR-0149
still has **Proposed** status despite the substantial implementation in these
crates, so code realization must not be mistaken for an accepted architecture
decision.

## Product and geometry crates

| Crate | Owns |
|---|---|
| `aether-kit-commons` | common standalone reference actors: camera + camera-controller, console overlay, mesh viewer |
| `aether-kit-widget` | reusable widget set and the `EditorShell` composition arbiter |
| `aether-mesh` | mesh DSL, parsing/serialization, cleanup, polygon tessellation, surface nets, shared eye-facing stroke ribbon geometry |
| `aether-puppet` | the wasm-hosted mascot actor: mesh-derived pen-plotter line art, authored face controls, rigging, and render mail |

These crates are valuable examples, but “in tree” does not mean “native.” The
`aether-kit-*` crates are actor code hosted by the same component machinery
available to other guest modules.

## Derive, fixture, and tooling crates

`aether-http-derive`, `aether-data-derive`, and `aether-derive` hold
code generation shared across the workspace. When a source annotation appears
to do more than its local file explains, inspect its derive implementation and
expanded tests.

Test-only packages are grouped by role rather than maintained here as an
exhaustive crate ledger. `aether-test-fixtures-*` packages provide deliberately
small wasm/native artifacts for replacement, capability-split, boot,
multi-actor, and behavior contracts. `aether-component-ui-tests` is the narrow
trybuild host for component route compile contracts. Derive crates also keep
their compile-pass/fail fixtures beside the macro they exercise. These are
often better executable examples than an old prose snippet.

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
| Add a message to a native capability | that capability's own crate, `aether-<cap>/src/kinds.rs` | runtime handler, descriptors, wasm-facing feature gates |
| Change delivery or settlement | `aether-substrate/src/mail` or `scheduler` | actor contexts, trace/lifecycle tests, ADRs |
| Add an MCP operation | `aether-mcp/src/tools` and `args.rs` | underlying capability kinds and hub RPC behavior |
| Change one-shot subprocess execution | `aether-process` | chassis installation, allowlist/confinement config, settlement behavior |
| Change Bloomery control behavior or projection | `aether-bloomery` or `aether-bloomery-github` | `aether-chassis-bloomery`, Proposed ADR-0149, durable journal/artifact boundaries |
| Change in-process or real-process test support | `aether-harness-substrate`, `aether-harness-substrate-capture`, or `aether-harness-fleet` | the consuming scenario's chassis and artifact requirements |
| Add a reusable guest actor | an `aether-kit-*` crate or a new component crate | `aether-actor`, export/cardinality rules |
| Change a process profile | `aether-chassis-<chassis>` | config layers, linked capabilities, packaging |
| Change a wire shape | owning kind plus `aether-data`/`aether-codec` | compatibility fixtures and any RPC framing |

Start with `rg` across callers and tests. Crate boundaries communicate intent,
but the contract may cross a macro, registry, chassis, and tool adapter.
