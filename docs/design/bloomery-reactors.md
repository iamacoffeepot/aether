# Bloomery reactor design

- **Status:** Frozen implementation scope; candidate APIs and deferred execution contracts
- **Date:** 2026-09-16

[ADR-0222](../adr/0222-reactor-bundles-own-their-views.md) records the narrow
architectural decision about bundle-owned views. This companion preserves the
authoring examples, runtime exploration, and implementation seams. Accepting
that ADR does not settle the execution or lifecycle choices discussed here.

## Context

Bloomery needs journal-defined reactors whose executable meaning remains
available after the application is rebuilt. A reactor declaration selects
immutable WASM content. That content must include the aggregation code and
types its rules depend on, rather than asking a future native host to implement
historical view semantics.

The existing pieces are useful but do not yet form this execution model:

- [ADR-0220](../adr/0220-the-journal-is-an-append-only-log-of-typed-events.md):
  `Journal`, `Entry`, `Seq`, `Head<K>`, and `Heads` provide the ordered log and
  head fold. `ViewRegistry` currently owns a native SQLite `Journal` and lends
  references to native callers.
- [ADR-0096](../adr/0096-multi-actor-wasm-modules.md): `aether_actor::export!` packages
  several actor exports in one WASM module. Actors have separate instance
  state; packaging them together does not give them shared Rust references.
- [ADR-0138](../adr/0138-opt-in-default-entry-for-multi-actor-modules.md): the default
  export is explicit. Generated packaging must not select a default by source
  order.
- [ADR-0082](../adr/0082-application-declared-lifecycle-sequence.md):
  `LifecycleGraphData` and `LifecycleCapability` order application stages using
  settlement. Cadence and application data are separate: a stage signal is not
  an arbitrary journal-entry payload.
- The existing guest/native boundary uses typed actor mail. This proposal
  does not require a new direct journal-access host ABI.

This draft records the agreed architecture and shows candidate code shapes.
The Rust below is illustrative, not a claim that the named macros or message
types already exist. Unsettled execution contracts are listed explicitly;
this document does not authorize implementation of those choices by inference.

## Frozen implementation scope

The first work orders implement portable journal/view machinery, reactor
authoring with named guards, generated WASM bundle actors, and ordered
Event/EventBatch preparation. They use typed stored-event triggers decoded
from journal entries, owned full view values, and the existing typed-mail
boundary. Generated views parent and reactor peer actors form one local
cluster; authors do not implement those actor wrappers or register views by
hand. Whole view values are the initial transport contract, without a claim
of cheap copying or zero-copy delivery.

The resulting intents go to an external output mailbox for inspection and
later execution integration. These work orders do not change program APIs,
implement asynchronous executors, commit intents, establish durable reaction
progress, or implement recovery. Automatic loading or replacement selected
by a reactor head is also deferred; a supplied bundle and stream binding are
enough to exercise the first implementation. No durable `Requested` event is
introduced.

The `Ran<P>` and `Request<P>` examples below explain eventual application
behavior. They do not require program-completion hydration or execution in
these work orders. Typed stored events and fixture intents must exercise the
same guard, view, and actor machinery without a program dependency.

## Design sketches

### 1. Compile a reactor set and its views into one bundle

Authors declare reactors and rules. A reactor macro generates the ordinary
WASM actor wrappers; authors do not implement `WasmActor` for each reactor.
Bundle composition collects those actors and generates one views actor for
the bundle's local cluster of reactor peers.

```text
core.wasm
  generated Views actor
    generated shared view state and concrete aggregation implementations
    output types, codecs, and generated input preparation
  generated SceneCompilation actor
    rule code and prepared-input codecs
  other generated reactor actors
    rule code and prepared-input codecs
```

The views actor is one instance per running bundle cluster, not a global
singleton shared by every load of the same artifact. Two independently
instantiated clusters have separate journal bindings and aggregation state.

The stored reactor definition must identify immutable executable content and
the bundle/actor selection and configuration required to run it. Its exact
storage shape remains to be designed against the existing artifact and
component-loading types. A moving `Head<Reactor>` may select a definition;
processing and recovery must identify the exact immutable definition they use.
Neither a head name nor a native Rust `TypeId` identifies durable executable
meaning.

### 2. Views aggregate locally and publish owned data to peers

All aggregation runs inside the bundled views actor. Generated wiring shares a
concrete view instance among the bundle's arms that need that view. Concrete
view implementations and their output types are statically linked during the
WASM build. There is no host-provided `Heads` service and no dynamically chosen
host implementation of a historical view.

