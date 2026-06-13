# ADR-0109: Handler reply contracts via return types

- **Status:** Proposed
- **Date:** 2026-06-13

## Context

Request/reply is the dominant shape on the mail surface — `aether.component.load` → `LoadResult`, `create_texture` → `create_texture_result`, `load_font` → `load_font_result`, and so on. The system has no way for a handler to *declare* what it replies with, and three things follow from that absence:

1. **The reply is hand-written in every handler** — `let r = self.handle(m); ctx.reply(&r);` (e.g. `aether-capabilities/src/component.rs:222`). Forgetting the `ctx.reply` leaves the caller's chain un-settled; replying the wrong kind mismatches the caller silently; nothing checks either.
2. **The reply kind never reaches the MCP/RPC layer.** ADR-0033's `#[actor]` macro emits an `aether.kinds.inputs` custom section whose per-handler record is `{ id, name, doc }` — inputs only (`aether-data/src/schema.rs:813`). `describe_component` serves that verbatim as `HandlerCapability { id, name, doc }` (`aether-kinds/src/lib.rs:1283`), so a caller — notably the Claude-in-harness driver — sees what a handler *accepts*, not what it *returns*. Worse, a reply whose kind a component defines isn't in the global vocabulary `send_mail` decodes against (`descriptors::all()`), so the reply falls through to opaque base64 (`aether-mcp/src/tools.rs:1557`): the driver can't read the response without already knowing its shape. It sends and guesses.
3. **The DAG validator can't type-check it.** ADR-0047's validator skips edges out of a `Source` because "a source's output kind is whatever the cap replies, which a handler never declares" (`aether-substrate/src/dag/validator.rs:12`). A mis-wired reply edge fails at runtime, not at submit.

The reply *machinery* is mature and not the problem. Every inbound envelope carries its sender + correlation (`Mail.sender: u32` / native `OwnedDispatch.sender: Source`, ADR-0042/0013); `ctx.reply::<K>()` routes back to it; ADR-0106 routes a reply through `InboundMail::reply` so the obligation discharges structurally. What's missing is the *contract*: a place on the handler that says "this returns X," checked by the compiler and readable by tools.

Two prior decisions bound the design. ADR-0074 retired `wait_reply` — request/reply is strictly asynchronous, handlers do not block on replies — so a reply contract must not reintroduce in-handler blocking. ADR-0093 already owns deferred replies (`dispatch_blocking` → `#[handler(task)]` → `resolve()`) for work that answers in a later turn; a contract has to cover that path too, or it captures only the minority of replies produced in-handler.

## Decision

A handler's **return type is its reply contract** — the single source of truth, checked by the compiler. No separate annotation: a `#[handler(reply = X)]` beside a `-> X` is two sources that drift, so the return type carries it alone. The `#[actor]` macro branches on three return shapes:

| return type | behaviour |
|---|---|
| `-> ()` (default) | reply nothing — fire-and-forget, unchanged |
| `-> R: Kind` | reply `R` to the inbound sender, synchronously, on handler return |
| `-> Pending<R>` | the reply is `R`, discharged later via ADR-0093 — obligation armed, not replied now |

```rust
// synchronous — the bulk of the *_result handlers
#[handler]
fn on_load_component(&mut self, ctx, m: LoadComponent) -> LoadResult {
    self.handle_load(ctx, m)            // macro: reply the returned value via InboundMail::reply
}

// errors need no new shape — *_result kinds are already Ok/Err enum kinds
#[handler]
fn on_create_texture(&mut self, ctx, m: CreateTexture) -> CreateTextureResult { /* … */ }

// deferred — Pending<R> names the reply kind on the request handler's signature
#[handler]
fn on_load_font(&mut self, ctx, m: LoadFont) -> Pending<LoadFontResult> {
    ctx.dispatch_blocking::<_, LoadFontResult>(move |w| fetch_and_parse(w, m))
}
#[handler(task)]
fn on_font_ready(&mut self, ctx, d: TaskDone<Font, LoadFontResult>) -> LoadFontResult {
    LoadFontResult::Ok { font_id: self.register(d.into_inner()), /* … */ }
}
```

