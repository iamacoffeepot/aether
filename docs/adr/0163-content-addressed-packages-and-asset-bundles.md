# ADR-0163: Content-Addressed Packages and Asset Bundles

- **Status:** Proposed
- **Date:** 2026-07-22

## Context

Aether needs a shippable form: an artifact a store platform (Steam is the concrete target) can install on a player's machine and keep up to date. Steam's content system converges an install toward a manifest of chunk-hashed files, deduplicating at ~1 MB chunk granularity across builds, so the layout that ships smallest deltas is loose content-addressed files under a small manifest — and the layout that ships worst is a monolithic archive repacked every build.

Three packaging surfaces exist today and none can be that artifact:

- The bundle-pack (`aether-chassis/src/bundle_pack.rs`, embedded by `aether-chassis-bundle` via `include_bytes!`) is a positional concatenated blob carrying component wasm and config bytes inline. Its own module doc scopes it to build-time only — "nothing on disk outlives a build" — and names its expiry condition: revisit if it ever becomes a persisted artifact. Shipping is that condition arriving.
- `dist/manifest.json` (`cargo xtask dist`) is a name-keyed map of loose files with no hashes; it exists for the fleet harness.
- The hub's artifact store (ADR-0115, ADR-0116; `ContentStore` per ADR-0149) is content-addressed sha256 objects with a name-to-hash index — the right shape, but a live service directory rather than a distributable layout.

Separately, aether has no asset container. The engine's terminal asset formats are already decided by the kind vocabulary — `aether.render.create_texture` takes raw RGBA8 pixels, `aether.audio.load_instrument` takes SFZ+WAV, and so on (ADR-0103, ADR-0105) — and the established pattern for everything above that line is importers-as-actors: `aether.kit.mesh` parses DSL and OBJ in userspace and replays triangles; `aether.text` ingests TTF and emits textured quads. A game needs a way to ship asset bytes alongside the component that knows what they mean, without the substrate learning any file format.

Forces on the asset side, established by measurement and by prior decisions:

- Bytes embedded in wasm data segments cost 1:1 file size (measured: 37 bytes of overhead on a 1 MiB payload) and are cheap at sprite scale, but a `'static` data segment is copied into linear memory and pinned there for the instance's life — unacceptable as the default for audio- and texture-scale payloads.
- Custom sections are ignored by wasm execution semantics: present in the file, never instantiated, never in linear memory. The `aether.kinds` section (ADR-0028, ADR-0032) already ships component metadata this way, parsed host-side at load.
- A payload-fetch surface that stays open for the instance's life couples every loaded component to the liveness of its backing store entry. A file evicted at hour three fails a code path nobody has exercised since boot, at a moment attributable to nobody. Failure surfaces belong at load, where failing is loud, cheap, and expected (the same fail-fast stance as config-at-init, ADR-0090).
- Retention policy cannot live in the engine. A cache is a bet about access patterns, and the engine cannot know whether a bundle is a HUD atlas touched every frame or a dialogue set touched once; whichever eviction policy the engine picked would be wrong for someone and grow knobs. The actor owning the assets owns the access pattern, so the actor decides what survives.

## Decision

### 1. The package: a materialized content store under one manifest

The shippable artifact is a directory holding the chassis binary, content-addressed objects, and one persisted manifest:

```text
<package>/
  aether-substrate            # chassis binary — the platform updates it
  pack/manifest               # the one manifest
  pack/objects/<sha256>       # component wasm and config bytes, immutable
```

The manifest is a persisted, versioned artifact (this ADR is the "revisit" the bundle-pack doc called for). It carries the chassis settings and a list of entries, each referencing objects by hash:

```rust
pub struct PackageManifest {
    pub settings: ChassisSettings,       // title, window_mode, tick_hz
    pub entries: Vec<PackageEntry>,
}

pub struct PackageEntry {
    pub object: Sha256,                  // -> pack/objects/<hash>
    pub config: Option<Sha256>,          // config bytes are objects too
    pub name: Option<String>,
    pub export: Option<String>,
    pub replicas: Option<u32>,
}
```

Identity is the content hash everywhere; a name is a label, never a key. The chassis boots by resolving manifest references against the local object store instead of receiving inline bytes. Deletion is manifest-shape: a file exists because the manifest references it. Platform update, integrity verify, and repair all reduce to converging the disk toward the manifest by hash — the platform's own machinery does this for the base install, and aether ships no updater.

The package is single-channel in this ADR. Object lookup is written as an ordered walk over a list of stores that today has one entry, so a later overlay channel (mods, server-pushed content) is a list append, not a redesign — but no layering machinery is built now.

### 2. Assets ride in wasm custom sections

A component that carries assets declares them at build time:

```rust
export_asset!("sprites/slime.png");

// expands to:
#[used]
#[link_section = "aether.asset.sprites/slime.png"]
static __AETHER_ASSET_0: [u8; 4823] = *include_bytes!("sprites/slime.png");
```

The bytes land in a custom section named for the asset — the same emission path as the `aether.kinds` section — so they exist in the `.wasm` file but are never instantiated into linear memory and are not addressable from guest code. A bundle stays one hash-named object in the store: one file, one identity, chunk-friendly.

### 3. Payload access is a load window; the catalog is for life

At component load, `aether-component` indexes `aether.asset.*` sections alongside the existing kinds parse, recording per-asset name, length, sha256, and byte range into the module file. Payload access is served only during the load window — `init` plus `wire` — by reading the recorded range straight from the store file:

