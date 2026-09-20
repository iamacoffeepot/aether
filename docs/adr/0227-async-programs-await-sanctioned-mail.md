# ADR-0227: Async Programs Await Sanctioned Mail

- **Status:** Proposed
- **Date:** 2026-09-20

Amends [ADR-0224](0224-programs-are-wasm-bundles.md) (synchronous `Pure`
only; deferred asynchronous `run` and `Sampled` effects through `Env`)
and [ADR-0225](0225-reactor-bundles-load-by-digest.md) decision 2
(per-seq invocation children exist so asynchronous `run` can hold
suspended state). Does not change
[ADR-0226](0226-native-bundle-driver.md)'s `Invoke` / `Invoked` wire:
the driver still sends `Invoke` and waits for `Invoked`. The child
simply does not reply until the program future finishes.

## Context

ADR-0224 ships programs as WASM-bundle functions over an injected
`Env<Pure>`: `fn run(input, env) -> Result<Result, Refusal>`. The
program cannot read the journal, send mail, or perform I/O. `#[program]`
(`crates/aether-bloomery-program-derive/src/check.rs`) refuses `async fn
run` and anything but `Mode::Pure`. The generated invocation child
(`crates/aether-bloomery-bundle-derive/src/expand/programs.rs`
`on_invoke`) calls `dispatch` and replies `Invoked` in the same handler.

That is the right sandbox for a digest→digest function. It cannot
express a program that needs a capability round-trip — the first
consumer is HTTP (`aether.http.fetch` → `aether.http.fetch_result`,
`HttpCapability` in `crates/aether-http`). Putting that I/O in the
native driver, or splitting one program into a flatten program and a
parse program with the driver in between, makes the driver a
per-program specialist and moves program logic out of the bundle.

Aether already has the mail FSM those programs need. Handlers do not
block: `wait_reply` was retired ([ADR-0042](0042-synchronous-mail-wait.md),
[ADR-0074](0074-unified-actor-macro.md)). The sanctioned pattern is send,
return from the handler, resume on the reply ([ADR-0139](0139-guest-reply-correlation-and-request-contexts.md)
`send_with_context` / `in_reply_to`). Settlement is exact at emit time
([ADR-0086](0086-decouple-settlement-from-trace.md)):
`(in_flight == 0 && held_open == 0)` on a causal root fires
`aether.trace.settled` `Settled { root }`
(`crates/aether-kinds/src/trace.rs`,
`SettlementRegistry::subscribe_settlement_mail`). A send that is its
own root, with replies and `spawn_inherit` workers on that root, is
quiet only when every invocation caused by that mail has finished. The
DAG `Call` node already closes a reply bundle on that signal
([ADR-0047](0047-dag-submit-cancel-status.md)).

Two gaps keep a program from using that FSM:

1. **Accept is typed; reply is not.** `HandlesKind<K>`
   (`crates/aether-actor/src/model/mod.rs`) is emitted per `#[handler]`
   and gates `ctx.actor::<R>().send(&k)`. The handler's return type is
   already the reply contract ([ADR-0109](0109-handler-reply-contracts.md)
   `HandlerReply`: `()`, `R`, `Pending<R>`), published on the inputs
   manifest as `ReplyContract`, but there is no Rust marker a caller can
   bound on. `#[handler::manual]` records `ReplyContract::Manual` with no
   kind; `HttpCapability::on_fetch` is that shape and replies
   `FetchResult` later from the task path.

2. **A program with a mail ctx can send anything.** Generic
   `env.request::<R, K>()` would compile against
   `aether.bloomery.driver.call`, `journal.append_records`,
   `reactor.warm`, and `program.invoke`. That circumvents Pure
   memoization, orphan checks, spend, and `Call`. ADR-0224's injected-data
   sandbox is the rule: `run` never sees `WasmCtx` / `MailSender`.

## Decision