The mutable aggregation and the delivered value have separate ownership;
they need not have different public types. An arm that needs head lookups
accepts `heads: Heads`. It receives an owned value at the prepared cursor,
with ordinary local methods, not a remote proxy or a borrowed reference into
the views actor. No `ViewData<Heads>` wrapper is required. The initial
publication contract sends complete owned view values through the existing
typed-mail codec. Declared projections remain a later optimization. No
promise of zero-copy transport or cheap cloning is made.

### 3. Declare cases and named guards in arm signatures

A reactor expresses application policy as a named set of arms. Each arm
consumes one trigger and its declared view or guard dependencies and returns
exactly one intent. Declining happens in the signature, through a trigger
pattern or a named guard. The arm does not return `Option` or a list of intents,
append journal entries, stage content, read the journal, or read a clock.

For example, an editor compiles a scene when its source head moves, then
publishes a finished compilation only if it still applies to the current
source and compiler:

```rust
#[reactor]
impl Reactor for SceneCompilation {
    const NAMESPACE: &'static str = "editor.scene_compilation";

    #[rule]
    fn source_changed(
        &self,
        change: Event<SceneSourceMoved>,
        ready: ReadyToCompile,
    ) -> Request<CompileScene> {
        Request(CompileSceneInput {
            scene: change.scene,
            source: ready.source,
            compiler: ready.compiler,
        })
    }

    #[rule]
    fn compilation_finished(
        &self,
        compiled: Ran<CompileScene>,
        current: CurrentCompilation,
    ) -> Move<CompiledScene> {
        Move::new(current.destination, compiled.result.scene)
    }
}

aether_actor::export!(
    public = [SceneCompilation],
    generators = [aether_bloomery_reactor::bundle_reactors],
);
```

`export!` remains the module's export entry point. `bundle_reactors` selects
actors carrying reactor metadata and generates their shared views actor and
peer wiring. Ordinary actors may appear in the same export list without
becoming reactors. Each reactor declares its own namespace; authors do not
name or declare the generated views actor.

The actor framework owns descriptor collection and the common actor metadata
envelope. Reactor-specific metadata is a namespaced extension owned by
Bloomery. Generators receive the collected descriptors and preserve extensions
they do not understand, allowing other generators to consume their own
metadata. Metadata association must survive ordinary actor imports, reexports,
and `use` aliases.

These are candidate APIs and domain types, not existing implementations.
In particular, `Ran<P>` and `Request<P>` illustrate deferred program
integration, rather than the first implementation's required trigger/output
types.
`SceneSourceMoved` represents a decoded scene-source head move; its mapping
from the generic journal head-move event belongs to typed trigger preparation,
not a requirement for a new stored event kind. `Event<K>` here is authoring
vocabulary for a typed trigger, distinct from the input transport below;
the final naming must disambiguate them.

`ReadyToCompile` resolves the scene's current source and selected compiler.
`CurrentCompilation` resolves only when the completed program's input source
and compiler still match their heads, and supplies the destination head.
Each guard names a reusable domain question and carries the answer needed by
the arm. A missing source or compiler can decline the first arm; a stale
compilation can decline the second. The already-recorded compilation result
remains available even when it is not published.

The guard contract from the handoff is:

```rust
trait Guard<T: Trigger> {
    type Views;

    fn resolve(trigger: &T, views: &Self::Views) -> Option<Self>
    where
        Self: Sized;
}
```

For example, `CurrentCompilation` implements `Guard<Ran<CompileScene>>`
with `type Views = (Heads,)`. The source/compiler comparisons belong in that
resolver, defined once beside the views it uses. `None` filters the arm;
`Some(current)` supplies its parameter. No guard expression language in
attributes is needed. Refutable trigger patterns select result cases in the
same signature; their macro expansion and coverage diagnostics need design.

A declined guard does not suspend an event or leave an arm waiting. The event
has been considered, and its data remains represented in the views. A later
event can make the guard resolvable. A join that needs two independent results
therefore needs a triggering arm for either result arriving last.

The generated dependency wiring includes the views required by guards, even
when the arm does not take those views directly. Guard resolution uses the
trigger and its prepared views at one cursor. The exact placement of resolution
in the generated actor pipeline remains an implementation decision; all guard
code and required data are bundled, and no missing dependency is queried
mid-evaluation. Resolving a guard establishes its condition at the prepared
event position. It does not establish what a future writer must do if state
changes before an intent is accepted. Any commit conditions or re-derivation
protocol belong to deferred execution design.

The resulting application loop is concrete:

