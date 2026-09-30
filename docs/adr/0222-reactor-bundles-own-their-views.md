# ADR-0222: Reactor Bundles Own Their Views

- **Status:** Proposed
- **Date:** 2026-09-16

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

A fold and a rule may read the artifacts their entry cites directly, one
level deep. An entry names its content by digest, so a fold sees only the
entry unless it is handed what the entry points at. The driver reads the
artifacts before delivery (ADR-0226) and each `Warm` / `Event` carries them;
the bundle scopes them per entry as a `Cited` value holding that entry's
cited digests and their artifacts. A `#[view]` fold takes it as an optional
third parameter, `cited: &Cited`; a rule takes `cited: Cited`, inferred as
the trigger's citations the way views and guards are inferred. Folds and
rules that do not ask for it are unchanged. `Cited::get(Ref<K>)` answers only
a digest the entry itself cites, so a ref found inside a cited artifact, or
one a neighbouring entry in the same warm batch cites, is never readable. It
verifies the bytes against the digest and the kind prefix against `K` on
every read, then decodes; a failed read is the fold's own error and poisons
the views like any failed fold, while a rule decides what to return. Reads
stay synchronous: there is no second hop, no resolver, and no suspended
fold. A rule that reacts to a program's run triggers on `Ran<P>`, the typed
`Transition` of program `P`, whose input and result are among its citations.

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
