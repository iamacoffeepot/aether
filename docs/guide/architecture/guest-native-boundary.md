# Guest, native, and wire boundaries

Aether does not make wasm actors call host services directly. Guest and native
actors meet at the same abstraction: typed mail addressed to a mailbox. The
host boundary is therefore a serialization and scheduling boundary, not a
second application API.

## Three layers in one interaction

```text
Rust kind value
    │ Schema + Kind identity
    ▼
canonical wire bytes
    │ mailbox + kind ids, reply lineage
    ▼
native handler or wasm export shim
```

The Rust type provides authoring ergonomics. Its schema describes the portable
shape. Its canonical kind identity says what operation the bytes mean. The
mailbox says which actor instance should receive them.

Changing any one of these can be compatibility-significant even if the code
still compiles locally.

## Guest actor surface

`aether-actor` provides the wasm-facing authoring model:

- `WasmActor` and `WasmInitCtx` for actor initialization;
- handler annotations for typed receive contracts;
- typed actor/capability mailboxes;
- request/reply and correlation helpers;
- `export!` for the module's actor manifest and FFI shims.

The deployable artifact is a wasm `cdylib`. An accompanying `rlib` can expose
the kinds and helpers other Rust crates need to talk to it. Runtime code is
normally feature-gated so a type-only consumer does not link the implementation.

## Multi-actor modules have explicit entry semantics

A wasm module may export several actor identities. Do not infer a default from
declaration order:

- `export!(default = Main, Helper, …)` declares `Main` as the default and
  emits the namespace compatibility metadata.
- `export!(Main, Helper, …)` is defaultless. A loader must select an exported
  actor; a bare load is an error.

This rule prevents a harmless reordering from changing what a selector loads.
It is governed by accepted ADR-0138 and enforced by the export manifest and
component loader.

## Native capability surface

`aether-capabilities` represents filesystem, HTTP, render, lifecycle, audio,
window, engine control, and other host responsibilities as native actors. A
capability generally has:

- a marker/identity type and stable namespace;
- capability-owned kind definitions;
- a runtime-only state type and `NativeActor` handlers;
- configuration gated away from transport-only or wasm builds;
- explicit chassis installation.

The identity/runtime split keeps public addressing types light while allowing
native state to hold adapters, devices, threads, and resource handles. See
[Capability module anatomy](../capability-anatomy.md).

## Capability-local kinds

Public messages exchanged with a capability live with that capability, for
example `aether-capabilities/src/render/kinds.rs`. `aether-kinds` remains for
genuinely upstream, cross-cutting substrate vocabulary and descriptors. This
ownership, adopted in ADR-0121, gives guest crates a stable type path without
turning one central crate into a catalog of unrelated service APIs.

Feature gates preserve the boundary:

- a transport/marker feature exposes types usable from wasm;
- a `runtime` or capability-runtime feature exposes native implementation and
  heavyweight dependencies;
- a chassis decides which runtime features and actors are actually installed.

## Native transforms are not actor handlers

A native `#[transform]` is a linked, discoverable function used for bounded
data conversion or folding. It appears in `describe_transforms`, not as an
ordinary mailbox handler. The inventory capability supplies reverse names and
templates for engine-local ids; it does not execute arbitrary code.

Choose a transform only when the operation is deterministic, bounded, and
naturally value-to-value. Stateful ownership, I/O, scheduling, and replies
belong in an actor.

## Behaviors are a smaller guest boundary

`aether-behavior` supports compact replaceable filters that consume an envelope
and produce a verdict plus effects. They are useful when a full actor lifecycle
and mailbox would be excess surface. Behaviors are hosted and replaced through
their own runtime contract; they do not silently gain arbitrary native access.

Read [Behaviors](../systems/behaviors.md) for the ABI and selection model.

## Replies carry lineage, not blocking calls

Request/reply syntax does not turn actor mail into a synchronous function call.
A request establishes correlation and reply expectations while the scheduler
continues to own delivery. Settlement tracks descendant work so an operator can
wait for a chain without making the guest directly block on a host stack frame.

When a handler crosses to a sidecar thread for blocking I/O, it must preserve
the actor's reply/settlement contract explicitly. Read
[Concurrency and blocking](../systems/concurrency.md) and
[Tracing and settlement](../systems/tracing-and-settlement.md) together.

## Compatibility checklist

Before changing a public boundary, ask:

1. Did the kind's canonical identity change?
2. Did its schema change, including enum variants, optionality, or byte fields?
3. Does a macro generate an export, descriptor, or reply contract from it?
4. Do native and wasm feature sets still expose the same transport types?
5. Can an existing registry selector still choose the intended actor?
6. Does replacement preserve or deliberately reshape state?
7. Do MCP JSON encoding and live descriptors agree with the new schema?
8. Is an ADR required because the boundary or compatibility policy changed?

Integration fixtures under `crates/aether-test-fixtures/` cover several
load/replace and multi-actor cases. Use them before inventing a new one-off
example.

## Implementation and decisions

- Schema, ids, and canonical identity: `crates/aether-data/src/`
- JSON/wire conversion: `crates/aether-codec/src/`
- Guest SDK and exports: `crates/aether-actor/src/`
- Wasm host: `crates/aether-substrate/src/actor/wasm/`
- Capability types/runtimes: `crates/aether-capabilities/src/`
- Behavior ABI: `crates/aether-behavior/src/`
- ADR-0096 and ADR-0099: multi-actor/component hosting
- ADR-0121 and ADR-0122: kind ownership and marker/runtime split
- ADR-0138: explicit/defaultless multi-actor entry semantics