```text
scene source head moves
    -> ReadyToCompile resolves
    -> request compilation of that source with that compiler
    -> program driver records the compilation result
    -> CurrentCompilation resolves, or declines a stale result
    -> propose moving the compiled-scene head
    -> accepted head move becomes a future input event
```

The immutable artifact reads needed to decode `Ran<P>` are separate from
folding journal entries and require an explicit preparation design. This
example illustrates policy; it does not introduce an external compiler
integration as implementation scope.

The macro generates a concrete delivery type for an arm, schematically:

```rust
// One possible delivery shape: resolve the guard in the reactor actor
// against the owned views prepared for this trigger.
struct CompilationFinishedInput {
    context: ReactionContext,
    compiled: Ran<CompileScene>,
    heads: Heads,
}

struct ReactionContext {
    definition: Ref<Reactor>,
    trigger: Seq,
    at: Seq,
}
```

The context correlates the prepared input and output. Rule identity and
delivery identity also need to be unambiguous; their representation is part
of the execution/recovery contract, not fixed by this sketch. For ordinary
live processing in this design, `at == trigger`.

### 4. Infer shared view state and wiring during compilation

Bundle composition collects direct view dependencies and the dependencies
declared by guards. Authors do not manually register them. Because the bundle
knows these concrete types at compile time, generated state can be as simple as:

```rust
struct BundleViews {
    heads: Option<Heads>,
}
```

The optional slot illustrates lazy construction; generated readiness handling
must warm it before any dependent arm receives an event. Both guards in the
example share this one aggregation instance. Additional view types add fields,
not copies for each arm. Generated update and publication code makes the
concrete aggregation and serialization implementations reachable in the build.

A dynamic registry is an implementation option, not an architectural
requirement. A procedural macro cannot inspect an associated type such as
`CurrentCompilation::Views` or determine trait implementations from a
parameter's source spelling. Generated trait calls and tuple dependency
visitors must perform that inference; an internal registry may deduplicate
the resulting view dependencies. The concrete fields above illustrate
ownership, not a requirement that a macro recover every dependency as source
syntax. The existing native registry can inform reuse of folding and
validation machinery. If an implementation uses `TypeId`, it is only an
in-instance key, never a serialized identity or a peer address.

The bundle generator also emits the views actor's routing table, prepared
message types, reactor receive handlers, and explicit actor export list.
Runtime startup instantiates and wires these known peer roles in one cluster.
It must not assume that packaging an actor export automatically instantiates
it or establishes a mailbox route.

### 5. Drive the pipeline with Event and EventBatch

The agreed flow is:

```text
cadence / Tick
    -> journal input selection
    -> Event or EventBatch
    -> fanout to bundle Views actors
    -> ordered aggregation
    -> prepared messages to reactor peers (live Event)
    -> intents
    -> execution / validated head moves
    -> journal append
    -> subsequent journal input
```

`Tick` is an opportunity to process work, not the logical unit of reactor
meaning. A fixed-rate cycle may select the last queued position and drain
through it; an event-driven cycle may wake for each entry. Both preserve
journal order. Tick timing and elapsed wall time do not become rule inputs.

Ordered reactions do not require serial program execution. A reaction cycle
can hand work off without waiting for the requested program to finish;
subsequent completion events drive later pipeline stages. Independent work
may overlap while journal events continue to be prepared in order. The
asynchronous execution handoff and receipt-recording mechanics are future
work, not a blocking call inside a generated reactor handler.

Candidate transport shapes are:

```rust
struct Event {
    entry: Entry,
}

struct EventBatch {
    entries: Vec<Entry>,
}
```

These are distinct message modes, not new journal event kinds to append for
every delivery. They travel within a stream bound to one journal and one
bundle activation. A transport envelope must correlate that stream so stale
mail from an old activation cannot affect a new one.

`Event(n)` advances required views through entry n, then prepares and sends
inputs for matching reactor arms using view data at n. No input for n may
include the effect of n+1. Independent bundles may do this in parallel; each
view is mutated serially by its owning actor.

`EventBatch` fast-forwards aggregation through a contiguous ordered range
without producing live reactions for those entries. It is for reconstructing
history whose reactions are already accounted for, or warming a view before
its declared activation boundary. A batch must not cross into outstanding
live work and silently skip it. The evidence that makes a range eligible for
this mode belongs to the recovery/activation contract still to be specified.

Batch folding must produce the same view state as folding those entries one
at a time. Contiguity, cursor checking, and poisoning remain mandatory.
Transporting live entries together is a separate optimization: it must still
prepare reactions at every relevant intermediate event boundary.

This is the essential difference in the generated views actor:

