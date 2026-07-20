# Distribution and standalone bundles

Aether has two related packaging commands with different consumers:

- `cargo xtask dist` builds a discoverable development/test artifact tree.
- `cargo xtask bundle` builds one standalone, hub-less product executable with
  an ordered component set embedded at build time.

Neither command is the same as merging a PR, tagging a version, or publishing a
GitHub Release.

## Distribution tree

`cargo xtask dist` discovers wasm components structurally from Cargo metadata:
a package depends on `aether-actor` and exposes a `cdylib` target. It builds
component packages in isolated Cargo invocations for `wasm32-unknown-unknown`,
optionally builds the chassis binaries, and writes an authoritative `dist/`
tree.

```text
dist/
  manifest.json
  components/
    <stem>.wasm
  bin/
    aether-substrate
    aether-substrate-headless
    aether-substrate-hub
```

The manifest records target, profile, component paths, and chassis paths
relative to `dist/`. `--no-bins` provides a wasm-only fast path. The command
regenerates `dist/` rather than allowing stale artifacts to masquerade as the
current manifest.

Behavior scripts are discovered separately from components. A behavior script
depends on `aether-behavior`, exposes a `cdylib`, and does not depend on
`aether-actor`. Host-carrying component variants are also built separately so
the ordinary component artifact is not forced to carry the behavior interpreter.

## Standalone bundle

`cargo xtask bundle` chooses either the desktop or headless generic bundle
binary and embeds an ordered pack of wasm plus optional component config. The
result boots without the hub artifact store or MCP coordinator.

The short form selects workspace packages or prebuilt `.wasm` paths:

```sh
cargo xtask bundle \
  --profile release \
  --chassis desktop \
  --components aether-kit \
  --title example
```

Component order is autoload order. Repeated `--config` flags pair by position
with `--components`; trailing components may omit config. Desktop-only options
include title and window mode. Headless bundles can set tick cadence.

For explicit actor export, instance name, or richer per-component control, use
the JSON `--spec` form. Relative paths in a spec resolve against the spec file,
not an arbitrary process working directory. The current `xtask` spec schema
does not expose runtime replica counts; an unrecognized `replicas` field does
not turn one packed component into several instances.

## Build-time pack and boot-time manifest

The bundle flow has two formats with different jobs:

1. `xtask` writes a build manifest describing chassis and ordered inputs.
2. `aether-substrate-bundle` encodes those files into the executable's component
   pack.
3. At process start, the generic bundle binary turns the pack into autoload
   entries and loads the selected exports/configs.

The hub spawn path also uses a boot manifest, but stages artifacts from the
hub's content-addressed stores. Do not couple a standalone bundle to hub
selectors or assume a development upload is present on an end-user machine.

## Multi-actor and replicas

A defaultless multi-actor module requires an explicit export in its bundle
spec. A module with `export!(default = …)` can use that default when omitted.

The runtime boot manifest used by the hub/MCP spawn path can expand configured
replicas into named instances. Replication is component instancing, not
multiple copies of the wasm bytes. The current `cargo xtask bundle` pack emits
one autoload entry per component spec; use runtime boot manifests when replica
fan-out is required. Names and route targeting must still follow the
actor/router contract.

## Choosing a packaging path

| Goal | Use |
|---|---|
| Let tests or an external harness locate every current artifact | `cargo xtask dist` |
| Build only component wasm quickly | `cargo xtask dist --no-bins` |
| Run an agent-controlled fleet | hub + uploaded binary/component registries |
| Ship one precomposed executable | `cargo xtask bundle` |
| Exercise runtime code without packaging | Cargo run/test or SubstrateHarness |

## Release terminology

Keep these operations distinct:

- **land**: merge an approved PR through the repository workflow;
- **dist**: produce the development/test artifact tree;
- **bundle**: produce a standalone executable;
- **release workflow**: the checked-in manual workflow currently builds a
  Windows `loco-motion` bundle artifact;
- **version/tag/publication policy**: not comprehensively specified today.

ADR-0092 proposes a release-branch workflow but remains Proposed; it is not
current repository policy. The `release-init` skill initializes lifecycle label
vocabulary and does not publish a software release.

## Verification and cleanup

Packaging is intentionally expensive and can leave large `target/` and `dist/`
trees. For normal implementation PRs, CI owns the expensive distribution proof
unless the issue or user asks for a local build. If you run it locally, report
what was generated and reclaim those artifacts when no longer needed.

Validate a bundle at the boundary it changes:

- component discovery changes: inspect `dist/manifest.json` and fixture set;
- pack format changes: round-trip `bundle_pack` tests;
- autoload changes: boot a bundle with multiple/export-selected components;
- chassis option changes: test the matching desktop or headless binary;
- release workflow changes: verify the workflow artifact, not only local Cargo.

## Implementation routes

- Discovery and commands: `xtask/src/{main,inventory}.rs`
- Autoload: `crates/aether-substrate-bundle/src/autoload.rs`
- Pack format: `crates/aether-substrate-bundle/src/bundle_pack.rs`
- Generic launchers: `crates/aether-substrate-bundle/src/bin/aether-bundle-*.rs`
- Current hosted artifact job: `.github/workflows/release.yml`
- Related decisions: ADR-0090, ADR-0115, ADR-0116; ADR-0092 is Proposed
