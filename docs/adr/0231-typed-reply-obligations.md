# ADR-0231: Typed Reply Obligations

- **Status:** Proposed
- **Date:** 2026-09-23

Amends [ADR-0109](0109-handler-reply-contracts.md), [ADR-0112](0112-handler-reply-classes.md), [ADR-0134](0134-multi-reply-class-and-explicit-handler-classes.md), [ADR-0227](0227-reply-contracts-are-type-markers.md) (the reply surface and handler classes), [ADR-0139](0139-guest-reply-correlation-and-request-contexts.md) (request contexts), [ADR-0093](0093-hold-until-resolve-dispatch-primitive.md) / [ADR-0158](0158-per-sender-bounded-async-egress-dispatch.md) (the deferred-reply drop guard), [ADR-0013](0013-reply-to-sender-host-fn.md) / [ADR-0017](0017-component-sender-handles.md) (reply handles), and the replace contract ([ADR-0038](0038-actor-per-component-dispatch.md), [ADR-0101](0101-replace-hooks-on-ffiactor.md)). Applies the unexportable-invariant rule of [ADR-0230](0230-proven-actor-references.md) §1 to reply targets.

## Context

A request that expects a reply creates a debt: the responder owes the requester one answer of a declared kind. The engine holds that debt in four shapes, and only one of them is a value the compiler tracks:

- **wasm `ReplyHandle`** (`crates/aether-actor/src/mail/mod.rs`). A `u32` that is `Copy`, `Serialize` / `Deserialize` / `Schema`, readable through `raw()`, and constructible by deserializing any number. It indexes the host's per-instance `ReplyTable` (`crates/aether-substrate/src/actor/wasm/reply_table.rs`). `OutboundReply::reply_to` discards the host's status code (`crates/aether-actor/src/wasm/ctx/send.rs`), so answering an unknown handle fails without a signal.
- **native `Source`** from `ctx.reply_target()`: `Copy` and serde, the native twin of `ReplyHandle`.
- **native `DeferredReply`** (`crates/aether-substrate/src/actor/native/offload/blocking.rs`): move-only, `#[must_use]`, no serde. It has the right shape, but it is untyped in the reply kind, and its `Drop` only `debug_assert!`s, so a release build loses the reply silently.
- **native `InboundMail`** from `ctx.take_inbound()`: a retained envelope with a reply capability. Dropping it unanswered is normal.

Where the debt is typed, the handler's return type carries it: `-> R` and `-> Pending<R>` (ADR-0109), from which `#[actor]` emits `Replies<K, Reply = R>` (ADR-0227). Everything else runs through the manual class (ADR-0112, ADR-0134), whose `ctx.reply` / `ctx.reply_to` send any kind to any handle, which is why ADR-0227 §3 leaves manual handlers unbound.

An inventory of every manual handler at `0b2994794` found 122 declarations in 42 files: 25 reply immediately, 40 originate or answer a deferred reply, 7 reply with a variable kind or choose between now and later, 12 relay, and 38 never reply (30 of those are kit-widget handlers that are manual only because shared helpers take the `Manual` ctx). Deferred debts sit in request contexts (anthropic, behavior, kit mesh, native audio and text, HTTP deferred routes), in actor fields (puppet, the bloomery bundle root, lifecycle, tcp, the component host, the fleet proxy) or in a `DeferredReply` (the bloomery driver and journal, tcp, fleet spawn, component load). The inventory found twelve paths on which an owed reply disappears with no signal. The fixes of the same week show the pattern:

- **#6409.** A `ReplyHandle` kept in a request context rode `replace_component`, but the host table it indexed restarted at 0. The reply was dropped or, once the replacement had received other mail, delivered to an unrelated requester with that requester's correlation. Fixed by carrying the table with the mailbox slot (ADR-0013 and ADR-0017 amended).
- **#6411.** The puppet held its owed load reply in one field. A second load overwrote it, and a replace discarded it.
- **#6419.** The substrate-harness `advance` answered through `HubOutbound::send_reply`, which drops `Component` senders, so wire and actor callers never got `AdvanceResult`.
- **#6421.** A panicking `dispatch_blocking` worker dropped its armed completion: the chain settled, the caller was never answered, and the engine carried on. A dropped `DeferredReply` or `TaskDone` only asserts in debug builds.
- **#6420.** The request-context table evicts the oldest entry at 1024. A context that carries a reply target loses its reply for good. ADR-0139 §4 says eviction "is memory hygiene only, never correctness"; that premise fails the moment a context holds a debt.

