# ADR-0250: A Component Reads Its Assets Whenever It Runs

- **Status:** Proposed
- **Date:** 2026-10-07

Replaces the load window of [ADR-0163](0163-content-addressed-packages-and-asset-bundles.md)
§3 and §4, and the parts of [ADR-0241](0241-code-is-published-not-loaded.md)
§2 and §9 that carry a module's bytes on a spawn. It also takes up the
unpublish that ADR-0241 deferred. This ADR is text only; the engine change is
its own issue. Until that change lands, ADR-0163 and ADR-0241 keep describing
the code as it is; the change that implements this ADR edits ADR-0241 §2 and
§9 in place and marks ADR-0163 §3 and §4 amended.

Three terms are used throughout. A **module file** is the wasm bytes a publish
brings, code and asset sections together. An **asset** is a named blob a module carries: the name is
the `<path>` of one `aether.asset.<path>` custom section in that file
(ADR-0163 §2) and the blob is that section's payload. A **publication** is the
binding of a namespace to a module (ADR-0241 §3).

## Context

A component's assets sit in its module file, outside the instance's linear
memory. The component reads one by asking the engine, through
`AssetWindow::asset` or `AssetWindow::asset_blob`
(`crates/aether-actor/src/asset.rs`), which the host serves from the module
file's bytes (`asset_fetch_p32` and `asset_blob_p32`,
`crates/aether-substrate/src/actor/wasm/host_fns.rs`).

Today those two calls work only inside `init` and `wire`. The host holds the
module file in a `LoadWindow`
(`crates/aether-substrate/src/actor/wasm/asset_manifest.rs`) for those two
hooks and `LoadWindow::close` lets go of it when `wire` returns. A call from
any later hook traps. ADR-0163 chose this so that the engine could free the
module file once the component had turned its assets into textures and meshes.

Three things are wrong with it.

**The calls have no meaning for most of a component's life.** A function the
engine offers to a component should do what it says whenever the component
runs. The type fence (only `WasmInitCtx` and `WireCtx` implement
`AssetWindow`) hides the trap from well-typed code and does not remove it.

