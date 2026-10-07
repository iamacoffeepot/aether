# ADR-0250: A Component Reads Its Assets Whenever It Runs

- **Status:** Proposed
- **Date:** 2026-10-07

Replaces the load window of [ADR-0163](0163-content-addressed-packages-and-asset-bundles.md)
§3 and §4, and the parts of [ADR-0241](0241-code-is-published-not-loaded.md)
§2 and §9 that carry a module's bytes on a spawn. This ADR is text only; the
engine change is its own issue. Until that change lands, ADR-0163 and ADR-0241
keep describing the code as it is; the change that implements this ADR edits
ADR-0241 §2 and §9 in place and marks ADR-0163 §3 and §4 amended.

Three terms are used throughout. A **module file** is the wasm bytes a publish
brings, code and asset sections together. An **asset** is the payload of one
`aether.asset.<path>` custom section in that file (ADR-0163 §2). A
**publication** is the binding of a namespace to a module (ADR-0241 §3).

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

The reasons ADR-0163 gave no longer carry the decision. Its worry about a read
failing at an arbitrary later time was about reading a file in the package
store on disk; a module file is now an immutable `Blob` in the engine's memory
(ADR-0238), and reading a range of it cannot fail. Its rule that nothing
payload-sized outlives the window was already given up when `asset_blob` let a
component keep the whole module file resident by holding one asset blob. What
is left is the memory the module file takes.

## Decision

### 1. A module keeps its file

`Module` (`crates/aether-substrate/src/actor/wasm/module/mod.rs`) holds the
`Blob` it was checked in from, beside its compiled code and manifest.
ADR-0241 §2 already says how long a module lives: while a publication, a
running instance, or a held `Module` holds it. The module file now lives
exactly that long.

### 2. The asset calls work in every hook

A component reads its assets from `init`, `wire`, every handler,
`on_rehydrate`, and `unwire`. The host serves the read from the instance's own
module. There is no window, no closed state, and no trap for a call made at
the wrong time. A name the catalog does not carry answers `None`, as today.

The guest surface is one trait in place of `AssetCatalog` and `AssetWindow`,
implemented by every wasm ctx:

```rust
pub trait Assets {
    fn assets(&self) -> &[AssetInfo];                      // names and lengths
    fn asset(&mut self, name: &str) -> Option<Vec<u8>>;    // copied into guest memory
    fn asset_blob(&mut self, name: &str) -> Option<Blob>;  // a view of the module file, no copy
}
```

This puts two verbs on the handler ctx that it does not have today. They are
the existing host calls with their restriction removed, not new operations.

### 3. A spawn brings no bytes

`Spawn` is `{ namespace, key, parent, config }`. Its `code` field is removed,
with the host's check that brought bytes match the publication. A spawn of a
published type always builds an instance that can read its assets, whoever
asks for it.

A republish needs no special case: the successor reads the new module's file
and the old guest reads the old module's file until it ends, because each
instance holds its own module.

### 4. What is removed

- `LoadWindow`, its `open` flag and optional source, and
  `ComponentCtx::load_window`.
- `Spawn.code` and the refusal for bytes that are not the published module.
- The code the `Autoloader` keeps for each boot entry between its publish and
  its spawns.
- The trap text that names "a spawn with its code, and a load" as the two
  doors.
- From ADR-0163: the load window (§3), "one door between cold and resident"
  (§4), and the absences "no runtime payload fetch" and "no instance-lifetime
  store pin". Nothing here pins the package store: the bytes held are the
  engine's own copy.

## Consequences

- A published module costs its file size in engine memory for as long as it is
  published or any instance of it runs. A component that copies an asset with
  `asset` holds that asset twice; one that takes `asset_blob` holds it once.
- ADR-0241 defers unpublish until memory held by dead publications is a
  measured cost. This decision raises that cost from compiled code alone to
  compiled code plus the file, so a world that publishes map squares as it
  moves needs unpublish. That is follow-on work this ADR creates.
- "What assets are resident" is answered by the list of publications, where
  ADR-0163 answered it with the list of components.
- A component may load its assets over several turns and report progress. The
  work no longer has to fit inside `wire`.
- The MCP `load_component` and `spawn` tools start a module with assets in a
  running engine without any change to them.
- `asset_blob` no longer changes how long a module file stays resident, so the
  guidance on choosing between the two verbs reduces to copy or no copy.
- Asset failures are no longer confined to load time, because there are none
  left: after a publish succeeds, a read of a catalogued asset cannot fail.

## Alternatives considered

- **Pass the module file from `load_component` to its spawn.** Fixes one
  harness tool and leaves the calls meaningless after `wire`, the nullable
  `Spawn.code`, and every in-engine spawner unable to bring the file.
- **Keep the window and have the publication hold the file.** A spawn by name
  works, and the calls still trap in a handler.
- **Make `Spawn.code` required.** Removes the nullable field and removes spawn
  by name with it.
- **Keep the module's hash and read the file from a store at each spawn.**
  Holds nothing while no instance runs, but an engine forked by the hub has no
  store to read from, and a read at spawn time can fail.
- **Have each instance hold the file and the publication hold none.** A type
  with no running instance is back to a publication that cannot be built.
