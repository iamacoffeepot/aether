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

1. **`ctx.hold::<R>()` returns `(Pending<R>, Held<R>)`.** It captures the current settlement hold and reply target, as `defer_reply_to(reply_target())` does today. `Held<R>` is `DeferredReply` with the reply kind in its type. It is `#[must_use]`, and its drop fails fast when unanswered. `answer(self, ctx, &R)` sends the terminal reply and releases the hold. `abandon_for_actor_close` remains the one silent discharge. `Held<R>` implements `IntoDeferredReply`, so `continue_from` and the other staging surfaces take it unchanged.

2. **`Pending<R>` stays the phantom receipt, and the return type stays the contract.** The receipt goes to the `#[actor]` macro and sets the row. The debt goes to state, a table, or a successor. One value cannot do both, because returning it would hand the debt to the framework. A handler may answer through its `Held<R>` before it returns, for an early `Err`; the receipt proves only that the obligation was armed.

3. **Only an armed obligation mints a receipt.** `NativeCtx::pending(DispatchId)` is removed. Today it mints a `Pending<R>` from any id, `DispatchId::NONE` included, so a handler can declare `-> Pending<R>` with nothing armed, which ADR-0109 §3 set out to prevent. Its three callers arm by hand just before they mint:
   - `aether-substrate`'s `TaskQueue::submit`;
   - `aether-http`'s client `PerSenderEgress::submit`;
   - `aether-bloomery-workspace`'s `RunQueue::submit`.

   When one of these dispatches at once, it returns the receipt that the dispatch call mints. When it queues, it enqueues a `Held<R>` in place of the raw `SettlementHold` and `Source` pair. `hold` and the offload dispatch calls are the only mint sites left.

4. **A native held table pairs a held reply with an outbound send.** A `Held<R>` owns a settlement hold and cannot be a kind, so the ADR-0139 request-context table (`send_with_context` / `take_context`, which stores a `Kind`) cannot carry it. `send_holding::<P>(&payload, held)` sends as `send` does and stores the `Held<R>` under the minted correlation. `take_held::<R>()`, in the handler for the peer's reply, removes it by `in_reply_to`. The table never evicts: an entry leaves only by `take_held`, or by `abandon_for_actor_close` when the actor closes. This replaces the hand-built pairs of `send_with_context` plus a stored `Source` or `InboundMail` (`aether-http`'s `DeferredSource`, `aether-window`'s `instance.rs` `pending` map).

5. **A `Held<R>` answers on its actor.** `answer` takes the actor's `NativeCtx`. Work on another thread posts a wake mail, and the woken handler answers from state; this is tcp's `ConnectReady` shape. A reply that must be sent from a thread outside the actor stays manual. `aether-substrate-harness-cap`'s `on_advance`, which hands its `InboundMail` to the embedder loop, is the one such site.

6. **Wasm guests get the same pair.** `WasmCtx::hold::<R>()` returns `(Pending<R>, Held<R>)`. The guest `Held<R>` is a typed `ReplyHandle` and can live in a request context or saved state. The host keeps its `ReplyTable` entry alive after the handler returns, and holds settlement open, until the handle answers. Today a single handler's return frees the handle (`component/dispatch.rs`), and a `ReplyEntry` carries no settlement hold. #6960 implements this.

7. **Manual keeps what it is for.** A handler stays `#[handler::manual]` when it:
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

### Neutral / forward

- This extends ADR-0109 and closes its "deferral outside ADR-0093" limit. The ADR-0093 hold mechanic, ADR-0139 request contexts, and ADR-0231 §6 manual rows are unchanged.
- Native work is #6959, wasm work is #6960, and #6961 moves the handlers that are manual for no deferral reason. #6955's remaining half, renaming the handler classes and giving a missing return type a meaning, is independent of this ADR.

## Alternatives considered

- **Inject the debt as a handler parameter** (`fn on_x(.., m: X, reply: Held<R>)`). This gives one value and no mint call, but the row would come from a parameter, against ADR-0109's rule that the return type is the contract.
- **A typed manual ctx** (`NativeCtx<'_, Self, Manual<R>>`). Rejected: manual means the handler answers by hand. Typing its ctx blurs the one class kept for untyped replies, and the deferred sites are not manual in kind; they answer exactly once, later.
- **Mint `Pending<R>` from `send_with_context` and answer by the peer-reply handler's return.** This covers only the peer-reply sites. Notices, long-polls, sans-io cores, and frame loops answer from handlers that do not handle the peer's reply.
- **Store `Held<R>` in the ADR-0139 request-context table.** That table holds serializable kinds shared with the guest runtime, and a native settlement hold is not one.
