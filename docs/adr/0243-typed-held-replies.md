# ADR-0243: Typed Held Replies

- **Status:** Proposed
- **Date:** 2026-09-28

## Context

ADR-0109 made a handler's return type its reply contract: `-> R` replies on return, and `-> Pending<R>` replies later through ADR-0093's hold. `Pending<R>` (`crates/aether-substrate/src/actor/native/offload/blocking.rs`) is a phantom receipt. Only the offload paths mint it: `NativeCtx::dispatch_blocking`, `NativeCtx::pending`, and the `TaskQueue` / `PerSenderEgress` `submit` helpers. Only a `#[handler(task)]` completion answers it. ADR-0109 names what it leaves out:

> **Deferral outside ADR-0093 isn't covered.** A handler that defers via the manual correlation FSM (stash correlation, reply on a later inbound handler) has no `Pending<R>` to return, so its contract stays uncaptured.

That gap is most of the unchecked surface. A census at `164e9625d` found 150 unchecked handlers, then spelled `#[handler::manual]`. About 33 native handlers are unchecked only because they answer one exact reply kind later, from somewhere other than a task completion:

- **a peer's reply:** audio and text loads and window commands;
- **a monitor notice:** tcp `on_unbind`;
- **a long-poll wake:** the bloomery journal's `on_watch_head`;
- **a sans-io core:** the bloomery driver;
- **a frame loop or event loop:** render `on_capture_frame`, window `on_create`;
- **a spawn continuation that also has an early `Err`:** component load, fleet spawn, tcp bind.

Two single handlers declare a `Silent` row but reply later: `aether-component`'s `on_replace_component` and `aether-rpc`'s `on_deferred_echo`.

The obligation these handlers carry is already a value. `DeferredReply` (`offload/blocking.rs`), minted by `NativeCtx::defer_reply_to`, holds the caller's `SettlementHold` and reply target. It is `#[must_use]`, and its `Drop` fails fast when it is dropped unanswered. `abandon_for_actor_close` is its one silent discharge, and `IntoDeferredReply` hands it to a successor (`HandlerSpawnBuilder::continue_from`, `TaskDone`). What it lacks is a reply type: `DeferredReply::reply` takes any `R: ActorMail`, so the handler's row cannot name what it will send, and the handler is unchecked (`Undeclared`, ADR-0231 §6).