1. **`-> R` replies synchronously through the inbound guard.** The macro captures the returned value and routes it through ADR-0106's `InboundMail::reply`, so the reply inherits inbound lineage and the settlement obligation discharges by construction — the same path a manual `ctx.reply` takes, emitted for you. `ctx.reply` / `ctx.reply_to` remain for what a return can't express (below).

2. **The mechanic does not change for deferred replies.** `Pending<R>` is type-level only. The actual reply is still ADR-0093's hold ledger re-replying from the task handler; `Pending<R>` is a phantom receipt that puts `R` on the request handler's signature so the contract is visible where the caller looks. `dispatch_blocking` and `TaskDone` gain the reply-kind parameter (`dispatch_blocking::<_, R>` returns `Pending<R>`; `TaskDone<Output, R>`), so the task handler's `-> R` is checked against the armed obligation. ADR-0093 §4's explicit `resolve(ctx)` / `resolve_err` is subsumed by the task handler's return: returning `R` re-replies then releases the hold (`Sent` before `Release`, unchanged); returning `()` releases the hold with no reply — making the drop-without-resolve case an explicit signature choice rather than a `debug_assert`.

3. **`Pending<R>` is framework-constructed, not user-fabricable.** It is returned only by `dispatch_blocking`, so "the signature declares `Pending<R>`" implies "an obligation for `R` was actually armed" — you cannot claim a contract you didn't arm.

4. **The reply contract is published to MCP/RPC.** The macro reads the reply kind off the return type — `R` for `-> R`, the inner `R` for `-> Pending<R>` — and extends the per-handler manifest with it. For wasm components that's a new field on `InputsRecord::Handler` (`aether-data/src/schema.rs:813`, encoded in `canonical/inputs.rs:20`) carried in the `aether.kinds.inputs` custom section, parsed on load into a `reply` field on `HandlerCapability`, so `describe_component` reports `In -> Out` per handler. The driver then reads what a call returns *before* issuing it, and `send_mail` / `send_mail_traced` decode the response against the declared kind — including component-defined reply kinds that today return base64 — using the component's own cached vocabulary instead of a global-vocab search (`aether-mcp/src/tools.rs:1557`). The RPC wire itself is unchanged: a reply already carries its `KindId` (`MailEnvelope.kind`, `aether-rpc/src/rpc.rs:141`); what's new is the caller knowing the expected kind ahead of time, so the response is decoded by contract, not by guess.

5. **Native chassis caps surface the same contract through a native handler manifest.** The unified `#[actor]` macro (ADR-0074) runs on native caps too, but unlike wasm there's no per-handler manifest today — `describe_kinds` and the inventory cap's `ListKinds` are flat kind-vocabulary lists, not request→reply maps (`aether-kinds/src/descriptors.rs:38`). Surfacing the reply contract for the caps the driver leans on most (`aether.fs`, `aether.render`, `aether.audio`) needs a native handler inventory the macro populates the way the custom section does for wasm. Scoped as a follow-on (below), so the first change lands the type/macro core plus the wasm path.

6. **Replies still route by kind — no `Kind::REPLY`.** A returned `R` is ordinary mail with kind `R::ID` landing in whatever `#[handler]` the sender has for `R` (ADR-0033). This adds a *responder-side* output declaration; it does not make the requester declare or block on an expected reply (ADR-0074). The "handlers promise nothing about replies" property the validator relied on is replaced by a declaration the validator can now *use*.

## Consequences

### Positive