**A published type cannot be built from its publication.** ADR-0241 split a
load into a publish and a spawn, and its `ModuleCache` keeps no bytes after a
publish (§2). A spawn that names a published type therefore has no module file
to read, and an `init` or `wire` that reads an asset fails. `Spawn.code`
(`crates/aether-kinds/src/lib.rs`) was added so the spawner could bring the
file again. That field is a `None` that selects a behaviour, which
[R-0048](../guide/contributing/design-rules.md#r-0048) bans, and whether a
spawn needs it depends on what the type's `init` does, which the spawner
cannot see. An actor inside the engine that spawns a published type by name
does not hold the file and cannot bring it. The MCP `load_component` tool does
not bring it either, so a module with assets could be started only from the
boot list (`Autoloader`, `crates/aether-chassis/src/autoload/loader.rs`).

**Every asset must be processed in one call.** Whatever a component will ever
need has to be read before `wire` returns. A component that loads a large
scene cannot spread the work over several turns or report progress.

ADR-0241 already says where this should end up: its migration lists a module's
"assets checked in as blobs", and its consequences say "a module's assets are
blobs shared like any other". The implementation kept the load window in place
of that.

## Decision

### 1. A publish checks each asset in as its own blob

When a module file is checked in, `ModuleCache` compiles the code and parses
the manifest as today, and checks each asset in to the engine blob store
(ADR-0238) as a blob of its own, keyed by the hash of its bytes. An asset
whose hash is already resident reuses that entry and copies nothing. The
module file is then let go, as it is today.

The `Module` (`crates/aether-substrate/src/actor/wasm/module/mod.rs`) holds
its assets: its `AssetIndex` maps each asset name to its `Blob`, where it maps
a name to a byte range of the file today.

### 2. A module's assets live as long as the module

ADR-0241 §2 already says how long a `Module` lives: while a publication, a
running instance, or a held `Module` holds it. Its asset blobs live exactly
that long, and the blob store frees each one when its last holder goes. Two
modules that carry the same asset hold one entry between them, and it is freed
when the second lets go.

### 3. An instance reads its module's table in every hook

The module's asset table is sidecar data, as its kind manifest is: the host
reads it from the module file's custom sections at publish, it stays with the
`Module`, and it is never copied into an instance. Every instance holds its
`Module` from the moment its context is built, so there is no instance without
one, no window, no closed state, and no trap for a call made at the wrong
time. A component reads its assets from `init`, `wire`, every handler,
`on_rehydrate`, and `unwire`.

Nothing reads an asset's bytes but the blob system. The host offers two calls
over the table and no third:

- a lookup that turns a name into that row's blob, admitted to the instance's
  blob table as a blob arriving on mail is. A name the table does not carry
  answers that there is none.
- the list of names and lengths, for a component that must find out what its
  module carries, such as a bundle packed after it was compiled.

The guest surface is one trait in place of `AssetCatalog` and `AssetWindow`,
implemented by every wasm ctx:

```rust
pub trait Assets {
    fn assets(&self) -> &[AssetInfo];                      // names and lengths
    fn asset(&mut self, name: &str) -> Option<Vec<u8>>;    // the blob, read into guest memory
    fn asset_blob(&mut self, name: &str) -> Option<Blob>;  // the asset's own blob, no copy
}
```

`asset_blob` is the lookup. `asset` is the lookup followed by the ordinary
blob read, so there is one way bytes reach a guest. The list is fetched once
for an instance and kept beside the actor, not on a ctx, because a ctx is
built again for every hook.

A blob from `asset_blob` is the asset's own entry. A component that keeps it,
or an actor it was sent to, holds that one asset and nothing else.

### 4. A spawn brings no bytes

`Spawn` is `{ namespace, key, parent, config }`. Its `code` field is removed,
with the host's check that brought bytes match the publication. A spawn of a
published type always builds an instance that can read its assets, whoever
asks for it.

A republish needs no special case: the successor reads the new module's assets
and the old guest reads the old module's until it ends, because each instance
holds its own module.

### 5. Unpublish withdraws a publication

`aether.component.unpublish` names a published namespace and withdraws its
publication. It is refused while an instance of that namespace is live, with
an error that names the instances. Once the publication and every instance are
gone, nothing holds the `Module`, and its compiled code and asset blobs are
freed, apart from an asset another module or actor still holds.

Unloading a bundle is ending its instances and unpublishing it.

### 6. What is removed

- `LoadWindow`, its `open` flag and optional source, and
  `ComponentCtx::load_window`.
- `Spawn.code` and the refusal for bytes that are not the published module.
- The code the `Autoloader` keeps for each boot entry between its publish and
  its spawns.
- The trap text that names "a spawn with its code, and a load" as the two
  doors.
- The host call that copies an asset's bytes to the guest (`asset_fetch_p32`),
  the separate way an asset blob enters an instance's blob table, and the
  instance context's optional module.
- From ADR-0163: the load window (§3), "one door between cold and resident"
  (§4), and the absence "no runtime payload fetch".
- From ADR-0241: the deferral of unpublish.

## Consequences

- A loaded bundle's assets are in engine memory for as long as it is published
  or one of its instances runs, and are freed when it is unloaded. That is the
  same memory the bundle's file takes during its load window today, held for
  longer.
- Assets shared between modules are held once. Bundles packed from a common
  set of models and textures cost the sum of their distinct assets, not the
  sum of their files.
- A publish copies each asset that is not already resident out of the module
  file once, and hashes every asset. Both are linear in the file's size.
- "What assets are resident" is answered by the list of publications, where
  ADR-0163 answered it with the list of components.
- A component may load its assets over several turns and report progress. The
  work no longer has to fit inside `wire`.
- A component that copies an asset with `asset` holds it twice, once in the
  store and once in its own memory; one that takes `asset_blob` holds it once.
- The MCP `load_component` and `spawn` tools start a module with assets in a
  running engine without any change to them. An `unpublish` tool is follow-on
  work.
- After a publish succeeds, a read of a catalogued asset cannot fail.
- The store stays in memory only. Backing asset blobs with disk, so that an
  unused asset costs no memory, would be a change to the blob store and is not
  decided here.

## Alternatives considered

- **Pass the module file from `load_component` to its spawn.** Fixes one
  harness tool and leaves the calls meaningless after `wire`, the nullable
  `Spawn.code`, and every in-engine spawner unable to bring the file.
- **Keep the whole module file for the module's life.** Makes the calls always
  work, but holds every file whole: bundles that share assets hold a copy
  each, and a kept asset blob holds its entire file.
- **Keep the window and have the publication hold the assets.** A spawn by
  name works, and the calls still trap in a handler.
- **Make `Spawn.code` required.** Removes the nullable field and removes spawn
  by name with it.
- **Embed assets in the code as data segments.** Always readable with no host
  call, but every asset sits in the instance's linear memory for its life and
  is copied again on each republish (ADR-0163, Alternatives considered).
- **Hand each instance a copy of the table when it is born.** Removes every
  asset host call, but each instance then holds every asset of its module for
  its whole life and carries the names in its own memory, and a kind manifest
  is not handed over this way.
- **Check the module file in as one blob and make each asset a range of it.**
  A publish hashes and copies nothing per asset, but a kept or mailed asset
  holds the whole file, and two modules with the same asset hold it twice. It
  can return as a publish optimization beneath the same table if per-asset
  check-in is measured to dominate a large bundle's publish.
- **Free a publication automatically when its last instance ends.** A
  publication has no instances between its publish and its first spawn, so
  this would withdraw it before it could be used.
