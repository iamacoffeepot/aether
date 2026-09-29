# ADR-0222: Reactor Bundles Own Their Views

- **Status:** Proposed
- **Date:** 2026-09-16
- **Amended:** 2026-09-28 — bundle-owned folds may resolve immutable typed artifacts through the existing driver fetch route (issue #7026).

## Context

Bloomery needs journal-defined reactors whose executable meaning survives
application rebuilds. A reactor's decisions depend on views computed from the
journal, so preserving reactor code alone does not preserve its behavior if a
future host supplies different aggregation code.

[ADR-0220](0220-the-journal-is-an-append-only-log-of-typed-events.md) establishes
the journal model: `Journal`, `Entry`, and `Seq` provide the ordered input.
The current native `ViewRegistry` owns a SQLite journal and lends views such
as `Heads` to callers. That borrowing API is not an inter-actor boundary.

[ADR-0096](0096-multi-actor-wasm-modules.md) provides
`aether_actor::export!` for multiple actors in one WASM module. Those actors
have separate instance state and communicate through typed mail.
[ADR-0138](0138-opt-in-default-entry-for-multi-actor-modules.md) makes the
default export explicit; packaging must not infer it from source order.

## Decision

A reactor bundle contains its reactor code, view aggregation implementations,
and the types and codecs used to exchange prepared view data. These are
statically linked into immutable WASM content. Historical bundles use their
own aggregation code, rather than a view implementation selected by the
current native host.

Each running bundle cluster has one views actor that maintains shared
aggregation state for its reactor peers. Independent instances of the same
bundle have separate view state and journal bindings.

The views actor supplies owned data to peers through typed mail. A prepared
value represents a particular journal position and cannot change when the
aggregation advances. Actors do not borrow references into another actor's
memory. Separate ownership does not require separate public Rust types.

Authors declare reactor arms and their dependencies. Compilation generates
the actor wrappers, shared view dependencies, peer message types, and wiring.
Dependencies include views needed by named guards. The bundle knows its types
at compile time, so a dynamic registry is optional; manual registration is
not part of the authoring contract. Startup must instantiate and connect the
generated roles; an export list alone does not establish a running cluster.

```text
immutable reactor bundle
    Views actor: shared aggregation
        -> owned prepared data -> reactor actor
        -> owned prepared data -> reactor actor
```

### Artifact-backed folds

The generated bundle root now hosts the shared views owner and evaluates its
reactors locally, as developed by [ADR-0225](0225-reactor-bundles-load-by-digest.md)
and [ADR-0226](0226-native-bundle-driver.md). View dependencies are inferred
from direct parameters and named guards. Exporting a reactor brings its view
implementations into the bundle; authors do not export a separate view list.

A fold may resolve immutable `Ref<K>` values through a read-only
`ArtifactResolver`. References come from the supplied entries, accumulated
view state, or an artifact already decoded by the fold. One event can require
zero, one, or several reads. The initial implementation permits one outstanding
read per active fold; it does not preload the event's transitive closure.

Resolution composes three existing boundaries:

1. Ordered journal delivery fixes the prefix being folded. Resolving bytes
   never samples a newer head binding.
2. Content-addressed typed references fix the requested values. The resolver
   checks the claimed digest, kind, complete content hash, and storage decode.
3. The bundle retains the live fold continuation and its held reply while a
   fetch travels through the driver. Resumption continues after the same
   await; it does not replay mutations performed before the read.

All required views must finish before the root acknowledges `Warm` or
evaluates an `Event`. During suspension, `Status` reports the previous trusted
boundary, and another delivery cannot overtake the active operation. A failed
read or fold poisons the operation and exposes no partially prepared inputs.
Ignoring a resolver error in author code cannot turn it into successful
advancement. The already recorded source event is not rolled back.

The resolver grants no staging, head writes, program invocation, general
actor mail, or current-head lookup. It reuses the existing artifact mail path
and driver cache; no direct host ABI or persistent per-view artifact cache is
introduced. Applications remain responsible for retaining referenced content
through typed storage citations. Resolution does not make an opaque embedded
digest a citation.

`View::advance` is awaitable and receives the resolver explicitly. Existing
synchronous authored fold bodies remain valid. The portable trait does not
require every view to implement `Clone`, `Publish`, or `Send`; the erased
actor owner requires sendable views and continuations. Generated async
`#[view]` folds require `Send` futures, matching async program authoring.
This explicit macro restriction lets stable Rust erase the authored future
without losing the owner's sendability guarantee. Manual portable view
implementations may still return non-`Send` futures.

## Consequences

- Rebuilding a source library creates new bundle content; it does not change
  the view semantics of an existing immutable bundle.
- View folding and pure journal decoding must be usable in WASM without
  SQLite. Shared data belongs in the existing kinds crate; fold behavior stays
  in the view crate. The unused native journal-owning registry is retired.
- Sharing aggregation avoids replaying and retaining the same view separately
  in each reactor peer. Sending owned data still has serialization and memory
  costs; this decision does not promise zero-copy transport.
- View dependencies and their codecs must be reachable in the bundle build.
  Native `TypeId` values must not become durable or wire identities.
- Preserving executable content does not remove compatibility obligations for
  journal encodings, storage-kind meanings, typed mail, or the VM contract.
- This boundary needs no new direct journal-access host ABI. Journal data can
  arrive through the existing mail mechanism.

The [companion reactor design](../design/bloomery-reactors.md) contains Rust
examples, named guards, generated wiring sketches, and implementation seams.
It also tracks decisions that this ADR does **not** settle: event batching and
lifecycle completion, effect ordering, intent fencing, durable progress,
activation, and recovery. Those contracts require separate decisions before
their implementation. Snapshot versus projection payloads and exact macro
syntax likewise remain design work.

## Alternatives considered

- **Host-provided historical views.** Requires the current host to preserve
  each old view implementation independently of its consuming reactor.
- **Aggregation in every reactor actor.** Duplicates state and replay across
  peers that need the same view.
- **Borrowed views across actors.** Conflicts with separate actor memory and
  typed-mail ownership.
- **Mandatory dynamic registry.** Adds a runtime mechanism where generated
  wiring already knows the dependency types; it remains an implementation
  option when justified.
