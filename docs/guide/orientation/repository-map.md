# Repository map

The workspace is layered. Most changes should move down this list only as far
as their responsibility requires.

```text
wire/schema/identity/math foundations                 aether-data, aether-codec, aether-math
mail runtime, wasm host, scheduler, chassis traits    aether-substrate
native services and their public mail contracts      aether-<capability> crates
guest actor and behavior authoring SDKs               aether-actor, aether-behavior
process profiles, binaries, packaging     aether-chassis, aether-chassis-*
operator bridge                                       aether-mcp
test harnesses                                        aether-harness-*
procedural macros                                     *-derive crates
reusable guest actors shipped with the engine        aether-kit, aether-widget
journal-driven programs and reactors (bloomery)      aether-bloomery-*
```

The list is also the order to read it in. Everything above `aether-kit` and
`aether-widget` is the engine; the remaining rows hold consumers that happen
to live in the same workspace.

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
| `aether-chassis` | shared chassis composition: boot fragments, config registry, CLI roots, autoload, boot-manifest and package-depot formats |
| `aether-chassis-desktop` / `aether-chassis-headless` / `aether-chassis-hub` / `aether-chassis-harness` / `aether-chassis-bloomery` | the five checked-in chassis binaries |
| `aether-substrate-harness-cap` | the `aether.substrate_harness` mailbox: the harness-chassis drive and the fail-fast stub every other chassis composes |
| `aether-harness-substrate` | composable in-process substrate harness with deterministic mail, lifecycle, and settlement control |
| `aether-harness-substrate-capture` | opt-in render/GPU capture and visual comparison support layered onto the core substrate harness |
| `aether-harness-fleet` | real-process hub/RPC/headless fleet scenarios over raw framed calls |
| `aether-harness-bloomery` | in-process journal-content scenarios over the shipped bloomery chassis: a seeded journal in, the appended records asserted against literals |
| `aether-harness-perf` | the `aether-perf-trial` / `-compare` / `-plot` / `-registry` binaries |
| `aether-mcp` | MCP tools, JSON/schema adaptation, hub RPC session, live-name caches; also carries the `aether-tunnel` binary |

The substrate is mechanism. A capability is policy and I/O represented as an
actor. A chassis chooses which capabilities and drivers form a process. The hub
supervises engine processes; it is not the engine runtime folded into a tool
server.

## Product and geometry crates

| Crate | Owns |
|---|---|
| `aether-kit` | common standalone reference actors: camera + camera-controller, mesh viewer |
| `aether-widget` | reusable widget set and the `EditorShell` composition arbiter |
| `aether-mesh` | mesh DSL, parsing/serialization, cleanup, polygon tessellation, surface nets, shared eye-facing stroke ribbon geometry |
| `aether-demo` | the release demo: its bring-up component (`aether.demo`, which sends the kit mesh viewer its load at boot), depot spec, boot manifest, and controller config |

These crates are valuable examples, but “in tree” does not mean “native.” The
`aether-kit` and `aether-widget` crates are actor code hosted by the same
component machinery available to other guest modules.

## Bloomery crates

The bloomery is a journal-driven engine built in this workspace: an
append-only journal of typed events drives stateless wasm programs and
journal-following reactors. `aether-chassis-bloomery` mounts the journal and
the bundle driver on the `aether-bloomery` binary.

