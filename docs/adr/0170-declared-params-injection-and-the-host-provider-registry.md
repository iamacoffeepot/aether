# ADR-0170: Declared Params Injection and the Host Provider Registry

- **Status:** Proposed
- **Date:** 2026-08-01

## Context

ADR-0156 split a wasm actor's init input in two. `Config` is what an operator could type — resolved argv > env > file > default and staged as bytes on the load mail. `Params` is the second, orthogonal channel: values the composer computes and hands in. On native the composer is a person at a `with_actor` call site, so `Params` is a live Rust value and a missing dependency is a compile error. On wasm there is no such person: the loader is the component host, reached over mail from a manifest, an MCP call, or the hub. ADR-0156 therefore shipped the channel empty — every component's `Params` is `()`, and the FFI passes zero bytes — and named the first real payload as follow-on work.

The case that names it is `replicas: N`. The fan-out is deliberate: one entry in a boot manifest, package manifest, or `load_component` call becomes N loads sharing one wasm and **one config**, each registered as `{base}-{index}` (issue 2626). Sharing the config is the contract — replicas are meant to be interchangeable — which is exactly why an instance has no principled way to learn which one it is. The index exists at the fan-out site and nowhere else: what reaches the component host is N independent `LoadComponent` mails that differ only in a name. A component that wants to shard work by index today can only parse its own name, which is a string convention masquerading as an interface.

The general shape behind that case is broader than replicas. There is a class of values a component needs that no operator can type and no composer is present to supply: its instance name, its mailbox id, its engine, its position in a fan-out. ADR-0156's decision rule sorts them — operator-typable is `Config`, and an actor reference resolved at Wire is addressing, not construction — but leaves a third bucket unserved: host-derivable facts, known at load, that the component wants before it runs.

Two other forces bear on the design. The `aether.kinds.inputs` custom section (ADR-0033, ADR-0090, ADR-0096) is already the channel through which a component tells the host what it is: its handlers, its fallback, its config kind, its exported types. And ADR-0090's posture on a bad known value is that boot aborts loudly rather than degrading — a silent default is the failure mode the config system was built to eliminate.

## Decision

A wasm actor's `Params` **declares** the host facts it needs; the component host is the container that supplies them; the host validates the declaration against a provider registry before it instantiates anything.

**1. A `Params` type declares per-field requests.** `#[derive(InjectedParams)]` on a struct whose every named field carries `#[param("<kind name>")]` and is typed as the kind it requests:

```rust
#[derive(aether_actor::InjectedParams)]
struct ShardParams {
    #[param("aether.component.replica_identity")]
    replica: ReplicaIdentity,
}
```

The field's type is what determines the request; the literal is a readable restatement, held to the type by a const assertion, so a spelling that drifts from `<FieldTy as Kind>::NAME` fails to compile at the declaration rather than at some later mismatch. `Params` itself is **not** a wire kind — only its fields are. That is what lets one `Params` gather facts from unrelated kind families without a wrapper kind existing for the combination, and it is why `WasmActor::Params` is bounded by `InjectedParams` rather than by `Kind + Default`.

**2. The requests ride the manifest.** `#[actor]` walks the declared `Params`' `REQUESTS` at const-eval time and writes one `InputsRecord::ParamRequest { id, name, field }` per request into `aether.kinds.inputs`, beside the handler, fallback, component-doc, and config records. The variant is appended, so a module that declares no `Params` emits nothing and decodes byte-identically under the existing reader — the same additive reasoning ADR-0096's `ActorBoundary` used, and for the same reason no section-version bump is needed. The records lift into `ComponentCapabilities.params`, which `describe_component` surfaces as the component's requires-list.

**3. The component host owns a provider registry.** A `ParamProviderRegistry` maps `KindId` to `fn(&LoadContext<'_>) -> Vec<u8>`. It is composer-supplied — it rides `ComponentHostParams` beside the wasmtime handles — and seeded by `ParamProviderRegistry::with_substrate_facts()`, which every chassis composes and a chassis that knows more extends. **Duplicate registration is an error**, propagated to boot: two composers claiming one kind must not be resolved by declaration order.

Providers are pure reads of `LoadContext` — the resolved instance name, the instance's own lineage-folded mailbox id, and its replica identity. No I/O, no mail, no clock. The signature says so rather than the documentation: a bare `fn` pointer cannot capture, so a value a provider could only obtain by doing work is unreachable from one. A fact a chassis knows but the context does not belongs on `LoadContext`, not in a capture.

**4. Every request is required, and validated before instantiation.** There is no `Option` inject-if-available form. A component whose behaviour silently differs by host is the footgun the whole config arc has been eliminating; a value that arrives is a value that is there. The component host validates the selected actor's whole requires-list against the registry in `prepare_load` — before any trampoline is spawned, long before the guest runs — and a request nothing provides is a clean `LoadResult::Err` naming the kind and the field. `replace_component` re-validates against the *replacement* module's requests before draining the live instance, so a replacement asking for a fact the chassis lacks leaves the running component untouched.

