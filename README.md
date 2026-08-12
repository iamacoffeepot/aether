# aether

aether is a pre-1.0 application engine built for games, tools, and interactive
systems. A thin native **substrate** owns privileged resources and hosts native
and WebAssembly **actors**. Actors communicate through typed **mail**. An
out-of-process operator—an agent, a human, a test harness, or another client—can
start engines, load and replace code, inspect live contracts, capture frames,
and gather evidence without linking into the runtime.

The project is a Rust 2024 workspace and is still moving. Current code defines
what ships; Accepted Architecture Decision Records under `docs/adr/` preserve
the load-bearing design and its rejected alternatives.

## Start here

- [Introduction](docs/guide/introduction.md) — what aether is and the main task paths.
- [First live-engine session](docs/guide/orientation/first-engine-session.md) — start, inspect, observe, and clean up one engine.
- [Architecture overview](docs/guide/architecture.md) — operator, hub, substrate, capability, and guest boundaries.
- [Repository map](docs/guide/orientation/repository-map.md) — crates and where a change belongs.
- [Subsystem map](docs/guide/systems.md) — runtime, hosting, I/O, media, tooling, and product systems.
- [Agent and contributor workflow](docs/guide/contributing/agent-workflow.md) — Plan artifacts, digest-bound approval, draft-PR proof, and landing.

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
  process: desktop, headless, hub, or substrate harness.
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
| Native services | `aether-render`, `aether-audio`, `aether-fs` and the rest of `aether-<cap>` | one crate per capability mailbox: render, text, audio, FS, HTTP, TCP, lifecycle, fleet, providers |
| Chassis and harnesses | `aether-chassis` + `aether-chassis-*` | per-chassis crates over a shared composition layer; harnesses in `aether-harness-*` |
| Product actors | `aether-kit-*`, `aether-mesh` | camera, widgets, workbench, terrain/world, simulation, geometry DSL |
| Operator bridge | `aether-mcp` | MCP tools, live schemas, RPC and bounded evidence projection |
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

GitHub Actions owns the expensive build/test/package matrix. The draft's current
head must also have direct-review acceptance, resolved threads, declared-surface
containment, and any required dogfood evidence. Landing is an explicit separate
operation. Keep PRs focused, preserve unrelated user changes, and do not push
directly to `main` or merge without that authority.

Repository-agent mechanics are surface-specific:

- Codex uses `AGENTS.md`, `.agents/skills/`, and the active Codex tool schema.
- Claude Code uses `CLAUDE.md` and `.claude/skills/`.

Architecture and public APIs are shared; tool syntax and workflow harnesses are
not translated by mechanical substitution. Human contributors can start at
[CONTRIBUTING.md](CONTRIBUTING.md).

## Documentation

- `docs/guide/` — task-oriented mdBook source.
- `docs/adr/` — numbered decision records; check status and supersession.
- `AGENTS.md` — concise Codex repository constraints.
- `.agents/skills/` — executable Codex issue/PR workflows.
- `.github/workflows/` — hosted CI; conventions in its README.

Build the guide with:

```sh
mdbook build docs
```

See [Maintaining the guide](docs/guide/contributing/documentation.md) before
changing navigation or adding a high-drift recipe.

## License

Licensed under either [Apache License 2.0](LICENSE-APACHE) or
[MIT](LICENSE-MIT), at your option. Contributions are dual-licensed under the
same terms unless explicitly stated otherwise; see [CONTRIBUTING.md](CONTRIBUTING.md).
