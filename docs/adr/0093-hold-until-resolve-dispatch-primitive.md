# ADR-0093: Hold-until-resolve dispatch primitive

- **Status:** Proposed
- **Date:** 2026-06-04

## Context

Settlement (ADR-0080) makes a whole causal chain of mail observable: it's how `send_mail_traced` knows a request is *fully* done rather than guessing with a timeout. For that to hold, every unit of in-flight work must be visible to the umbrella. Two rules follow: a handler runs to completion as a single-threaded unit and **must not block on replies**, and any thread it spawns must stay inside the trace + settlement umbrella or it silently opts the work out (ADR-0074 §9; raw `std::thread::spawn` pushes rootless mail invisible to settlement).

The sanctioned spawn primitives cover two shapes of offloaded work, and only two:

| primitive | holds the caller's chain? | for how long |
|---|---|---|
| `NativeCtx::spawn_inherit` | yes | the **worker thread's** lifetime (hold drops when the worker ends) |
| `NativeCtx::spawn_detached` | no | n/a — each send mints a fresh root |

There is a third shape with no sanctioned home: **work that replies in a *later* handler turn** — kick off a slow blocking call, let the worker push a result and die, and send the real reply from a *subsequent* handler invocation when that result lands. Here the hold must outlive the worker (it must span accept → the later re-reply), so `spawn_inherit` releases too early and reintroduces the premature-settlement window; `spawn_detached` doesn't hold at all. `spawn_inherit` additionally carries a documented settlement-contract gap (issue #716): the parent may settle before the worker's first send arrives.

The content-gen caps (Gemini, Anthropic) hit exactly this shape — multi-second provider calls that can't block the single-threaded actor — and solve it with a hand-rolled helper, `aether_capabilities::contentgen::dispatch::InFlightDispatch`:

- eagerly acquire a `SettlementHold` on the current root **in the handler, before it returns** (so the chain never transiently settles between handler-return and the async reply — #1031 / #1043);
- park the hold in a `request_id`-keyed correlation map in plain actor state, bounded by an in-flight counter + pending queue (rate-limits paid endpoints, ADR-0050 §2);
- spawn an ephemeral worker with a **raw** `thread::spawn` that runs the call and pushes a loopback wake carrying the original `ReplyTo`;
- on landing, re-reply through the stored `ReplyTo` **first**, then drop the hold (`Sent` before `Release`, ADR-0080 §12).

This is correct, and it's already shared across both content-gen caps. But it has three problems:

1. **The one raw spawn lives in the capability layer**, below the actor model but above the umbrella, where there is no sanctioned spawn to call. It's a bucket-(b) exception in #1050's audit — a raw spawn that must justify itself per-site instead of being umbrella-aware once.
2. **Each cap re-writes the submit + landing handlers and the correlation map** — boilerplate around a pattern that is fully general.
3. **Correctness rests on discipline, not construction.** The hold is balanced only because a `!Copy` guard happens to be stored in the right actor-state slot and dropped on the right path. Nothing makes "forgot to release" or "settled early" a *type* error.

`InFlightDispatch` is, in effect, a battle-tested prototype of the missing primitive, sitting one layer too high. Its dependencies already point at the substrate (`SettlementHold`, `Mailer`), so promoting it is a move-and-generalize, not a greenfield build.

## Decision

Promote the pattern into a first-class ctx/SDK primitive: a **hold-until-resolve dispatch**. The cap dispatches a blocking closure and receives the completion in a `#[handler(task)]` handler — it stores nothing and correlates nothing.

```rust
// request side — one closure that owns its inputs; no pre-declarations
#[handler]
fn on_generate(&mut self, ctx, req: NanobananaGenerate) {
    let provider = self.provider.clone();        // the only capture: a real worker resource
    ctx.dispatch_blocking(move || {
        let resp = provider.call(&req);          // borrow — req stays alive
        build_result(&req, resp)                 // shape the result here, off-thread
    });
}

// completion side — a handler variant, not inbound mail
#[handler(task)]
fn on_generate_done(&mut self, ctx, done: TaskDone<NanobananaResult>) {
    done.resolve(ctx);                           // re-reply the carried output via reply_to, drop the hold
}
```

Semantics and decisions:

1. **`dispatch_blocking(closure)`** eagerly acquires a `SettlementHold` on the current root *before the handler returns* (so `HoldOpen` precedes `Finished` and the #716 window is closed by construction), records `(hold, reply_to)` in the framework's **per-actor in-flight table**, and spawns the worker with only the closure. It returns a cheap `Copy` `DispatchId` for *optional* cancellation; the cap stores nothing on the happy path. A per-source concurrency bound + pending queue (from `InFlightDispatch::max_in_flight`) is honoured — over-bound requests queue rather than spawn.

2. **The framework holds the in-flight table; the cap holds nothing.** The table is opaque plumbing — `(hold, reply_to)` keyed by dispatch id — touched only on the actor's own thread (single-threaded, no lock; not a violation of the plain-actor-state rule, ADR-0038). This is the deliberate line between the framework owning the cap's *business* state (which it must not) and owning an *opaque* in-flight ledger (which is just the primitive's own bookkeeping, fine to centralise).

3. **Completion arrives as `TaskDone<Output>` in a `#[handler(task)]` handler.** When the worker finishes, the framework reunites the held `(hold, reply_to)` with the worker's `output` and routes a move-only `TaskDone<Output>` to the cap's task handler — matched by its `TaskDone<K>` parameter the same way a mail `#[handler]` matches its mail-kind parameter. The completion handler is a *variant* of `#[handler]`, not a separate attribute. Three spellings, one family: `#[handler]` is the default inbound-mail handler; `#[handler(mail)]` is the same thing written explicitly (accepted for symmetry, never required); `#[handler(task)]` marks a dispatch completion. Keeping all three under one `handler` family unifies the concept, while the explicit `(task)` / `(mail)` marker states the category — so neither the author nor the dispatch inference has to ask whether `TaskDone<K>` is a mail kind.

4. **`resolve` consumes the `TaskDone` and replies, then drops the hold** — so `Sent` precedes `Release` (ADR-0080 §12) *by construction* rather than by remembering the drop order. The common form `done.resolve(ctx)` re-replies with the carried `output` through the carried `reply_to` (the worker already shaped it); variants map the carried output — and the context, when present — to a different reply, or land a provider-failure error (`resolve_err`). Dropping a `TaskDone` without resolving releases the hold and `debug_assert`s (a silent lost reply — caught, where discipline misses it today).

5. **No context by default; context is an opt-in fed by `Into`.** The closure already `move`-captures everything the worker needs and produces a self-contained `output`, so the default `TaskDone<Output>` carries no extra cap state — and the call site declares nothing (own `req`, read it inside the closure, borrow rather than consume). When the *completion* handler genuinely needs actor-thread state the worker shouldn't take (a non-`Send` handle, or a deliberately pure worker), an opt-in `dispatch_blocking_with(cx, closure)` carries a `C` derived from the request via `Into`/`From` (e.g. `req.context()`) and surfaces as `TaskDone<C, Output>` — never hand-assembled fields at the call site.

6. **Explicit `resolve`, not framework auto-reply.** The cap shapes its own typed result / staged artifacts (a megabyte PNG becomes a staged path, not wire bytes — ADR-0050 §2). The framework owns the *hold lifecycle*; the cap owns the *reply*.

7. **Native-only first; wasm/FFI deferred.** Consumers are native caps. The umbrella applies to guests too (ADR-0074), so a future FFI `dispatch_blocking_p32` is a clean superset — deferred, not foreclosed.

`resolve` is the keystone throughout: it ties the hold's release to an explicit later event instead of a thread's stack frame — the thing neither existing spawn primitive can express.

## Consequences

### Positive

- **Fills the gap** between `spawn_inherit` and `spawn_detached` with the hold-until-resolve shape, so "reply in a later turn" has a sanctioned home.
- **The capability layer goes raw-spawn-free.** The one raw spawn moves into the umbrella-aware substrate, justified once; #1050's lint then passes over `aether-capabilities` with no per-cap `#[allow]`s.
- **Per-cap boilerplate genuinely goes away.** The cap holds no correlation map, stores nothing, does no id bookkeeping — it writes a `dispatch_blocking` call and a one-line `#[handler(task)]` handler. Gemini and Anthropic stop hand-rolling submit/landing handlers; `InFlightDispatch` retires into the substrate.
- **Correct-by-construction where it can be.** The move-only `TaskDone` + consuming `resolve` make `Sent`-before-`Release` structural; eager acquire closes the #716 window; drop-without-resolve is caught.

### Negative

- **New surface to maintain**: the ctx primitive, a framework per-actor in-flight table, and the `#[handler(task)]` variant + its dispatch-routing in the `#[actor]` macro.
- **Migration of the two content-gen caps** onto the primitive, with the existing settlement tests re-pointed.
- **The resolve-or-cancel invariant is a runtime `debug_assert`, not a compile-time proof.** "This hold outlives the worker and is resolved in the right later turn" is inherently cross-thread, cross-handler-turn state; Rust's static guarantees stop at the call stack. The primitive narrows the failure surface (you can't fumble the drop order) but can't statically prove you eventually resolved.

### Neutral / forward

- **Supersedes the content-gen slice of #1050.** `dispatch.rs`'s raw spawn stops being a documented exception and becomes the primitive's internals. #1050's lint + the remaining infra `#[allow]`s (TCP/audio/RPC threads) are unaffected.
- **Generalizes beyond content-gen.** Any future cap with a "slow work, reply later" shape (subprocess tools, long compute that isn't a DAG) gets the primitive for free.
- **Wasm reach is the obvious follow-on** once a guest needs it.
- Extends ADR-0080 §12 (the eager-acquire + `Sent`-before-`Release` ordering it already specifies); this ADR makes that ordering a property of a primitive rather than a per-cap convention.

## Alternatives considered

- **Keep `InFlightDispatch` hand-rolled + document the raw spawn** (#1050 bucket-b). Rejected: leaves the gap unfilled, keeps correctness on discipline, and repeats the boilerplate per cap.
- **Migrate content-gen to `spawn_inherit`.** Rejected: the hold dies with the worker thread, reintroducing the premature-settlement window (#1031), plus the #716 gap.
- **Migrate to `spawn_detached`.** Rejected: correctness-safe (the re-reply already rides a freshly-minted root via the stored `reply_to`, so settlement is governed by the hold, not the worker's mail root), but its `RootCtx` can't cleanly express the hand-built, `reply_to`-stamped loopback mail, and it infects the ctx-free helper with the `Actor` generic — more code, less clarity, zero behavioral gain.
- **Cap-held `Ticket<C>` map** (the cap stores the move-only ticket and looks it up by id on landing). Rejected in favour of framework-held `TaskDone`: it's more explicit, but it reintroduces the per-cap correlation map and id bookkeeping the primitive is meant to delete. The only state worth centralising is the opaque in-flight ledger, which has no business reason to live in the cap.
- **Framework auto-reply on landing.** Rejected: caps need to build typed results and stage artifacts; auto-reply over-constrains the reply shape.
- **Pure inference — a bare `#[handler]` taking `TaskDone<K>`, no category marker.** Rejected: it muddies the "is this a mail kind?" inference and blurs the line between inbound mail and a dispatch completion. `#[handler(task)]` keeps the unified `handler` family but restores an explicit marker.
- **A separate top-level attribute (`#[done]`).** Rejected: it fragments the handler vocabulary into two attributes to learn; folding completions under `#[handler(task)]` keeps one family with a variant.
