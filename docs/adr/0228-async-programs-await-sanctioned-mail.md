# ADR-0228: Async Programs Await Sanctioned Mail

- **Status:** Proposed
- **Date:** 2026-09-20

Amends [ADR-0224](0224-programs-are-wasm-bundles.md) (synchronous
`Env<Pure>` and `Mode::Pure` only; deferred asynchronous `run` and
`Sampled` effects through `Env`). Those two deferrals are **different
axes**: `Mode` is memoization (`crates/aether-bloomery-kinds/src/program/mode.rs`);
`Env<Sync>` vs `Env<Async>` is whether `run` can await mail. A program
may be Pure and Async. Also amends
[ADR-0225](0225-reactor-bundles-load-by-digest.md) decision 2
(per-seq invocation children exist so asynchronous `run` can hold
suspended state). Does not change
[ADR-0226](0226-native-bundle-driver.md)'s `Invoke` / `Invoked` wire:
the driver still sends `Invoke` and waits for `Invoked`. The child
simply does not reply until the program future finishes.

Depends on [ADR-0227](0227-reply-contracts-are-type-markers.md)
(`Replies<K>` / `Streams<K>`), now shipped in `aether-actor`
(`crates/aether-actor/src/model/mod.rs`).

## Context

ADR-0224 ships programs as WASM-bundle functions over an injected
`Env<Pure>`: `fn run(input, env) -> Result<Result, Refusal>`. The
program cannot read the journal, send mail, or perform I/O. `#[program]`
(`crates/aether-bloomery-program-derive/src/check.rs`) refuses `async fn
run` and anything but `Mode::Pure`. The generated invocation child
(`crates/aether-bloomery-bundle-derive/src/expand/programs.rs`
`on_invoke`) calls `dispatch` and replies `Invoked` in the same handler.
`Mode` already means only "same input digest ⇒ same result digest"
(`Pure`) vs "never memoized" (`Sampled`). It is not a synonym for
sync vs async. Tying `AsyncProgram` to `Mode::Sampled` would forbid a
Pure program that awaits a deterministic cap (lazy closure `read` as
mail, ADR-0224's other deferred item).

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

Typed replies are ADR-0227 (`Replies<K>` / `Streams<K>`). The remaining
gap is mail from `run`:

**A program with a mail ctx can send anything.** Generic
`env.request::<R, K>()` would compile against
`aether.bloomery.driver.call`, `journal.append_records`,
`reactor.warm`, and `program.invoke`. That circumvents Pure
memoization, orphan checks, spend, and `Call`. ADR-0224's injected-data
sandbox is the rule: `run` never sees `WasmCtx` / `MailSender`.

## Decision

1. **Sync vs async is the `Env` parameter. Pure vs Sampled stays
   `Mode`.** Rename today's `Env<Pure>` to `Env<Sync>`. Add
   `Env<Async, Caps>`. Two program traits:

   ```rust
   trait Program {
       const MODE: Mode; // Pure or Sampled
       fn run(input: Self::Input, env: &mut Env<Sync>) -> Result<Self::Result, Refusal>;
   }

   trait AsyncProgram {
       const MODE: Mode; // Pure or Sampled
       type Caps;
       async fn run(
           input: Self::Input,
           env: &mut Env<Async, Self::Caps>,
       ) -> Result<Self::Result, Refusal>;
   }
   ```

   `#[program]` pairs them: `fn run` takes `Env<Sync>` only; `async fn
   run` takes `Env<Async, _>` only. `Env<Async>` on a synchronous `run`
   does not compile. `async fn run` with `Env<Sync>` does not compile.
   An `AsyncProgram` that never awaits still completes in the first
   `on_invoke` and replies `Invoked` immediately.

   `Mode::Pure` remains memoizable (same input digest ⇒ same result
   digest) whether `run` is sync or async. `Mode::Sampled` is never
   memoized. `Env<Sync>` has no effect methods, so `Mode::Sampled` on
   `Program` (sync) is a compile error until a sync sampling surface
   exists. HTTP and other non-deterministic caps live on `Env<Async>`
   and require `Mode::Sampled`. Deterministic async caps (when added)
   are legal on `Mode::Pure`.

2. **Sync programs do not read. The driver injects their arguments
   before `Invoke`.** Journal `ReadArtifact` / `ReadClosure` is mail
   (ADR-0226 step 4). Putting that behind `Env<Sync>::read` would make
   a synchronous `run` need to await. There is no second DI crate.

   The dependency list is already on the input: `Input: Storage + Cites`
   (`crates/aether-data/src/storage/cites.rs`). Every `Ref` the input
   (transitively) cites is a required argument. The injector is the
   native driver: it walks `Cites`, `ReadClosure`s under the byte cap,
   and puts those blobs on `Invoke.closure`. Missing or oversized
   closure is a driver `Fault`, not a guest await.

   `Env<Sync>` can **stage** and **look up** what was injected. Lookup
   is not I/O; a miss is `Refusal::InputMissing` (the injector failed
   the citation graph). It never sends mail. Written:

   ```rust
   fn run(input: SummarizeInput, env: &mut Env<Sync>) -> Result<SummarizeResult, Refusal> {
       let text = env.injected_text(input.text)?; // map lookup, not a fetch
       Ok(SummarizeResult { text: env.stage_text(&format!("summary:{text}")) })
   }
   ```

   That shape is the existing Pure fixture
   (`crates/aether-test-fixtures-program/src/lib.rs`, `Summarize`).
   `env.injected` is today's `Env<Pure>::read` renamed so it cannot be
   mistaken for journal fetch. `Env<Async>` may later grow
   `read(r).await` as on-demand fetch (ADR-0224's deferred lazy
   closure). Sync `run` never gets that method.

   A typed `Deps` argument (`fn run(input, deps: SummarizeDeps, env)`)
   is rejected as the framework: each program would need a custom
   injector. `Cites` + `ReadClosure` is one injector for every
   program.

3. **`async` / `await` is the mail FSM, not a blocking host import.**
   `async fn run` means this invocation is a future held by the per-seq
   child (ADR-0224 §5, ADR-0225 decision 2). While the future is
   yielded, no handler is on the stack; the WASM instance can deliver
   other mail. `await` is legal only on a sanctioned Env send. It is not
   `wait_reply`, not `asyncify`, and not a Wasmtime-parked host function.

4. **An await completes when that send's tree has settled.** The
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

5. **The `Invoke` chain stays open until `Invoked`.** `on_invoke`
   returns at the first await. Without a hold, `Invoke`'s `Finished`
   would settle the request while the future is live. The child holds
   that chain (the ADR-0080 `SettlementHold` contract) until it replies
   `Invoked` and drops the hold. The driver protocol does not gain
   `Invoked::Waiting`. Outstanding `Invoke` is the wait. A trap, drop, or
   engine stop while yielded is `Interrupted` (ADR-0226 decision 4), not
   a successful empty result.

6. **Inbound replies land on `#[fallback]`, not a per-kind handler.**
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

7. **Sanctioned Env methods bound on ADR-0227 markers.** `http().fetch`
   requires `HttpCapability: Replies<Fetch, Reply = FetchResult>`. This
   ADR does not introduce those markers.

8. **Programs send only through named Env cap methods.** `Env<Sync>`
   stages and looks up injected artifacts (decision 2). `Env<Async, Caps>`
   adds methods the SDK names, not
   `request::<R, K>()`, not `actor::<T>()`, not `send_to` /
   `send_to_named`. The first Sampled cap is HTTP:

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

9. **The generated invocation child is the only actor with a ctx during
   `run`.** Authors never see it. That child may send only the kinds
   those Env methods name, and only to those cap mailboxes. `#[fallback]`
   is inbound (payloads + `Settled`). Same-module WASM can still call
   `send_mail_p32`; the host refuses invocation-child outbound that is
   not on the declared cap list. The SDK contract is the sanctioned
   methods. The substrate contract is that those are the only sends that
   child is allowed to make.

## Consequences

- ADR-0224's deferred "asynchronous `run`, where `read` becomes an
  on-demand fetch" is a **Pure + Async** program: `Env<Async>` with
  deterministic read caps, still `Mode::Pure`. This ADR opens that
  slot; it does not add those read caps. Sampled HTTP is the other
  occupant of `Env<Async>`.
- `aether-bloomery-program` renames `Env<Pure>` to `Env<Sync>` and
  `read` to `injected`. `Env<Sync>` never fetches. `AsyncProgram` /
  `Env<Async, Caps>` is the await surface. `#[program]` accepts
  `async fn run` for `Mode::Pure` or `Mode::Sampled`; it rejects
  `Env<Async>` on a synchronous `run` and `Env<Sync>` on `async fn
  run`. HTTP methods require `Mode::Sampled` plus `Caps` that include
  `Http`. The bundle generator keeps the future on the invocation
  child, emits `#[fallback]`, holds the `Invoke` chain, and replies
  `Invoked` when the future completes.
- Reply typing is ADR-0227. This ADR only consumes `Replies` /
  `Streams` on sanctioned Env methods. `HttpCapability: Replies<Fetch>`
  still requires `on_fetch` to leave `#[handler::manual]` for
  `-> Pending<FetchResult>` (not this ADR).
- Guest `subscribe_settlement` (mail of `Settled` to the invocation
  child) is required; it is native-only today (`subscribe_settlement_mail`
  in lifecycle, render, rpc).
- Host-side outbound allowlisting for invocation children is required
  so a bundle cannot mail Bloomery internals from `run` via the send
  import.
- The driver FIFO (ADR-0226: one `Invoke` in flight per root) is
  unchanged. Overlapping async waits on one digest is follow-on, not
  this ADR.
- First consumer is a program that calls `env.http().fetch`. A Muse
  turn is one such program; it is not a new driver path.

## Alternatives considered

- **Color every program `async fn`.** Rejected: sync `Env<Sync>` stays
  the default. Pure programs may be async; they are not required to be.
- **Tie `AsyncProgram` to `Mode::Sampled`.** Rejected: Pure async is
  real (deterministic caps, lazy `read`). `Mode` is memoization, not
  the `Env` parameter.
- **`Env<Sync>::read` as on-demand journal fetch.** Rejected: that is
  mail. Sync `run` cannot await it. Prefetch is the driver's
  `ReadClosure`.
- **Typed `Deps` struct per program as the injector API.** Rejected:
  every program would need custom native construction. `Cites` on
  `Input` plus one `ReadClosure` walk is the injector for all
  programs.
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
