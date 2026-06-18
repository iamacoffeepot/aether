# aether

An application engine — built for games — developed collaboratively with Claude.

The vision is a harness where Claude sits as assistant, engineer, and designer — driving a running engine, observing it, modifying it. A thin native **substrate** owns I/O, GPU, and audio and hosts a WebAssembly runtime; engine **actors** — WASM components and native chassis capabilities — run on the substrate and communicate with it and with each other only through **mail**.

## Status

Pre-1.0 Rust project (edition 2024). The design is still moving; the ADRs under `docs/adr/` are the record of what is committed and why.

## Repository layout

Infrastructure crates (no actor of their own):

- **`aether-data`** — the universal data layer (`no_std` + `alloc`): typed-id newtypes, wire identity, the schema vocabulary, and the `Kind` / `Schema` traits everything else builds on. Proc macros live in `aether-data-derive`.
- **`aether-codec`** — schema-driven JSON ↔ wire-byte conversion plus length-prefixed stream framing.
- **`aether-kinds`** — the substrate's own kind vocabulary (ticks, input, render, audio, window, filesystem, …).
- **`aether-math`** — `Vec*`, `Mat4`, `Quat`, `Aabb` (`no_std`, column-major, right-handed Y-up).

Runtime and chassis:

- **`aether-substrate`** — the shared native runtime (scheduler, mail queue, WASM host).
- **`aether-capabilities`** — native chassis capabilities (render, audio, filesystem, input, …).
- **`aether-substrate-bundle`** — the four chassis as binaries (see below), plus the hub client and the in-process test bench.

Actor SDK:

- **`aether-actor`** — the guest SDK: the `Actor` / `FfiActor` traits, `Mailbox<K>`, `FfiCtx`, the `#[actor]` macro, and `export!`. Macros live in `aether-actor-derive`.

Reference actors and tools: **`aether-mesh-viewer`** (a single-actor component) and **`aether-kit`** (the gameplay-systems layer — a multi-actor module whose `camera` export is the reference camera component), **`aether-mesh`** (a mesh DSL + mesher), and **`aether-mcp`** (the out-of-process MCP harness Claude drives the engine through).

## Building and running

```sh
cargo build                       # debug build of the workspace (release: --release)
cargo nextest run --workspace     # run the test suite
cargo clippy --all-targets -- -D warnings
cargo fmt
```

The workspace root has no default binary. The chassis binaries all live in `aether-substrate-bundle`; pick one with `--bin`:

```sh
cargo run -p aether-substrate-bundle --bin aether-substrate          # desktop (windowed, GPU)
cargo run -p aether-substrate-bundle --bin aether-substrate-headless # headless (timer-driven ticks)
cargo run -p aether-substrate-bundle --bin aether-substrate-hub      # hub (supervises a fleet of engines)
```

## Driving the engine (the MCP harness)

Claude drives a running engine through MCP — the concrete form of the "Claude-in-harness" vision. The harness (`aether-mcp`) is an RPC client that relays each tool call to the hub as wire mail, fronted by a long-lived tunnel so the volatile backends can restart without dropping the MCP session. Bring the stack up with `scripts/ensure-tunnel.sh` (idempotent). From there an agent spawns substrates, loads components, sends mail, captures frames, runs computation DAGs, and reads per-actor logs.

The tools and the process topology are documented in `CLAUDE.md` (the *MCP harness* section) and in `docs/guide/mcp-harness.md`.

## Writing a component

A component is an actor: an `#[actor] impl FfiActor for C` block declaring handlers, plus an `export!(C)` that emits the FFI shims the substrate links against. Address peers and chassis capabilities by type (`ctx.actor::<RenderCapability>().send(&kind)`); mail is fire-and-forget unless a reply kind is noted. See [Writing components](CLAUDE.md#writing-components) in `CLAUDE.md` for the full shape.

A component lives in one crate that produces both outputs (`crate-type = ["cdylib", "rlib"]`):

- The crate's **rlib** is the public API for *talking to* the component — the kind types (mail shapes), parameter structs, and helpers that other components import to build the wire shapes they send it.
- The crate's **cdylib** is the deployable wasm. The runtime (the `FfiActor` impl) lives in the crate's `runtime` module behind a default-on `runtime` feature; under `wasm32` the `export!()` invocation produces the component's FFI exports. The host-target rlib build leaves those exports inert, so integration tests link the same artifact. A consumer that only needs the kind types opts out with `default-features = false`.

Reference components in-tree: `aether-mesh-viewer` (single-actor) and `aether-kit`'s `camera` export (a multi-actor module, ADR-0096). Third-party components follow the same shape — the convention is symmetric, not first-party-privileged. `aether-kinds` is reserved for the chassis primitives the substrate itself emits or consumes.

## Testing and pre-flight

- **Tests** run under `cargo nextest run --workspace`. Timing-sensitive concurrency tests live in a `mod heavy` submodule so the runner serializes them; see the *Heavy tests* section of `CLAUDE.md`.
- **Pre-flight**: `scripts/preflight.sh` runs the CI-equivalent checks locally (fmt, clippy, doc, tests, and the wasm32 component cross-build). Install the pre-push hook once per clone with `scripts/setup-githooks.sh`.

## Documentation

- **`CLAUDE.md`** — the working-directory guide: crate map, runtime surfaces, the MCP harness, and the repo conventions. Start here when working in the tree.
- **`docs/adr/`** — Architecture Decision Records, numbered sequentially. Read the ADR that owns a subsystem before changing it; `docs/adr/TEMPLATE.md` starts a new one.
- **`docs/guide/`** — the longer-form guide (an mdBook): philosophy, architecture, the type system and actor model, the MCP harness, and the engine systems.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions. See [CONTRIBUTING.md](CONTRIBUTING.md) for contribution guidelines.