Each fix was local. The shared cause is that the debt is a copyable number, or a value whose loss is silent, or bytes in storage (a serialized context, guest memory) that can be evicted or discarded underneath it. ADR-0230 §1 names the rule: a type that cannot uphold its invariant across a boundary is not exportable. A reply target's invariant, "answering this reaches the requester that asked, once", holds only in the memory of the engine that minted it.

The caller side has the mirror hole. Nothing stops an actor from sending a request whose reply it has no handler for; the reply arrives and dies in a fallback or a strict receiver's miss.

## Decision

### 1. `Reply<R>` is a typed, move-only obligation

```rust
#[must_use = "an unanswered Reply fails fast when dropped"]
pub struct Reply<R: Kind> { /* framework-private slot, correlation, lineage */ }

impl<R: Kind> Reply<R> {
    pub fn send(self, ctx: &mut impl ReplyCtx, reply: &R);
    pub fn forward_to<T, K>(self, ctx: &mut impl ReplyCtx, peer: &ActorRef<T>, request: &K)
    where
        T: Replies<K, Reply = R>;
}
```

The framework mints it; there is no public constructor. It is not `Clone` or `Copy` and implements no `Serialize`, `Deserialize`, `WireEncode`, `WireDecode` or `Schema`, so it cannot be a kind field, a config field or saved-state bytes. `send` consumes it, so a second answer does not compile.

A handler that answers before it returns keeps `-> R`. A handler that answers later takes the obligation as a parameter, and the macro reads `R` off it:

```rust
#[handler::single]
fn on_count(&mut self, _ctx: &mut WasmCtx<'_, Self>, _q: CountQuery) -> CountReport { self.report() }

#[handler::single]
fn on_load(&mut self, ctx: &mut WasmCtx<'_, Self>, load: Load, reply: Reply<LoadResult>) {
    let read = ctx.send_tracked::<FsCapability>(&load.read_request());
    self.loading.insert(read, reply);
}
```

Both emit `Replies<Load, Reply = LoadResult>` and `ReplyContract::One(LoadResult::ID)`. A handler with both a non-unit return and a `Reply<R>` parameter is a compile error. The offload helpers take the obligation by value (`ctx.dispatch_blocking(reply, work)`), the `#[handler(task)]` completion receives it back inside `TaskDone`, and `Pending<R>` retires as a separate receipt type.

The manual class is retired: `#[handler::manual]`, the `Manual` ctx mode and `OutboundReply` (`reply`, `reply_to`, `reply_target`) are removed. The mapping for the other shapes:

- **Immediate** manual handlers become `-> R`.
- **Never-replying** manual handlers (kit-widget, the component host's `on_registry_changed`, the bloomery invocation child) become `#[handler::single]` returning `()`.
- **Relays** call `reply.forward_to(ctx, &peer, &request)`. The bound `T: Replies<K, Reply = R>` means a relay can only hand the obligation to a peer that owes the same reply kind for the request it receives.
- **Variable reply kinds** use an enum reply kind, keeping `single` total (ADR-0112). An actor answering several request kinds from one completion handler stores an enum of obligations (`enum Owed { Track(Reply<PlayTrackResult>), Instrument(Reply<LoadInstrumentResult>) }`), which replaces the window instance's runtime kind check and its `fatal_abort`.
- **Streams** stay on `multi` (ADR-0134). The class set becomes `single` and `multi`.

### 2. The obligation lives where the actor stores it

`Reply<R>` is an ordinary value. The actor keeps it in a field, in a map keyed by `RequestId`, or beside a fan-out's pending set, and one concept covers single deferral, fan-out and fan-in:

```rust
struct Loading { reply: Reply<LoadResult>, waiting: BTreeSet<RequestId> }       // fan-out: puppet, audio bank
struct Journal { watchers: BTreeMap<Sequence, Vec<Reply<WatchHeadResult>>> }   // fan-in: one commit wakes many
```

Overwriting a held obligation drops it, which fails fast (§5), so the puppet's second load has to answer the first explicitly (`old.reply.send(ctx, &LoadResult::Err { .. })`) before it stores the new one.

There is no request attachment (`.owing()`) for obligations. Request contexts (ADR-0139 §4) stop carrying reply targets: a context holds the caller's own bookkeeping, and a `Reply<R>` cannot be put in one because it has no schema. Eviction can then lose only the evicting actor's bookkeeping, never a debt owed to another actor, which removes #6420's failure and corrects ADR-0139 §4's premise.

### 3. A sender must handle the reply it asks for

A typed send to an actor that declares `Replies<K, Reply = O>` requires the sending actor `A: HandlesKind<O>`. Without the handler, the send does not compile:

```rust
// For every typed send verb, from a ctx typed by its actor A:
//   target R handles K and is silent                  -> compiles
//   target R: Replies<K, Reply = O>, A: HandlesKind<O> -> compiles
//   target R: Replies<K, Reply = O>, otherwise         -> E0277:
//     "`A` sends `K` to `R`, which replies `O`, and `A` has no handler for `O`"
//     note: "add a `#[handler]` for `O`, or call `send_ignoring_reply`"
ctx.send_ignoring_reply::<FsCapability>(&write);   // the one written way to discard a reply
```

The macro emits a reply shape for every handler: `Replies<K, Reply = O>` for replying handlers, and a framework `Silent` answer, which is not a kind, for silent ones. A `Receives<Silent>` bound holds for every actor, and `Receives<O>` for a kind `O` holds exactly when `A: HandlesKind<O>`. Silent targets therefore keep plain `send`. `#[fallback]` does not count as handling `O`, because it emits no `HandlesKind`. Non-actor callers (MCP `send_mail`, RPC `Call`) are exempt: they receive replies as data and never go through a typed send verb.

The bound needs the ctx to know its actor. The macro types the handler ctx it passes by the actor by default (`WasmCtx<'_, Self>`, `NativeCtx<'_, Self>`); an `Erased` ctx cannot make a typed send to a replying target.

### 4. Mail is best effort; liveness is a monitor concern

There is no synthetic abandonment reply. A reply, like any mail, may not arrive. A caller that must know whether its responder is still there monitors it (`ctx.monitor`, `MonitorNotice`, ADR-0079 §8) and handles the departure notice.

### 5. Dropping an unanswered obligation fails fast

A live actor that drops an unanswered `Reply<R>` has a bug. `Drop` escalates through the binding's fatal aborter (ADR-0063) in every build; on wasm the guest's drop traps, which takes the ADR-0063 component-trap path. The caller's monitor sees the crash. The one quiet release is explicit and framework-only: when the actor that owns the obligation is closing (`abandon_for_actor_close`, called from teardown). A panicking `dispatch_blocking` worker, which drops its caller's obligation, also takes the fatal path. #6421 implements this for native `DeferredReply`, `TaskDone` and worker panics.

### 6. Replace carries only what the replacement can honour

Carrying state across `replace_component` is sound only if it cannot degrade state the rest of the stack expects to exist.

- **Pure numbering is carried unconditionally**: the correlation cursor (#6400), the reply-lineage counter (#6426) and the reply table's numbering (#6409). Carrying it only prevents collisions.
- **Expected state is carried only on proof**: request contexts and outstanding obligations. Before the swap, every carried context's `KindId` must appear in the replacement's kind manifest, and every outstanding obligation's request kind `K` and reply kind `R` must match a `Replies<K, Reply = R>` row in the replacement's handler manifest. If any fails, the replace is refused before the replacement is created and the old module keeps running, as the `#[actor(depends(R))]` refusal does (ADR-0230). There is no partial carry and no silent reclaim.

An obligation held in wasm actor state crosses through the dehydrate hook, which hands it to the framework; user serde never touches it:

```rust
fn on_dehydrate(&mut self, ctx: &mut WasmDropCtx<'_>) {
    if let Some(loading) = self.loading.take() {
        ctx.save_state_kind(1, &loading.progress);
        ctx.carry_reply(loading.reply);
    }
}

fn on_rehydrate(&mut self, _ctx: &mut WasmCtx<'_, Self>, prior: PriorState<'_>) {
    self.loading = prior.take_reply::<LoadResult>().map(Loading::resume);
}
```

An obligation that `on_dehydrate` drops fails fast under §5. One that it neither answers, carries nor drops (still in guest memory when the hook returns) was not handed over, and the replace is refused. Native actors are never replaced, so their obligations need no carry. #6429 implements the proof check for the state carried today.

### 7. `ReplyHandle` is removed

`ReplyHandle`, its serde impls and `raw()` leave the public API. The host reply table stays, reachable only through `Reply<R>`'s private field, so no user code can hold, store or forge a reply-table index.

## Consequences

### Positive

- Forgetting to reply, replying twice, replying the wrong kind and overwriting a held debt become compile errors or visible moves. A leaked debt fails fast in release builds, where today it vanishes.
- One concept covers single deferral, fan-out, fan-in and relay. Every replying handler reports `ReplyContract::One(R)`, so the 122 handlers that report an opaque `Manual` today become introspectable, and ADR-0109's gap ("deferral outside ADR-0093 isn't covered") closes.
- Reply targets leave serialized storage, so #6420's eviction loss and #6409's misdelivery cannot recur from user code.
- No actor can send a request whose reply lands on nothing without writing `send_ignoring_reply`.

### Negative

- **Migration size.** 122 manual handlers move: the 25 immediate to `-> R`, the 59 deferred, variable and relay handlers to `Reply<R>`, and the 38 silent to `single`. Four wasm consumers keep reply targets in contexts today (anthropic, behavior, kit mesh, and puppet's dead `reply` field), plus native audio, text and HTTP deferred routes; each moves its obligations into keyed actor state. The native holders of `DeferredReply`, `InboundMail` or a bare `Source` (the bloomery driver and journal, tcp, fleet, the component host, window, render, lifecycle) move to `Reply<R>`.
- **The handler ctx is typed by its actor by default.** A macro change, and helpers written against the `Erased` ctx need a type parameter or an actor-typed ctx.
- **`send_ignoring_reply` churn.** Every existing send to a replying target from an actor without the reply handler must add the handler or say `send_ignoring_reply`. The compiler enumerates the sites when the bound lands; the count is unknown until then.
- **Latent leaks become crashes.** Intended under ADR-0063, but the first builds with the fatal drop will surface them.
- **Visible replace refusals.** A replacement that drops a handler, or changes a reply kind, while obligations for it are outstanding cannot swap until they are answered.
- **Non-actor answerers need a framework path.** The harness embedder loop, the fleet proxy's raw reply stream, the perf harness's hand-written `Dispatch` impls and the desktop chassis driver's hand-built `Source` sit outside the macro and need a chassis-minted obligation or a `multi` handler. Tracked as follow-on work.
- `ReplyContract::Manual` is retired; its wire discriminant stays reserved and the inputs custom-section version bumps when the variant is removed.

### Neutral

- Settlement is unchanged. A deferred `Reply<R>` holds the chain's settlement hold as `DeferredReply` does (ADR-0093, ADR-0106), and a reply keeps its lineage (ADR-0080 §5 and §6).

## Alternatives considered

- **An `.owing()` attachment on request contexts.** A second valid way to hold a debt. It needs the target serialized into the context or a side table, keeps eviction a correctness question, and does not express fan-in.
- **A synthetic `aether.reply.abandoned` kind.** Breaks the contract that the reply is `R`, forces every caller to handle a framework kind for every request, and duplicates what monitors already report.
- **A per-kind error value produced on drop (`R: FromDropped`).** Every reply kind would need a constructor that fabricates its required echo fields, and a bug would arrive looking like an ordinary `Err` reply.
- **Refuse any replace while obligations are outstanding.** Makes hot reload unusable whenever a request is in flight. The proof check refuses only what the replacement cannot honour.
- **Runtime-only checks** (a lint, a manifest comparison at send, a `debug_assert`). Runtime and debug-only checks are what produced the inventory above; the compiler already has the facts.
- **Keep `manual` as an escape hatch.** ADR-0227 left it unbound, and every silent-drop path in the inventory ran through it or through the untyped `DeferredReply`.

## Amendments

Each amended ADR gains a dated amendment line in the implementing PR; their bodies stay as written.

- **ADR-0013 / ADR-0017.** `ReplyHandle` leaves the public API (§7). The reply table stays slot-owned (the #6409 amendments), and outstanding entries cross a replace only under §6's proof.
- **ADR-0063.** A dropped unanswered `Reply<R>` and a panicking offload worker join the fail-fast cases.
- **ADR-0075** (superseded by ADR-0076; its `HandlesKind` send gating survives). Typed sends to a replying target also require the sender to handle the reply; `send_ignoring_reply` is added (§3).
- **ADR-0079 §8.** Monitoring is the liveness channel for outstanding requests; no reply-level abandonment exists (§4).
- **ADR-0093 / ADR-0158.** The resolve-or-leak `debug_assert` becomes a fatal abort in every build, and offload helpers take `Reply<R>` by value.
- **ADR-0109.** `Pending<R>` retires; a deferred reply is a `Reply<R>` parameter. The `ctx.reply` second path is removed.
- **ADR-0112 / ADR-0134.** The manual class is removed; the classes are `single` and `multi`.
- **ADR-0139.** Request contexts no longer carry reply targets. §4's "eviction never affects correctness" premise is corrected: eviction loses only the evicting actor's own bookkeeping. Carried contexts cross a replace only under §6's proof.
- **ADR-0227.** The `manual` row is removed. A `Reply<R>` parameter emits `Replies<K, Reply = R>`, and every handler emits a reply shape so the caller-side bound can be stated (§3).
- **ADR-0038 / ADR-0101** (the replace contract; ADR-0022 was superseded by ADR-0038). `replace_component` refuses a replacement that cannot honour carried contexts and obligations, and the dehydrate hook gains `carry_reply` (§6).