```rust
fn on_event(&mut self, event: Event) -> Result<PreparedDeliveries, ViewError> {
    self.views.advance(std::slice::from_ref(&event.entry))?;
    self.prepare_matching_inputs(&event.entry)
}

fn on_event_batch(&mut self, batch: EventBatch) -> Result<(), ViewError> {
    self.views.advance(&batch.entries)
}
```

These methods assume all required views have been warmed to the preceding
position. The generated handler sends the prepared deliveries through the
existing actor mail path and retains their lifecycle correlation.

### 6. Place construction and updates in the application lifecycle

The application graph schedules input selection, view preparation, reactor
evaluation, and effect handling. The reactor does not discover mid-evaluation
that it needs to synchronously query the journal or another actor.

Lazy construction still applies, but it must obey the event boundary:

- Declarations do not construct view state.
- Before an enabled arm receives its first live event, its required views
  must be instantiated and caught up during a preparation stage.
- A newly needed view cannot start at zero and consume only the current
  event. Schedule its historical batch replay before enabling delivery.
- Once ready, the views actor advances those views from pushed entries.
  Reactors receive complete prepared inputs.

After an owned input for n has been prepared, advancing the aggregation cannot
change that input. This permits pipelining subject to the effect-ordering and
backpressure policy. It does not authorize unordered publication of effects.

The current `LifecycleGraphData` emits stage signals; it does not carry an
arbitrary `Event` payload through a graph node. The journal input actor sends
the actual Event/EventBatch mail within the scheduled phase. That work and
its dependent deliveries must participate in the phase's completion protocol.

Current `LifecycleCapabilityState::on_advance` has timeout/force-completion
behavior. A stage settling or timing out is not proof of successful view/input
preparation. The first implementation must distinguish successful preparation
from scheduling settlement: failed aggregation, missing prepared data, or a
trap cannot become a successful preparation acknowledgment. This does not
introduce a reactor-specific durable checkpoint or execution recovery protocol.
Global lifecycle failure-policy changes and durable effect completion remain
follow-on work.

### 7. Keep durable progress distinct from rebuildable state

An intent is transient. No separately journaled `Requested` event is introduced
by this design. Outputs become input again only after the responsible driver
records their journal events. Replaying a view does not execute those outputs.

The views actor's cursor proves only which entries its aggregation includes.
It does not prove that reactor peers ran, that all arms produced outcomes, or
that program execution and its receipt committed. Likewise, message delivery
and lifecycle settlement are not durable execution checkpoints.

Recovery must select the exact immutable reactor definition, rebuild its views
through an eligible completed prefix, and resume outstanding reactions with
their original event boundaries. A program completion without a committed
receipt may require rerunning; this architecture does not promise exactly-once
execution of external effects.

The following contracts must be settled before an execution work order:

1. **Effect order.** Journal input is ordered, but concurrent peers may finish
   in a different order. Define how outputs of arms, reactors, and bundles enter
   journal order; mailbox completion timing must not accidentally select an
   intended deterministic order. Define how external appends interleave too.
2. **Progress and recovery.** Identify completed work by exact definition,
   trigger, and arm; cover no-match/no-intent cases, multiple arms, partial
   completion, faults, and atomicity with receipts or head moves. Do not infer
   completion from one receipt or invent a generic cursor that skips work.
3. **Activation.** Specify which definition handles which event when an active
   head changes, how a new cluster warms up, and what happens to old in-flight
   reactions. An upgrade must not silently reinterpret an old reaction.
4. **Prepared payloads.** The first implementation uses complete owned view
   data. Later work may introduce projections or additional in-flight limits;
   program triggers also need immutable artifact hydration. Do not add
   on-demand view queries as an implicit substitute.
5. **Driver boundary.** Current `program::apply` takes a program reference and
   an input digest. The source-level `Request<P>` sketch constructs a value.
   Specify the conversion and input durability transaction; do not assume the
   current driver already accepts that value or separately persist every intent.

### 8. Preserve compatibility by preserving executable content

An immutable bundle retains its aggregation code, output types, codecs, and
reactor consumers. Rebuilding a Rust library creates different executable
content; it does not replace code inside an existing bundle reference.
Messages between peers use their co-packaged types, not the native host's Rust
layout. Shared source libraries are build-time dependencies.

This does not remove all compatibility obligations. Journal envelopes, existing
storage-kind meanings, typed-mail transport, and the VM execution contract must
remain readable/executable. Unfamiliar journal kinds need defined handling;
incompatible reuse of an old kind is not repaired by bundling an old decoder.
Durability of WASM bytes alone is not a guarantee that every future runtime can
execute them without preserving those contracts.

