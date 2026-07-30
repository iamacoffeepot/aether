# ADR-0165: Handlers Read Views, Emit Effects

- **Status:** Proposed
- **Date:** 2026-07-23
- **Last amended:** 2026-07-30

## Context

Actor birth writes shared, globally locked structures, while the hottest of those structures—the mailbox routing table in `aether-substrate`’s `Registry`—is read on every mail dispatch.

One `std::sync::RwLock` currently guards `mailboxes`, `kinds`, and `name_index`. `route_lookup` takes a read guard per mail and clones the route entry before dispatch. Every runtime spawn takes the write guard through `Spawner::spawn_actor`. ADR-0087 parallelized dispatch across the worker pool, but spawn-heavy workloads still serialize against routing reads.

The `spike/registry-view-contention` branch provides the measurement and source audit behind this decision:

- Reads through the `RwLock` degraded approximately 15× as reader threads were added and approximately 24× under writer churn. The shared reader count itself becomes a cache-line contention point.
- Snapshot reads through an `arc-swap` load remained wait-free and were approximately 30× faster at eight reader threads.
- A single owner draining batched mutations met or exceeded the contended direct-lock write path because batches self-sized under load.
- Cloning the entire routing map for every publication became prohibitive at high actor counts.
- Double-buffer publication with operation replay retained ordinary `FxHashMap` lookup performance without a whole-map clone on every write.
- The production consumer audit found that most registry readers already operate on point-in-time information. `route_lookup` itself drops the guard before dispatch, so dispatch has always used a per-mail micro-snapshot rather than a transactionally current route.

A handler’s own state already has a single writer: its inbox. Other engine systems already use similar ownership patterns:

- input subscriptions use capability-local ownership;
- draw work accumulates locally and publishes at a frame boundary;
- cost accounting uses local `CostCell` mutation with later aggregation.

The original version of this ADR correctly selected published views and staged effects, but understated the lifecycle required to stage actor birth safely.

In particular:

- worker-pool construction is not the boot/runtime authority boundary;
- a built chassis may still claim mailboxes, boot pumped actors, register capabilities, and seed costs during the driver’s `Start` stage;
- returning a deterministic actor id does not mean the actor is live or that its spawn succeeded;
- `wire` is actor-authored code and may require a particular execution home, especially a Wasmtime `Store`;
- the registry owner must not execute arbitrary native or guest actor code;
- a route must be addressable while activation is underway without becoming dispatchable or publicly enumerable;
- actor state may initially live in the existing boxed dispatcher, but future native cohorts and Wasm Store shards must be possible without changing the spawn, completion, mail-ordering, or routing contracts.

ADR-0166 now supplies declared `Root` and `ChildOf<P>` placement relationships. The checked TCP lineage migration landed in PR #4072. Runtime enforcement remains part of the staged-birth foundation described here.

This ADR therefore defines both the shared-state ownership rule and the storage-neutral actor-birth protocol needed for the registry’s first conversion.

## Decision

### State ownership classes

A handler’s relationship to state is classified by ownership. Each class has one access shape:

1. **Own state**

   The actor’s `&mut A::State`. The inbox serializes mutation. This remains unchanged.

2. **Read-dominated shared state**

   One owner mutates the working state. Other code reads published snapshots through views and requests mutation through staged effects.

3. **Write-hot shared state**

   Writes remain local and are periodically folded or aggregated. The existing `CostCell` hot path remains in this class.

If an owner becomes hot enough to limit the workload, or a true synchronous commit requirement emerges, the state must be reclassified or the owner sharded from measurement. Callers must not bypass the ownership model.

### Views

A `View<T>` is the ownable read handle for a single-writer structure’s published state. It is a cheap-to-clone wrapper around an `arc-swap` slot and is supplied through the ADR-0156 parameter channel.

Loading the view yields a point-in-time snapshot guard:

- the currently published snapshot is pinned by the slot;
- superseded snapshots remain valid while readers hold them;
- readers never acquire the writer’s synchronization primitive;
- consumers must not infer that a loaded snapshot remains current after they act on it.

`ViewPublisher<T>` is the non-`Clone` writer half. Exactly one authoritative owner holds it after the runtime seal.

View surfaces are separated from the beginning:

- **keyed views** resolve a specific identity or route;
- **enumeration views** expose the publicly live inventory.

This distinction is required because an actor may be addressable while `Starting` but must not appear in public inventory until it is `Live`. It also permits later route-view sharding without changing consumers.

### Publication structure

The routing view uses two `FxHashMap` buffers with operation replay:

1. a drained owner batch is applied to the standby buffer;
2. the previous publication’s replay lag is also applied;
3. the buffers swap through the `arc-swap` slot;
4. publication is O(1);
5. each mutation is normally replayed into two ordinary hash maps.

If a reader still pins the two-publications-old snapshot, `Arc::make_mut` may clone that buffer for the cycle. This is the straggler valve, not the ordinary publication path.

Replay preserves owner inbox order. Tests must cover order-sensitive sequences such as register-then-drop and drop-then-register.

`kinds` and `name_index`, which mutate together, use the same ownership rule and may use a simpler published representation where measurement permits.

### Effects and the registry owner

Handlers do not mutate shared registry state directly. They append effects to the handler’s work buffer. Handler completion flushes the ordered effects as one batch envelope to a private registry owner.

The owner:

- drains effects in inbox order;
- applies a batch to its private working state;
- publishes each affected view once per drained batch;
- emits publication events only after the corresponding view is visible;
- never executes actor-authored lifecycle or dispatch code;
- never blocks waiting for an actor handler or execution home.

The registry owner is an internal runtime authority, not a public `NativeActor` that application code can address with arbitrary mail.

During migration, owner-applied effects and legacy direct effects share the existing transitional writer serialization. Both paths publish the same views and events. The seal is what removes that shared writer: it takes the last boot token out of circulation, leaving the owner as the only caller that can name the guard at all. The guard itself survives as a plain `Mutex` — it never had a reader half, because every table read loads a published view.

A handler must never synchronously wait for owner completion. Such a wait can deadlock a one-worker pool and prevents the parent actor from receiving its own completion turn.

## Published route lifecycle

A published route has lifecycle state as well as an endpoint. The private representation is equivalent to:

```rust
struct RouteRecord {
    canonical_name: Arc<str>,
    lifecycle: RouteLifecycle,
}

enum RouteLifecycle {
    Starting {
        token: ActivationToken,
        endpoint: StartingEndpoint,
    },
    Live {
        endpoint: RouteEndpoint,
    },
    Dropped,
}
```

`RouteEndpoint` and `StartingEndpoint` are storage-neutral private values. They must not expose `MailboxEntry`, `DispatcherSlot<A>`, `Box<A::State>`, an arena id, a slab coordinate, or a Wasmtime `Store`.

The observable lifecycle contract is:

| Operation | `Starting` | `Live` |
| --- | --- | --- |
| Keyed route lookup | Finds the reserved actor | Finds the live actor |
| Sending mail | Parks through the owner | Dispatches normally |
| Exact canonical-name lookup | Resolves the reserved identity | Resolves the live identity |
| Descriptor enumeration | Excluded | Included |
| Inventory change event | Not emitted | Emitted after Live publication |
| `ActorRegistry::is_live` | False | True |
| Monitor installation | Rejected | Allowed |
| Seize handle | Not installed | Installed |
| Wake or handler dispatch | Forbidden | Allowed |

`Starting` is not partially live. It exists so that:

- `wire` can resolve the actor’s stable identity;
- subscription validation can find the actor;
- self-mail and racing mail can be retained;
- no ordinary envelope can enter actor dispatch before `wire` finishes.

For every owner batch, all accepted `Starting` records are installed into the working state and the keyed view is published once before any activation job from that batch is scheduled.

Activation completions do not form a globally ordered prefix. Each valid token promotes its own actor independently. One slow or hung activation must not prevent unrelated completed actors from becoming `Live`.

## Actor runtime identity

Native handler execution carries one canonical runtime identity:

```rust
struct ActorRuntimeIdentity {
    /// Actor-owned namespace identity.
    logical: ActorId,

    /// Concrete lineage-derived address for this instance.
    mailbox: MailboxId,

    /// Lineage fold used to derive descendants.
    carry: u64,

    /// Canonical rendered path.
    canonical_name: Arc<str>,
}
```

For a native actor `A`:

```rust
logical = ActorId::singleton(A::NAMESPACE)
```

This logical identity is:

- the runtime key used to validate declared parent relationships;
- the native prototype identity from which a later cohort may be selected;
- independent of the actor’s concrete mailbox address;
- independent of its current storage location;
- not a Rust `TypeId`;
- not an arena id.

Several runtime implementations may intentionally represent the same logical actor namespace. Storage placement may also change without changing actor identity.

This native logical identity is insufficient to choose a Wasm guest prototype. Every guest actor is hosted through the native `WasmTrampoline` identity, while one module may export several guest actor types and state layouts.

A Wasm prototype requires at least:

```rust
struct WasmPrototypeKey {
    module_family: ModuleFamilyId,
    guest_actor: ActorTypeTag,
    abi_revision: AbiRevision,
    state_schema: StateSchemaRevision,
}
```

The Store shard or slot coordinate is selected only after the prototype. Native logical identity, guest prototype identity, instance address, and storage coordinate remain distinct concepts.

## Immediate build and staged commit

Actor birth is divided into work that can be completed immediately and work requiring authoritative shared-state mutation.

Everything that can be done safely at the caller’s current execution home is done immediately.

All post-seal births use the same storage-neutral lifecycle:

```text
construct and init -> publish Starting -> wire at the execution home -> publish Live
```

For a handler-spawned detached sibling or companion, the caller-local reservation records uniqueness and completion but does not create a parent-drop cascade; ADR-0097’s independent-lifetime rule remains unchanged. A logical child additionally retains its declared parent relationship and relative-addressing metadata. A post-seal root or other external actor has no parent-local reservation or parent task ledger: it uses the same owner and activation protocol with an external control completion. Pre-seal boot actors remain under the private boot authority described below.

For a handler-spawned child, the builder:

1. validates the subname and local configuration;
2. verifies the compile-time `Child: ChildOf<DeclaredParent>` relationship;
3. compares the executing binding’s logical identity with `DeclaredParent`;
4. reserves the child key in the parent binding’s local reservation table;
5. derives the deterministic mailbox id and canonical name;
6. constructs the actor state;
7. runs `init` immediately;
8. prepares the route, activation adapter, cost cells, bootstrap mail, and completion;
9. appends one storage-neutral spawn effect to the handler work buffer.

The parent-local reservation key is equivalent to:

```rust
struct ChildReservationKey {
    child_type: ActorId,
    child_node: ActorId,
}
```

Because the table belongs to one parent binding, the key does not repeat the parent mailbox.

A synchronous validation, construction, or `init` failure releases the reservation and returns directly. The deferred completion is armed only after locally fallible preparation has succeeded.

`ParentReservation` is a move-only weak capability into the spawning binding’s child-key table. It does not keep the binding alive and does not itself imply lifecycle ownership. On authoritative rejection, it releases the staged key after rollback and before completing the failed `SpawnOutcome`. On successful Live publication, it promotes the staged key to the binding’s live-child set before completing the successful one; ordinary live teardown later releases that live key. If the binding has disappeared, finalization is a no-op and the independently live actor retains its accepted lifetime policy. No path may finalize the reservation twice.

Arbitrary application effects explicitly permitted from `init` remain application behavior. The runtime guarantee is narrower: handler-time spawn preparation performs no direct write to the routing registry, global namespace table, actor-liveness registry, or global cost index.

For handler-spawned birth, the permanent effect vocabulary is equivalent to:

```rust
struct PreparedSpawnCommit {
    identity: ActorRuntimeIdentity,
    route: PreparedRoute,
    activation: Box<dyn PreparedActivation>,
    costs: PreparedCostCells,
    after_init: Vec<Envelope>,
    parent_reservation: ParentReservation,
    completion: DeferredCompletion<SpawnOutcome>,
}
```

The value must remain storage-neutral. A legacy adapter may privately own a boxed dispatcher. A future native adapter may own a typed page lease. A Wasm adapter may target a Store shard. Those representations do not cross the effect contract.

A conceptual activation interface is:

```rust
trait PreparedActivation: Send {
    /// Reserves storage and produces a one-shot activation job.
    /// This method never invokes actor-authored code.
    fn reserve(
        self: Box<Self>,
        token: ActivationToken,
        homes: &ExecutionHomes,
    ) -> Result<ReservedActivation, SpawnError>;
}

struct ActivationReady {
    token: ActivationToken,
    live: Box<dyn LiveActivation>,
}

trait LiveActivation: Send {
    /// Retains already-wired state and exposes its live endpoint.
    ///
    /// All recoverable validation and reservation happened before `wire`,
    /// so installing a valid token is recoverably infallible.
    fn install(
        self: Box<Self>,
        owner: &mut RegistryOwnerState,
    ) -> LiveEndpoint;

    /// Runs post-wire cancellation at the same execution home.
    fn cancel_at_home(
        self: Box<Self>,
        homes: &ExecutionHomes,
    );
}
```

These dynamic calls occur once per birth or cancellation, never per mail dispatch or actor update.

## Staged birth sequence

The complete handler-spawn sequence is:

```text
parent handler
    validate declared placement and parent runtime identity
    reserve the parent-local child key
    derive mailbox id and canonical name
    construct state
    run init immediately
    prepare route, activation, costs, bootstrap mail, and completion
    append PreparedSpawnCommit
    return the deterministic id and completion receipt

handler flush
    fold same-flush mail for the child into its parked bootstrap tail
    submit one ordered effect batch
    flush causally later announcement mail after the spawn effect

registry owner
    validate authoritative namespace, name, id, and tombstone state
    reserve lifecycle and storage
    install the exact prepared cost cells
    preload bootstrap and already parked mail
    install a keyed-only Starting route
    publish every Starting route accepted from the batch
    schedule activation jobs only after that publication

registry owner, on authoritative rejection before wire
    roll back token-owned storage and cost rows
    drop initialized state exactly once without unwire
    release the staged parent-local reservation
    complete SpawnError into the parent ledger

actor execution home
    run wire without draining the actor inbox
    flush wire-time mail and effects into the parked tail
    return ActivationReady(token, LiveActivation)

registry owner
    validate the activation token
    retain live storage
    install liveness and seize metadata
    install the live route with the parked tail already present
    promote this actor independently to Live
    publish the live keyed and enumeration views
    emit the inventory change
    issue one catch-up wake
    promote the parent-local reservation to the live-child set
    complete SpawnOutcome into the parent ledger

parent actor’s later turn
    receive TaskDone<SpawnOutcome>
    commit monitors, maps, resources, and success replies
    or perform typed failure cleanup
    reply through the TaskDone or explicitly release without reply
```

The deterministic mailbox id returned during the first step is a reservation, not evidence of success.

Both authoritative success and authoritative failure complete on a later parent turn. Consumers must not install monitors, business indexes, resources, or success replies merely because the deterministic id was returned.

A valid activation token cannot encounter an ordinary recoverable failure during promotion. Name conflicts, namespace conflicts, tombstones, storage capacity, cost installation, and other expected rejection conditions are resolved before `wire`.

Fatal allocator failure, poisoning, or violated internal invariants retain Aether’s fatal posture rather than becoming a post-`wire` `SpawnError`.

## Execution homes

`wire`, `unwire`, and storage access run at the actor’s execution home.

An execution home is an access and affinity capability, not necessarily one permanently dedicated operating-system thread.

Initial and future homes include:

- the existing legacy `Drainable`/dispatcher scheduler;
- the caller thread for a pumped actor;
- a future native typed cohort or page;
- the Wasmtime `Store` shard that owns a guest instance.

The registry owner schedules work at the home but never runs actor lifecycle code itself.

For the legacy native adapter, the one-shot activation job owns the initialized dispatcher slot while it runs `wire`. A future arena adapter instead owns the typed page/slot lease. The surrounding route, completion, settlement, and mail-ordering protocol does not change.

## Pending mail and birth ordering

Mail to a `Starting` actor is retained, not dispatched.

The parked FIFO includes:

- explicit `after_init` mail;
- same-flush mail addressed to the deterministic child id;
- cross-flush mail racing route publication;
- wire-time self-mail;
- other mail received before Live promotion.

The explicit bootstrap prefix is installed first. Subsequent envelopes retain owner-observed order.

Same-flush mail is folded directly into the prepared birth and requires no route-miss round trip.

A route-view miss submits an ordered `ParkOrDrop` effect:

```text
route-view miss for envelope E
    submit ParkOrDrop(E) to registry owner

owner examines authoritative working state
    Starting -> append E to that actor's parked tail
    Live     -> forward E through the current live endpoint
    absent   -> apply the existing unknown-recipient policy
```

There is no concurrently writable pending-birth map.

Spawn-before-announcement follows from effect order:

- the spawning handler submits the prepared spawn before mail announcing the child;
- any receiver of that announcement can only send after the original spawn batch was submitted;
- the owner therefore observes the reservation before the causally later miss;
- mail parks until the child is live.

Before publishing `Live`, the owner installs the live endpoint with the parked FIFO already attached. New senders therefore append behind retained mail rather than overtaking it.

## Deferred completion and settlement

Staged birth reuses the ADR-0093 in-flight task ledger. It does not create a second completion system and does not spawn a blocking operating-system thread merely to wait for an owner result.

The ledger gains an operation that arms a deferred result directly:

```rust
fn arm_deferred<O, C>(
    binding: &Arc<NativeBinding>,
    hold: SettlementHold,
    reply_to: Source,
    context: C,
) -> (DispatchId, DeferredCompletion<O>);

struct DeferredCompletion<O> {
    binding: Weak<NativeBinding>,
    actor: MailboxId,
    id: DispatchId,
    _output: PhantomData<fn(O)>,
}

impl<O: Send + 'static> DeferredCompletion<O> {
    /// Returns false if the binding or entry disappeared, was cancelled,
    /// or was already completed.
    fn complete(self, output: O) -> bool;
}
```

