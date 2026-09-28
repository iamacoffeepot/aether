# ADR-0243: Typed Held Replies

- **Status:** Proposed
- **Date:** 2026-09-28

## Context

ADR-0109 made a handler's return type its reply contract: `-> R` replies on return, and `-> Pending<R>` replies later through ADR-0093's hold. `Pending<R>` (`crates/aether-substrate/src/actor/native/offload/blocking.rs`) is a phantom receipt. Only the offload paths mint it: `NativeCtx::dispatch_blocking`, `NativeCtx::pending`, and the `TaskQueue` / `PerSenderEgress` `submit` helpers. Only a `#[handler(task)]` completion answers it. ADR-0109 names what it leaves out:

> **Deferral outside ADR-0093 isn't covered.** A handler that defers via the manual correlation FSM (stash correlation, reply on a later inbound handler) has no `Pending<R>` to return, so its contract stays uncaptured.

That gap is most of the manual surface. A census at `164e9625d` found 150 `#[handler::manual]` handlers. About 33 native handlers are manual only because they answer one exact reply kind later, from somewhere other than a task completion:

- **a peer's reply:** audio and text loads, window commands, and the HTTP router glue's deferred routes;
- **a settlement or monitor notice:** lifecycle `on_advance`, tcp `on_unbind`;
- **a long-poll wake:** the bloomery journal's `on_watch_head`;
- **a sans-io core:** the bloomery driver;
- **a frame loop or event loop:** render `on_capture_frame`, window `on_create`;
- **a spawn continuation that also has an early `Err`:** component load, fleet spawn, tcp bind.

Two single handlers declare a `Silent` row but reply later: `aether-component`'s `on_replace_component` and `aether-rpc`'s `on_deferred_echo`.

The obligation these handlers carry is already a value. `DeferredReply` (`offload/blocking.rs`), minted by `NativeCtx::defer_reply_to`, holds the caller's `SettlementHold` and reply target. It is `#[must_use]`, and its `Drop` fails fast when it is dropped unanswered. `abandon_for_actor_close` is its one silent discharge, and `IntoDeferredReply` hands it to a successor (`HandlerSpawnBuilder::continue_from`, `TaskDone`). What it lacks is a reply type: `DeferredReply::reply` takes any `R: ActorMail`, so the handler's row cannot name what it will send, and the handler is manual (`Undeclared`, ADR-0231 §6).

Protocols already accept a deferred answer. A target's `-> Pending<O>` handler covers the protocol row `-> O` (`aether-actor-derive/src/protocol.rs`), so now versus later is invisible to the sender. The HTTP router shows the cost of the gap. Its glue replies only `HttpServerResponse`, and it is manual only because a deferred route answers from the `#[http::reply]` handler through `answer_deferred`. That keeps `HttpRouter` at `-> Undeclared` and forces every hand-written HTTP handler to be manual too (#6935, #6957).

## Decision

A deferred reply is a typed pair. The handler returns the receipt, and the obligation is a separate move-only value that answers exactly one `R`.

```rust
// main: untyped debt, manual handler, Undeclared row
#[handler::manual]
fn on_watch_head(.., ctx: &mut NativeCtx<'_, Self, Manual>, m: WatchHead) {
    let reply: DeferredReply = ctx.defer_reply_to(ctx.reply_target());
    self.watchers.park(reply);
}

// decision: row WatchHeadResult; only WatchHeadResult can answer
#[handler::single]
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
   pub struct Held<R>    { id: DispatchId, _reply: PhantomData<fn() -> R> }   // the ticket; move-only
   ```

   - **The ledger owns the obligation, not the value.** A `Held<R>` is a typed ticket to its ledger entry.
   - **`answer` completes the entry.** `answer(self, ctx, &R)` sends the terminal reply to the entry's target and releases its hold.
   - **An unanswered drop fails fast.** `Held<R>` is `#[must_use]`, and dropping it unanswered fails fast, as `DeferredReply` does.
   - **Staging takes it unchanged.** `Held<R>` implements `IntoDeferredReply`, so `continue_from` and the other staging surfaces accept it.
   - **Actor close is the one silent discharge.** The ledger's existing teardown settles the entry.

   `DispatchId` widens from "an offload dispatch" to "an armed reply obligation". The `DispatchId::NONE` sentinel, a hold captured with no worker, loses its last use.