## Implementation seams and validation

The native view implementation needs a dependency split by responsibility:

| Crate | Responsibility |
| --- | --- |
| `aether-bloomery-kinds` | Shared `no_std + alloc` data contracts, entries, and decoding |
| `aether-bloomery-view` | Portable folds and view behavior |
| `aether-bloomery-journal` | Native persistence |

The former native journal-owning `ViewRegistry` has no remaining production
consumer and is retired rather than moved into another crate. Generated
reactor actors maintain the shared instances they need. Native tests can use
the same portable folds without a separate native registry API.

Portable consumers use shared types directly, without opting out of SQLite
through feature flags. This separation follows responsibilities, not a general
prohibition on justified features elsewhere.

- `Entry`, `Seq`, and pure event decoding belong in the existing shared kinds
  crate. The native journal may reexport them for source compatibility.
- `Heads::binding_from` currently calls `Journal::decode`. Move that pure
  decoding dependency out of the native journal implementation.
- Remove the unused native registry API. The bundled views actor receives
  entries and publishes data. Its internal machinery must enforce construction,
  cursor validation, and poisoning contracts without exposing a native journal
  reader or requiring manual author registration. Do not preserve an adapter
  solely because the previous implementation had one.
- Add the published data representation and generated peer messages. Local
  borrowed injection remains useful inside one actor, but is not the inter-actor
  contract.
- Bind each cluster to one journal stream. Existing `JournalIdentity` is a
  process-local native token; it must not be serialized as durable stream or
  bundle identity.

The following topics warrant separate ADRs once their contracts are settled:

- **Ordered event processing:** lifecycle phases, Event/EventBatch semantics,
  warmup, readiness, and failure handling.
- **Execution and recovery:** intent fencing, effect ordering, durable progress,
  and activation or replacement of reactor definitions.

Named guards and signature-based declines belong to the authoring design above.
Their exact API can be developed here without making acceptance of the bundle
ownership decision depend on macro syntax.

These are the implementation seams to turn into work orders. Execution and
recovery remain outside the frozen scope, rather than a prerequisite for
implementing and testing the other seams:

| Seam | Independently reviewable result |
| --- | --- |
| Portable view core | Guest-compatible entry/decoder/fold code without SQLite |
| Published view data | Owned view outputs and precise preparation semantics |
| Bundle authoring | Reactor macro, trigger patterns, named guards, inferred shared views, peer wiring, exports |
| Ordered input lifecycle | Event/EventBatch fanout, warmup, readiness and failure handling |
| Deferred execution and recovery | Explicit effect ordering, completion identity, receipt atomicity and replay |

Validation should include two reactor peers sharing a view, identical state
after batch versus single-entry folding, event n prepared before a conflicting
head move at n+1, retention of an already-prepared value after later updates,
guard dependencies inferred without explicit view parameters, a declined guard
preventing arm invocation, a resolved guard supplying its value to the arm,
and failed preparation never being acknowledged as successful merely because
a lifecycle stage settled. Recovery testing, including restart without skipped
outstanding reactions, belongs to the deferred execution work. Building and
running an old bundle after
rebuilding its source dependency should still use the old bundled behavior.

Persisted view checkpoints, snapshot optimization, and fast index construction
remain separate concerns. The batching interface must permit efficient replay,
but this design does not claim to solve the deferred performance investigation.
No filesystem materialization, Git projection, or external executor integration
is introduced here.

## Approaches discussed

- **Native host owns historical view implementations.** Moves executable
  semantics out of the immutable reactor bundle and creates an unnecessary
  host-side compatibility surface for each view.
- **Each reactor actor keeps its own aggregation.** Duplicates state and replay
  within a bundle; the chosen first stage shares aggregation among its peers.
- **Borrow `&Heads` across actors.** Incompatible with separate actor ownership
  and typed-mail delivery. Owned prepared data is the boundary.
- **Reactors query views or journal pages while evaluating.** Adds readiness
  coordination that the application lifecycle can establish ahead of delivery.
- **New direct journal-access ABI.** A possible future mechanism, but unnecessary
  for the agreed mail-driven pipeline and not part of this decision.
- **Fold to the tick's last event, then react to earlier entries using that
  state.** Lets earlier reactions observe future entries.
- **Use EventBatch to skip uncompleted live reactions.** Loses work; fast-forward
  aggregation is not evidence that the corresponding reactions completed.
- **Treat settlement or a view cursor as a durable checkpoint.** Confuses
  scheduling and reconstructed state with committed execution progress.
