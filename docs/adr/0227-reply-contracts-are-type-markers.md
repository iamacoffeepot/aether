# ADR-0227: Reply Contracts Are Type Markers

- **Status:** Proposed
- **Date:** 2026-09-20

Amends [ADR-0109](0109-handler-reply-contracts.md) (the handler return
type is the reply contract, published on the inputs manifest) and
[ADR-0112](0112-handler-reply-classes.md) / [ADR-0134](0134-multi-reply-class-and-explicit-handler-classes.md)
(single / manual / multi). Parallel to `HandlesKind<K>`
([ADR-0075](0075-actor-typed-sender-api-and-chassis-cap-marker-split.md)
decision 1, `crates/aether-actor/src/model/mod.rs`).

## Context

`R: HandlesKind<K>` means actor `R` has a `#[handler]` that accepts kind
`K`. The `#[actor]` macro emits one impl per handler kind. Senders are
gated at the call site: `ctx.actor::<R>().send(&k)` is an `E0277` unless
`R` handles `K`. Authors never write those impls by hand.

The matching fact on the way back is already known and already checked
in two other places, neither of which a Rust caller can bound on:

1. **The return type** (ADR-0109). `-> ()` replies nothing, `-> R` and
   `-> Pending<R>` reply `R`. There is no `#[handler(reply = X)]`; the
   return type is the single source of truth. The classifier is
   `HandlerReply` in `crates/aether-actor-derive/src/handler_parse.rs`.
2. **The inputs manifest** (`ReplyContract` on
   `InputsRecord::Handler`). `(0, 0)` for single `-> ()`, `(1, R::ID)`
   for single `-> R` / `Pending<R>`, `(2, K::ID)` for multi emitting
   `K`, `(3, 0)` for manual. Tools can read this. Generic Rust cannot.

So a caller can be told at compile time "do not send `Fetch` to an actor
that does not handle it," and cannot be told "when you send `Fetch` to
`HttpCapability`, the reply is `FetchResult`." That pairing is reconstructed
by convention (`*_result` names) or by reading the manifest at runtime.

`HttpCapability::on_fetch` (`crates/aether-http/src/client/runtime.rs`)
shows the hole: it is `#[handler::manual]`, replies `FetchResult` from
the task path, and the manifest records `ReplyContract::Manual` — no
kind. The rustdoc says `Reply: FetchResult`. Callers have nothing to
bound on.

A later program `Env` (and any other typed request helper) needs
`R: Replies<K, Reply = FetchResult>` the same way it already needs
`R: HandlesKind<K>`. Inventing a `Request` trait on the *kind* (`Fetch:
Request<Reply = FetchResult>`) puts the contract on the wrong type: two
actors can handle the same request kind and reply differently, and the
handler signature is already the source of truth.

## Decision

1. **`#[actor]` emits reply markers next to `HandlesKind`, from the same
   signature.** Authors never write them.

   ```rust
   pub trait Replies<K: Kind>: HandlesKind<K> {
       type Reply: Kind;
   }

   pub trait Streams<K: Kind>: HandlesKind<K> {
       type Item: Kind;
   }
   ```

   | Handler | Marker |
   | --- | --- |
   | `#[handler::single]` `-> R` | `Replies<K, Reply = R>` |
   | `#[handler::single]` `-> Pending<R>` | `Replies<K, Reply = R>` |
   | `#[handler::single]` `-> ()` | `HandlesKind<K>` only |
   | `#[handler::multi]` emitting `I` | `Streams<K, Item = I>` |
   | `#[handler::manual]` | `HandlesKind<K>` only |

   `Pending<R>` and `-> R` are the same marker: the caller gets `R`.
   When it arrives is ADR-0109 / ADR-0093, not this trait.

2. **No second annotation.** `#[handler(reply = X)]` stays forbidden
   (ADR-0109). Changing the return type changes the marker. A kind does
   not impl `Request`; the actor does.

3. **Manual is the escape hatch, not a typed reply.** A manual handler
   can `ctx.reply` any kind, so it does not get `Replies` or `Streams`.
   A cap that wants callers to bound on a reply kind names that kind on
   the signature (`-> Pending<R>` for deferred work). HTTP's `on_fetch`
   becomes `-> Pending<FetchResult>` so
   `HttpCapability: Replies<Fetch, Reply = FetchResult>` holds.

4. **One impl per `(actor, request kind)`.** Handler kinds are already
   unique on an impl block; two `HandlesKind<K>` impls are a coherence
   error. The reply markers follow that uniqueness. Handler-set
   adoption ([ADR-0169](0169-shared-handler-sets-via-dispatch-miss-delegation.md))
   pastes `Replies` / `Streams` the same way it pastes `HandlesKind`.

5. **Callers that need a typed reply bound on `Replies` / `Streams`.**
   `HandlesKind` remains the bound for fire-and-forget. A helper that
   awaits or decodes a reply is generic over `R: Replies<K>` (or
   `Streams<K>`) and uses `R::Reply` (or `R::Item`). Sending a kind the
   actor does not reply to, or assuming the wrong reply kind, is an
   `E0277`.

## Consequences

- `aether-actor` grows two marker traits. `aether-actor-derive` emits
  the impls wherever it already emits `HandlesKind` (wasm `#[actor]`,
  native `#[runtime]`, split identity structs, handler sets).
- Caps that today reply from `#[handler::manual]` and want a typed
  caller migrate the reply kind onto `-> Pending<R>` (or `-> R`). HTTP
  `on_fetch` is the first of those. Caps that genuinely reply with an
  unbound kind stay manual and stay uncallable from `Replies`-bounded
  helpers.
- The inputs manifest `ReplyContract` stays the tool-facing copy of the
  same fact. The markers are the Rust copy. They must not disagree: both
  are generated from `HandlerReply` / handler class.
- `Publishes<K>` stays handwritten. It describes a mailbox's fan-out
  vocabulary, not a handler return.
- This ADR does not add program `Env`, async `run`, or settlement-closed
  await. Those can bound on these markers later.

## Alternatives considered

- **A `Request` trait on the kind (`Fetch: Request<Reply = FetchResult>`).**
  Rejected: the reply belongs to the handler, not the payload type. Two
  actors can handle `K` and reply differently.
- **`#[handler(reply = X)]` beside the return type.** Rejected: ADR-0109.
  Two sources drift.
- **Read `ReplyContract` from the manifest at the call site.** Rejected:
  that is runtime / proc-macro reflection, not an `E0277`. `HandlesKind`
  already proved the compile-time shape.
- **Give `#[handler::manual]` `Replies` from rustdoc or a comment.**
  Rejected: not the signature, not checked. Manual stays unbound.
- **One trait with an associated `enum { None, One, Stream }`.**
  Rejected: `notify` / `request` / `request_many` want distinct bounds,
  and `type Reply = ()` would collide with a real unit kind. Absence of
  `Replies` is `-> ()`.
)
