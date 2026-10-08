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

A guest measures elapsed time with `ctx.now()`, on `WasmCtx` (and so `WireCtx`)
and on `WasmInitCtx`. It returns an `aether_actor::Instant`, and
`later.since(earlier)` is a `core::time::Duration`. The call is one host import,
`now_nanos_p32`, answered inside the handler that asks; a module built against it
is refused at load by a host that lacks the import. `std::time::Instant::now()`
compiles for `wasm32-unknown-unknown` and traps when called, so a component reads
`ctx.now()` and never the std clock.

An `Instant` stays in the actor that read it:

- It has no schema and is no kind's field, so it cannot be mailed, saved with
  `save_state_kind`, or put in a request context. A measurement that spans two
  handlers keeps its start in actor state.
- A successor installed by a republish takes its own reading in `init` or
  `on_rehydrate`; its predecessor's readings do not carry over.
- A measured duration is not replayable. A bloomery rule must not write one into
  a journaled result: a restart replays the rule and measures a different
  duration. Durable time comes from recorded time.

## Multi-actor modules need an explicit export selector

A wasm module may export several actor identities. Do not infer a selection
from declaration order:

- `export!(public = [Main, Helper, …])` needs the load's `export` selector to
  name one of them; a bare load is refused, naming the exports.
- A module that exports exactly one actor, `export!(public = [Hello])`, still
  loads without a selector, and only that case emits the namespace
  compatibility metadata (`aether.namespace`).

This rule prevents a harmless reordering from changing what a selector loads.
It is governed by ADR-0241 §9 and enforced by the export manifest and
component loader.

A `boot = Boot` key may join either form, as in `export!(boot = Boot, public = […])`.
The boot type is instantiated once per loaded module whatever selector the
caller names, and is not itself selectable; it is governed by accepted
ADR-0147.

## Native capability surface

The `aether-<capability>` crates represent filesystem, HTTP, render, lifecycle,
audio, window, engine control, and other host responsibilities as native
actors. A capability generally has:

- a marker/identity type and stable namespace;
- capability-owned kind definitions;
- a runtime-only state type and `NativeActor` handlers;
- configuration gated away from transport-only or wasm builds;
- explicit chassis installation.

The identity/runtime split keeps public addressing types light while allowing
native state to hold adapters, devices, threads, and resource handles. See
[Capability module anatomy](../capability-anatomy.md).

A native actor measures elapsed time with the same call a guest does:
`ctx.now()` on `NativeCtx` and on `NativeInitCtx`, returning the same
`aether_actor::Instant`. Both sides read the engine's one actor clock, so a
guest's and a native actor's readings in one engine are of the same clock, and
the rules for an `Instant` above hold on both.

## Capability-local kinds

Public messages exchanged with a capability live with that capability, for
example `aether-render/src/kinds.rs`. `aether-kinds` remains for
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

Integration fixtures under `crates/aether-test-fixtures-*/` cover several
load/publish and multi-actor cases. Use them before inventing a new one-off
example.

## Implementation and decisions

- Schema, ids, and canonical identity: `crates/aether-data/src/`
- JSON/wire conversion: `crates/aether-codec/src/`
- Guest SDK and exports: `crates/aether-actor/src/`
- Wasm host: `crates/aether-substrate/src/actor/wasm/`
- Capability types/runtimes: `crates/aether-<capability>/src/`
- ADR-0096: multi-actor wasm modules
- ADR-0099: actor identity and addressing
- ADR-0121 and ADR-0122: kind ownership and marker/runtime split
- ADR-0138: multi-actor module export selection (superseded by ADR-0241 §9)
- ADR-0147: the module boot actor, instantiated once per module load outside the export selector