- **Reply correctness by construction:** forgot-to-reply, wrong-kind, and double-reply collapse into one compiler-checked return, riding ADR-0106's discharge path.
- **The reply contract reaches MCP/RPC.** `describe_component` shows `In -> Out`; `send_mail` decodes replies by declared kind — including the component-defined reply kinds that fall to opaque base64 today. The in-harness driver reads a call's return before issuing it, and the async-edge gap closes: `Pending<R>` carries the contract on the request handler, so deferred sources are as visible as synchronous ones.
- **The DAG validator stops punting** on `Source` output edges (ADR-0047).
- **One reply concept across sync and deferred:** `ctx.reply` and `resolve()` both become "return the value." ADR-0093 keeps its submit/landing mechanic but loses the explicit `resolve` call.

### Negative / limits

- `ctx.reply` / `ctx.reply_to` don't disappear — a reply redirected to a third party can't be a single return, so a second reply path coexists with the return-type default.
- **Streaming is out of scope by design, not omission.** A handler that emits many replies over time can't be a single return value. That shape belongs to the pub-sub topic layer (input streams, ADR-0021/0068), where a subscription is keyed by `KindId`, decoupled from any one request, and survives `replace_component` — a better model for streams than a request-bound response stream, and the reason a `-> Stream<R>` return would only re-import a competing one. Its completion semantics (when does an open stream settle?) belong with the settlement-closure primitive, not here. A declare-only `Stream<R>` contract could surface a stream's element kind to introspection later, but only once a stream-completion primitive exists to back the promise.
- **Deferral outside ADR-0093 isn't covered.** A handler that defers via the manual correlation FSM (stash correlation, reply on a later inbound handler — ADR-0042) has no `Pending<R>` to return, so its contract stays uncaptured. Only `dispatch_blocking` deferral gets the typed contract.
- A `-> R` handler invoked by a fire-and-forget sender (no reply-to) silently drops the reply, consistent with today's `ctx.reply` no-op on `NO_REPLY_HANDLE` — worth a `debug_assert`, but it can't be a compile error (the sender's intent isn't on the type).
- New surface: the `Pending<R>` type, the reply-kind params on `dispatch_blocking` / `TaskDone`, the macro's return-shape branch, and the output manifest field.

### Neutral / forward

- Amends ADR-0033 (output added to the inputs manifest), ADR-0074 (a responder-side reply contract; requester still async), and ADR-0093 (`resolve` subsumed by the task handler's return; `Pending<R>` is a type-level wrapper over its hold). Routes through ADR-0106's `InboundMail::reply`.
- **Native-cap reply surfacing is a follow-on.** It needs a native handler manifest (the macro populating it the way the wasm custom section is populated); the first change covers the type/macro core and the wasm `describe_component` path, where component replies are currently undecodable.
- Migration is opportunistic: existing `ctx.reply`-tailed handlers keep compiling (they're `-> ()` that reply by hand); converting one to `-> R` is a local edit.

## Alternatives considered

- **`#[handler(reply = X)]` annotation.** Rejected: redundant with — and able to drift from — the return type.
- **A real awaitable future / `async fn` handler.** Rejected: holding `&mut self` across `.await` forces either head-of-line blocking (the `wait_reply` ADR-0074 retired) or aliasing; wasm `receive_p32` has no async ABI; what you'd await on is inbound mail, i.e. ADR-0093's continuation. `Pending<R>` gets the type flowing upward without a poller the runtime can't provide.
- **Framework auto-reply for deferred work too** (no `Pending`, infer from the task handler). Rejected: the reply kind would sit on the completion handler, not the request handler — invisible at the point a caller looks, which is the gap this ADR closes.
- **A streaming return (`-> Stream<R>`) to complete the request/reply/stream space.** Rejected here: it would be a second, gRPC-shaped streaming model competing with the existing pub-sub topics (ADR-0021/0068), with no single emission mechanic and a completion question that's really the settlement-closure primitive's. Streaming stays a topic concern.
- **Cover the manual correlation FSM as well.** Deferred: no single return value spans an arbitrary number of later inbound turns; forcing one re-grows toward the awaitable-future design.