1. **Two program APIs, two environments.** `Program` stays
   `fn run(input: Self::Input, env: &mut Env<Pure>) -> Result<Self::Result, Refusal>`
   with `Mode::Pure`. A new `AsyncProgram` is
   `async fn run(input: Self::Input, env: &mut Env<Async, Self::Caps>) -> Result<Self::Result, Refusal>`
   with `Mode::Sampled`. Pure programs are not colored `async`. An
   `AsyncProgram` that never awaits still completes in the first
   `on_invoke` and replies `Invoked` immediately. `Waiting` from a Pure
   program is not representable: that trait has no yield.

2. **`async` / `await` is the mail FSM, not a blocking host import.**
   `async fn run` means this invocation is a future held by the per-seq
   child (ADR-0224 §5, ADR-0225 decision 2). While the future is
   yielded, no handler is on the stack; the WASM instance can deliver
   other mail. `await` is legal only on a sanctioned Env send. It is not
   `wait_reply`, not `asyncify`, and not a Wasmtime-parked host function.

3. **An await completes when that send's tree has settled.** The
   sanctioned send is dispatched as its own causal root (`MailId` of that
   send). Nested work the recipient does — replies, streams,
   `spawn_inherit` workers — inherits that root. The invocation child
   subscribes `Settled { root }` for that `MailId` (the guest form of
   `SettlementRegistry::subscribe_settlement_mail`). Correlated replies
   are collected in arrival order. `.await` returns when `Settled`
   arrives for that root, not when the first payload arrives:

   | Recipient behaviour | Await value |
   | --- | --- |
   | Handler returns `()`, no further mail | empty bundle (`notify`) |
   | One reply, then quiet | 1-element bundle |
   | Stream of replies, then quiet | N-element bundle |

   If the send inherited `Invoke`'s root, `Settled` would mean the whole
   program finished. Each `request` therefore has its own root so send A
   can settle before send B starts.

4. **The `Invoke` chain stays open until `Invoked`.** `on_invoke`
   returns at the first await. Without a hold, `Invoke`'s `Finished`
   would settle the request while the future is live. The child holds
   that chain (the ADR-0080 `SettlementHold` contract) until it replies
   `Invoked` and drops the hold. The driver protocol does not gain
   `Invoked::Waiting`. Outstanding `Invoke` is the wait. A trap, drop, or
   engine stop while yielded is `Interrupted` (ADR-0226 decision 4), not
   a successful empty result.

5. **Inbound replies land on `#[fallback]`, not a per-kind handler.**
   The invocation child cannot name every reply kind a program might
   await, and `Settled` is a different kind from the payload. Without
   `#[fallback]` the child is a strict receiver
   ([ADR-0033](0033-handler-driven-inputs-manifest.md)): unknown kinds
   return `DISPATCH_UNKNOWN_KIND` and never reach the future. The
   catch-all takes `Mail<'_>` (`crates/aether-actor/src/mail/mod.rs`).
   Three checks, all required, never implied by the handler signature:

   | Check | Proves |
   | --- | --- |
   | `ctx.in_reply_to()` | this envelope is a reply to a send from this child |
   | pending map keyed by that id / root | it is **this** sanctioned send |
   | `mail.kind() == expected`, then `decode_kind` | the payload is the declared reply kind |

   A kind mismatch or decode failure does not resume the future as
   `Ready`. `Settled { root }` closes the bundle. Payload checks never
   decide that the send is done.

6. **Actors declare replies in the type system, from the same
   signature that already declares them.** `#[actor]` emits, next to
   `HandlesKind<K>`:

   ```rust
   trait Replies<K: Kind>: HandlesKind<K> { type Reply: Kind; }
   trait Streams<K: Kind>: HandlesKind<K> { type Item: Kind; }
   ```

   `-> R` and `-> Pending<R>` produce `Replies<K, Reply = R>`.
   `#[handler::multi]` producing `K` produces `Streams<Req, Item = K>`.
   `-> ()` produces neither: that kind is `notify` only.
   `#[handler::manual]` produces neither until the handler names a
   return type. There is no `#[handler(reply = X)]` and no `Request`
   trait on the kind. HTTP's `on_fetch` becomes `-> Pending<FetchResult>`
   so `HttpCapability: Replies<Fetch, Reply = FetchResult>` holds. Manual
   remains the escape hatch; sanctioned program APIs will not compile
   against it.