2. **`Pending<R>` stays the phantom receipt, and the return type stays the contract.** The receipt goes to the `#[actor]` macro and sets the row. The debt goes to state, a table, or a successor. One value cannot do both, because returning it would hand the debt to the framework. A handler may answer through its `Held<R>` before it returns, for an early `Err`; the receipt proves only that the obligation was armed.

3. **Only an armed obligation mints a receipt.** `NativeCtx::pending(DispatchId)` is removed. Today it mints a `Pending<R>` from any id, `DispatchId::NONE` included, so a handler can declare `-> Pending<R>` with nothing armed, which ADR-0109 §3 set out to prevent. Its three callers arm by hand just before they mint:
   - `aether-substrate`'s `TaskQueue::submit`;
   - `aether-http`'s client `PerSenderEgress::submit`;
   - `aether-bloomery-workspace`'s `RunQueue::submit`.

   When one of these dispatches at once, it returns the receipt that the dispatch call mints. When it queues, it enqueues a `Held<R>` in place of the raw `SettlementHold` and `Source` pair. `hold` and the offload dispatch calls are the only mint sites left, and every receipt names an armed ledger entry.

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
   - **It encodes as its ticket, never as the hold.** A `Held` is an actor-local kind that serializes and deserializes, because contexts are stored as kind bytes that survive a guest replace. Encoding writes the ticket, and `take_context` decodes it back into a live `Held`:
     - **native:** the ticket is the ledger entry's `DispatchId`;
     - **guest:** the ticket is the reply handle, and the host keeps the hold in the reply-table slot.

     This is the pattern the guest `Blob` (ADR-0238, `BlobTable`) and `ReplyHandle` (`ReplyTable`) already use: the value is an id, and the runtime's table owns the resource.
   - **Its schema names its reply kind.** `aether-data` gains one `SchemaType` node, `Ticket { reply: KindId }`, which describes a runtime-owned obligation that answers `reply`. `Held<R>` emits `Ticket { reply: R::ID }`. So a context holding `Held<A>` has a different kind id from one holding `Held<B>`, and the ADR-0139 carried-context check refuses a replacement that changed a held reply's kind. The node has actor reach only, and the JSON and MCP codecs refuse it as they refuse any field that cannot leave the engine.
   - **Dropping the context drops the debt.** An untaken context whose `Held` is live fails fast like any unanswered `Held`. When the actor closes, the ledger's teardown settles its entries silently. A context that holds several variants, such as `aether-text`'s load and metrics requests, is one enum context and one take.
   - **The ledger never evicts.** An entry leaves only when it is answered, when it is staged onto a successor, or when actor close settles it. This replaces the hand-built pairs of `send_with_context` and a stored `Source` or `InboundMail`: `aether-http`'s `DeferredSource` and `aether-window`'s `instance.rs` `pending` map.

5. **A `Held<R>` answers on its actor.** `answer` takes the actor's `NativeCtx`. Work on another thread posts a wake mail, and the woken handler answers from state; this is tcp's `ConnectReady` shape. A reply that must be sent from a thread outside the actor stays manual. `aether-substrate-harness-cap`'s `on_advance`, which hands its `InboundMail` to the embedder loop, is the one such site.

6. **Wasm guests get the same pair.** `WasmCtx::hold::<R>()` returns `(Pending<R>, Held<R>)`. The guest `Held<R>` wraps a `ReplyHandle`, travels in a request context as in §4, and traps when it is dropped unanswered.

   **Across a replace, the ticket and its obligation both survive:**
   - **The obligation.** The host reply table, with each held slot's settlement hold, moves to the next occupant in `PendingReplies` (#6409), so a ticket still resolves to its requester.
   - **The ticket.** It crosses inside a carried request context, or inside saved state that `on_dehydrate` writes and `on_rehydrate` decodes. The old instance's memory is freed without running `Drop`, so no trap fires there.
   - **The guard.** The guest SDK tracks each live ticket. After `on_dehydrate` returns, a ticket that is live and not encoded traps, which fails the replace and rolls it back (ADR-0101) instead of stranding its requester. A second decode of the same ticket also traps.

   Native capabilities are not replaced at run time (ADR-0231 §5), so a native ticket lives only within one process. The host keeps its `ReplyTable` entry alive after the handler returns, and holds settlement open, until the handle answers. Today a single handler's return frees the handle (`component/dispatch.rs`), and a `ReplyEntry` carries no settlement hold. #6960 implements this.