| Crate | Owns |
|---|---|
| `aether-bloomery-kinds` | the shared `no_std` vocabulary: digests, typed citations, the tree, programs, heads, driver and reactor records and mail |
| `aether-bloomery-journal` | the append-only, single-writer journal root: a `SQLite` log of typed events plus one digest-named blob file per content-addressed artifact, held under an exclusive lock (ADR-0220) |
| `aether-bloomery-view` | folds over a journal prefix: the typed `Heads` last-move fold and the ADR-0226 request and activation folds |
| `aether-bloomery-program`, `aether-bloomery-program-derive` | the guest SDK for stateless wasm programs (`Program`, `Env`, invoke mail) and its `#[program]` macro |
| `aether-bloomery-reactor`, `aether-bloomery-reactor-derive` | reactor preparation and pure evaluation of typed stored-event arms, and the `#[reactor]` / `#[rule]` macros (ADR-0222) |
| `aether-bloomery-bundle`, `aether-bloomery-bundle-derive` | the `bundle` export generator: one root for a module's programs and reactors |
| `aether-bloomery-driver` | the sans-io driver core: journal folds in, driver commands out, for both programs and reactors (ADR-0226) |
| `aether-bloomery-muse` | the `muse.turn` Sampled program: one stateless responses-API turn per run (ADR-0234) |
| `aether-bloomery-workspace-programs` | the bundle of workspace programs: `environment.merge`, the Pure program that places the imported toolchain directory in the imported base userland and declares the `Environment` it makes, and `proof.clippy`, the Sampled program that runs clippy over a source tree in that environment through the workspace and cites the step's stderr in its `Passed` or `Failed` result, and `vendor.cargo`, the Sampled program that runs `cargo vendor --locked` over a source tree with the network on and cites the vendor tree `proof.clippy` mounts (ADR-0237) |
| `aether-workspace` | ADR-0237's run, environment, and import kinds, valid by construction, and the `aether.workspace` actor that answers them: a private Docker Engine API client, over a Unix socket or TCP with mutual TLS, that answers `Import` (a digest-pinned image into the journal as a tree) and `Run` (steps over a stored tree in a stored environment) |

## Derive, fixture, and tooling crates

`aether-http-derive`, `aether-data-derive`, and `aether-derive` hold
code generation shared across the workspace. When a source annotation appears
to do more than its local file explains, inspect its derive implementation and
expanded tests.

Test-only packages are grouped by role rather than maintained here as an
exhaustive crate ledger. `aether-test-fixtures-*` packages provide deliberately
small wasm/native artifacts for replacement, capability-split, boot,
multi-actor, and behavior contracts. `aether-subscribe-ui-tests` is the narrow
trybuild host for the publisher gate on the lifecycle and window subscribe
surfaces. Derive crates also keep
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
| `.claude/skills/` | the Claude Code workflows |
| `.codex/` | Codex MCP configuration and hook wiring |
| `.hooks/` | local guardrail hook scripts, wired by `.claude/settings.json` and `.codex/hooks.json` |
| `.github/workflows/` | hosted CI, review, reconciliation, and release jobs |
| `scripts/` | developer/operator helpers, including the MCP tunnel |
| `fuzz/` | isolated nightly fuzz targets |

## Route a change before editing

| Change | Likely starting point | Also inspect |
|---|---|---|
| Add a message to a native capability | that capability's own crate, `aether-<cap>/src/kinds.rs` | runtime handler, descriptors, wasm-facing feature gates |
| Change delivery or settlement | `aether-substrate/src/mail` or `scheduler` | actor contexts, trace/lifecycle tests, ADRs |
| Add an MCP operation | `aether-mcp/src/tools` and `args.rs` | underlying capability kinds and hub RPC behavior |
| Change one-shot subprocess execution | `aether-process` | chassis installation, allowlist/confinement config, settlement behavior |
| Change in-process or real-process test support | `aether-harness-substrate`, `aether-harness-substrate-capture`, `aether-harness-fleet`, or `aether-harness-bloomery` | the consuming scenario's chassis and artifact requirements |
| Add a reusable guest actor | `aether-kit`, `aether-widget`, or a new component crate | `aether-actor`, export/cardinality rules |
| Change a process profile | `aether-chassis-<chassis>` | config layers, linked capabilities, packaging |
| Change a wire shape | owning kind plus `aether-data`/`aether-codec` | compatibility fixtures and any RPC framing |

Start with `rg` across callers and tests. Crate boundaries communicate intent,
but the contract may cross a macro, registry, chassis, and tool adapter.