`DeferredCompletion` is move-only and completion consumes it.

The in-flight table becomes fill-once:

- the first valid fill stores the output and sends `TaskCompletionWake`;
- a missing, cancelled, or already-filled dispatch id returns `false`;
- a duplicate or stale completion cannot overwrite an output;
- a duplicate or stale completion cannot send a second wake.

An ADR-0080 settlement hold is acquired when the staged operation is armed. It remains held through owner apply, activation, and the parent’s completion handler. The parent releases it only after its business continuation and reply are resolved, or explicitly through the no-reply path.

If the parent binding disappears:

- its in-flight ledger and settlement holds are dropped;
- the completion’s weak binding upgrade fails;
- no spurious wake is sent;
- the child retains ADR-0097’s independent lifetime policy.

Parent disappearance does not silently introduce a parent-drop cascade.

## Cost cells

Hot cost recording remains class 3: actor-local `CostCell` values are mutated without an owner hop and folded later.

Spawn-time cost-index installation is part of the fused owner commit.

Preparation constructs the exact cells that both the actor and global cost index will share:

```rust
struct PreparedCostCells(Vec<(KindId, Arc<CostCell>)>);
```

Build places these `Arc<CostCell>` values into the actor’s local `CostCells`. Owner apply installs the same arcs globally under `(MailboxId, KindId)` before activation.

Tests must verify pointer identity between the actor-local and globally indexed cells.

`WasmTrampoline::init` must not independently seed the global `CostTable`. It prepares the guest manifest’s handler cells locally and hands those exact cells to the owner effect.

Rollback removes only cost rows installed by the corresponding activation token.

Replacement and teardown cost mutations remain on their explicitly assigned lifecycle paths until migrated. This decision does not claim that every application effect reachable from `A::init` is globally write-free.

## Activation cancellation

Cleanup depends on how far activation progressed:

| Phase | Cleanup |
| --- | --- |
| Initialized but never wired | Drop state and owned resources; do not call `unwire` |
| Wired but not transferred to owner | If the owner channel closes, the activation job runs `unwire` locally, then drops |
| Owner receives stale or invalid token | Schedule cancellation at the same execution home; the owner never calls actor code |
| Live | Use the normal close, drain, `unwire`, and drop lifecycle at the storage home |

After `wire`, ownership transfers only when the owner accepts `ActivationReady`.

```text
owner accepts ActivationReady(token)
    owner owns the wired lease

owner channel closes before transfer
    activation job unwires locally and drops

owner rejects a stale or invalid token
    owner schedules same-home cancellation
```

For post-wire cancellation, the owner retains the `Starting` reservation and installed cost rows until same-home `unwire` completes. Only then does it remove the route, lifecycle reservation, and token-owned rows.

Owner shutdown proceeds in this order:

1. stop accepting new submissions;
2. drain or reject queued effects;
3. cancel every `Starting` actor;
4. wait for every activation home to acknowledge cancellation or local cleanup;
5. stop scheduler and Wasmtime execution homes;
6. drop owner working state and publishers.

An execution home must not disappear while the owner still depends on it to unwind a `Starting` actor.

## Boot seal and authority domains

Worker-pool construction is not the registry-authority boundary.

The actual boot sequences are:

```text
Built chassis
    create worker pool
    resolve passive graph
    claim passive mailboxes
    init, wire, and spawn passives
    run driver Start-stage boot
        driver may claim mailboxes
        driver may boot pumped actors
        driver may register capabilities
        driver may seed costs
    seal registry authority
    return BuiltChassis
```

```text
Passive chassis
    create worker pool
    resolve passive graph
    claim passive mailboxes
    init, wire, and spawn passives
    seal registry authority
    return PassiveChassis
```

Direct boot mutation is allowed only through crate-private authority carrying an unforgeable boot token, `BootAuthority`. Every route into the direct writer takes one by reference, so the seal is the moment the last token leaves circulation rather than a flag anything has to consult.

That last token is the one on the chassis `Spawner`, which is the only holder that outlives boot. `Spawner::seal` takes it; nothing can mint another, because `BootAuthority::new` is crate-private and every remaining mint site (the shared boot, the chassis ctx, the owner attach) is spent or dropped by then.

The authority domains are therefore distinguished by what a caller can reach, not by three parallel builder types:

```rust
/// Handler authority. Can only stage a prepared effect and receipt.
struct HandlerSpawnBuilder<A, Parent> {
    /* no eager/direct terminal */
}

/// Boot and post-seal external authority, resolved at commit time.
struct SpawnBuilder<A> {
    /* pre-seal: direct apply under the Spawner's token
       post-seal: submits through the owner and waits outside actor execution */
}
```

