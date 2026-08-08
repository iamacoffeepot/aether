# ADR-0133: Reply-Based Stream Handles for the HTTP Server Data Phase

- **Status:** Accepted (shipped — reply-based stream handles for the HTTP server data phase in `crates/aether-http/src/stream.rs`)
- **Date:** 2026-07-04

Amends **ADR-0128** (response streaming), **ADR-0129** (websocket upgrade), and **ADR-0132** (explicit stream id): replaces their handler→cap addressing — the typed singleton send at `HttpServerCapability` — with a durable stream handle that addresses whoever actually dispatched the stream to the handler. The payload `stream_id` keying ADR-0132 established is unchanged. Builds on the causal-chain model of **ADR-0080** and the reply machinery of **ADR-0013** / **ADR-0017**.

## Context

The HTTP server's request/response handshake honors a substitution invariant: a handler answers whoever dispatched to it. `HttpServerResponse`, `HttpResponseStreamOpen`, and `WebSocketAccept` are one-shot `ctx.reply`s — the reply routes to the dispatch's reply target, so the cap, a mock, or a forwarding middleware all receive the handler's answer without the handler knowing which one is on the other side.

The data phase breaks that invariant. Every mid-stream handler→cap leg — `HttpResponseChunk`, `HttpResponseStreamEnd`, `HttpRequestCredit`, outbound `WebSocketMessage`, outbound `WebSocketClose` — is sent as `ctx.actor::<HttpServerCapability>().send(..)`: compile-time singleton resolution against the cap's root mailbox. A mock that dispatches a request at a streaming handler receives the handshake reply but never the stream; a middleware forwarding requests in front of the cap is bypassed by every byte of the response. The handler cannot be tested, wrapped, or delegated without the real capability on the other end.

The data phase cannot use `ctx.reply`, for reasons that are structural rather than fixable policy:

