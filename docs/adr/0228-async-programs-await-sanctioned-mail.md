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
sync vs async. Tying async `run` to `Mode::Sampled` would forbid a
Pure program that awaits a deterministic cap (lazy closure `read` as
mail, ADR-0224's other deferred item).

That is the right sandbox for a digest→digest function. It cannot
express a program that needs a round-trip during `run`. The first
async need is on-demand artifact fetch (ADR-0224's deferred lazy
`read`): the blob was not in the injected closure, journal read is
mail, sync `run` cannot await it. Configurable caps (HTTP, …) are
follow-on; this ADR does not add a `Caps` type parameter.

Aether already has the mail FSM those programs need. Handlers do not
block: `wait_reply` was retired ([ADR-0042](0042-synchronous-mail-wait.md),
[ADR-0074](0074-unified-actor-model-for-substrate-and-guests.md)). The sanctioned pattern is send,
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

1. **One `Program` trait. Sync vs async is the `run` form, paired with
   `Env`.** Rename today's `Env<Pure>` to `Env<Sync>`. Add `Env<Async>`
   with no cap type parameter. Rust cannot overload `fn run` and
   `async fn run` on the same trait, so the SDK may keep two *private*
   supertraits for the two signatures. Authors do not implement those.
   They write one `#[program] impl Program for X` and one of these
   `run`s:

   ```rust
   trait Program {
       const NAME: &'static str;
       const MODE: Mode; // Pure or Sampled — memoization, not I/O
       const INTENT: &'static str;
       type Input: Storage + Clone + Cites;
       type Result: Storage + Clone + Cites;
   }

   // Summarize, full closure already injected:
   fn run(input: Self::Input, env: &mut Env<Sync>) -> Result<Self::Result, Refusal>;

   // same program, fetch a cited blob that was not injected:
   async fn run(input: Self::Input, env: &mut Env<Async>) -> Result<Self::Result, Refusal>;
   ```

   `#[program]` pairs the form with the env: `fn run` takes `Env<Sync>`
   only; `async fn run` takes `Env<Async>` only. The other pairing does
   not compile. A sync `run` is invoked directly (no future). An
   `async fn run` that never awaits still completes on the first poll
   and replies `Invoked` immediately.

   This is not OOP inheritance and not a public `AsyncProgram: Program`.
   Shared declaration lives on `Program` (name, mode, input, result).
   Behavior is which `run` the impl writes.

   `Mode::Pure` remains memoizable (same input digest ⇒ same result
   digest) whether `run` is sync or async. `Mode::Sampled` is never
   memoized. `Env<Sync>` has no effect methods, so `Mode::Sampled` with
   a synchronous `run` is a compile error until a sampling surface
   exists. This ADR's `Env<Async>` methods are deterministic (artifact
   fetch by digest), so they are legal on `Mode::Pure`. Sampled methods
   (HTTP, …) are not in this slice.

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
   mistaken for journal fetch. Sync `run` never gets an awaitable
   `read`.

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

7. **`Env<Async>` baseline is injected lookup, stage, and awaitable
   artifact read.** Same `injected` / `stage_*` as `Env<Sync>`. Plus:

   ```rust
   impl Env<Async> {
       async fn read<K: Storage>(&mut self, r: Ref<K>) -> Result<K, Refusal>;
       async fn read_text(&mut self, r: Ref<Utf8Text>) -> Result<String, Refusal>;
   }
   ```

   If the digest is already in the injected map, `read` returns it
   without mail (same as `injected`). If it is missing, the invocation
   child sends the journal read for that digest (the same
   `ReadArtifact` / closure-child path the driver uses in ADR-0226
   step 4), and `.await` waits until that send's tree has settled
   (decision 4). The reply kind is whatever that journal handler
   declares (`Replies`, ADR-0227). A miss after settlement is still
   `Refusal::InputMissing`. No `type Caps`. No `http()`. No
   `request::<R, K>()`, `actor::<T>()`, or `send_to`.

   ```rust
   async fn run(input: SummarizeInput, env: &mut Env<Async>) -> Result<SummarizeResult, Refusal> {
       let text = env.read_text(input.text).await?;
       Ok(SummarizeResult { text: env.stage_text(&format!("summary:{text}")) })
   }
   ```