Protocols already accept a deferred answer. A target's `-> Pending<O>` handler covers the protocol row `-> O` (`aether-actor-derive/src/protocol.rs`), so now versus later is invisible to the sender. The HTTP router shows the cost of the gap. Its glue replies only `HttpServerResponse`, and it is unchecked only because a route may answer later, from the handler that receives a peer's reply. That keeps `HttpRouter` at `-> Undeclared` and forces every hand-written HTTP handler to be unchecked too (#6935, #6957).

## Decision

A deferred reply is a typed pair. The handler returns the receipt, and the obligation is a separate move-only value that answers exactly one `R`.

```rust
// main: untyped debt, unchecked handler, Undeclared row
#[handler::unchecked(reason = "…")]
fn on_watch_head(.., ctx: &mut NativeCtx<'_, Self, Unchecked>, m: WatchHead) {
    let reply: DeferredReply = ctx.defer_reply_to(ctx.reply_target());
    self.watchers.park(reply);
}

// decision: row WatchHeadResult; only WatchHeadResult can answer
#[handler::request]
fn on_watch_head(.., m: WatchHead) -> Pending<WatchHeadResult> {
    let (pending, held) = ctx.hold::<WatchHeadResult>();
    self.watchers.park(held);
    pending
}
// later, from any handler on this actor:
held.answer(ctx, &WatchHeadResult { .. });
```

1. **`ctx.hold::<R>()` returns `(Pending<R>, Held<R>)`.** It arms one entry in the in-flight ledger, the table `dispatch_arm` already fills for offload and spawn staging. The entry holds the current settlement hold and reply target, and has no worker. Both values carry the entry's `DispatchId`:

   ```rust
   pub struct Pending<R> { id: DispatchId, _reply: PhantomData<fn() -> R> }   // the receipt
   pub struct Held<R>    { id: DispatchId, ledger: Weak<NativeBinding>, _reply: PhantomData<fn() -> R> }   // the ticket; move-only
   ```

   - **The ledger owns the obligation, not the value.** A `Held<R>` is a typed ticket to its ledger entry. It also keeps a weak link to that ledger, because its `Drop` gets no ctx. The weak link is never encoded. `DeferredCompletion` is the in-tree precedent for a ticket holding a weak binding link.
   - **`answer` completes the entry.** `answer(self, ctx, &R)` sends the terminal reply to the entry's target and releases its hold.
   - **An unanswered drop fails fast.** `Held<R>` is `#[must_use]`, and dropping it unanswered fails fast, as `DeferredReply` does.
   - **It stays on its actor.** A `Held<R>` never converts into another obligation and never moves to another ledger entry. Work the actor stages (offload, a child birth, a registry batch) owes no reply, and the `Held` waits in that work's context or in actor state (§9). `hand_off` is the one way a debt leaves its actor.
   - **Actor close answers every live debt.** `R` must implement `HeldReply`, which names the reply a caller receives when the actor holding its debt closes before answering:

     ```rust
     pub trait HeldReply: ActorMail {
         fn unanswered() -> Self;
     }
     impl HeldReply for CloseWindowResult {
         fn unanswered() -> Self { Self::Err { error: "window endpoint closed before answering".into() } }
     }
     ```

     `hold` stores a monomorphized `answer_unanswered::<R>` function pointer in the ledger entry, so nothing is encoded until it is needed. At actor close, the ledger's teardown sends `R::unanswered()` to every live or parked entry's target, then releases its hold, so `Sent` precedes `Release` as for `answer`. This holds when an actor closes while the engine keeps running. Engine teardown settles held debts silently, because every requester is closing with it and a reply would reach only closing actors. While the engine runs, no debt is discharged silently: a caller always receives an `R`, its reply handler runs, and its `take_context` frees any context and any `Held` it parked. The failure propagates up a chain of held requests instead of stranding each caller's context until that caller also closes. `DeferredReply::abandon_for_actor_close` and the per-actor `unwire` drains built on it (tcp's `pending_connects`, window's shutdown `Err` replies) are removed. A native handler panic aborts the engine (`scheduler/pool.rs`), so close is the only path that ends a live debt without an `answer`.

   `DispatchId` widens from "an offload dispatch" to "an armed reply obligation". The `DispatchId::NONE` sentinel, a hold captured with no worker, loses its last use.

2. **`Pending<R>` stays the phantom receipt, and the return type stays the contract.** The receipt goes to the `#[actor]` macro and sets the row. The debt goes to state, a table, or a successor. One value cannot do both, because returning it would hand the debt to the framework. A handler may answer through its `Held<R>` before it returns, for an early `Err`; the receipt proves only that the obligation was armed.

3. **Only an armed obligation mints a receipt.** `NativeCtx::pending(DispatchId)` is removed. Today it mints a `Pending<R>` from any id, `DispatchId::NONE` included, so a handler can declare `-> Pending<R>` with nothing armed, which ADR-0109 §3 set out to prevent. Its three callers arm by hand just before they mint:
   - `aether-substrate`'s `TaskQueue::submit`;
   - `aether-http`'s client `PerSenderEgress::submit`;
   - `aether-bloomery-workspace`'s `RunQueue::submit`.

   Each one holds the reply with `hold` and returns that receipt, whether its work starts at once or waits for a free slot, and keeps the `Held<R>` in place of the raw `SettlementHold` and `Source` pair (§9). `hold` is the only mint site left, and every receipt names an armed ledger entry.

4. **A `Held<R>` is a field of the request context.** The debt is part of the state a request carries to its reply handler, so it travels in the ADR-0139 context, with no verbs of its own:

   ```rust
   #[aether_data::kind(name = "aether.kit.mesh.load_context")]
   struct MeshLoadContext { held: Held<MeshLoadResult>, namespace: String, path: String }

   let (pending, held) = ctx.hold::<MeshLoadResult>();
   ctx.send_with_context::<FsCapability>(&read, MeshLoadContext { held, namespace, path });
   pending

   // the reply handler: one take claims the whole context, debt included
   let Some(context) = ctx.take_context::<MeshLoadContext>() else { return };
   context.held.answer(ctx, &MeshLoadResult { .. });
   ```

   - **`send_with_context` takes the context by value.** Parking moves the debt into the table, so the sender cannot answer it again after the reply's take. The existing `&context` callers become `context`.
   - **`Held<R>` is a kind with actor reach.** It implements neither `CrossesActors` nor `CrossesWire`, so under ADR-0242 it, and any context that holds it, is never `ActorMail`, never a handler's kind, and never sent.
   - **It encodes as its ticket, never as the hold, through the codec hooks.** A `Held` is an actor-local kind that serializes and deserializes, because contexts are stored as kind bytes that survive a guest replace. Its codec reaches the engine only through the hooks `Blob` and `ProtocolPath` already use (`aether-data/src/wire/attach.rs`, `context.rs`): `Encoder::held` on the way out and `DecodeCtx::claim_held` on the way in. Both default to refusing. Only the request-context table's encoder and decode ctx grant them, and only the dehydrate encoder does for saved state. An encode anywhere else fails, so a stray encode can never defuse a debt. The codec reads no thread-local or module-global state. Encoding parks the ticket, and `take_context` claims it back into a live `Held`:
     - **native:** the ticket is the ledger entry's `DispatchId`;
     - **guest:** the ticket is the reply handle, and the host keeps the hold in the reply-table slot.

     This is the pattern the guest `Blob` (ADR-0238, `BlobTable`) and `ReplyHandle` (`ReplyTable`) already use: the value is an id, and the runtime's table owns the resource.
   - **A context holding a `Held` needs no flag.** `#[aether_data::kind]` recognizes a `Held<..>` field and leaves `Clone` and serde out of the derives, the same way `#[actor]` reads a handler's ctx type from its tokens. Serde exists only for the wire, and a `Held` field makes the kind actor-reach, so the kind never needs it. A `Held` hidden behind a type alias is not recognized, and the build then fails at the field.
   - **Its schema names its reply kind.** `aether-data` gains one `SchemaType` node, `Ticket { reply: KindId }`, which describes a runtime-owned obligation that answers `reply`. `Held<R>` emits `Ticket { reply: R::ID }`. So a context holding `Held<A>` has a different kind id from one holding `Held<B>`, and the ADR-0139 carried-context check refuses a replacement that changed a held reply's kind. The node has actor reach only, and the JSON and MCP codecs refuse it as they refuse any field that cannot leave the engine.
   - **Dropping the context drops the debt.** An untaken context whose `Held` is live fails fast like any unanswered `Held`. When the actor closes, the ledger's teardown answers its entries with `R::unanswered()` (§1). Requests of different reply kinds that wait on the same work are waiters in actor state, keyed by that work (§9), not an enum of `Held`s in one context.
   - **The ledger never evicts.** An entry leaves only when it is answered, when it is handed off (§9), or when actor close answers it with `R::unanswered()`. This replaces the hand-built pairs of `send_with_context` and a stored `Source` or `InboundMail`: `aether-http`'s `DeferredSource` and `aether-window`'s `instance.rs` `pending` map.

5. **A `Held<R>` answers on its actor.** `answer` takes the actor's `NativeCtx`. Work on another thread posts a wake mail, and the woken handler answers from state; this is tcp's `ConnectReady` shape. A reply that must be sent from a thread outside the actor stays unchecked. `aether-substrate-harness-cap`'s `on_advance`, which hands its `InboundMail` to the embedder loop, is the one such site.

6. **Wasm guests get the same pair.** `WasmCtx::hold::<R>()` returns `(Pending<R>, Held<R>)`. The guest `Held<R>` wraps a `ReplyHandle`, travels in a request context as in §4, and traps when it is dropped unanswered.

   The host cannot call into the guest at unload, because the instance may already be gone, so the guest registers its `R::unanswered()` when it holds. Before its dispatch returns `DISPATCH_HANDLED_HOLD`, the shim passes the reply kind and the encoded `R::unanswered()` to the host, which stores them in the held slot beside its settlement hold. When the slot is settled without an answer (unload, or actor close), the host sends those bytes to the requester before releasing the hold.

   **Across a replace, the ticket and its obligation both survive, and both
   travel with the rest of the member's state through prepare, commit, and
   abort (ADR-0241 §7):**
   - **The obligation.** During prepare, the host reply table, with each held
     slot's settlement hold and registered `unanswered` reply, moves to the
     candidate in `PendingReplies` (#6409), so a ticket still resolves to
     its requester once the member commits. On abort, the table moves back
     to the reinstated old guest, so no ticket is orphaned by another
     member's failure. The candidate does not re-register: the stored reply
     is the kind the requester asked for, and a ticket claims only with a
     matching reply kind id, which hashes the kind's schema, so a candidate
     that changed `R` could not answer the slot and could not produce the
     old encoding either.
   - **The ticket.** It crosses inside a carried request context, or inside
     saved state that `on_dehydrate` writes and `on_rehydrate` decodes. On
     abort, that context, ticket included, returns to the reinstated old
     guest with the rest of its request-context table (ADR-0139 §4), and a
     ticket in saved state returns to it too: the host hands the old guest
     the bundle it saved through its own `on_rehydrate`, whose decode claims
     the ticket back to live. The old instance's memory is freed without
     running `Drop`, so no trap fires there.
   - **The guard.** The guest's per-actor registry, the one that already
     holds its request-context table and is reached through the ctx, tracks
     each live ticket. After `on_dehydrate`, a ticket that is still live and
     was not encoded makes the hook return a refusal status. The refusing
     hook still saves what it encoded, so the tickets it moved into saved
     state return with that state to the reinstated guest. The host maps
     that status onto a refusal that aborts the whole group (ADR-0241 §7):
     every member reinstates its old guest, so the requester is not
     stranded and a healthy member is not swapped out for another member's
     dropped ticket. The hook refuses, not traps, because the host contains
     `on_dehydrate` traps and lets the replace proceed (ADR-0015). That
     long-standing behavior is out of scope here. A dropped guest `Held`
     checks only a flag on the value that a granted encoder sets, so no drop
     path reads global state.
   - **Limits.** A candidate whose `on_rehydrate` does not decode a saved
     `Held` leaves the host slot held until actor close, when the requester
     receives the slot's registered `unanswered` reply; neither side can see
     the stranded slot earlier without a format change to the state
     envelope. The untaken-reply guard (§7) does not cover a context carried
     across a replace.

   Native capabilities are not replaced at run time (ADR-0231 §5), so a native ticket lives only within one process. The host keeps its `ReplyTable` entry alive after the handler returns, and holds settlement open, until the handle answers. Today a single handler's return frees the handle (`component/dispatch.rs`), and a `ReplyEntry` carries no settlement hold. #6960 implements this.

7. **Misuse fails fast.** The runtime checks what the types cannot:
   - **Unreturned receipt.** A `Pending<R>` has a fail-fast `Drop`, and the `Unchecked` dispatch view accepts the one its handler returns, through a doc-hidden `__accept_pending` the `#[actor]` / `#[handler_set]` arms call once the handler's own `as_single` reborrow has ended — the dispatch view a single handler never holds, so a handler cannot disarm its own receipt and declare a false `Silent` row. An unchecked handler already holds the `Unchecked` view directly and gains nothing by accepting: it has no receipt to return, because it answers through `Held` itself. A handler that holds and discards the receipt, which would lie with a `Silent` row, panics.
   - **Second hold.** A second `hold` in one dispatch panics. Two debts on one request would send two replies.
   - **Untaken reply.** When a handler runs on a reply whose context holds a live `Held` and does not take that context, the framework fails fast after the handler returns. The failure names the stored context kind.
   - **Forward with no reply.** An inherited forward cannot settle while its `Held` keeps the root open. Only a native detached forward (`send_detached_to_with_context`) could settle with its `Held` unclaimed, and a warning for that case is follow-on work. A guest gets no settlement notice for its own sends, so the guest side has no warning. The requester's timeout still ends the chain.

8. **Unchecked keeps what it is for.** A handler stays `#[handler::unchecked(reason = "…")]` when it:
   - forwards or relays its obligation (`forward_to`, the fleet proxy, bundle relays);
   - replies zero or many times;
   - chooses its reply kind at run time with no enum kind to name the choice;
   - answers from a thread outside the actor.

   The `Undeclared` row and ADR-0231 §6 are unchanged.

9. **A task takes its context the way a reply does.** Work an actor stages — an offload dispatch, a child birth, a registry batch — is a request to the engine. It gets a `RequestId` from the same counter as outbound requests, and its context is an ADR-0139 request context stored under that id. The completion wake is delivered correlated to that id and on the staging turn's chain, so the completion handler takes its context with the same `ctx.take_context::<C>()` a reply handler uses, and the §7 untaken-context guard covers it. A reply handler can instead take its context as a parameter (§10).

   ```rust
   // main: the context is a generic of the completion, and the debt is converted into the work
   ctx.spawn_child::<FleetProxy>(..).continue_from(held, FleetSpawnContext { engine_id, origin, supervision });
   #[handler(task)] fn on_spawn_done(state, ctx, done: TaskDone<SpawnOutcome<FleetProxy>, FleetSpawnContext>) {
       done.resolve_value(ctx, &reply);
   }

   // decision: the context is a kind taken from the ctx; live values and debts wait in state under its key
   state.pending_engines.insert(engine_id, PendingEngine { held: Some(held), supervision, .. });
   ctx.spawn_child::<FleetProxy>(..).stage_with(FleetSpawnKey { engine_id });
   #[handler(task)] fn on_spawn_done(state, ctx, done: TaskDone<SpawnOutcome<FleetProxy>>) {
       let Some(FleetSpawnKey { engine_id }) = ctx.take_context() else { return };
       if let Some(held) = state.pending_engines.get_mut(&engine_id).and_then(|p| p.held.take()) { held.answer(ctx, &reply) }
   }
   ```

   - **A task context is a kind.** It describes the work (an index, a path, an id), as a request context does. Live values — channels, `Arc`s, prepared plans, `Held`s — wait in actor state keyed by what the context names; the actors that stage work already keep such a table (`aether-http`'s shard slots, `aether-component`'s `pending_boots`, `aether-fleet`'s `pending_engines`, `aether-audio`'s `track_loads`). An actor handles one mail at a time, so the entry a staging handler inserts is always present when the completion runs.
   - **Waiters on the same work are a join in state.** Requests of different reply kinds that need the same work wait under one key, each list typed by its own reply: `aether-text` keeps `{ load: Vec<Held<LoadFontResult>>, metrics: Vec<Held<FontMetricsResult>> }` per font, so one read and one parse serve every request for that font.
   - **Staged work owes no reply.** Its ledger entry has no reply target. `TaskDone<O>` carries only the output.
   - **A task takes its chain when it is staged.** Staging holds the chain of the turn that stages it, if that turn had one, until the completion is handled, so a completion that stages the next step stays in the causal tree. Staging is separate from starting: a bounded queue stages a request's work in that request's turn and starts it when a slot frees, from whichever turn frees it, so each task holds the chain of the request it serves and never the chain of the turn that happens to start it.
   - **`hand_off` is the one way a debt leaves its actor.** `held.hand_off(ctx, target, &payload)` sends `payload` to an actor the holder staged, with the requester as its reply target, and ends the entry, so that actor answers in its own name and the requester keeps its stamped sender as its reference (ADR-0230 §3). The target is a proven reference whose row for the payload's kind replies `R`: an `ActorRef<T>` whose handler for that kind returns `R`, or a `ProtocolRef<P>` whose row for it is `Row<K, R>`, so the requester is answered with the kind it waits for. An `ErasedActorRef` proves no row and does not compile (#6895). The component host's load hand-off to the guest it staged, through the guest's control reference, is the one consumer.
   - **Removed:** `HandlerSpawnBuilder::continue_from`, `NativeCtx::stage_registry_batch_from`, `IntoDeferredReply`, `dispatch_blocking_held_with`, the `dispatch_blocking` variants that arm a reply, and `TaskDone`'s `resolve`, `resolve_with`, `resolve_value`, `resolve_err`, `release_no_reply`, `hand_off`, and `forward_tracked`. `stage_with` and `stage_registry_batch` take the context alone, and a failed stage hands the context back. `DeferredReply` and `defer_reply_to` remain for unchecked handlers only.

10. **A response takes its context as a parameter.** A `#[handler::response]` handles the answer to this actor's own request, and may take the context that request stored as a fourth parameter (#7201). The `#[actor]` arm takes it on the dispatch ctx before the call, with the same `take_context::<C>()`, and passes it:

    ```rust
    // main: the take is written by hand, and an absent context drops the reply without a word
    #[handler::response]
    fn on_read(&mut self, ctx: &mut WasmCtx<'_>, result: ReadResult) {
        let Some(context) = ctx.take_context::<MeshLoadContext>() else { return };
        context.held.answer(ctx, &MeshLoadResult::from(result));
    }

    // decision: the context is a parameter the arm fills
    #[handler::response]
    fn on_read(&mut self, ctx: &mut WasmCtx<'_>, result: ReadResult, context: MeshLoadContext) {
        context.held.answer(ctx, &MeshLoadResult::from(result));
    }
    ```

    - **`context: C` runs only with its context.** When the reply arrives without a `C` — it was uncorrelated, its context was stored as another kind, or none was stored — the arm does not run the handler. It logs an error in the actor's own log ring (ADR-0081 §7) naming the handler, the reply kind, the context kind, and the request id, and counts the mail as handled, so it does not fall through to a `#[fallback]`.
    - **It is not a panic.** A reply kind is mail any sender can send, so a panic would let one uncorrelated mail trap a wasm component or fail a native dispatch. §7 panics only for a debt the actor stranded itself, and it still runs after the arm: a context of another kind holding a `Held` that is left behind is still named.
    - **`context: Option<C>` runs either way.** It is for a handler that is correct with and without its context, and receives the take as it is. With no fourth parameter a handler may still call `ctx.take_context` itself, for example to try several context kinds in turn.
    - **The successful path costs what the hand-written take cost.** The arm adds the one take the handler made itself, and the intent word is not published: the row, the manifest record, and the wire are unchanged.

## Consequences

### Positive

- A deferred handler's row names its reply. The about 33 native unchecked sites, the two false `Silent` rows, and the three guest sites (#6960) cover protocol rows `-> R`, and `describe_component` / `describe_handlers` report their reply kind.
- Answering with the wrong kind is a compile error, and answering twice cannot compile, because `answer` consumes the `Held<R>`. The existing runtime guarantees carry over: a lost reply fails fast, and a parked reply holds its chain open.
- The HTTP router glue returns `-> HttpRouterResult`, and a handler that forwards to a peer holds its reply and returns `-> Pending<HttpRouterResult>` (§4). With the #6957 enum, `HttpRouter` becomes `fn request(mail: HttpServerRequest) -> HttpRouterResult`, and every HTTP handler is single.

### Negative / limits

- The type does not prove that a stored `Held<R>` is ever answered. Rust has no linear types. An unanswered drop fails fast, but a debt parked forever is legitimate for a long-poll and is ended only by the requester's timeout.
- There are two values where one handler used to have none. The receipt is the cost of keeping the contract on the return type.
- Every reply kind a handler holds implements `HeldReply`, one line per kind. A kind with no failure variant must gain one: a reply that can go unanswered has to be able to say so. The field shapes differ between kinds (`ConnectResult::Err` also carries `addr`), so the impl is written by hand, not derived.
- A guest `hold` encodes its `unanswered` reply once, whether or not it is ever needed.
- A task context must be a kind, so a staging actor keeps its live values in state under a key instead of in the context, which some staging sites do not do today.
- Taking a task's context checks its type at run time, as a reply's does; today a completion's context type is a compile-time generic.
- `send_with_context` changes from `&context` to `context` at every existing caller.

### Neutral / forward

- This extends ADR-0109 and closes its "deferral outside ADR-0093" limit. ADR-0139 request contexts take their context by value, may carry an actor-reach `Held`, and now also key an actor's staged work (§9). ADR-0093's completion changes shape: `TaskDone<O>` carries the output alone, and its reply surface moves to the `Held` the actor keeps. The ADR-0231 §6 unchecked rows are unchanged.
- Native work is #6959, wasm work is #6960, and #6961 moves the handlers that are unchecked for no deferral reason. #6955's remaining half, renaming the handler classes and giving a missing return type a meaning, is independent of this ADR.

## Alternatives considered

- **Inject the debt as a handler parameter** (`fn on_x(.., m: X, reply: Held<R>)`). This gives one value and no mint call, but the row would come from a parameter, against ADR-0109's rule that the return type is the contract.
- **A typed unchecked ctx** (`NativeCtx<'_, Self, Unchecked<R>>`). Rejected: unchecked means the handler answers by hand. Typing its ctx blurs the one class kept for untyped replies, and the deferred sites are not unchecked in kind; they answer exactly once, later.
- **Mint `Pending<R>` from `send_with_context` and answer by the peer-reply handler's return.** This covers only the peer-reply sites. Notices, long-polls, sans-io cores, and frame loops answer from handlers that do not handle the peer's reply.
- **A separate held table with its own verbs** (`send_holding` / `take_held`, with or without a `with_context` modifier). This splits one request's state across two tables and two takes, so a handler could claim one half and strand the other. The debt belongs in the context that already carries the request's state.
- **Convert the debt into the staged work** (`continue_from`, `IntoDeferredReply`). This was the first design. The conversion drops `R`, so a staged debt cannot be answered with `unanswered()` at close, and a completion that answers several kinds re-selects the kind by hand.
- **Move the `Held`'s ledger entry into the staged work in place.** This keeps `R`, but it ties a reply obligation to one piece of work: a completion that chains further work, or several requests waiting on one work, need the entry to move again or split.
- **A reply-kind parameter on `TaskDone<O, C, R>`.** One completion can answer different kinds by context variant (`aether-component`'s load and replace publication), which one `R` cannot express.
- **Take the context from the completion value** (`done.take_context::<C>()`). This removes the generic, but it is a second context system beside the request-context table, and the §7 guard does not cover it.
- **Actor close settles held debts silently.** The caller's chain ends, but it never receives an `R`: its reply handler never runs, and a context it stored with `send_with_context` stays in its table until it too closes, stranding any `Held` inside it. Silence also differed from what some actors sent before (window answered its in-flight commands with a shutdown `Err`).
- **Each actor answers its own debts in `unwire`.** This keeps a typed failure, but every actor has to remember it; tcp already abandons its connects silently. The engine owns the ledger, so the engine answers.
- **A generic `RequestAbandoned` notice to the caller.** The caller's handler for `R` would not run, so its context would still be stranded unless the framework intercepted the notice. A typed `R` reaches the handler the caller already has.
- **Encode a context `Held` by reference.** The sender would keep a live copy after parking it. It could answer that copy and then answer again after the take, and a fail-fast drop would fire on every correct park.