7. **Misuse fails fast.** The runtime checks what the types cannot:
   - **Unreturned receipt.** A `Pending<R>` has a fail-fast `Drop`, and the `#[actor]` macro defuses the one its handler returns. A handler that holds and discards the receipt, which would lie with a `Silent` row, panics.
   - **Second hold.** A second `hold` in one dispatch panics. Two debts on one request would send two replies.
   - **Untaken reply.** When a handler runs on a reply whose context holds a live `Held` and does not take that context, the framework fails fast after the handler returns. The failure names the stored context kind.
   - **Forward with no reply.** When a forward settles and its context's `Held` is still unclaimed, the framework logs a warning that names the request. It cannot make up an `R`, so the requester's timeout still ends the chain.

8. **Manual keeps what it is for.** A handler stays `#[handler::manual]` when it:
   - forwards or relays its obligation (`forward_to`, the fleet proxy, bundle relays);
   - replies zero or many times;
   - chooses its reply kind at run time with no enum kind to name the choice;
   - answers from a thread outside the actor.

   The `Undeclared` row and ADR-0231 §6 are unchanged.

## Consequences

### Positive

- A deferred handler's row names its reply. The about 33 native manual sites, the two false `Silent` rows, and the three guest sites (#6960) cover protocol rows `-> R`, and `describe_component` / `describe_handlers` report their reply kind.
- Answering with the wrong kind is a compile error, and answering twice cannot compile, because `answer` consumes the `Held<R>`. The existing runtime guarantees carry over: a lost reply fails fast, and a parked reply holds its chain open.
- The HTTP router glue returns `-> Pending<HttpRouterReply>`. With the #6957 enum, `HttpRouter` becomes `fn request(mail: HttpServerRequest) -> HttpRouterReply`, and every HTTP handler is single.

### Negative / limits

- The type does not prove that a stored `Held<R>` is ever answered. Rust has no linear types. An unanswered drop fails fast, but a debt parked forever is legitimate for a long-poll and is ended only by the requester's timeout.
- There are two values where one handler used to have none. The receipt is the cost of keeping the contract on the return type.
- `send_with_context` changes from `&context` to `context` at every existing caller.

### Neutral / forward

- This extends ADR-0109 and closes its "deferral outside ADR-0093" limit. ADR-0139 request contexts take their context by value, and may carry an actor-reach `Held`. The ADR-0093 hold mechanic and ADR-0231 §6 manual rows are unchanged.
- Native work is #6959, wasm work is #6960, and #6961 moves the handlers that are manual for no deferral reason. #6955's remaining half, renaming the handler classes and giving a missing return type a meaning, is independent of this ADR.

## Alternatives considered

- **Inject the debt as a handler parameter** (`fn on_x(.., m: X, reply: Held<R>)`). This gives one value and no mint call, but the row would come from a parameter, against ADR-0109's rule that the return type is the contract.
- **A typed manual ctx** (`NativeCtx<'_, Self, Manual<R>>`). Rejected: manual means the handler answers by hand. Typing its ctx blurs the one class kept for untyped replies, and the deferred sites are not manual in kind; they answer exactly once, later.
- **Mint `Pending<R>` from `send_with_context` and answer by the peer-reply handler's return.** This covers only the peer-reply sites. Notices, long-polls, sans-io cores, and frame loops answer from handlers that do not handle the peer's reply.
- **A separate held table with its own verbs** (`send_holding` / `take_held`, with or without a `with_context` modifier). This splits one request's state across two tables and two takes, so a handler could claim one half and strand the other. The debt belongs in the context that already carries the request's state.
- **Encode a context `Held` by reference.** The sender would keep a live copy after parking it. It could answer that copy and then answer again after the take, and a fail-fast drop would fire on every correct park.
