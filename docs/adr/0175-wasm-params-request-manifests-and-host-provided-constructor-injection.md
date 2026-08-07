# ADR-0175: Wasm Params Request Manifests and Host-Provided Constructor Injection

- **Status:** Proposed
- **Date:** 2026-08-06

## Context

ADR-0156 split actor construction into operator-resolvable `Config` and composer-supplied `Params`. Native composers now pass live
`Params` values at typed `with_actor` call sites. Wasm cannot do that: the component host is the composer across the guest boundary,
but the shipped wasm path still requires `Params: Kind + Default` and always constructs `Params::default()`. Every current component
therefore has `Params = ()`.

That placeholder cannot express host-derived construction facts. Replicas intentionally share one operator config, yet an instance has
no principled way to learn its replica index and count. Putting those facts in `Config` makes an operator state facts only the host
knows; adding runtime mail makes construction depend on a later event; and making the load caller encode one aggregate `Params` value
turns the caller into a second component host.

The required properties are structural. A component must advertise every host fact it needs before execution; an unsupported request
must fail by kind before the guest is instantiated; and the guest must receive typed values in `init`. The host must remain extensible
by a chassis without allowing providers to perform I/O or send mail during construction. Replacement must check a new component's
requirements before the old component is unwired. Existing components with no requests must keep loading, including raw or older FFI
guests, and native actors must not change.

The surrounding accepted decisions constrain the shape:

- ADR-0033's `aether.kinds.inputs` section and ADR-0096 group metadata by the actor selected from a multi-actor module.
- ADR-0090 delivers guest construction bytes at init rather than by post-init mail.
- ADR-0024 evolves pointer-bearing wasm32 exports additively under `_p32` names.
- ADR-0156's operator-typable/config, composer-derived/params split remains the decision rule, but its wasm aggregate
  `Params: Kind + Default` mechanism must be amended.

## Decision

Wasm `Params` becomes required, per-field constructor injection. A field asks for a kind; the component host validates and provides all
requested kinds from an immutable load context; generated guest code constructs the aggregate before calling `Lifecycle::init`. This
amends only ADR-0156's wasm Params mechanism. Native `Lifecycle` use and native actor composition are unchanged.

### 1. Params declares required fields, not one aggregate wire kind

A wasm params struct derives `aether_actor::Params`, and every injected field carries `#[param("<kind-name>")]`:

```rust
#[derive(aether_actor::Params)]
struct PanelParams {
    #[param("aether.component.replica_identity")]
    replica: ReplicaIdentity,
}
```

Each field type must implement `Kind`. The derive emits the field's name and the requested kind's `KindId` and kind name, plus a
constructor that decodes that field from the delivered value bag. All declared fields are required. `Option<T>` does not mean
"inject when available" in v1, and a field cannot silently use `Default` when its request is absent.

The derive implements a doc-hidden construction trait used by the generated wasm init shims. `WasmActor` changes its transport bound
from `Params: Kind + Default` to that generated construction trait; the aggregate params struct itself is neither a `Kind` nor
`Default`. The SDK supplies the empty implementation for synthesized `Params = ()`, preserving no-Params components without author
work. The general `Lifecycle::Params: Send + 'static` bound and the native transport's handling of it do not change.

### 2. Requests have an independently versioned, actor-grouped manifest

Requests ride a new `aether.params.requests` custom section, separate from `aether.kinds.inputs`. Receiving mail and requiring a
construction value are different capabilities and must be independently readable and evolvable; adding requests therefore does not
bump `INPUTS_SECTION_VERSION`.

Version 1 is a concatenated record stream. Every record is framed as
`[version: u8 = 0x01][aether_data::wire(ParamsRequestRecord)]`. A `Required { field, id, name }` record describes one injected field.
Multi-actor `export!` output precedes each actor's request records with `ActorBoundary { namespace }`, in the same selected-actor order
and grouping model as ADR-0096's inputs manifest; the single-actor form remains one boundary-free group. Unknown versions, a request
outside a group in a multi-actor section, duplicate actor groups, or duplicate request `KindId`s within one actor are malformed-module
load errors.

The host projects the selected actor's records into a new `ComponentCapabilities.requires: Vec<ParamRequirement>` field. Thus
`describe_component` reports the required field name and kind id/name without instantiating the module. Older or raw modules with no
request section have an empty requires-list.

### 3. The component host owns a typed provider registry

`ComponentHostParams` owns a `ParamsProviderRegistry` assembled by the chassis before the component-host actor boots. The public
registration surface is typed by kind and installs one function under `K::ID`:

```text
register::<K>(fn(&LoadContext) -> Result<K, ParamsProviderError>)
```

The registry encodes the returned `K` into bytes for delivery. Registering a second provider for the same `KindId` is a component-host
boot error, never last-writer-wins. A provider is a pure, fallible read of `LoadContext`: it may derive a value from the supplied fields,
but it performs no I/O, sends no mail, reaches no guest, and mutates neither the registry nor the context. A provider failure becomes a
load or replacement error naming both the requested kind and the provider cause.

`LoadContext` contains the canonical instance name, logical lineage, engine id, and `ReplicaIdentity { index, count }`. Those values are
fixed before request resolution and retained in trampoline state for the instance's lifetime, including drop/refill and replacement.
Replacement changes code, config, selected actor, and requirements; it does not silently change the instance's host provenance.

