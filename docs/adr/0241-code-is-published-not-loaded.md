# ADR-0241: Code Is Published, Not Loaded

- **Status:** Proposed
- **Date:** 2026-09-26

Builds on [ADR-0238](0238-engine-blob-store.md) (`Blob`, a value of
immutable bytes shared in process and written as bytes everywhere else),
[ADR-0230](0230-proven-actor-references.md) (actor paths name actors;
references prove them), and [ADR-0231](0231-protocol-typed-references-and-reply-checks.md)
(published contract rows, rows only grow). Supersedes the
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
| `ActorPath<R>` child paths (#6853) | written as `parent/C::NAMESPACE:key`, which never names a guest child |
| Route actor type (#6850, stopped) | a `Native` / `Guest` / `Untyped` tag on every route, to recover the identity a guest's name does not carry |
| `Embedded` / `EmbeddedMany` resolvers | fold `aether.embedded:<NS>` beneath the runtime parent so a bare-type send reaches a component; replica 0 keeps the bare load name for this alone (`aether-kinds/src/lib.rs:607-617`) |
| Namespace ownership | claimed per Rust `TypeId` (`prepare.rs:116`), so every guest shares `WasmTrampoline` and no load can choose its namespace (ADR-0240 D4) |
| Checks on code | the contract check runs in the trampoline and again in the registry; the dependency check runs at load, boot, replace, and the module-wide inline check |
| Replace | may change the hosted type (`export: Some(other)`), targets one instance, and compiles outside `ModuleCache` |
| Drop | unloads the guest and vacates the mailbox (`NativeCtx::vacate`) without closing the trampoline, so the name never tombstones, a new load of it answers `SubnameInUse`, and a replace can refill the empty slot |
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
placement, `depends`, contract rows, lifecycle hooks, replace, and
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
    manifest: Arc<ModuleManifest>,    // parsed once: exports, rows, depends, lineage, boot, kinds
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
  entry is its publication. It has no blob; its code is the binary. The table
  records the namespace, the linked types that declare it, and which of them
  this engine has born there: its rows, `Dispatch::capabilities()`, already
  stand on every route it publishes at birth, and join the table with their
  first reader. Several linked types may share one native namespace, because
  a chassis composes exactly one of them (a capability and its headless stub,
  say). The engine's first birth at a native namespace holds it for the
  engine's lifetime, and a birth of any other type there is refused, so a
  second type composed at a held namespace fails the boot and with it the
  bootstrap (R-0046).
- **A module publishes as one set.** A publish admits every namespace the
  module exports, all or nothing. Its private and inline child types are not
  published: they belong to the module and cannot be spawned from outside it.
- **A content-addressed module publishes per build.** A module marked with
  the `aether.content_addressed` custom section publishes each namespace it
  exports as `NS.<hash>`, its BLAKE3 hash (§2) in 64 lowercase hex, so every
  build is its own publication and no build is another's predecessor or
  successor.
  - Inside the module each type keeps its declared `NS`: the export selector
    and the type tag read it.
  - The declared `NS` is at most 191 bytes, so the qualified name is one
    ADR-0166 segment.
  - Every Bloomery bundle is content-addressed. Its root publishes as
    `aether.bloomery.bundle.<hash>` and is spawned per unit as
    `aether.bloomery.bundle.<hash>:<unit key>`. Two units on one bundle share
    its publication. A unit moving to a new bundle spawns the new root and
    closes its old one; it never republishes.
  - Publications accumulate one per build (see Unpublish).
  - A content-addressed type has no typed path, because its `NAMESPACE` is
    not its published name. It is reached through the reference its spawn or
    load reply stamps.
- **A guest never publishes over a native namespace.** A namespace the
  binary published is refused to every module.
- **Republishing** points a module's namespaces at a new module. The set of
  exported namespaces may grow and never shrinks: a namespace, once
  published, stays published for the engine's lifetime, and its successor
  must export it. Otherwise a namespace with no live instance could drop out
  and return with fewer rows, and the rows-only-grow rule (§4) would hold
  only while an instance happened to be live.

The table is an engine system owned by the registry owner, which already
applies route contracts (`RepublishContract`). It is not an actor and has no
address.

### 4. One admission check runs at publish

Every check that today runs at load, boot, replace, or resolve collapses into
one admission step when a module is published:

| Check | Rule | Replaces |
|---|---|---|
| Namespace | each exported NS is not yet published, or published by this module's predecessor; a republish exports every NS its predecessor did; never native. A shared native namespace is selected by composing one of its types, and a second type's birth there is refused (§3) | `try_claim_namespace` by `TypeId`; ADR-0240 D4 |
| Contract growth | for a republish, each NS's rows only grow and a fallback is kept (`first_contract_break`), and each private child type the predecessor declares is still declared, privately or as an export, with rows that only grow | trampoline `check_contract` and the registry `RepublishContract` guard (ADR-0231 §5); #6845's unchecked inline-child rows |
| Same type | a namespace's implementation is replaced only by the same namespace | `ReplaceComponent.export: Some(other)`; #6850's replace refusal |
| Dependencies | none at publish: `depends(R)` is checked when an actor is stood up (ADR-0230), since a publish imports code and a module cannot say which of its actors will be stood up | nothing; the spawn-time checks stay |
| Kinds | the module's kinds register in the same owner batch | `RegistryBatch::register_kinds` at load |

Admission is static: it reads manifests, never a live instance. It refuses
the whole publish with the first failing namespace and rule.

Admission checks no dependencies. A publish imports code, and a spawn stands
an actor up. A module cannot say which of its actors a given engine will
stand up, and leaving one unstood is fine, so requiring every dependency of
every actor it exports at publish would refuse code the engine never runs.
`depends(R)` is checked when an actor is stood up: it requires `R` live when
an instance is spawned (ADR-0230). Likewise `resolve` still proves a path on
receipt (ADR-0231 §3).

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
  module. Admission holds its rows to the growth rule (§4), so its alias's
  published rows stay true across a republish, and the swap (§7) publishes the
  successor's added rows.
- Several instances of one component are `NS:key1`, `NS:key2`. MCP and
  package `replicas` become N spawns of one namespace; the `base-i` load
  names retire.
- A typed child path, `ActorPath::<C>::child(&parent, &key)`, is correct for
  every child, because every child is `parent/C::NAMESPACE:key`.
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

A rehydrate failure in one instance restarts that instance in place on the
new module from `init`, with its state dropped and a `MonitorNotice`, so that
every instance of a namespace always runs the published module. The instance
does not close, so its name stays live.

### 8. An instance ends by closing, and its name tombstones

An instance ends one way: it closes, and its name tombstones for the
engine's lifetime (ADR-0079 §7). There is no verb that frees a name for
reuse.

This is how a native actor ends today. `NativeCtx::shutdown` flags the
actor; its dispatcher runs `unwire` and the close tail
(`finalize_close_and_fan_out`), where `ActorRegistry::close_actor` moves the
slot `Live` → `Dead`, adds the id to the tombstones, and sends each watcher a
`MonitorNotice`. Mail to the name then drops, and a later spawn of it is
refused with `SubnameRetired`. A guest is a native actor (§1), so a guest
instance ends the same way.

Today's `DropComponent` closes nothing. The trampoline runs the guest's
`unwire`, drops the wasm instance, and calls `NativeCtx::vacate`, which sends
the same `MonitorNotice`s but leaves the trampoline `Live`: an empty slot
that `ReplaceComponent` can refill, whose name a new load cannot take
(`SubnameInUse`), and which tombstones only when the substrate stops. Under
this ADR, `DropComponent` becomes a request that the named instance close:
it runs `unwire`, closes, and its name tombstones. The empty refillable slot
and refill by replace retire with the trampoline, and `vacate` loses its one
production caller.

An inline child (ADR-0114) is an actor with a native name (§5), so it ends by
closing too and its name tombstones. Today `despawn_inline_child` retires the
child's alias route to `Dropped`, which lets the same alias be published
again; that reuse retires.

A boot actor (ADR-0147) is a root singleton the module declares. The engine
spawns it once, when the module is first published, and a republish swaps it
like any other instance (§7). It is not refcounted against the module's other
actors and is never torn down with them. If it closes, its name tombstones
and it does not return.

### 9. The mail surface

Remote callers (MCP, RPC, the Bloomery driver, chassis autoload) publish and
spawn by mail, answered by the registry owner's engine mailbox: a front door
to an engine system, not an address parent.

- `Publish { code: Blob }` checks the bytes in, builds the `Module`, and runs
  admission. Publishing a module whose namespaces already point at the same
  hash is a no-op. Its reply names each namespace it bound, so a caller of a
  content-addressed module never recomputes the hash.
- `Spawn { namespace, key, parent, config }` asks for an instance to exist,
  and the name decides the answer. A live name: the reply names it and
  nothing is re-initialised. An absent name: the engine stands the instance
  up. A tombstoned name: the spawn is refused, because the name is spent
  (§8). It covers native and guest types alike: a native type is spawned
  from its boot-time publication, which gives outside callers a way to start
  a native instance by mail that they lack today.

`LoadComponent` becomes a convenience that publishes and spawns in one call.
`ReplaceComponent` becomes `Publish` of a successor. `DropComponent` becomes
the close request: it asks the named instance to close, and its name
tombstones (§8). `LoadResult.path` is the spawned actor's own canonical path.
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
| 0010 runtime component loading | Accepted | §1, §3, §5: load and replace become publish, spawn, and republish; drop becomes a close request, and the closed name tombstones |
| 0038 actor per component | Accepted | §4 drop becomes a close request and the name tombstones; §5 replace splices every live instance of a namespace |
| 0079 instanced actors | Accepted | §7 now governs guests and inline children: a closed name tombstones; the §8 vacate amendment loses the component drop, its one production caller |
| 0096 multi-actor modules | Accepted | §1, §3: a module publishes its export set; no export selector at load |
| 0097 sibling spawn | Accepted | §3, §4: a sibling is an ordinary spawn of a published or module-private type |
| 0099 identity and addressing | Accepted | §5 the `Embedded` fold and §6 `aether.embedded` superseded; the 2026-08-05 runtime-parent amendment superseded |
| 0101 / 0016 / 0113 hooks | Accepted | hooks run per instance on a republish; a failed rehydrate restarts the instance |
| 0114 inline children | Accepted | D2 the child is `parent/<child NS>:key`; D5 rebuilt from the republished module; the 2026-07-08 `despawn_inline_child` becomes a close, and the name tombstones |
| 0119 resolver strategies | Accepted | `Embedded` and `EmbeddedMany` retire |
| 0138 opt-in default entry | Accepted | moot: every spawn names its namespace; `aether.no_default` retires |
| 0147 module boot | Accepted | §1: boot is spawned once, at the module's first publish, and is no longer refcounted or torn down with the module's other actors; if it closes, its name tombstones. §2, §4: the `default` slot is moot, since every spawn names its namespace |
| 0166 lineage and short paths | Accepted | §5, §6: the component-host worked example retires; the index reads publications |
| 0165 | Accepted | line 206: guests are hosted by the forwarding host, not `WasmTrampoline` |
| 0224 / 0225 / 0226 / 0240 | Proposed | every bundle is content-addressed: its root publishes as `aether.bloomery.bundle.<hash>` and is spawned per unit as `aether.bloomery.bundle.<hash>:<unit key>`; 0226 D2 a unit moving to a new bundle closes its old root; 0226 D9 adoption keys on a live `NS:key`, not `SubnameInUse`; 0224 §5, 0225 §1 and §8, and 0240 D4, D5, D8, and D9 edited in place |
| 0230 / 0231 | Proposed | edited in place: no route actor-type tag; replace growth moves to admission |
| 0238 blob store | Proposed | no decision changes: code arrives and leaves as a `Blob`; a module's assets are blobs |

## Consequences

### Positive

- One actor model. A path names what an actor is on either runtime, and
  every typed path and verb (`ActorPath`, `resolve`, `depends`, `send`) works
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
- A closed name is spent. A caller that wants a fresh instance after a close
  picks a new key, and each tombstone costs one registry entry for the
  engine's lifetime (ADR-0079 §7).
- A dead publication, one nothing will spawn again, stays resident, because
  there is no unpublish (see Alternatives considered). Every built Bloomery
  bundle adds one, because every bundle is content-addressed (§3).

### Neutral

- Generators stay a build concern; only their output sections are read.

## Migration

Each step lands on its own:

1. **Module cache**: `Module` built from a `Blob`, compiled and parsed once
   per hash, assets checked in as blobs; `ModuleCache` and every section re-parse move onto it.
2. **Publication table and admission**: native publications at boot, by
   namespace; module publish with the namespace, contract-growth, and kind
   checks, in one registry-owner batch the component host stages on every load
   and replace; content-addressed modules. Admission runs beside the per-site
   checks: the same-type rule
   lands, and the trampoline's `check_contract` and the `RepublishContract`
   guard retire, with step 4. `try_claim_namespace` by `TypeId` has retired:
   each native birth holds its namespace in the publication table (§3), and
   dependencies stay a stand-up check (§4). The `Publish` mail door (§9) and the module
   cache's move to the registry owner land with step 5, when a remote caller
   first publishes by mail.
3. **Forwarding host and native naming for guests**: guests spawn as
   `NS` / `NS:key` / `parent/NS:key`; `Embedded` retires.
4. **Republish replaces replace**; `DropComponent` closes the instance and
   its name tombstones; `despawn_inline_child` closes the child the same way;
   the boot refcount and teardown retire.
5. **Surface**: MCP, RPC kinds, SubstrateHarness `load::<R>`, FleetHarness,
   CLAUDE.md, the guide.
6. **Deletion**: `ComponentHostCapability` as an address parent,
   `WasmTrampoline`'s namespace, `aether.embedded`, `categorise_mailbox_name`'s
   component category, and the component short path.

In-flight work: #6852, #6837 (#6856), and #6857 (#6860) have landed; #6858
is being re-scoped; #6850 and #6851 are withdrawn; #6829 is re-planned on
step 3.

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
- **A despawn verb that frees a key for reuse.** Rejected: names tombstone
  on close and are never reused (ADR-0079 §7), so a path proven once cannot
  later name a different actor.
- **One bundle namespace, exempt or per unit.** Rejected. Exempting bundle
  roots from the one-implementation rule breaks it, and a per-unit namespace
  refuses a rebuild that drops a program as a republish.
- **Unpublish.** Deferred. Publications are content-addressed and a
  republish already points a namespace at new code, so the one thing an
  unpublish would add is reclaiming the memory of a dead publication, one
  nothing will spawn again. Revisit when memory held by dead publications
  becomes a measured cost.