An eager or synchronous return does not imply a direct write.

`SpawnBuilder::finish()` synchronously returns:

```rust
Result<MailboxId, SpawnError>
```

Post-seal it does so by submitting through the owner and waiting outside an actor handler — first for the owner to accept the birth, then for the activation's execution home to hand the route back `Live`, so a caller that sees `Ok` can address the mailbox it was given. Waiting is safe here and only here: the caller is an embedder thread, never a pool worker, so it is never the worker the owner needs.

Post-build pumped actors use a two-ack handshake:

```text
external caller reserves a Starting route
registry owner publishes Starting and returns its token
caller-thread execution home runs init and wire
caller hands the owner the wired endpoint
registry owner promotes Live and releases the parked mail
external caller receives final result
```

The window between the two acks is why the reservation exists: mail addressed to the actor while its caller is still wiring parks in the owner and continues to the promoted endpoint in owner-observed order, instead of warn-dropping against a name that does not exist yet.

The caller thread never receives direct post-seal registry writer authority.

## Component-runtime writers

Every steady-state routing writer must pass through the same owner.

Component kind registration becomes a continuation:

```text
component handler
    parse and locally validate descriptors
    stage RegisterKinds
    wait for deferred owner completion

owner
    atomically register or match descriptor batch
    publish kind view
    return typed result

component continuation
    on success, continue ModuleBoot or RequestedActor
    on failure, return LoadResult::Err
```

The continuation constructs its reply from its own staged payload and the typed owner result. It does not read the newly published view merely to reconstruct data it already owns.

The Wasm inline-child host function stages a logical alias route:

```rust
struct PreparedAliasRoute {
    alias: MailboxId,
    rendered_name: Arc<str>,
    target_parent: MailboxId,
}
```

The alias targets the logical parent route. It does not clone or retain a `MailboxEntry`, inbox closure, dispatcher slot, arena coordinate, or Wasmtime instance handle.

Guest inline-child state initialization remains immediate inside the current Store. The alias becomes globally visible only after the guest handler flush reaches the owner.

Repeating the same alias-to-parent mapping is idempotent. A conflicting target completes with a typed failure using the existing detached-sibling failure and settlement posture.

## Inventory publication

Registry publication and public inventory publication are related but distinct.

A keyed `Starting` publication:

- updates keyed lookup generation;
- does not add a descriptor;
- does not emit the public inventory wake.

A `Live` promotion:

1. installs the public descriptor;
2. publishes keyed and enumeration views;
3. advances the public inventory generation;
4. emits `RegistryChanged`.

A live removal performs the corresponding public-enumeration publication and event.

A cancelled actor that never became `Live` does not emit a misleading inventory removal for an actor that was never announced.

Inventory consumers treat `RegistryChanged` as a coalescing wake. They load the latest enumeration view and compare its generation rather than expecting one event per mutation.

The old `set_on_mailbox_change` callback is removed; it has no production installer.

## Migration order

The single-writer seal cannot precede the staged handler path. Blocking handlers on the owner would deadlock at one worker, while retaining direct calls after the seal would create a second writer.

The safe rollout is:

```text
#4031 view primitive
    |
    v
#4033 endpoint-neutral keyed and enumeration views
    - legacy writer lock remains
    - direct and owner paths publish through the same publisher
    |
    v
#4062 additive registry owner/effect foundation
    - ordered batch apply
    - private prepared route/activation vocabulary
    - publication events
    - owner and legacy effects share transitional serialization
    - no actor wire
    - no production seal yet
    |
    +--> #4063 inventory subscriber
    |
    v
#4058 native runtime enforcement of declared child placement
    |
    v
#4064 storage-neutral staged native birth foundation
    - ActorRuntimeIdentity
    - parent-local reservations
    - prepared cost cells
    - Starting routes
    - execution-home activation
    - deferred completion
    - pending-mail ordering
    - independent token promotion
    |
    +--> #4065 component and Wasm continuation migration
    +--> #4066 TCP continuation migration
    +--> #4067 HTTP continuation migration
    +--> #4068 fleet continuation migration
    `--> #4069 game continuation migration
             |
             v
#4070 remove handler eager authority
    - distinct handler builder surface
    - compile-time proof that handlers cannot name direct/eager apply
             |
             v
new #4035 final-seal child
    - seal after passive or driver boot
    - transfer working tables and publishers to exclusive owner authority
    - remove transitional writer/direct steady-state mutation
    - add synchronous external owner control
    - add pumped execution-home handshake
    - establish shutdown ordering
