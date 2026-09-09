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
    aether-desktop
    aether-headless
    aether-hub
```

The manifest records target, profile, component paths, and chassis paths
relative to `dist/`. `--no-bins` provides a wasm-only fast path. The command
regenerates `dist/` rather than allowing stale artifacts to masquerade as the
current manifest.

Behavior scripts are discovered separately from components. A behavior script
depends on `aether-behavior`, exposes a `cdylib`, and does not depend on
`aether-actor`. Host-carrying component variants are also built separately so
the ordinary component artifact is not forced to carry the behavior interpreter.

## Chassis binary inventory

`xtask/src/inventory.rs` holds the one list of chassis binaries the workspace
ships, and `cargo xtask bins` publishes it so a script or workflow reads the
names instead of re-spelling them:

```sh
cargo xtask bins           # one `<package> <bin> <file>` line per binary
cargo xtask bins --json    # the same, plus the depot filename per `--chassis`
```

`file` is the host-platform filename, so a Windows runner is told
`aether-desktop.exe`. The `--json` form adds `package_chassis`, keyed by the
values `cargo xtask package --chassis` accepts, which is how
`.github/workflows/release.yml` finds the executable it renames for hand-out.
Prefer this over hardcoding a binary name: that workflow is manually triggered,
so nothing in CI catches a name that has gone stale.

## Package depot

`cargo xtask package` is the shipping channel (ADR-0163 §1). It emits a depot
directory: the chassis binary alongside a `pack/` tree whose `manifest`
references each component's wasm (and optional config) bytes by content hash
into `pack/objects/`.

```text
<out>/
  aether-desktop              # the chassis binary (`aether-headless` under `--chassis headless`; .exe on Windows)
  pack/manifest               # the persisted, versioned package manifest
  pack/objects/<sha256>       # component wasm + config bytes, content-addressed
  pack/assets/…               # the `--assets` tree, verbatim
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
and per-component `package`-or-`wasm` plus `config` / `config_json`, `name`, and
`export`. Relative paths in a spec resolve against the spec file's directory,
not an arbitrary process working directory.

`config` names a file of init-config **bytes** — the wire image of the
component's `Config` kind, which is what a machine stages. `config_json` names a
**JSON** file instead, encoded at build time against the `Config` schema the
component's own wasm declares (ADR-0090 + ADR-0028). Prefer `config_json` for
anything checked in: a reviewer can read it, and a field the component does not
declare fails the emit naming the file and the field rather than arriving as a
decode error inside the guest. The JSON boot manifest takes the same pair of
fields, so a spec and a manifest can share one config file. Setting both on one
entry is an error, not a precedence question.

### Shipping assets

`--assets <dir>` copies a directory verbatim into `pack/assets`, and the
packaged chassis roots the `assets` namespace there. Assets are the one part of
`pack/` that is not content-addressed, and deliberately: a component reaches a
file by mailing `aether.fs.read` with the path an author wrote, so the shipped
tree has to keep those paths. Objects are hash-named because the manifest names
them; assets are path-named because the running program does.

The depot's root slots in **below** argv/env/file and **above** the compiled
default, the same precedence a manifest's title and tick cadence take, so an
operator's `AETHER_ASSETS_DIR` / `--assets-dir` still overrides a shipped depot.
The check is per member rather than per field: any pinned `aether.fs` root — save
or config as much as assets — keeps the operator's whole `NamespaceRoots`.

```sh
cargo xtask package \
  --spec demo/puppet-turntable.json \
  --assets crates/aether-mesh/examples
```

That is the checked-in demo (`demo/README.md`): a depot that draws a turning
line-art teapot when its binary is run with no flags at all.

## Boot-time manifests

Two boot channels feed a chassis its component set, and they are distinct from
the persisted package manifest above:

- The JSON boot manifest (`crate::boot_manifest`) names component files by
  path. The hub's `spawn_substrate` writes it and injects it through
  `AETHER_BOOT_MANIFEST`; the spawned chassis reads the listed wasm itself. Its
  entries take `config` (bytes) or `config_json` (encoded at read time against
  the component's declared `Config` schema), so a checked-in manifest is the
  no-packaging developer path — `--boot-manifest demo/puppet-turntable.boot.json`
  boots the same composition the depot ships. Manifest paths are resolved as-is,
  against the process working directory rather than the manifest's own
  directory.
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
- **bump**: move the workspace version and re-lock — see below;
- **version/tag/publication policy**: only the bump is specified today.

ADR-0092 proposes a release-branch workflow but remains Proposed; it is not
current repository policy. Contributor lifecycle skills do not publish a
software release.

## Bumping the workspace version

Every crate takes its version from `[workspace.package] version` in the root
`Cargo.toml` — no crate carries a literal, and no doc, script, or workflow
spells one either. The ten `env!("CARGO_PKG_VERSION")` sites read it at compile
time. So the bump is one edit and the lockfiles that edit invalidates:

```sh
cargo xtask bump 0.4.0-alpha --dry-run   # print the files and the commands
cargo xtask bump 0.4.0-alpha             # write them
```

The command refuses an argument semver will not parse, and refuses a version
the workspace is already on.

There are **two** lockfiles. `fuzz` is a `[workspace] exclude` entry with its
own standalone workspace and its own `Cargo.lock`, which pins `aether-codec`
and `aether-data` by version through path dependencies — a root `cargo update`
never opens it, and skipping it leaves the fuzz build resolving against a
version that no longer exists. `bump` finds it by reading the `exclude` list
and re-locking every excluded crate that carries a lockfile, so a future
excluded crate is covered without editing the command. Re-locking `fuzz` needs
network access: its lockfile lags the workspace, so the resolve pulls crates
the offline cache may not hold.

The cut is four steps:

1. **Bump.** `cargo xtask bump <version>`, dry-run first.
2. **Read the three files it touched** — `Cargo.toml`, `Cargo.lock`,
   `fuzz/Cargo.lock`. The two lockfiles should show the workspace crates moving
   to the new version; `fuzz/Cargo.lock` may also carry registry churn it had
   accumulated while nothing re-locked it.
3. **Land the bump as its own PR** (`chore(release): …`) through the ordinary
   flow. Nothing else rides that PR, so the version move is one commit.
4. **Tag the merged commit** and run the `Release` workflow against it for the
   hand-out artifact.

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
- Version bump + lockfile regeneration: `xtask/src/bump.rs`
- Published chassis-binary inventory: `xtask/src/bins.rs`
- Autoload: `crates/aether-chassis/src/autoload.rs`
- Boot manifest schema: `crates/aether-chassis/src/boot_manifest.rs`
- Package manifest + store-backed boot: `crates/aether-chassis/src/package.rs`
- JSON init-config encoding: `crates/aether-chassis/src/component_config.rs`
- Current hosted artifact job: `.github/workflows/release.yml`
- Related decisions: ADR-0090, ADR-0115, ADR-0116, ADR-0163; ADR-0092 is Proposed
