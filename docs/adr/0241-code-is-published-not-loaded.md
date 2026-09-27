# ADR-0241: Code Is Published, Not Loaded

- **Status:** Proposed
- **Date:** 2026-09-26

Builds on [ADR-0238](0238-engine-blob-store.md) (`Blob`, a value of
immutable bytes shared in process and written as bytes everywhere else),
[ADR-0230](0230-proven-actor-references.md) (actor paths name actors;
references prove them), and [ADR-0231](0231-protocol-typed-references-and-reply-checks.md)
(published contract rows, declared links, rows only grow). Supersedes the
hosting and naming half of [ADR-0099](0099-actor-identity-and-addressing.md)
and the decisions listed under [Superseded and amended](#superseded-and-amended).

## Context

A native actor's name says what it is: a root singleton is `NS`, a root
instance is `NS:key`, and a child is `parent/NS:key`
(`spawn/spawner/prepare.rs:91-98`). A wasm actor's name says who hosts it.
Every load spawns one Rust type, `WasmTrampoline`, whose namespace is
`aether.embedded`, as an instanced child of the singleton
`ComponentHostCapability` (`aether.component`). A loaded component is
`aether.component/aether.embedded:NAME`, and an inline child is
`<parent>/aether.embedded:SUB` (`host_fns.rs:428`). The component's own
`NAMESPACE` appears nowhere in its address; it is only the default `NAME`
(`component/runtime/load.rs:304-308`).

Everything built this week to type the actor graph has had to route around
that split:

| Mechanism | Workaround the split forced |
|---|---|
| `ActorPath<R>` / `link_child` (#6853) | writes `parent/C::NAMESPACE:key`, which never names a guest child |
| Route actor type (#6850, stopped) | a `Native` / `Guest` / `Untyped` tag on every route, to recover the identity a guest's name does not carry |
| `Embedded` / `EmbeddedMany` resolvers | fold `aether.embedded:<NS>` beneath the runtime parent so a bare-type send reaches a component; replica 0 keeps the bare load name for this alone (`aether-kinds/src/lib.rs:607-617`) |
| Namespace ownership | claimed per Rust `TypeId` (`prepare.rs:116`), so every guest shares `WasmTrampoline` and no load can choose its namespace (ADR-0240 D4) |
| Checks on code | the contract check runs in the trampoline and again in the registry; the dependency check runs at load, boot, replace, and the module-wide inline check; #6851 adds the link check at load and replace |
| Replace | may change the hosted type (`export: Some(other)`), targets one instance, and compiles outside `ModuleCache` |
| Drop | vacates the slot but keeps the route `Live` and the name taken forever (`SubnameInUse`) |
| Short paths | `aether.component/:NAME` works only because `WasmTrampoline` is the sole instanced child of the host; guest lineage never feeds the address index |

Meanwhile the engine already has the machinery code needs. ADR-0238's `Blob`
is content-addressed, deduplicated, shared by hash in process, written as
bytes on every other path, and checked back in on arrival. `ModuleCache`
(`component/runtime/module_cache.rs:31-55`) already keys compiled modules by
a hash of their bytes. What is missing is a model in which code is a value
the engine holds, not a child that a host actor spawns.

## Decision

### 1. A guest is a native actor whose handlers run wasm

A wasm actor behaves exactly as a native actor does. Its name, spawn,
placement, `depends`, `links`, contract rows, lifecycle hooks, replace, and
refusals are the native ones. The only difference is where a handler body
runs: a guest's host forwards each dispatch into a wasm instance instead of
calling Rust. Nothing outside the host can observe which one runs.

There is no guest-only topology, no guest-only resolver, and no route field
that says "guest". `WasmCtx` stays as the in-wasm SDK; its verbs become host
functions that call the same operations `NativeCtx` calls.

### 2. Code is a `Module`: a compiled cache entry made from a `Blob`

Code arrives as a `Blob` of wasm bytes. The engine keeps one **module cache**
keyed by that blob's hash. Checking a blob in derives everything the engine
needs from it once, and then lets the bytes go:

```rust
pub struct Module {
    hash: BlobHash,                   // identity: the hash of the wasm bytes it was made from
    compiled: Arc<wasmtime::Module>,  // compiled once per hash per engine
    manifest: Arc<ModuleManifest>,    // parsed once: exports, rows, depends, links, lineage, boot, kinds
    assets: Arc<[(AssetName, Blob)]>, // each `aether.asset.*` section, checked in as its own blob
}
```

The wasm bytes are used once, to compile and to parse, and are not retained.
Nothing re-parses a section per load or per replace: every reader today that
re-reads the bytes (a replace's predecessor kinds and boot namespace, the
inline contracts, the asset window) reads the manifest or an asset blob
instead. An entry lives while anything holds it: a publication, a running
instance, or a held `Module`.

A `Module` never leaves its engine. Compiled code is tied to the engine's
wasmtime version, configuration, and target, so the portable form of code is
its source bytes, and those live with whoever supplied them: the hub's store,
the Bloomery journal, a package file. Standing code up on another engine is a
`Publish` there from that source (§9).

The hash is the ADR-0238 blob hash (BLAKE3). The hub's store and the
Bloomery journal keep their own content keys for their own records and
verify bytes against them before check-in; neither key is the engine's code
identity.

### 3. Publishing binds namespaces to a module

The engine keeps one **publication table**: `NS → (Module, group)`. At most
one implementation is published per namespace per engine.

- **Native code publishes at boot.** Each native actor's link-time inventory
  entry is its publication, with its `Dispatch::capabilities()` as its rows.
  It has no blob; its code is the binary.
- **A module publishes as one set.** A publish admits every namespace the
  module exports, all or nothing. Its private and inline child types are not
  published: they belong to the module and cannot be spawned from outside it.
- **A guest never publishes over a native namespace.** A namespace the
  binary published is refused to every module.
- **Republishing** points a module's namespaces at a new module. The set of
  exported namespaces may grow; a namespace with live instances may not
  disappear.

The table is an engine system owned by the registry owner, which already
applies route contracts (`RepublishContract`). It is not an actor and has no
address.

### 4. One admission check runs at publish

Every check that today runs at load, boot, replace, or resolve collapses into
one admission step when a module is published:

| Check | Rule | Replaces |
|---|---|---|
| Namespace | each exported NS is unpublished, or published by this module's predecessor; never native | `try_claim_namespace` by `TypeId`; ADR-0240 D4 |
| Contract growth | for a republish, each NS's rows only grow and a fallback is kept (`first_contract_break`) | trampoline `check_contract` and the registry `RepublishContract` guard (ADR-0231 §5) |
| Same type | a namespace's implementation is replaced only by the same namespace | `ReplaceComponent.export: Some(other)`; #6850's replace refusal |
| Dependencies | every `depends(R)` names a published `R` | the load, boot, replace, and module-wide inline checks |
| Links | every `links(R)` record's rows are covered by `R`'s published rows | #6851's load and replace checks |
| Kinds | the module's kinds register in the same owner batch | `RegistryBatch::register_kinds` at load |

Admission is static: it reads manifests, never a live instance. It refuses
the whole publish with the first failing namespace and rule. Liveness is not
an admission question: `depends(R)` still requires `R` live when an instance
is spawned (ADR-0230), and `resolve` still proves a path on receipt
(ADR-0231 §3).

### 5. Actors are named by what they are, on either runtime

A published guest type spawns with the native verbs and gets the native
names: `NS`, `NS:key`, or `parent/NS:key`. Its cardinality (singleton or
instanced) and placement (`root`, `child_of(P)`) come from its `#[actor]`
declaration, recorded in the manifest, exactly as a native type's come from
its derive.

- `Embedded`, `EmbeddedMany`, `CallerScope::Parent`, and the `aether.embedded`
  namespace retire. A wasm `#[actor]` gets `One` or `Many`, like a native one.
- A bare-type `ctx.send::<R>` reaches a root singleton `R`, as it does for a
  native `R`. `depends(R)` names a root singleton, as ADR-0230 already
  requires of native dependencies.
- An inline or private child is `parent/<child NS>:key`. It is still hosted
  in its parent's instance, and its contract rows come from the parent's
  module.
- Several instances of one component are `NS:key1`, `NS:key2`. MCP and
  package `replicas` become N spawns of one namespace; the `base-i` load
  names retire.
- `link_child::<P, C>` (#6853) is correct for every child, because every
  child is `parent/C::NAMESPACE:key`.
- The short-path index (ADR-0166) reads the publication table, so a hole
  covers guest children once their parent's module is published.
  `aether.component/:NAME` has no successor; a guest is addressed by its own
  namespace.

### 6. A spawn shares the compiled module

Spawning an instance of a published guest type looks up its `Module` and
starts a host on it, sharing the compiled code with every other instance of
that hash.

The host is one native actor type, parameterised by the published group, that
owns one wasm instance and forwards dispatch into it. It is the trampoline
without its topology: it has no namespace of its own and is never an address
parent.

### 7. Replace is a republish

Replacing code publishes a new `Module` under the same namespaces. After
admission passes, every live instance of each republished namespace swaps
to the new module with the existing sequence: instantiate the new guest
before touching the old, `unwire`, `on_dehydrate`, carry the correlation
cursor and reply table, `on_rehydrate`. Routes, names, and mailbox ids never
change, and the hosted type cannot change because the namespace is the type.

A rehydrate failure in one instance restarts that instance on the new module
from `init`, with its state dropped and a `MonitorNotice`, so that every
instance of a namespace always runs the published module.

### 8. Despawn and unpublish are separate

- **Despawn** ends one instance: its route goes `Dropped` and its key may be
  spawned again. The permanent `SubnameInUse` / vacate-but-keep-`Live`
  behaviour of today's drop retires.
- **Unpublish** removes a module's publications. It is refused while any
  instance of its namespaces is live.
- A boot actor (ADR-0147) is a root singleton the module declares. It is
  spawned when the module is first published and despawned when the module
  is unpublished (open question 1).

### 9. The mail surface

Remote callers (MCP, RPC, the Bloomery driver, chassis autoload) publish and
spawn by mail, answered by the registry owner's engine mailbox: a front door
to an engine system, not an address parent.

- `Publish { code: Blob }` checks the bytes in, builds the `Module`, and runs
  admission. Publishing a module whose namespaces already point at the same
  hash is a no-op.
- `Spawn { namespace, key, parent, config }` asks for an instance to exist.
  If it is already live, the reply names it and nothing is re-initialised;
  if it is not, that is the signal to stand it up. It covers native and guest
  types alike: a native type is spawned from its boot-time publication, which
  gives outside callers a way to start a native instance by mail that they
  lack today.
- `Despawn { path }` ends one instance.

`LoadComponent` becomes a convenience that publishes and spawns in one call.
`ReplaceComponent` becomes `Publish` of a successor. `DropComponent` becomes
`Despawn`. `LoadResult.path` is the spawned actor's own canonical path.
Bloomery restart adoption (ADR-0226 D9) becomes a `Spawn` that finds its
instance live.

### 10. Guest crates split identity from runtime

A crate whose guest types other crates name splits them as ADR-0122 splits a
native capability. The identity half is always compiled: the `Addressable`
marker for each type, with its `NAMESPACE`, cardinality, placement, and
contract rows. The runtime half, behind a feature, holds the `#[actor]` impl
and `export!`. A caller names a guest type through its identity and never
links its code; the kind crates of ADR-0066 are where these markers live.

## Superseded and amended

| ADR | Status | What changes |
|---|---|---|
| 0010 runtime component loading | Accepted | §1, §3, §5: load, replace, and drop become publish, spawn, republish, despawn |
| 0038 actor per component | Accepted | §4 drop becomes despawn; §5 replace splices every live instance of a namespace |
| 0096 multi-actor modules | Accepted | §1, §3: a module publishes its export set; no export selector at load |
| 0097 sibling spawn | Accepted | §3, §4: a sibling is an ordinary spawn of a published or module-private type |
| 0099 identity and addressing | Accepted | §5 the `Embedded` fold and §6 `aether.embedded` superseded; the 2026-08-05 runtime-parent amendment superseded |
| 0101 / 0016 / 0113 hooks | Accepted | hooks run per instance on a republish; a failed rehydrate restarts the instance |
| 0114 inline children | Accepted | D2 the child is `parent/<child NS>:key`; D5 rebuilt from the republished module |
| 0119 resolver strategies | Accepted | `Embedded` and `EmbeddedMany` retire |
| 0138 opt-in default entry | Accepted | moot: every spawn names its namespace; `aether.no_default` retires |
| 0147 module boot | Accepted | §1, §2, §4: boot is spawned at first publish and despawned at unpublish |
| 0166 lineage and short paths | Accepted | §5, §6: the component-host worked example retires; the index reads publications |
| 0165 | Accepted | line 206: guests are hosted by the forwarding host, not `WasmTrampoline` |
| 0224 / 0226 / 0240 | Proposed | a bundle's root is a published namespace; 0226 D9 adoption keys on a live `NS:key`, not `SubnameInUse`; 0240 D4, D5, D8 edited in place |
| 0230 / 0231 | Proposed | edited in place: no route actor-type tag; the link check and replace growth move to admission |
| 0238 blob store | Proposed | no decision changes: code arrives and leaves as a `Blob`; a module's assets are blobs |

## Consequences

### Positive

- One actor model. A path names what an actor is on either runtime, and
  every typed verb (`link`, `link_child`, `resolve`, `depends`, `send`) works
  the same for both.
- One admission check replaces checks spread over the component host, the
  trampoline, and the registry, and it runs once per publish instead of per
  load and per replace.
- Code is parsed and compiled once per hash per engine, and its bytes are not
  held after that; a module's assets are blobs shared like any other.
- The hosted type cannot change on replace, by construction.
- The Bloomery driver publishes bundles it already fetches as `Blob`s from
  the journal, with no separate path.

### Negative

- A large migration: `aether.component/` appears on 78 lines in 41 crate
  files and `aether.embedded` on 111 lines in 53, plus the guide, CLAUDE.md,
  the MCP tools, and both harnesses.
- A guest crate that other crates name gains an identity/runtime split like
  ADR-0122's (§10).
- A republish touches every live instance of a namespace, so its cost grows
  with the instance count.

### Neutral

- Generators stay a build concern; only their output sections are read.
- ADR-0137 behavior scripts are not actors and keep their own `wasmi` path.

## Migration

Each step lands on its own:

1. **Module cache**: `Module` built from a `Blob`, compiled and parsed once
   per hash, assets checked in as blobs; `ModuleCache` and every section re-parse move onto it.
2. **Publication table and admission**: native publications at boot; module
   publish with the §4 checks; #6851's link record becomes the manifest's
   links section.
3. **Forwarding host and native naming for guests**: guests spawn as
   `NS` / `NS:key` / `parent/NS:key`; `Embedded` retires.
4. **Republish replaces replace**; despawn and unpublish replace drop.
5. **Surface**: MCP, RPC kinds, SubstrateHarness `load::<R>`, FleetHarness,
   CLAUDE.md, the guide.
6. **Deletion**: `ComponentHostCapability` as an address parent,
   `WasmTrampoline`'s namespace, `aether.embedded`, `categorise_mailbox_name`'s
   component category, and the component short path.

In-flight work: #6852 and #6837 land unchanged; #6851 lands its record and
moves its check into step 2; #6850 is withdrawn; #6829 is re-planned on
step 3.

## Open questions

1. **Boot actors.** Spawn at first publish and despawn at unpublish
   (recommended), or fold boot into ordinary root singletons the module
   declares and let callers `Spawn` them?

## Alternatives considered

- **Keep the host, fix the names.** Name guests `NS:key` under
  `aether.component`. Rejected: a path would still name the host, and the
  host would still be a second actor model with its own checks.
- **A loader actor without a parent.** Rejected: code is a value the engine
  holds, like a blob; an actor in front of it adds a hop and a second place
  for checks.
- **Tag routes with their runtime (#6850).** Rejected: it recovers, at every
  reader, an identity the name should carry by construction.
- **Keep per-load and per-replace checks.** Rejected: the same rules would
  run in four places on every load, instead of once at publish.