- A guest reply handle is **one-shot** — the host's reply table removes the entry on first use — and a stream is one-to-many.
- The handle is **instance-local** (an opaque index into the receiving component's own table), so it cannot be handed to a worker actor that produces the stream.
- Server push has **no inbound dispatch** to reply to.
- A reply **joins the request's causal chain and holds it open** (ADR-0080 §6). The request chain must settle promptly: settlement is the cap's 502 safety net, what `send_mail` awaits, and what lets the trace ring reclaim. An unbounded stream cannot ride it — which is why ADR-0128 moved the stream key into the payload in the first place.

But the reply mechanism bundles two things with different lifetimes: a per-request correlation (rightly consumed by the handshake reply) and a **counterparty address**. The address half is exactly what the data phase needs, and it is already delivered on every dispatch: the cap's sends stamp `reply_to = SourceAddr::Component(cap_mailbox)` (`NativeBinding::push_envelope_buffered`), which the wasm deliver path threads into the guest's source slot, so `ctx.source_mailbox()` resolves the dispatching actor for every request, credit grant, and inbound websocket message. No host change is required to capture it.

One gap remains on the guest send surface: the SDK has typed-singleton and by-name sends in both chain modes, and a by-id send (`send_to`) only in inherit mode. The data-phase sends currently inherit whatever chain hosts the send site — a chunk pumped from a `Tick` handler rides the tick's chain, delaying the tick's settlement on the cap's chunk handling and attributing stream traffic to unrelated roots. The cap's own side already models the data phase correctly: every cap→handler leg (`HttpStreamCredit`, `HttpRequestChunk`, inbound `WebSocketMessage`, …) is `send_envelope_as_root` — a fresh bounded chain per message, keyed by payload `stream_id`.

## Decision

Make the handler side symmetric with the cap side: every data-phase message is a **detached chain root addressed at a stored counterparty**, held in a durable stream handle.

1. **`MailSender::send_detached_to<K: Kind>(&mut self, recipient: MailboxId, payload: &K)`** — the by-id detached send, filling the hole in the send grid (typed / by-name / by-id × inherit / detached). Wasm bodies route through the inline registry with `ChainMode::Detached`; native bodies encode and push a detached envelope (the typed sibling of `send_envelope_as_root`).

2. **A wasm-safe `stream` module in `aether-capabilities::http`** (beside `typed.rs`, no native runtime required) with three handle types, one per stream role:
   - `ResponseStream` — emits `HttpResponseChunk` / `HttpResponseStreamEnd`;
   - `RequestStream` — emits `HttpRequestCredit`;
   - `WebSocketStream` — emits outbound `WebSocketMessage` / `WebSocketClose`.

   Each is plain data — `{ counterparty: MailboxId, stream_id: u64 }`, public fields — constructed by capturing `ctx.source_mailbox()` from the dispatch that delivered the stream's opening leg (the first `HttpStreamCredit` for response streams and websockets, the `HttpRequestStreamOpen` for streamed requests), plus the payload's `stream_id`. Construction fails (`None`) when the dispatch carries no component source (session- or broadcast-origin), which is not a supported way to drive a streaming handler. Send methods take the ctx as `&mut impl MailSender` and issue `send_detached_to` at the stored counterparty.

3. **The handshake is untouched.** `HttpServerResponse`, `HttpResponseStreamOpen`, and `WebSocketAccept` remain chained one-shot replies: the 502 settlement net is sound precisely because a genuine answer holds the request chain open until it delivers, so the buffered path and the stream-open legs stay on `ctx.reply`.

4. **The cap is untouched.** Its data-phase handlers are already keyed by payload `stream_id` and sender-agnostic, and its outbound legs are already detached roots. The change is entirely guest-side: the handle replaces the singleton-typed send in the test-fixture handlers and the serving-http recipe, and the singleton form stops being the documented way to answer a stream.

The handle is the address half of a `ReplyHandle` made durable: multi-shot (no host table entry to consume), transferable (an address, not an instance-local index — a worker actor can hold it), valid with no inbound mail in flight (server push), and substitution-safe (the counterparty is whoever dispatched, so a mock or middleware receives the stream it asked for).

## Consequences

- **Positive — the invariant holds end to end.** A plain test actor can stand in for the cap: dispatch a request and a credit grant at a handler, receive the entire stream back at its own mailbox. Middleware that forwards requests sees the response stream. A handler can hand its stream to a sibling actor. Handlers no longer name `HttpServerCapability` at all in the data phase.
- **Positive — chain accounting matches the transaction shape.** Each stream message in either direction is a bounded detached root between two fixed actors; a `Tick` that pumps a chunk settles on its own schedule, and stream traffic stops appearing under unrelated roots in traces.
- **Negative — trace linkage from the producing dispatch to the emitted chunk is severed** (a detached root has no parent). The payload `stream_id` remains the domain-level correlation, as ADR-0132 already established for the cap side.
- **Neutral — spoofing posture unchanged.** The cap keys data-phase mail by `stream_id` without validating the sender, before and after this change; sender validation is a separate hardening decision if it is ever wanted.
- **Follow-on** — the naming-symmetry rename (`send_envelope_as_root` → detached vocabulary) is tracked separately and does not gate this work.

## Alternatives considered

- **Multi-shot host reply handles** (resolve-without-take plus an explicit release on stream end): still instance-local (no worker delegation), still nothing to hold for server push, and adds a host-table lifecycle with a leak mode when a stream never terminates — the host change buys reply syntax, not more substitutability.
- **A guest surface over the unchained reply** (`Mailer::send_reply_unchained`): the right chain semantics (no lineage, no settlement hold) but the same one-shot, instance-local, push-less limits as any reply-table path; it is the degenerate single-shot case of the handle.
- **Keep singleton addressing, add a test seam** (register a mock at the cap's root name in TestBench): fails the invariant rather than satisfying it — middleware and delegation stay broken, and the seam is a bespoke test-only mechanism where the handle makes the production path itself substitutable.