`ReplicaIdentity`, kind name `aether.component.replica_identity`, is the first built-in provider. A one-off load has `{ index: 0,
count: 1 }`. MCP `replicas: N` and chassis autoload expansion attach `{ index, count: N }` to each generated `LoadComponent` instead
of discarding that provenance after suffixing the name. A wasm sibling spawned from a resident instance inherits that instance's
replica identity and engine id while receiving its own instance name and extended lineage. An independently loaded sibling is a one-off.

### 4. Validation and materialization precede guest execution

After selecting the actor group and resolving its canonical name and lineage, the component host checks every requested `KindId`
against the registry. A missing provider is a load error naming the field and kind. It then invokes every provider and materializes one
bag before any `Component::instantiate` call. No guest code runs on manifest, provider, or bag failure.

The bag has one canonical version-1 representation: a version byte followed by `aether_data::wire(ParamsBag { entries })`, where every
entry is `{ kind: KindId, value: Vec<u8> }` and `value` is the requested kind's normal wire encoding. Entries are strictly ordered by
ascending `KindId`. The host emits exactly the selected actor's request set. Host and guest reject non-increasing order (including a
duplicate tag), a missing required tag, an unexpected tag, trailing bytes, or a field value that does not decode as its declared kind.
The guest generated constructor performs this validation before it calls the actor's `init`.

Replacement parses the new module, selects the effective actor, validates its requires-list, and resolves the complete bag from the
persisted `LoadContext` before taking, unwiring, dehydrating, or dropping the current guest. A failure through that point leaves the old
guest live and wired. The existing state-transfer and swap behavior begins only after construction inputs are known valid.

### 5. Params delivery uses additive parent-aware wasm32 init exports

The new bag is delivered beside config with two additive exports:

```text
init_with_parent_and_params_p32(
    mailbox_id: u64, parent_mailbox_id: u64,
    config_ptr: u32, config_len: u32,
    params_ptr: u32, params_len: u32,
) -> u32

init_typed_with_parent_and_params_p32(
    mailbox_id: u64, parent_mailbox_id: u64, type_tag: u64,
    config_ptr: u32, config_len: u32,
    params_ptr: u32, params_len: u32,
) -> u32
```

The untyped path probes the new six-argument export first, then the existing `init_with_parent_p32`,
`init_with_config_p32`, and legacy `init` shapes in their current order. The typed path probes the new seven-argument export first,
then `init_typed_with_parent_p32`, then `init_typed_p32`; it never falls through to an entry-actor export. Falling back to an older
shape is permitted only when the selected actor's requires-list is empty. If it is non-empty, absence of the new export is a clean load
error rather than an implicit default. New SDK guests retain the older compatibility exports: they construct an empty-Params actor as
before, but return an init failure if an old host selects an actor with required params. Config and params use the existing
allocator-backed init placement and its size/error rules as two disjoint byte slices.

## Consequences

- A component's construction needs become discoverable from bytes and are validated before execution. Missing host support fails at the
  load boundary with a kind-specific diagnostic instead of surfacing as guest behavior.
- `ReplicaIdentity` gives replicated instances and their wasm-spawned actor trees stable cohort provenance while preserving the contract
  that replicas share one operator config.
- Hosts can add deterministic construction facts without expanding the guest host-function surface or teaching load callers to encode
  component-specific aggregate Params values.
- The wasm authoring and ABI surface changes: request-bearing Params structs need the derive and field markers, component metadata gains
  a second grouped section, `ComponentCapabilities` gains `requires`, and macro-built guests export two more init shims. No-request
  components remain source- and load-compatible.
- Required-only injection deliberately forbids host-dependent optional behavior in v1. A future optional-request design would need an
  explicit manifest and guest type distinction; `Option<T>` alone is not that distinction.
- Provider functions run on the synchronous load/replacement path. Keeping them pure and context-only makes that bounded and
  deterministic, but a chassis must surface provider registration collisions during boot and provider errors during the attempted
  operation.
- Implementation is split into three ordered children: **guest manifest/ABI** defines the Params derive, construction trait, request and
  bag wire vocabulary, grouped metadata, capability field, and new shims; **host registry/validation** consumes that contract and adds
  context persistence plus pre-instantiation/pre-unwire validation; **replica provenance** extends MCP/autoload load carriers, installs
  the `ReplicaIdentity` provider, propagates context to wasm siblings, and adds the end-to-end fixture and guide updates. The latter two
  depend on the guest contract, and replica propagation depends on the generic host registry/context surface.

## Alternatives considered

- **Treat ADR-0156 as already sufficient.** Rejected: its aggregate `Params: Kind + Default` contract, empty payload assumption, and
  current init ABI are exactly the public decisions this ADR changes.
- **Keep Params as one caller-encoded `Kind`.** Rejected: the caller would have to manufacture host facts and no per-kind provider
  registry or requires-list could validate the selected actor.
- **Extend `aether.kinds.inputs` with request records.** Rejected: receive capability and constructor requirements have different
  consumers and compatibility lifecycles; a separate versioned section avoids coupling every request change to the inputs decoder.
- **Silently default a missing request or inject `Option<T>` when available.** Rejected: identical component bytes would construct
  differently by host, and unsupported requirements would escape load-time validation.
- **Let providers perform I/O, or let guests pull params through host functions.** Rejected: both make initialization effectful and
  timing-dependent; guest interaction remains init-time host push, with runtime interaction over mail.
- **Generalize provider injection through native actors.** Rejected: native composition already supplies live, typed Params at
  compile-time `with_actor` sites and has neither the wasm boundary nor this problem.