```rust
pub struct AssetInfo { pub name: String, pub len: u64, pub sha256: [u8; 32] }

pub trait AssetCatalog {                      // on every ctx, for the instance's life
    fn assets(&self) -> &[AssetInfo];
}

pub trait AssetWindow: AssetCatalog {         // implemented only by the init/wire ctx
    fn asset(&mut self, name: &str) -> Option<Vec<u8>>;
}
```

`wire` takes a window-bearing context (`WireCtx`, dereferencing to `WasmCtx` and implementing `AssetWindow`) so "fetch later" is a compile error rather than a runtime surprise. When `wire` returns, the host drops the asset index and the payload path is gone: no hostcall remains, no cache exists, and the store pin taken for the load is released — store eviction can never strand a live actor, by construction. The catalog (names, sizes, hashes — a few hundred bytes) stays queryable for the instance's life and surfaces through `describe_component`, so tooling answers "what does this bundle carry" without executing anything.

The actor pulls bytes through the window, transforms them into engine residents (`create_texture`, `load_instrument`, replayed geometry) or into its own state, and drops the rest. What survives the window is whatever the actor chose to keep — in practice handles and layout tables, not payload bytes.

### 4. Residency is lifecycle

There is one cold tier — `pack/objects/<sha256>` — and one door between cold and resident: the load window.

```text
cold    pack/objects/<sha256>     the platform converges it; verify-by-hash repairs it
          |
          |  load window (init + wire) — the only door bytes pass through
          v
warm    actor state               what the actor chose to keep (layouts, handles)
          |
          v
hot     engine residents          texture_id / instrument_id / replayed geometry
```

- **Unload** is destroying handles, or dropping the whole component — the same decision at coarser grain. A bundle is exactly the set of assets that live and die together.
- **Reload and recovery** walk back through the door: `load_component`, or `replace_component` with the component's own selector, with `on_dehydrate` / `on_rehydrate` (ADR-0101) carrying live state across the swap. Recovery from a mismanaged window is one deliberate, visible re-wire, never a side channel.
- **The census is the component list.** "What assets are resident" and "what components are loaded" are the same question. Bundle actors uphold the invariant by symmetric teardown — `unwire` destroys what `wire` created — and the reference bundle actor bakes that convention in, since every future bundle starts as a copy of it.
- **Content too large to be resident together is mis-granulated.** The fix is structural: split the bundle, and let component lifecycle be the paging mechanism, fleet-visible.

Deliberate absences, each a decision: no runtime payload fetch (dead-data hazard at arbitrary times), no engine-side asset cache (the wrong bet for someone, always), no instance-lifetime store pin (no liveness coupling), no extracted-asset cache files (no second copy to keep consistent), no standalone asset container format (the kind vocabulary is the terminal format; meaning-of-bytes is userspace), and no patch code in content (packages are inert objects under a manifest).

## Consequences

- The Steam depot is the package directory uploaded verbatim; updates delta at chunk granularity because unchanged objects hash identically. Aether owns zero download, update, or repair code for the base install.
- The failure surface for asset access collapses to load time, where `LoadResult::Err` already reports loudly. No environmental failure mode exists after `wire` returns.
- Retention frameworks become userspace: an asset-server actor with an LRU in its own state is a legitimate pattern, adoptable per game, replaceable without touching the substrate.
- `wire` changes signature to take `WireCtx`. This is the one breaking change to the existing actor surface.
- Authors must uphold `unwire` symmetry or the census over-reports; the engine does not enforce it. The reference bundle actor is the enforcement-by-example.
- Load work is front-loaded: everything a component will ever need from its payload must be pulled and transformed inside the window. Sprite-scale bundles pay microseconds; large content must be granulated into separately loaded bundles.
- Follow-on work, each its own change: the manifest format and store-backed chassis boot; the `export_asset!` macro in `aether-actor-derive`; the section indexer, load window, and `WireCtx` in `aether-component` / `aether-actor`; the reference bundle actor in `aether-kit`; an `xtask` package target emitting the depot layout; retirement of the bundle-pack's inline-bytes form (the standalone bundle binaries embed the package artifact instead).
- Implementation risk to verify first: `#[link_section]`-to-custom-section emission on the wasm target has sharp edges; the macro must be validated against the same toolchain path that emits `aether.kinds` today.
- Foreclosed while this ADR stands: runtime asset fetch surfaces, engine-owned asset caching, and a separate asset interchange format.

## Alternatives considered

- **Assets in data segments (`include_bytes!` in the component).** Measured overhead is zero and it is fine at sprite scale, but payload bytes are pinned in linear memory for the instance's life and re-copied on every hot swap; at audio/texture scale that is the wrong default, and two defaults is one too many.
- **Fetch-anytime payload hostcall with an instance-lifetime store pin.** Couples every live actor to store liveness, moving failure to an arbitrary later moment attributable to nobody; also makes pin persistence load-bearing. Rejected for the load window's single, attributable failure surface.
- **Engine-side asset cache / retention policy.** The engine cannot know access patterns; any policy is wrong for someone and accretes tuning knobs. Retention is the actor's state, backed for repeat reads by the OS page cache, which already exists and sizes itself.
- **A standalone asset container format with engine importers.** Grows the substrate a format at a time and forecloses userspace flexibility; the importers-as-actors precedent (`aether.kit.mesh`, `aether.text`) already carries this weight.
- **Extracted-asset cache files on disk.** Custom sections are contiguous plain ranges in an uncompressed file, readable in place; extraction buys a second copy with a consistency obligation and nothing else.
- **An append/tombstone patch log as the install state.** Order-dependent, vulnerable to partial application, and answerable only by replay. Manifest convergence makes state declarative and repair mechanical; ordered overlays return, deliberately, only if a second content channel ever exists.
