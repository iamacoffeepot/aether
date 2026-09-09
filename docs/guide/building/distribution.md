# Distribution and packaging

Aether has two packaging commands with different consumers:

- `cargo xtask dist` builds a discoverable development/test artifact tree.
- `cargo xtask package` emits a shippable package depot: one chassis binary
  plus a content-addressed pack of components.

Neither command is the same as merging a PR. Tagging a version is what
publishes a GitHub Release, and it does so by running `cargo xtask package` on
each platform — see [Cutting a release](#cutting-a-release).

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
values `cargo xtask package --chassis` accepts. `.github/workflows/release.yml`
reads both: `package_chassis.desktop` tells it which binary the depot already
carries, and each remaining `chassis_bins` entry is a binary it builds and
ships as its own archive. Prefer this over hardcoding a binary name — no pull
request runs that workflow, so nothing in CI catches a name that has gone
stale.

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
  --components aether-puppet \
  --title aether
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
- **bump**: move the workspace version and re-lock — see below;
- **release**: push the version tag, which packages every platform and
  publishes the archives on that tag's GitHub Release — see
  [Cutting a release](#cutting-a-release).

ADR-0092 proposes a release-branch workflow but remains Proposed; it is not
current repository policy. Contributor lifecycle skills do not publish a
software release — the tag push does.

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
4. **Tag the merged commit and push the tag**, which publishes the release —
   see below.

## Cutting a release

`.github/workflows/release.yml` turns a version tag into a published GitHub
Release. It triggers on a push of a bare-semver tag; the repository's tags
carry no `v` prefix (`0.1.0-alpha`, `0.3.0-alpha`), so the cut is:

```sh
git tag -a 0.4.0-alpha -m "…"
git push origin 0.4.0-alpha
```

Each platform builds a package depot from the checked-in
`demo/puppet-turntable.json` spec plus one archive per remaining chassis
binary, and every archive is attached to the release:

| Platform | Depot | Chassis binaries |
| --- | --- | --- |
| `linux-x86_64` | `aether-<version>-linux-x86_64.tar.gz` | `aether-headless-…`, `aether-hub-…`.tar.gz |
| `macos-arm64` | `aether-<version>-macos-arm64.tar.gz` | `aether-headless-…`, `aether-hub-…`.tar.gz |
| `windows-x86_64` | `aether-<version>-windows-x86_64.zip` | `aether-headless-…`, `aether-hub-…`.zip |

Every name carries `<version>-<platform>`, and the platform half is read from
the toolchain's own host triple, so it follows a runner image that changes
architecture rather than asserting a stale one. Each archive unpacks to a
single directory of that same name.

The depot archive holds what the spec says: the chassis binary it names, the
components it selects, and both workspace license files. The spec owns that
list, which is why the other chassis binaries are their own archives rather
than extra files inside a depot they are not part of — each ships with the
same two license files, since a statically linked binary is redistributed
with its notices. The set of those binaries comes from
`cargo xtask bins --json`, so a chassis added to the inventory ships from the
next tag with no workflow edit.

The release is marked a pre-release whenever the version carries a
pre-release suffix, which every tag cut so far does (`-alpha`). Its body is
the `CHANGELOG.md` section for that exact version when the file exists on the
tag, and the tag's own message otherwise — the live path today, since the
repository has no changelog.

Running the workflow from the Actions tab (`workflow_dispatch`) is the dry
run: the identical build, archives named from the current workspace version
and uploaded as workflow artifacts, and no release created. Reach for it to
exercise a change to the workflow before a tag depends on it.

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
- Current hosted artifact job: `.github/workflows/release.yml`
- Related decisions: ADR-0090, ADR-0115, ADR-0116, ADR-0163; ADR-0092 is Proposed