7. **Programs send only through named Env cap methods.** `Env<Pure>` is
   still read/stage. `Env<Async, Caps>` adds methods the SDK names, not
   `request::<R, K>()`, not `actor::<T>()`, not `send_to` /
   `send_to_named`. The first cap is HTTP:

   ```rust
   impl Env<Async, Caps> {
       fn http(&mut self) -> Http<'_> where Caps: HasHttp;
   }
   impl Http<'_> {
       fn fetch(&mut self, req: &Fetch) -> Mail<FetchResult>
       where HttpCapability: Replies<Fetch, Reply = FetchResult>;
   }
   ```

   `http()` exists only when `AsyncProgram::Caps` includes `Http`. The
   recipient is `HttpCapability`; the program never names a mailbox.
   Bloomery driver, journal, reactor, and program-invoke kinds are not
   on this surface.

8. **The generated invocation child is the only actor with a ctx during
   `run`.** Authors never see it. That child may send only the kinds
   those Env methods name, and only to those cap mailboxes. `#[fallback]`
   is inbound (payloads + `Settled`). Same-module WASM can still call
   `send_mail_p32`; the host refuses invocation-child outbound that is
   not on the declared cap list. The SDK contract is the sanctioned
   methods. The substrate contract is that those are the only sends that
   child is allowed to make.

## Consequences

- ADR-0224's deferred "asynchronous `run`, where `read` becomes an
  on-demand fetch" is **not** this decision. Lazy closure `read` stays
  deferred. This decision is the other deferred item: `Sampled` effects
  through `Env`, realized as sanctioned mail, not as host-blocking
  `read`.
- `aether-bloomery-program` gains `AsyncProgram`, `Env<Async, Caps>`,
  and the HTTP cap method. `#[program]` accepts `async fn run` when
  `MODE` is `Sampled` and `Caps` is nonempty. The bundle generator
  keeps the future on the invocation child, emits `#[fallback]`, holds
  the `Invoke` chain, and replies `Invoked` when the future completes.
- `aether-actor` gains `Replies` / `Streams` marker emission. Caps that
  want to be callable from programs must put the reply kind on the
  handler signature (`Pending<R>` for deferred). HTTP migrates
  `on_fetch` off `#[handler::manual]`.
- Guest `subscribe_settlement` (mail of `Settled` to the invocation
  child) is required; it is native-only today (`subscribe_settlement_mail`
  in lifecycle, render, rpc).
- Host-side outbound allowlisting for invocation children is required
  so a bundle cannot mail Bloomery internals from `run` via the send
  import.
- The driver FIFO (ADR-0226: one `Invoke` in flight per root) is
  unchanged. Overlapping Sampled waits on one digest is follow-on, not
  this ADR.
- First consumer is a program that calls `env.http().fetch`. A Muse
  turn is one such program; it is not a new driver path.

## Alternatives considered

- **Color every program `async fn`.** Rejected: Pure is a
  digest→digest function with no await points. `Mode` already splits
  memoization from effects.
- **Restore `wait_reply` inside `run`.** Rejected: ADR-0074. Handlers
  do not park a worker.
- **Native driver performs the HTTP lap; programs only flatten and
  parse.** Rejected: the driver becomes a per-program I/O specialist.
  The program sends mail. The driver still only `Invoke`s and records.
- **Replay `run` from scratch with a larger closure (`Invoked::Waiting`,
  despawn the child).** Rejected: the per-seq child was reserved to hold
  suspended state (ADR-0225 decision 2). Settlement-closed await needs
  that live actor. Replay makes `async` pointless.
- **Generic `env.request::<R, K>()`.** Rejected: `R` can be Bloomery
  internals. Named Env methods are the allowlist.
- **Per-kind reply handlers on the invocation child.** Rejected: the
  child cannot enumerate reply kinds, and `Settled` would need its own
  arm anyway. `#[fallback]` plus the pending slot is the check.
- **Let `run` take `WasmCtx` and send like an actor.** Rejected:
  ADR-0224's sandbox. Programs are functions over `Env`. Generated code
  owns the actor.
)