8. **The generated invocation child is the only actor with a ctx during
   `run`.** Authors never see it. For this slice it may send only the
   journal-read kinds `Env<Async>::read` needs, and only to the journal
   mailbox. `#[fallback]` is inbound (payloads + `Settled`). Same-module
   WASM can still call `send_mail_p32`; the host refuses
   invocation-child outbound that is not that read. Bloomery driver,
   reactor, program-invoke, and HTTP are not on this surface.

## Consequences

- ADR-0224's deferred "asynchronous `run`, where `read` becomes an
  on-demand fetch" **is** this slice: `Env<Async>::read`, `Mode::Pure`.
  HTTP and a `Caps` parameter are follow-on.
- `aether-bloomery-program` keeps one `Program` trait, renames
  `Env<Pure>` to `Env<Sync>` and `read` to `injected`. `Env<Sync>`
  never fetches. `Env<Async>` adds `read` / `read_text` that await a
  journal fetch on a miss. `#[program]` accepts `fn run` + `Env<Sync>`
  or `async fn run` + `Env<Async>`; mixed pairings do not compile. The
  bundle generator calls a sync `run` directly, or holds the future
  for `async fn run`, emits `#[fallback]`, holds the `Invoke` chain,
  and replies `Invoked` when that run finishes.
- Reply typing is ADR-0227. `read` consumes `Replies` on the journal
  read handler. HTTP `on_fetch` / `Pending<FetchResult>` is not this
  ADR.
- Guest `subscribe_settlement` (mail of `Settled` to the invocation
  child) is required; it is native-only today (`subscribe_settlement_mail`
  in lifecycle, render, rpc).
- Host-side outbound allowlisting for invocation children is required
  so a bundle cannot mail Bloomery internals from `run` via the send
  import.
- The driver FIFO (ADR-0226: one `Invoke` in flight per root) is
  unchanged. Overlapping async waits on one digest is follow-on, not
  this ADR.
- First consumer is an async `Summarize`: `env.read_text(input.text).await`
  with a closure that omitted the cited text. Muse / HTTP is not this
  ADR.

## Alternatives considered

- **Color every program `async fn` on the public trait.** Rejected as
  the authoring API: a sync `run` is a function, not a future the
  invocation child must poll. The impl may still be `async fn` when
  it needs `Env<Async>`.
- **Public `AsyncProgram: Program` supertrait.** Rejected: that is two
  traits for authors to pick, which is the `Read` / `AsyncRead` split.
  Shared declaration stays on `Program`; `#[program]` selects the
  private run supertrait. Rust has no implementation inheritance.
- **Tie async `run` to `Mode::Sampled`.** Rejected: Pure async is
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
- **Configurable `type Caps` / `Env<Async, Caps>`.** Deferred: this
  slice has one async surface (`read`). HTTP and other methods can
  join `Env<Async>` later without a cap type parameter in the first
  cut.
- **HTTP `fetch` as the first `Env<Async>` method.** Deferred: it is
  Sampled, needs `Replies<Fetch>` on a still-manual handler, and is
  not required to prove await. Artifact `read` is.
- **Native driver performs the HTTP lap; programs only flatten and
  parse.** Rejected: the driver becomes a per-program I/O specialist.
  The program sends mail. The driver still only `Invoke`s and records.
- **Replay `run` from scratch with a larger closure (`Invoked::Waiting`,
  despawn the child).** Rejected: the per-seq child was reserved to hold
  suspended state (ADR-0225 decision 2). Settlement-closed await needs
  that live actor. Replay makes `async` pointless.
- **Generic `env.request::<R, K>()`.** Rejected: `R` can be Bloomery
  internals. The allowlist is the methods on `Env<Async>` (this slice:
  `read` / `read_text`).
- **Per-kind reply handlers on the invocation child.** Rejected: the
  child cannot enumerate reply kinds, and `Settled` would need its own
  arm anyway. `#[fallback]` plus the pending slot is the check.
- **Let `run` take `WasmCtx` and send like an actor.** Rejected:
  ADR-0224's sandbox. Programs are functions over `Env`. Generated code
  owns the actor.