**5. The bag crosses the FFI beside config, backward-compatibly.** The host ships a wire-encoded `Vec<ParamEntry>` — kind-tagged byte entries, one per request, in declaration order. `export!` emits `init_with_params_p32(mailbox_id, ptr, config_len, params_len)` and `init_typed_with_params_p32(…)`; the host writes `[config][params]` back to back into one delivery region, so both channels cost one ADR-0095 placement and the added arity is a single length. The substrate probes widest-first — params-bearing, then config-only, then the legacy shapes — and the config-only exports remain emitted, forwarding with `params_len = 0`. A component with no requests ships **zero** params bytes, not an encoded empty vec, so its path is byte-identical to before. A guest with no params export and a non-empty bag is a clean boot error rather than a silent drop.

**6. The guest constructs `Params` before `init`.** The generated `from_entries` looks each field up by kind and decodes it, so `init` receives a whole value or the load fails — constructor injection, never a partially-filled struct. A missing or undecodable entry stages its field and kind through `init_failed_p32`, surfacing as `LoadResult::Err`.

**7. The first provider is `ReplicaIdentity { index, count }`.** `LoadComponent` gains `replica: Option<ReplicaIdentity>`, stamped by each fan-out site — `expand_replicas` for manifest and package boots, aether-mcp's `load_component` loop — because those sites are the only ones that know both halves. `None` is an unreplicated load, which the host reads as `{ index: 0, count: 1 }`: a single instance is replica 0 of 1, so a component sharding by index needs no am-I-replicated branch and can never see a zero count.

**8. Native actors are untouched.** Their composer is a person at a compile-time `with_actor` call site who hands them live values directly; a native cap declares no requests and its `capabilities()` reports an empty list.

## Consequences

- A component states what it needs from its host in one place, and the host answers before the component runs. The failure mode moves from "boots and misbehaves" to "does not load, naming the kind and field".
- `replicas: N` acquires a real interface. Instances stop being distinguishable only by a name string, and a sharded handler is `index`/`count` arithmetic rather than a parse.
- `describe_component` reports the requires-list, so an agent reads what a component demands of a chassis before trying to load it there.
- The registry is the chassis's public surface for the facts it knows. Adding one is a `register::<K>` call plus, if the fact is not already on `LoadContext`, a field there; the duplicate check keeps two composers from disagreeing about a kind's meaning.
- `LoadComponent` and `AutoloadComponent` gain a field, so every construction site changes. They are all in-repo and the change is mechanical, but it is wide.
- An inline child (ADR-0114) is constructed in-process by its parent, not by the component host, so it receives an empty bag: a request-bearing inline child fails its spawn. Injecting into inline children is reachable — the parent would have to carry a registry — but is not built here.
- The `aether.kinds.inputs` reader gains a variant. An older substrate meeting a request-bearing component fails the section decode loudly, which is the established rebuild boundary for that section, not a new one.
- Provider purity is enforced by the `fn`-pointer signature, which forecloses a chassis provider that legitimately needs captured state. That is deliberate for v1; the escape is to widen `LoadContext`, which keeps the fact visible to every provider rather than hidden in one closure.
- Requested kinds get no `aether.kinds` retention static (unlike config kinds). A kind the host has a provider for is by definition a kind the host already knows, so retention would be redundant. A chassis that registers a provider for a kind only the guest defines would break that assumption and needs it revisited.

## Alternatives considered

- **A bare `#[param]` with no kind literal** (the field type alone declares the request) — strictly less to write and impossible to spell wrong, but the declaration then reads as an ordinary field and a reader must resolve the type to see that it is a host request. The literal plus a const assertion buys legibility at no correctness cost; rejected on readability, not on safety.
- **`Option<T>` fields as inject-if-available** — makes a component's behaviour depend on which chassis loaded it, discoverable only at runtime and only sometimes. Rejected as the footgun the required-request rule exists to prevent.
- **Making `Params` itself a wire `Kind`** — the ADR-0156 bound, and the obvious symmetry with `Config`. It forces a nominal kind to exist for every combination of facts a component wants, and puts a schema on a struct that is never sent anywhere. Rejected.
- **Deriving the replica index from the `{base}-{index}` name** — needs no new field on `LoadComponent` and no fan-out change, but makes a naming convention load-bearing and gives no count. Rejected.
- **Boxed `Fn` providers** — permits a chassis to close over a value it resolved at Compose, at the cost of making "pure read of load-time context" a documented convention rather than a type-level fact. Rejected in favour of widening `LoadContext` when a fact is genuinely needed.
- **Evaluating providers at the cap rather than in the trampoline** — simpler (validation and evaluation in one place), but the instance's own mailbox id does not exist until its spawn has an identity, so `LoadContext` would lose lineage. Validation stays at the cap; evaluation moved to the trampoline, still before instantiation.
- **A second custom section for requests** — keeps `aether.kinds.inputs` untouched, but a component's requests are part of what it declares about itself, which is exactly what that section is. Rejected as a parallel channel for one record type.
