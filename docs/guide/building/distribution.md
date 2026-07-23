# Distribution and packaging

Aether has two packaging commands with different consumers:

- `cargo xtask dist` builds a discoverable development/test artifact tree.
- `cargo xtask package` emits a shippable package depot: one chassis binary
  plus a content-addressed pack of components.

Neither command is the same as merging a PR, tagging a version, or publishing a
GitHub Release.

## Distribution tree

`cargo xtask dist` is the dev/test artifact channel: the harnesses locate the
headless and hub binaries and component wasm by stem through `dist/manifest.json`,
and CI pre-builds scenario wasm through it. It discovers wasm components
structurally from Cargo metadata — a package depends on `aether-actor` and
exposes a `cdylib` target — builds each component package in an isolated Cargo
invocation for `wasm32-unknown-unknown`, optionally builds the chassis
binaries, and writes an authoritative `dist/` tree.

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

## Package depot

`cargo xtask package` is the shipping channel (ADR-0163 §1). It emits a depot
directory: the chassis binary alongside a `pack/` tree whose `manifest`
references each component's wasm (and optional config) bytes by content hash
into `pack/objects/`.

```text
<out>/
  aether-substrate            # the chassis binary (desktop or headless; .exe on Windows)
  pack/manifest               # the persisted, versioned package manifest
  pack/objects/<sha256>       # component wasm + config bytes, content-addressed
```

The depot writes to `target/package/` unless `--out` names another directory.
The chassis boots by decoding `pack/manifest` and resolving each entry's hash
against `pack/objects/`, so identity is the content and a `name` is a label.
A depot ships release artifacts, so `--profile` defaults to release.

### Selecting content

With no `--components` and no `--spec`, `package` runs the discover-everything
dev sweep: every structurally discovered component, the desktop chassis, and
default settings, with names mirroring the `dist` wasm stems.

For a real product, name the chassis and the components:

```sh
cargo xtask package \
  --profile release \
  --chassis desktop \
  --components aether-kit-commons \
  --title loco-motion
```

`--chassis` selects `desktop` or `headless`. Component order is autoload order.
Repeated `--config` flags pair by position with `--components`; trailing
components may omit config. `--title` and `--window-mode` apply to the desktop
chassis; `--tick-hz` applies to headless. Those three settings ride into
`pack/manifest` and the depot boot applies them below argv/env and above the
compiled defaults, so a shipped depot comes up titled and in its window mode
while an operator's `AETHER_WINDOW_*` still overrides it.

For explicit actor export, instance name, or richer per-component control, use
the JSON `--spec` form. A spec carries the chassis, the three chassis settings,
and per-component `package`-or-`wasm` plus `config`, `name`, and `export`.
Relative paths in a spec resolve against the spec file's directory, not an
arbitrary process working directory.

## Boot-time manifests

Two boot channels feed a chassis its component set, and they are distinct from
the persisted package manifest above:

- The JSON boot manifest (`crate::boot_manifest`) names component files by
  path. The hub's `spawn_substrate` writes it and injects it through
  `AETHER_BOOT_MANIFEST`; the spawned chassis reads the listed wasm itself.
- The package manifest (`crate::package`) references bytes by content hash and
  is what a shipped depot boots from.

Both drain into the same `env.autoload` list, which each chassis's
`Chassis::build` turns into `aether.component.load` mail. The runtime boot
manifest can expand a configured `replicas` count into named instances; the
package manifest carries the same `replicas` field.

## Choosing a packaging path

| Goal | Use |
|---|---|
| Let tests or an external harness locate every current artifact | `cargo xtask dist` |
| Build only component wasm quickly | `cargo xtask dist --no-bins` |
| Run an agent-controlled fleet | hub + uploaded binary/component registries |
| Ship a precomposed depot | `cargo xtask package` |
| Exercise runtime code without packaging | Cargo run/test or SubstrateHarness |

## Release terminology

Keep these operations distinct:

- **land**: merge an approved PR through the repository workflow;
- **dist**: produce the development/test artifact tree;
- **package**: produce a shippable package depot;
- **release workflow**: the checked-in manual workflow currently builds a
  Windows `loco-motion` package artifact — a zip of the depot;
- **version/tag/publication policy**: not comprehensively specified today.

ADR-0092 proposes a release-branch workflow but remains Proposed; it is not
current repository policy. The `release-init` skill initializes lifecycle label
vocabulary and does not publish a software release.

## Verification and cleanup

Packaging is intentionally expensive and can leave large `target/` and `dist/`
trees. For normal implementation PRs, CI owns the expensive distribution proof
unless the issue or user asks for a local build. If you run it locally, report
what was generated and reclaim those artifacts when no longer needed.

Validate a change at the boundary it touches:

- component discovery changes: inspect `dist/manifest.json` and the fixture set;
- package manifest format changes: round-trip `aether_chassis::package` tests;
- autoload changes: boot a depot with multiple or export-selected components;
- chassis option changes: test the matching desktop or headless binary;
- release workflow changes: verify the workflow artifact, not only local Cargo.

## Implementation routes

- Discovery and commands: `xtask/src/{main,inventory}.rs`
- Autoload: `crates/aether-chassis/src/autoload.rs`
- Boot manifest schema: `crates/aether-chassis/src/boot_manifest.rs`
- Package manifest + store-backed boot: `crates/aether-chassis/src/package.rs`
- Current hosted artifact job: `.github/workflows/release.yml`
- Related decisions: ADR-0090, ADR-0115, ADR-0116, ADR-0163; ADR-0092 is Proposed
