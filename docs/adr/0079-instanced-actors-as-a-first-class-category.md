# ADR-0079: Instanced actors as a first-class category

- **Status:** Proposed
- **Date:** 2026-05-06
- **Amended:** 2026-05-09 — Section 6 retired the `on_close` name in favour of `unwire`, added the symmetric `wire` hook, and recorded the rationale for moving away from `Drop`'s reserved Rust semantics.

## Context

ADR-0074 collapsed components and capabilities into one actor model. Every long-lived state-owning entity is an actor: one mpsc inbox, one OS thread, one `MailboxId`, communicating exclusively via mail. The model has worked well for the entities that exist today: chassis caps (Render, Log, Io, Audio, Net, Control, etc.) and wasm components loaded by name (player, camera, mesh-viewer).

But every actor in that model is *singular*. There is exactly one `RenderCapability`, one `LogCapability`, one player component. The framework's lookup primitive — `ctx.actor::<R: Singleton>()` — keys by `TypeId`, which only works because `R` resolves to exactly one running instance. Wasm components address each other by name only as a special case that lives outside the chassis `actors` map; it works, but it's a separate code path with its own dispatcher (`ControlPlaneCapability::ComponentRouter`) and its own runtime-named lookup (`resolve_actor`).

This ADR is forced by **socket actors**. The natural shape for a network capability is a singleton listener (`NetCapability`, owning the accept loop) that spawns a per-connection actor (`SessionActor`, owning that connection's read/write state and OS handle) for each accepted connection. The same pattern recurs whenever the framework needs N distinct actors of the same compile-time type: per-monster AI, per-camera state, per-document editor, per-session game logic. Today there's no first-class way to express it. The wasm-component name-keyed path is the closest analogue, but it's only available to wasm; native types have no equivalent.

The constraints carried in:

- **ADR-0029.** `MailboxId` is a 64-bit FNV-1a hash of the mailbox name. Whatever shape we adopt for instanced names must hash to a `MailboxId` through the existing function — wire format unchanged.
- **ADR-0038.** Actor lifecycle is channel-drop + join. One OS thread per actor.
- **ADR-0074 §Decision 5.** `FRAME_BARRIER` lets render coordinate per-frame consistency without sinks-as-sync.
- **ADR-0063.** Substrate fail-fast on traps; deferred Phase 2 (epoch deadlines) is the long-term backstop for stuck actors.

The forces we're balancing:

- Singular actors stay singular — most caps and components are conceptually one-of-a-kind.
- Instanced actors must be first-class enough that the framework participates in their lifecycle (registration, monitoring, cleanup) rather than each cap reinventing it.
- The wire format and existing actor SDK should accommodate the new category additively, without re-shaping `MailboxId` or breaking existing types.
- The threading model under ADR-0038 (one thread per actor) extends to instances naturally for game-engine workloads (10s–100s of actors) but pushes against operational ceilings at hub-server scale (1000s+). That's a separable concern — a future scheduler ADR may revise threading without invalidating this one.

## Decision

Eight sub-decisions, designed together. Implementation phases follow in *Consequences*; the model is non-divisible.

### 1. Cardinality is an axis distinct from transport

Two orthogonal trait axes:

- **Transport** (existing): `NativeActor` / `WasmActor`. Where the code runs.
- **Cardinality** (new): `Singleton` / `Instanced`. How many of this actor exist at runtime.

All four cells reachable. Author picks based on intent.

|              | NativeActor                                                                | WasmActor                                                       |
| ------------ | -------------------------------------------------------------------------- | --------------------------------------------------------------- |
| Singleton    | `RenderCapability`, `LogCapability`, `NetCapability` (listener)            | most components today (player, camera)                          |
| Instanced    | `SessionActor` (per-connection), per-monster AI, per-camera                | "spawn me N of these" components (opt-in by author)             |

`Singleton` and `Instanced` are mutually exclusive at the type level. A given actor type implements one or the other, not both. The existing `Singleton` marker stays unchanged; `Instanced` is the new one:

```rust
pub trait Actor: Sized + Send + 'static {
    const NAMESPACE: &'static str;        // existing
    const FRAME_BARRIER: bool = false;
}

pub trait Singleton: Actor {}              // existing — NAMESPACE = full mailbox name
pub trait Instanced: Actor {}              // new       — NAMESPACE = prefix
```

`NAMESPACE` is reused with semantic that differs by sub-trait. For singletons it's the full mailbox name (e.g. `"aether.render"`). For instanced types it's the prefix; full mailbox names are `"{NAMESPACE}:{subname}"` (e.g. `"aether.net.session:42"`). `:` is the structural discriminator — currently free in chassis names — and reverse-parses unambiguously because we forbid `:` in `NAMESPACE` itself.

Wire format unchanged. `mailbox_id_from_name("aether.net.session:42")` hashes the same way `mailbox_id_from_name("aether.render")` does. ADR-0029 invariant preserved.

### 2. Naming uniqueness is global across cardinalities

`NAMESPACE` is unique per type. No Singleton/Singleton, Singleton/Instanced, or Instanced/Instanced collisions. One namespace, one owner.

**Convention** (documented, not framework-enforced): instanced types extend the owning listener's namespace with a segment. `NetCapability::NAMESPACE = "aether.net"` (singleton); `SessionActor::NAMESPACE = "aether.net.session"` (instanced). The dotted-prefix relationship reads "listener owns these instances" while keeping the strings themselves distinct so collision detection stays uniform.

A unified `validate_namespace_segment(s)` covers both registration time (NAMESPACE const) and spawn time (subname):

```rust
pub enum NamespaceError {
    Empty,
    ContainsSeparator,            // ':'
    ContainsControlOrWhitespace,
    TooLong { limit: usize },
}
```

### 3. Accessor split mirrors cardinality at the call site

```rust
// SDK-side (Ctx) — typed-send handles, returns ActorMailbox<...>:
fn actor<R: Singleton>(&self) -> ActorMailbox<'_, R, T>;                     // existing
fn resolve_actor<R: Instanced>(&self, subname: &str) -> ActorMailbox<...>;   // existing — bound tightened

// Infra-side (PassiveChassis / BuiltChassis) — peer at booted state, returns Arc<A>:
fn actor<A: Singleton + NativeActor>(&self) -> Option<Arc<A>>;               // existing — bound tightened
fn resolve_actor<A: Instanced + NativeActor>(&self, subname: &str) -> Option<Arc<A>>;
fn resolve_actors<A: Instanced + NativeActor>(&self) -> impl Iterator<Item = (&str, Arc<A>)>;
```

Calling the wrong API for a given cardinality fails to compile. Same names on `Ctx` vs `PassiveChassis` return different types — already true for the singleton path (`ActorMailbox` vs `Arc<A>`), so the divergence-by-surface convention extends unchanged.

### 4. Boot vs spawn split, with bootstrap mail closing the post-spawn race

Singletons boot at chassis-build time; instanced actors spawn at runtime in response to events. The existing `with_actor` tightens to `A: Singleton`, and a new `spawn_child` returns a builder for the instanced path:

```rust
impl<C> Builder<C> {
    fn with_actor<A: Singleton>(self, config: A::Config) -> Self;            // existing — bound tightened
}

impl NativeCtx<'_> {
    fn spawn_child<A: Instanced + NativeActor>(
        &self,
        subname: Subname<'_>,
        config: A::Config,
    ) -> SpawnBuilder<'_, A>;                                                 // new
}

pub enum Subname<'a> {
    Counter,            // listener-allocated monotonic
    Named(&'a str),     // caller-supplied
}

impl<'ctx, A: Instanced + NativeActor> SpawnBuilder<'ctx, A> {
    fn after_init<K: Kind>(self, mail: K) -> Self where A: HandlesKind<K>;
    fn finish(self) -> Result<MailboxId, SpawnError>;
}
```

`after_init` mail pre-populates the new actor's inbox before the dispatcher loop starts. The framework guarantees these mails are the first events the actor sees, no matter what other senders are racing. This closes a real race: between `spawn_child` returning and the spawner sending follow-up mail (e.g. listener registering itself as monitor), a third party could mail the new actor first; if that mail triggered self-shutdown, the spawner's follow-up would land at a dead mailbox. Bootstrap mail eliminates the window.

`spawn_child` returns `MailboxId` only — no Arc, no strong handle. Listeners track their children via `peer_addr → MailboxId` maps in their own state.

### 5. Init runs on the caller's thread

Init for an instanced actor runs on the spawning thread, before any dispatcher thread is created. If init fails, no thread is created.

```rust
fn init(&mut self, ctx: NativeInitCtx<'_>, subname: &str) -> Result<(), BootError>;
```

Lifecycle:

1. Validate subname (`NamespaceError` checks).
2. Check name uniqueness (`SubnameInUse` / `SubnameRetired`).
3. Construct the actor (`A::Config → A`) on caller's thread.
4. Run `actor.init(...)` on caller's thread. May send mail.
5. Init failed: tear down partial state, return `Err(InitFailed(...))`. No dispatcher created.
6. Init succeeded: register the mailbox.
7. Pre-load bootstrap mail (from `after_init`) into the inbox in builder order.
8. Spawn the dispatcher thread, moving the actor in. Steps 6–8 atomic under registry write lock.
9. Return `Ok(MailboxId)`.

`NativeInitCtx::self_id()` exposes the precomputed `MailboxId` so init can send subscribe-style mail referencing its own future address. The id is deterministic — `mailbox_id_from_name(full_name)` — so it's available before registration completes; replies route correctly once registration lands.

This model is honest in two ways the prior strawman wasn't:

- **Cheap failure path.** High-churn workloads (failing handshakes, repeatedly-loaded misbehaving wasm) don't pay thread-spawn + teardown for every miss.
- **Mailbox-existence ↔ actor-aliveness.** Registration happens iff init succeeds. An actor whose init failed never had a mailbox; no half-registered ghost states, no init-time tombstones to clean up.

Singleton init aligns to the same model as a separate cleanup pass — both cardinalities use caller's-thread init.

### 6. Lifecycle hooks: wire, unwire, and self-initiated termination

Actors get three lifecycle hooks beyond `init`. Two are mail-allowed; one signals the dispatcher to terminate. Termination itself remains self-initiated only — external triggering is a mail-level convention, not a primitive.

```rust
fn wire(&mut self, ctx: NativeCtx<'_>);                        // post-init, mail-allowed (default no-op)
fn unwire(&mut self, ctx: NativeCtx<'_>);                      // pre-shutdown, mail-allowed (default no-op)
fn shutdown(&self);                                            // on NativeCtx — signals termination
```

Lifecycle order: `init` (sync constructor, no mail) → `wire` (mail-allowed; subscribe, register, hello peers) → handler dispatches → `unwire` (mail-allowed; unsubscribe, goodbye peers) → dispatcher exits → registry close.

Three termination flows:

1. **Self-termination.** Actor decides it's done (e.g. socket EOF). Calls `ctx.shutdown()`. Dispatcher loop drains remaining inbox, runs `unwire`, exits.
2. **Cooperative external.** Listener (or anyone) mails the actor a "please close" kind. Actor's handler does cleanup and calls `ctx.shutdown()`. **No new framework primitive** — just a regular handler.
3. **Substrate shutdown.** Runtime drops every actor's inbox sender; each `recv()` returns `Disconnected`; dispatchers drain, run `unwire`, exit. Symmetric with self-shutdown from the dispatcher's perspective.

No "force kill arbitrary actor" admin primitive. The misbehaving-actor leak (an actor with no shutdown path or that refuses to handle close mail) is application correctness — author responsibility, not framework gap. Stuck-actor recovery falls back to ADR-0063 deferred Phase 2 and ultimately `terminate_substrate` (process-level SIGTERM → SIGKILL).

#### Naming: why not `on_drop`?

`Drop` is a Rust language item with reserved semantics — it runs at value-drop time (deterministically when the value goes out of scope or the owner is freed), can't return errors, can't be explicitly invoked, and is automatic for every type. Reusing the name for an SDK trait method invited confusion: which one runs when? Does overriding `on_drop` shadow `Drop`? Does the language hook fire too?

`unwire` names the hook for what it does (notify peers via mail before the actor disappears) rather than for the language feature it superficially resembled. The pair `wire` / `unwire` reads as a bracketed lifecycle phase: wire up to peers, do work, unwire from peers. Sync resource release continues to use Rust's `impl Drop` — the SDK trait surface stays out of `Drop`'s territory entirely.

The same logic extends to the FFI side: `FfiActor::on_drop` retires (issue 584). Wasm guests get the symmetric `wire` / `unwire` exports; sync cleanup is the wasm runtime's responsibility, not surfaced to SDK authors.

### 7. Names are tombstoned on close, never reused

When an actor closes, its mailbox name is permanently retired for the substrate's lifetime. Mail to a retired name drops with a warn-log; `spawn_child` with a previously-used subname is a hard error (`SubnameRetired`).

Registry slot transitions `Live(Arc<Dispatcher>)` → `Dead`, where `Dead` is a single static sentinel — not a per-name allocation. Memory cost is bounded by total lifetime spawn count. Per-tombstone storage is roughly one HashMap entry.

The actor *value* drops at close; its *name slot* persists as a tombstone. Identity outlives value.

We deliberately do not commit to **generational MailboxIds** (encoding a counter into the id so retired names can be safely reused). That would force a wire change — the id is currently `hash(name)` and would have to become `hash(name) ⊕ generation` or carry an extra field. Compaction (if a long-running workload measures real cost) is registry-internal and never touches spawn / lookup / hashing. The escape hatch for long-running workloads is substrate restart.

### 8. Discovery and monitoring are split between broadcast and framework primitive

Two distinct concerns:

**Spawn = per-cap broadcast.** When a listener spawns an instance, it broadcasts a cap-specific kind with rich metadata (e.g. `SessionAccepted { subname, peer_addr }` for `NetCapability`). External observers subscribe globally, the same way they subscribe to input streams. Cap owns the schema — different caps want different metadata.

**Close = framework-managed monitor primitive.** Per-cap convention here has four classes of subtle bug: forgotten demonitor, bidirectional ambiguity, accumulation of dead monitors, fan-out-failure semantics. The framework gets these right once:

```rust
fn monitor(&self, target: MailboxId) -> Result<MonitorHandle, MonitorError>;   // on NativeCtx

pub struct MonitorHandle { /* registry ref + target + entry id */ }
impl Drop for MonitorHandle { /* demonitor via registry */ }

pub struct MonitorNotice { pub target: MailboxId }                              // framework kind

pub enum MonitorError { TargetNotFound, TargetTombstoned }
```

Registry gains two indices (forward + reverse) for bidirectional bookkeeping:

- `monitors_of[X]`: who watches X.
- `monitoring[X]`: what X watches.

On actor close: drain `monitors_of[X]`, send `MonitorNotice` to each live monitor; iterate `monitoring[X]`, remove X from `monitors_of[t]` for each target. Both directions clean each other up — no accumulation of dead monitors.

Default unidirectional, like Erlang `monitor` (not `link`). Compose two unidirectionals if bidirectional is wanted. No `CloseReason` field on `MonitorNotice` for v1 (purely additive if needed). No monitoring of not-yet-existent targets — `monitor()` errors if target isn't Live at call time. No explicit `Demonitor` mail kind — registration via direct registry call, deregistration via `MonitorHandle::Drop`.

Replace semantics mesh cleanly with this: replace is "actor continues with new code/state," not "actor dies." `MonitorNotice` does *not* fire on replace. The mailbox stays Live throughout the splice; `monitors_of` entries are unaffected.

## Consequences

### Positive

- **Socket actors become a clean fit.** `NetCapability` (singleton) accepts connections and spawns `SessionActor` (instanced) per connection. The framework participates in lifecycle: monitors clean up in both directions, names are tombstoned, init failure is cheap, bootstrap mail closes the post-spawn race.
- **Framework gets honest about cardinality.** "How many of this actor exist" was implicitly answered by every existing type (always one); now it's a typed property. Wrong API for the cardinality is a compile error.
- **Monitor primitive eliminates a class of per-cap boilerplate.** Any cap that wants to learn about an actor's death uses `ctx.monitor(target)` and a `MonitorNotice` handler. No Vec<ReplyTo> bookkeeping in cap state, no fan-out logic in `unwire`, no demonitor registration to remember.
- **Init lifecycle aligns across both cardinalities.** Both singleton and instanced actors initialize on the caller's thread. Failed inits don't leak threads.
- **Replaceable doesn't have to compose with Instanced for native.** Native instances that want "swap implementation" semantics handle a `Reset` mail kind in their own protocol — the mailbox is the trampoline, the inner state is the actor's business. Replaceable narrows to the wasm hot-reload domain.

### Negative

- **Registry gains state.** Two new indices (`name_owners` for cardinality-uniformly enforcing namespace ownership; `monitors_of` + `monitoring` for the monitor primitive). Plus the actor lookup gains a third map (`tombstones`) to detect retired names. All bounded by lifetime spawn count, but non-zero.
- **Threading is delegated to a future ADR.** Under ADR-0038's actor-per-component, instanced actors mean one OS thread per session. Fine for game-engine workloads (10s–100s of actors); painful at hub-server scale (1000s+). This ADR doesn't solve that; it preserves the existing model and accepts the ceiling. A separate ADR on a topo-sort scheduler may revise — and threading falls out of that — but until then, `aether-scale` is bounded by thread count.
- **Wasm-side spawn is deferred.** Native is the v1 surface. When wasm components need to spawn instanced children (e.g. a game-logic component spawning N enemies), the host-fn shape needs settling. `aether.control.replace_component` generalizing to `replace_actor` rides on this.
- **Wasm components remain Singleton-by-default.** Today's latent flexibility (any component can be multi-loaded by name through `resolve_actor`) tightens under strict mutual exclusion: only components that opted into `Instanced` could be multi-loaded. We park that reframe until a forcing function arrives — the existing flexibility stays, but only as a runtime fallback, not as a model statement.

### Neutral

- **Convention for instanced namespace depth.** `aether.net.session` (instance type) under `aether.net` (listener) is documented, not framework-enforced. Authors can structure their namespaces however they want; the dotted convention is a reading aid, not a check.
- **`MailboxId` storage and wire format unchanged.** `:` is just bytes in the name. Generational ids explicitly out of scope.

## Alternatives considered

- **Marker trait rather than separate sub-traits.** A single `cardinality_is_instanced: bool` const on `Actor` could gate behavior at runtime. Rejected because the accessor APIs differ in *signature* (the instanced version takes a name argument); dispatching at runtime can't enforce wrong-API-doesn't-compile, which is the point of the type-level split.
- **Wasm components as Instanced (the original 607 framing).** Reframe `WasmHostActor` as the single Instanced type, with each loaded component a runtime instance. Rejected because it conflated two different things: addressing path (singletons by type, instances by name) and singleton-ness (whether the type is structurally one-of-a-kind). A player or camera component is conceptually a singleton even if the loader gave it a name; multi-instance loads are a latent capability, not a model statement.
- **Per-cap monitoring via convention.** Each cap manages a `Vec<ReplyTo>` of monitors and fans out in `unwire`. Rejected because of dead-monitor accumulation, ambiguous bidirectional semantics, fan-out-failure handling, and per-cap boilerplate that's easy to forget. Framework primitive gets the answer right once.
- **Framework-level lifecycle kinds for both spawn and close.** A single global `ActorSpawned`/`ActorDropped` pair every actor emits. Rejected because cap-specific spawn metadata is what subscribers actually want (peer_addr for net, position for monsters); generic `mailbox_name` only is a weak event. Spawn naturally splits to per-cap; close splits to per-actor monitoring.
- **Forced-kill admin primitive (`drop_actor`).** A framework path to terminate any actor by id. Rejected because actors are solely responsible for their own termination — a force-kill API just hides bad shutdown protocols rather than fixing them. Substrate-level recovery (ADR-0063 fail-fast, `terminate_substrate`) covers the genuinely-stuck case.
- **Generational MailboxIds.** Encode a per-name generation counter so retired names can be safely reused without race. Rejected because it forces a wire change (`MailboxId` shape) for a cost that isn't yet measured. Tombstones-as-permanent + substrate restart is the v1 stance; compaction can be added later as a registry-internal concern.
- **Spawn returning `Arc<A>` or `ActorHandle<A>`.** Bundle the spawn id with a strong handle so the listener can call methods on the new actor directly. Rejected because the contact surface for instanced actors is mail; peer-into-state via `Arc<A>` is a singleton pattern that doesn't generalize, and the listener's bookkeeping (peer_addr → MailboxId) doesn't need an Arc to be useful.
- **Boot-time `with_actor` overload for instanced types.** Boot N instances of an Instanced type at chassis-build via repeated `with_actor::<A>(config)`. Rejected because the natural shape for instanced is runtime spawn — caps that genuinely want N at boot just call `spawn_child` N times during their own init. Fewer API shapes; same outcome.

## Related

- ADR-0029 — `MailboxId = hash(name)`. Preserved unchanged.
- ADR-0038 — Actor-per-component thread model. This ADR extends it to actor-per-instance; threading revision is out of scope.
- ADR-0045 — Computation DAG + handles. Adjacent prior art if a topo-sort scheduler ADR unparks.
- ADR-0063 — Substrate fail-fast. Phase 2 (epoch deadlines) is the long-term backstop for stuck instanced actors.
- ADR-0021 — Input-stream subscriber cleanup on actor drop. Sets the precedent the monitor primitive follows for bidirectional cleanup.
- ADR-0022 + ADR-0038 — Replace_component splice. The existing Replaceable machinery, extended to instanced wasm via `replace_actor` when wasm-side spawn unparks.
- Issue 607 — design conversation thread; this ADR is the load-bearing decision capture.
- Issue 584 — `wire` / `unwire` lifecycle hooks; `FfiActor::on_drop` retirement. Implements the §6 surface this ADR amendment landed.
