# aether

[![CI](https://github.com/iamacoffeepot/aether/actions/workflows/ci.yml/badge.svg)](https://github.com/iamacoffeepot/aether/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

aether is an application engine for games, tools, and interactive systems. A
thin native **substrate** owns the window, the GPU, audio, files, and the
network, and hosts both native and WebAssembly **actors**; actors never call
each other, they only send typed **mail** to a **mailbox**. A **chassis**
picks which native capabilities a process composes, so the same actor runs
under the desktop profile with a window and a wgpu renderer, under the
headless profile driven by a timer, or inside a test harness that reads back
frames.

What that buys you: a world drawn from world-space triangles, textured quads,
GPU shapes and text; a mesh DSL and OBJ import; audio with sampled instrument
banks and streamed tracks; keyboard, mouse and window input as subscriptions;
files, HTTP, TCP, subprocesses and the clipboard as ordinary mail. Guest code
compiles to `wasm32-unknown-unknown` and is loaded, replaced in place, and
introspected while the engine runs.

The engine is also driveable from outside the process. A test harness, a human
at a console, or an agent can start engines, load and replace code, inspect
live contracts, capture frames, and gather evidence over framed RPC without
linking into the runtime.

The project is a Rust 2024 workspace and is still moving. Current code defines
what ships; Accepted Architecture Decision Records under `docs/adr/` preserve
the load-bearing design and its rejected alternatives. A few applications built
on the engine live in this tree today, the bloomery coordinator and the puppet
mascot among them; they consume the library rather than belong to it.

## Start here

- [Introduction](docs/guide/introduction.md) — what aether is and the main task paths.
- [Run the demo](#run-the-demo) — package the desktop chassis and watch it draw.
- [First live-engine session](docs/guide/orientation/first-engine-session.md) — start, inspect, observe, and clean up one engine.
- [Architecture overview](docs/guide/architecture.md) — operator, hub, substrate, capability, and guest boundaries.
- [Repository map](docs/guide/orientation/repository-map.md) — crates and where a change belongs.
- [Subsystem map](docs/guide/systems.md) — runtime, hosting, I/O, media, tooling, and product systems.

The full mdBook navigation lives in [SUMMARY.md](docs/guide/SUMMARY.md).

## Architecture in one minute

```text
operator
   │ MCP / framed RPC
   ▼
stable tunnel → aether-mcp → hub + artifact stores
                               │ engine id
                               ▼
                         child substrate
                 ┌──────────────────────────┐
                 │ chassis capabilities     │ native actors
                 │ registry + scheduler     │ mail runtime
                 │ wasm components          │ guest actors
                 │ logs/traces/cost/capture │ evidence
                 └──────────────────────────┘
```

- A **kind** is a named message schema; a **mailbox** is an actor address.
- Native capabilities and wasm components use the same actor/mail model.
- A **chassis** selects the drivers and native capability runtimes for a
  process: desktop, headless, hub, substrate harness, or bloomery coordinator.
- The **hub** supervises child engines and stores content-addressed chassis and
  component artifacts.
- `aether-mcp` adapts agent-facing JSON tools to live engine RPC/mail. The tool's
  active schema is the argument reference.

Read [Process topology and chassis](docs/guide/architecture/process-topology.md)
and [Guest/native boundaries](docs/guide/architecture/guest-native-boundary.md)
for the detailed model.

## Build and run

The workspace root has no default binary.

```sh
cargo build
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt -- --check
```

Run a chassis explicitly:

```sh
# Desktop: window/input/GPU/audio profile
cargo run -p aether-chassis-desktop --bin aether-desktop

# Headless: timer-driven engine profile
cargo run -p aether-chassis-headless --bin aether-headless

# Hub: fleet supervision and artifact stores
cargo run -p aether-chassis-hub --bin aether-hub
```

Use `--print-config` after the binary separator to inspect the knob registry and
its environment/default selections without booting the engine. This discovery
dump runs before config-file and per-capability CLI overlays, so it is not the
final effective configuration:

```sh
cargo run -p aether-chassis-headless --bin aether-headless -- --print-config
```

## Run the demo

`aether-puppet` is the in-tree mascot: a subject mesh redrawn every frame as
pen-plotter line art, with an idle motor and a turntable driving it. Package it
into a desktop depot and run that depot. The module exports three actors
(`Puppet`, `Idle`, `Turntable`) and declares no default, so the component entry
names the export it wants, which is the `--spec` form rather than
`--components`:

```sh
cat > puppet.json <<'JSON'
{
  "chassis": "desktop",
  "title": "aether puppet",
  "components": [{ "package": "aether-puppet", "export": "aether.puppet" }]
}
JSON

cargo xtask package --spec puppet.json --out target/puppet-demo
./target/puppet-demo/aether-desktop
```

A window opens on an empty sheet; mail `aether.puppet.load` with a subject mesh
in the `assets` namespace and she is inked in, draggable with the mouse.

<!-- demo capture pending -->

## Status

Pre-1.0. Every crate carries one workspace version (`0.3.0-alpha` on this
commit) and none are published to crates.io yet. APIs move between minor
versions; the ADRs record why, and `docs/adr/` is the place to check before
depending on a subsystem.

Working today: the desktop, headless, hub, substrate-harness, and bloomery
chassis; the mail scheduler and settlement tracking; wasm component load, drop,
and in-place replace with state carried across the swap; rendering (world
triangles, textured quads, GPU shapes, text) with a depth-tested camera; audio
with built-in and sampled instruments plus streamed tracks; window control and
native menus; input subscriptions; file, HTTP, TCP, subprocess, and clipboard
capabilities; per-actor logs, traces, and cost tables; frame capture; the
package depot; and the two in-repo test harnesses.

Not here yet: no scene or asset editor application (the widget crate composes
UI, but nothing ships as an editor); no replication or netcode layer, only the
framed RPC the hub and its engines speak plus the HTTP and TCP capabilities; no
asset pipeline beyond the mesh DSL, OBJ import, WAV and SFZ audio, and TTF
fonts, so there is no importer, no texture format past raw RGBA pixels, and no
asset build step.

## Drive a live engine

Start the local MCP stack only when a task needs it:

```sh
scripts/ensure-tunnel.sh
```

Codex reads the `aether-hub` endpoint from `.codex/config.toml`; MCP clients that
consume `.mcp.json` use the same local endpoint. Starting a subprocess cannot add
tools to an already-open client session, so reconnect the server in that surface
if the `mcp__aether-hub__*` tools are still absent.

A safe session follows this loop:

```text
list engines
  → spawn one owned engine
  → inspect live kinds/handlers
  → upload before selecting an artifact
  → load/send/observe
  → terminate the exact owned engine
```

Stored artifacts, running engines, and loaded component instances are separate
resources. Read [Operating a live engine](docs/guide/operating/index.md) before
automating fleet or replacement work.

## Write a component

A component crate exposes a wasm `cdylib` and depends on `aether-actor`.

```rust
use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_kinds::{Ping, Pong};

pub struct Echo;

#[actor]
impl WasmActor for Echo {
    const NAMESPACE: &'static str = "example.echo";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[handler::single]
    fn on_ping(&mut self, _ctx: &mut WasmCtx<'_>, ping: Ping) -> Pong {
        Pong { seq: ping.seq }
    }
}

aether_actor::export!(Echo);
```

For multi-actor modules, declare the default explicitly:

```rust
aether_actor::export!(default = Console, Inspector, Worker);
```

Without `default =`, a multi-actor module is defaultless and every load must select
an export. Build for `wasm32-unknown-unknown`, call `upload_component` with the
artifact path, then call `load_component`/`replace_component` with the returned
registry selector—not a host wasm path.

See [Writing a component](docs/guide/recipes/writing-a-component.md) and
[Components and lifecycle](docs/guide/systems/components.md).

## Workspace map

| Layer | Crates | Responsibility |
|---|---|---|
| Data and wire | `aether-data`, `aether-codec`, `aether-math`, `aether-kinds` | ids, schemas, canonical encoding, framing, shared vocabulary |
| Guest SDKs | `aether-actor`, `aether-behavior` and derive crates | actor/behavior authoring, exports, contexts, replies |
| Runtime | `aether-substrate` | registry, mail, scheduler, native/wasm hosts, settlement |
| Native services | `aether-render`, `aether-audio`, `aether-fs` and the rest of `aether-<cap>` | one crate per capability mailbox: render, text, audio, clipboard, window, FS, HTTP, TCP, process, RPC, component, lifecycle, fleet, inventory, trace |
| Chassis and harnesses | `aether-chassis` + `aether-chassis-*` | per-chassis crates over a shared composition layer; harnesses in `aether-harness-*` |
| Guest actors | `aether-kit-commons`, `aether-kit-widget`, `aether-mesh`, `aether-puppet`, `aether-anthropic` | camera, console and mesh viewer; the widget tree; the geometry DSL library; the pen-plotter line-art mascot; the model-provider component |
| Operator bridge | `aether-mcp` | MCP tools, live schemas, RPC and bounded evidence projection |
| Consumers in this tree | `aether-bloomery` + `aether-bloomery-*`, `aether-chassis-bloomery`, `aether-harness-bloomery` | the bloomery coordinator built on the engine: work-order reducer and value vocabulary, git and GitHub adapters, coordinator chassis, operator board, scenario harness |
| Tooling | `xtask`, fixture crates, excluded `fuzz/` | dist/bundle discovery, compatibility artifacts, nightly fuzz targets |

Capability request/reply kinds normally live with their capability, in that
capability's own crate at `aether-<cap>/src/kinds.rs`. `aether-kinds` is
reserved for genuinely cross-cutting or explicitly upstream contracts.

## Testing and packaging

Choose the narrowest boundary that proves the change:

- unit tests for codecs, parsers, validation, and state machines;
- **SubstrateHarness** for the real in-process scheduler/capability/wasm/frame boundary;
- **FleetHarness** for real hub RPC, artifact stores, and forked child engines;
- performance trials for paired latency/throughput/keep-up evidence;
- the isolated nightly `fuzz/` crate for untrusted parsers and wire boundaries.

See [Tests that earn their place](docs/guide/testing.md) and
[SubstrateHarness and FleetHarness](docs/guide/testing/substrateharness-and-fleetharness.md).

Packaging commands have distinct outputs:

```sh
cargo xtask dist      # component wasm + chassis artifacts + dist/manifest.json
cargo xtask package --chassis desktop --components aether-kit-commons
                      # a shippable depot: chassis binary + content-addressed pack/
```

They are not the same operation as landing a PR or publishing a versioned
release. Read [Distribution and packaging](docs/guide/building/distribution.md).

## Contributing

Planned work lives in GitHub issues. Managed issue-body sections hold the Plan,
declared surface, and size/model route; a hidden trusted record binds approval
to that Plan digest and an exact base commit. Implementation uses one isolated
issue worktree and draft PR, never the primary `main` checkout. PR titles and
commits use Conventional Commits. Before opening or updating a draft, run:

```sh
cargo fmt -- --check
cargo clippy --all-targets -- -D warnings
```

GitHub Actions owns the expensive build/test matrix; `CI pass` and `Lint title`
are the required checks. The draft's current head must also have direct-review
acceptance, resolved threads, declared-surface containment, and any required
dogfood evidence. Landing is an explicit separate operation. Keep PRs focused,
preserve unrelated user changes, and do not push directly to `main` or merge
without that authority. The full lifecycle is written up in
[Agent and contributor workflow](docs/guide/contributing/agent-workflow.md).

Repository-agent mechanics are surface-specific:

- Codex uses `AGENTS.md`, `.agents/skills/`, and the active Codex tool schema.
- Claude Code uses `CLAUDE.md` and `.claude/skills/`.

Architecture and public APIs are shared; tool syntax and workflow harnesses are
not translated by mechanical substitution. Human contributors can start at
[CONTRIBUTING.md](CONTRIBUTING.md).

## Documentation

- `docs/guide/` — task-oriented mdBook source.
- `docs/adr/` — numbered decision records; check status and supersession.
- `AGENTS.md` / `.agents/skills/` — Codex repository constraints and workflows.
- `CLAUDE.md` / `.claude/skills/` — the same for Claude Code.
- `.github/workflows/` — hosted CI; conventions in its README.

Build the guide with:

```sh
mdbook build docs
```

See [Maintaining the guide](docs/guide/contributing/documentation.md) before
changing navigation or adding a high-drift recipe.

## How this is built

This codebase is developed with Claude Code. Every load-bearing decision is
recorded as an Architecture Decision Record before the code lands: 215 of them
sit under `docs/adr/`, each carrying the alternatives that were rejected and
why. Every change arrives through a pull request that a required CI aggregate
gates on formatting, clippy with warnings denied, rustdoc, the sharded test
suite, duplicate-code detection, unused-dependency detection, and a check on
newly added lint suppressions. `main` is squash-merged and never pushed to
directly.

## License

Licensed under either [Apache License 2.0](LICENSE-APACHE) or
[MIT](LICENSE-MIT), at your option. Contributions are dual-licensed under the
same terms unless explicitly stated otherwise; see [CONTRIBUTING.md](CONTRIBUTING.md).