```

Supporting work retains its existing purpose:

- #4032 extracts build from commit without exposing the boxed storage representation.
- #4034 constructs component load results from staged payloads rather than stale readback.
- #4035 remains the registry-owner coordination umbrella.
- #4036 remains the staged-handler-birth coordination umbrella.

#4036 may close after #4070 proves handler authority is staged. #4035 closes only after its owner foundation, inventory publication, and final-seal child are complete.

`ActorRegistry` monitor/liveness teardown and `CostTable` teardown are separate lifecycle working sets. Their remaining direct mutations must be assigned explicitly when teardown migrates; neither is permission to retain a post-seal routing writer.

The final-seal issue must classify every production routing mutation as exactly one of:

- private pre-seal boot authority;
- owner apply;
- synchronous external owner control;
- test-only setup.

An unexplained post-seal direct routing mutation fails the seal.

At minimum, the final audit searches for:

```text
register_inbox
try_register_inbox
register_inline
try_register_inline
register_kind
register_kind_with_descriptor
register_or_match_all
remove_closure
drop_mailbox
install_seize_handle
set_on_mailbox_change
spawn_inline_child_p32
```

Test fixtures may use explicit pre-seal setup or a test-only synchronous owner helper. Production mutation authority must not be widened for test convenience.

## Required invariants and verification

Implementation of this ADR must prove:

- `Starting` is visible through keyed identity and exact-name lookup;
- `Starting` is excluded from descriptor enumeration;
- `Starting` cannot dispatch an envelope;
- `Starting` is not reported live and cannot be monitored, seized, or woken;
- every accepted `Starting` record in a batch is published before any activation job from that batch is scheduled;
- activation tokens promote independently, without a global completed-prefix barrier;
- `wire` runs at the legacy scheduler home for boxed native actors;
- pumped-actor `wire` runs on the caller-thread home;
- Wasm `wire` runs in the owning Wasmtime Store shard;
- the registry owner never runs actor-authored `wire` or `unwire`;
- every recoverable apply failure happens before `wire`;
- installing a valid activation token is recoverably infallible;
- initialized-but-unwired rollback drops exactly once and does not call `unwire`;
- authoritative apply rejection releases the staged parent-local reservation before completing failure;
- post-wire cancellation calls `unwire` exactly once at the same execution home;
- owner-channel closure after `wire` causes local same-home cleanup;
- a stale token cannot install state or complete a parent twice;
- duplicate deferred completion cannot overwrite output or emit a second wake;
- parent binding loss releases its ledger and settlement hold without a spurious wake;
- parent binding loss does not accidentally cancel an independently live child;
- Live promotion converts the staged parent-local reservation before completing success, and live teardown releases the resulting live-child key;
- root, detached companion or sibling, and logical-child births share the init/Starting/wire/Live protocol without changing their accepted relationship or cascade semantics;
- explicit `after_init` mail remains the bootstrap prefix;
- same-flush, cross-flush, and wire-time mail remain FIFO behind that prefix;
- newly dispatched Live mail cannot overtake the parked tail;
- `Starting` publication emits no public inventory event;
- Live promotion publishes enumeration before emitting inventory change;
- the actor-local and globally indexed cost entries contain the same `Arc<CostCell>`;
- handler-time Wasm initialization performs no global cost-table seed;
- a built chassis seals only after successful driver `Start`;
- a passive chassis seals immediately before returning `PassiveChassis`;
- post-seal external spawn uses owner control;
- pumped post-seal spawn uses the two-ack activation handshake;
- owner shutdown cancels and joins every `Starting` actor before execution homes stop;
- no production post-seal routing writer survives outside the owner.

The existing contention benchmark remains the performance baseline for the view and owner conversion. Later arena measurements are separate and must compare equivalent lifecycle, routing, settlement, and mail workloads.

## Consequences

- Dispatch reads become wait-free and avoid reader-count cache-line contention.
- Spawn and routing no longer convoy on one reader/writer lock after the final seal.
- Shared mutations gain an explicit ordering and completion contract.
- Actor construction and `init` remain immediate.
- Actor success becomes asynchronous for handlers.
- Spawn success requires at least one owner turn, one execution-home activation turn, and one later parent completion turn.
- Deterministic addressability precedes liveness.
- A slow or hung `wire` stalls its own actor and execution home or Store shard, not the registry owner or unrelated activation tokens.
- Pending birth mail consumes owner-managed memory, similar to an unbounded actor inbox. A later bounded-mail policy may cover both without changing ordering.
- The routing publisher holds two map buffers, increasing resident memory.
- Cold-path dynamic dispatch occurs once per birth or cancellation; ordinary actor updates and mail dispatch do not gain a new virtual call.
- Post-seal external synchronous spawn remains possible without restoring a second registry writer.
- The registry owner coordinates route metadata and lifecycle transitions but does not own every actor state allocation.
- A later native arena or Wasm cohort changes the prepared activation adapter and live endpoint, not actor-birth semantics.
- The transitional writer lock is migration machinery, not accepted steady-state architecture.

Sharding remains deferred until measurement identifies a reason.

The keyed view and single effect-submission chokepoint preserve a contained sharding seam. Revisit sharding when at least one of these occurs:

- sustained churn exceeds approximately 5% of the measured single-owner ceiling;
- a genuine commit-latency requirement attaches to spawn or route mutation;
- a consumer appears that requires an atomic cross-shard snapshot and justifies the coordination cost.

## Non-goals

This ADR does not decide:

- native actor arena layout or allocator implementation;
- page, slab, chunk, free-bit, or generation-key representation;
- whether all `A::State` values must implement `Kind`;
- property indexes, actor queries, or query mail;
- secondary typed arenas for maps, vectors, strings, or buffers;
- shared host/Wasm linear memory;
- Wasm module-family sharding policy;
- automatic arena compaction;
- per-actor rolling code replacement;
- component hot-swap policy;
- population-hint or preallocation APIs;
- frame-stage bulk update APIs.

Those decisions may build on the storage-neutral route and activation seams established here. They must not leak their storage coordinates into actor identity or the permanent effect vocabulary.

## Alternatives considered

### Shard the existing `RwLock`

Sharding divides contention but leaves every read lock-bound and preserves ambient direct mutation. It does not produce the view/effect ownership contract. It remains a possible publisher implementation only if measurement reaches a sharding trigger.

### Fleet-wide barrier or zipped RCU registration

A global barrier requires cross-batch merge rules and two-tier visibility. Ordered handler effect batches already reduce mutation ordering to the owner inbox.

### Persistent map publication

Structural sharing avoids a whole-map clone but was materially slower on both the update and hot lookup paths in the spike.

### Clone the complete map for every publication

The cost becomes prohibitive at high actor counts. Double-buffer operation replay retains O(1) publication.

### Capability-local routing for ephemeral actors

This avoids global registration but removes universal addressability, actor logs, cost queries, settlement lineage, and ordinary typed mail. It remains appropriate for non-actor capability-local state, not actors.

### Synchronous concurrent map

A sharded-lock or lock-free map can reduce contention but retains mutation as an ambient side effect. It does not expose read-after-write assumptions or solve ownership and view injection.

### Run `wire` on the registry owner

Rejected. Native and guest lifecycle code can perform arbitrary work, block, trap, send mail, and require a Wasmtime Store affinity. Running it on the owner creates global head-of-line blocking and couples route ownership to actor storage.

### Publish `Live` before `wire`

Rejected. It permits dispatch, monitoring, inventory publication, and seizure before lifecycle activation completes.

### Hide `Starting` from keyed lookup

Rejected. Wire-time validation and self-mail would race the same route-publication gap the lifecycle state is intended to close.

### Globally ordered activation completion

Rejected. One slow actor or Wasm hook would prevent already completed unrelated actors from becoming live.

### Concurrent pending-birth map

Rejected. It creates a second shared mutation authority. Ordered `ParkOrDrop` effects preserve the necessary guarantee through the owner.

### Block the parent handler on owner completion

Rejected. It deadlocks with one worker and prevents the parent from handling its own completion.

### Report immediate success and settle only failures

Rejected. Returning the deterministic id proves only reservation. Success is not real until authoritative validation, execution-home `wire`, and Live publication complete.

### Keep direct post-seal embedder mutation

Rejected. It creates a second writer. Synchronous external owner control preserves the caller-facing result without violating ownership.

### Make the registry owner a public application actor

Rejected. The mutation vocabulary is a private runtime capability. Application mail must not be able to forge registry effects or interfere with owner scheduling.

### Give the owner a dedicated operating-system thread

Rejected as the default. A private drainable owner can use existing scheduler capacity and batching. A dedicated thread may be reconsidered only from measurement.

### Seal at worker-pool creation

Rejected. Passive boot and driver `Start` execute after pool creation and still require direct boot authority.

### Expose boxed state or arena coordinates in spawn effects

Rejected. It would freeze the first storage implementation into routing, completion, and consumer APIs and make the later arena experiment a cross-runtime rewrite.
