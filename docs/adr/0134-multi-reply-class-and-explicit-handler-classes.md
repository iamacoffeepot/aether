# ADR-0134: Multi Reply Class and Explicit Handler Classes

- **Status:** Accepted (shipped — multi reply class + explicit `#[handler::{single,multi,manual}]` classes in `aether-actor-derive`; amends ADR-0112)
- **Date:** 2026-07-05

Amends **ADR-0112** (handler reply classes): the reserved `stream` class is renamed **multi** and goes live on the detached-emission model **ADR-0133** established for the HTTP server data phase, and the bare `#[handler]` default is removed — every mail handler spells its reply class. Builds on the causal-chain model of **ADR-0080** and the settlement discharge of **ADR-0106**.

## Context

ADR-0112 defined three reply classes and shipped two. `single` (the return value is the reply) and `manual` (the handler issues replies by hand) are live; the third — a handler that answers one dispatch with several mails — was reserved as `stream` and hard-rejected by the macro, "awaiting its primitive": ADR-0109 had deferred the streaming shape behind a stream-completion question (when does an open stream settle?) that no primitive answered.

ADR-0133 answered it for the HTTP server, and the answer generalizes because its reasons are structural rather than HTTP-specific:

- **Settlement latency is read as meaning.** A settled chain is what `send_mail` awaits, what the HTTP cap's 502 net watches, and what lets the trace ring reclaim. Emissions riding the request chain hold it open until the counterparty consumes them, so every settlement consumer reads a slow stream as a dead handler.
- **A chain closes by structural discharge, and there is no hold primitive.** Once the handler returns and its spawned work drains, the chain settles; an emission made from a later dispatch cannot join it. Keeping a chain open across dispatches is exactly the deferred completion primitive — which ADR-0133 chose not to build, moving correlation into the payload instead.
- **Inheriting the hosting dispatch's chain makes the chain home timing-dependent.** A fast producer's message rides the eliciting dispatch's chain; a slow producer's identical message rides some unrelated tick's. Same protocol leg, different causal shape, decided by scheduling.

So the HTTP data phase settled on: every stream message is a detached chain root at the dispatch counterparty, correlation is payload-level, and the handshake stays a chained one-shot reply. That is a complete reply-class semantics waiting for the class to claim it.

Two further problems surfaced while claiming it. First, the reserved name is wrong twice over: "stream" collides with the substrate's input streams (the ADR-0021 pub-sub topics — and ADR-0109 explicitly kept "streaming" a topic-layer concern to avoid a competing model), and it promises a session lifecycle — open, end, credit, ordering — that the class deliberately does not own. The HTTP capability *builds* streams out of this reply shape plus its own kinds (`stream_id`, `HttpResponseStreamEnd`, credit); the class itself declares only how many replies a handler emits and on what chain. Second, the bare `#[handler]` default: with two classes, defaulting to `single` was benign shorthand; with three live classes the default hides a load-bearing choice, on a surface authored substantially by agents, where the explicit marker is also what a reader greps and what the manifest promises.

## Decision

### 1. The trio is single / multi / manual

The reserved class is renamed **multi** before anything ships under the old name: attribute `#[handler::multi]`, reply-mode marker `Multi<K>` (replacing the unit `Stream` in `aether-actor/src/model/ctx/reply_mode.rs`), manifest variant `ReplyContract::Multi(KindId)` (a relabel of discriminant 2 in `aether-data/src/schema.rs` — the wire pins numeric discriminants, so no wire break and no custom-section version bump). One term per concept across the whole machine surface: attribute, marker, manifest variant, macro diagnostics. The runner-up `many` was rejected for sharing the `man` prefix with `manual` — the two must stay distinguishable to a reader scanning attribute sites. HTTP keeps its own stream vocabulary (`stream_id`, `ResponseStream`, `WebSocketStream`): those name genuine sessions with lifecycle, which is the word doing correct work there.

### 2. Multi semantics: detached emissions of a declared kind at the dispatch source

A multi handler answers the dispatch that invoked it with **0..n mails of one declared kind `K`, each a detached chain root addressed at the dispatch source**.

- **The marker is typed.** The handler's ctx is `WasmCtx<'_, Multi<K>>` / `NativeCtx<'_, Multi<K>>`. A new `Emit<K>` trait (`fn emit(&mut self, payload: &K)`, in `aether-actor`'s ctx model beside `OutboundReply`) is implemented only for the `Multi<K>`-mode ctx types; `emit` sends `K` as a detached root at the current dispatch's source mailbox — the by-id detached send ADR-0133 added (`MailSender::send_detached_to`), with the recipient captured from the dispatch rather than named by the handler. The substitution invariant holds: the emissions go to whoever dispatched — the real peer, a mock, or a middleware.
- **The manifest is true by construction.** The `#[actor]` macro reads `K` off the signature's `Multi<K>` marker and records `ReplyContract::Multi(K::ID)` on the handler's inputs-manifest record. As with ADR-0109/ADR-0112, there is no side declaration that can drift: the type the compiler enforces is the type the manifest reports.
- **The return type must be `-> ()`.** Multi replies go through `emit`; a handshake-style one-shot answer belongs to a single or manual handler, which is exactly how the HTTP server splits its phases (chained `ctx.reply` handshake, detached data phase). A multi handler with a non-`()` return is a compile error, as is a class/marker mismatch (`#[handler::multi]` with a `Single` or `Manual` ctx fails to unify, per the ADR-0112 double-statement rule).
- **The dispatch chain settles on return**, like any `-> ()` handler. Emissions never join it: one chain is one transaction, and each emission is its own bounded transaction with the counterparty — symmetric with the counterparty side, which already sends every data-phase leg as a fresh root. Correlation across emissions is payload-level and domain-owned (the ADR-0132 rule).
- **A sourceless dispatch warn-drops the emission.** Session-, broadcast-, and substrate-origin mail carries no component source; `emit` on such a dispatch logs a warning and drops, mirroring ADR-0133's posture that this is an unsupported way to drive a streaming handler.
- **Only the declared `K` rides `emit`.** A protocol with more legs (terminators, acks) sends them through stored handles over `MailSender::send_detached_to`, which every mode already has. The manifest names the element kind; the domain names its protocol.

### 3. Mail handlers spell their class

Bare `#[handler]` and `#[handler(mail)]` become pointed compile errors naming the three classes; every mail handler is `#[handler::single]`, `#[handler::multi]`, or `#[handler::manual]` (trigger axis composing in parens as before). `#[handler(task)]` stays classless — the task variant has no reply class (its reply rides `TaskDone`), and the macro already rejects class markers there. The ctx type parameter's `M = Single` default stays: the attribute carries the explicit declaration, and the signature marker is required exactly where it adds information (`Manual`, and `Multi<K>` with its element kind). This supersedes ADR-0112's "the bare form stays the overwhelming common case" clause; the migration of existing bare sites is tracked as its own mechanical change.

## Consequences

### Positive

- **The reply-class trio is complete and truthfully introspectable.** A many-reply handler reports `Multi(K)` on `describe_component` / `describe_handlers` instead of being forced onto `manual`'s undeclared escape hatch; the driver learns both that the handler emits repeatedly and what kind arrives.
- **The class is explicit at every handler site.** An agent authoring a handler states the reply shape; a reader greps one attribute form per class; the default that silently meant "single" is gone.
- **No competing streaming model.** The class claims cardinality and chain shape only; sessions, completion, and flow control stay domain-owned, so the topic layer (ADR-0021) and protocol kinds (ADR-0128/0132/0133) keep their territory.

### Negative / limits

- **A whole-tree migration** (~200 bare `#[handler]` sites plus the guide and CLAUDE.md examples) — mechanical, behavior-preserving, tracked separately from this ADR's implementation.
- **Trace linkage from dispatch to emission is severed** — a detached root has no parent. Payload correlation remains the domain-level linkage for multi emissions and other detached data phases, as ADR-0133 accepted for HTTP; one-shot request/reply paths use the envelope request id instead.
- **A session-origin driver cannot sink emissions.** MCP `send_mail` dispatches carry no component source, so multi handlers are component-to-component surfaces — the same limit HTTP's data phase already has.
- **One declared kind per multi handler.** Multi-kind conversations keep their other legs on `MailSender`, invisible to the reply contract. That is the price of a manifest that names one kind rather than reporting `Manual`-style opacity.

### Neutral / forward

- **The HTTP capability is untouched.** Its ADR-0133 handles and its cap-side detached roots already implement this model; re-expressing its guest-side handlers as `#[handler::multi]` is possible later but not planned — this ADR is class-system completeness.
- **ADR-0109's deferred "stream-completion primitive" is retired as an open question**: completion is a domain kind, and settlement never carries it.
- **Extends ADR-0112's marker machinery** — the `Multi<K>` marker stays a ZST for every `K`, preserving the layout-identity downgrade coercions (`as_single`, and the new `as_multi`).

## Alternatives considered

- **Keep the name `stream`.** Collides with the input-streams topic vocabulary and claims a lifecycle the class doesn't provide; both misdirect the next reader (and the next agent) toward session semantics that live in the domain.
- **`many` instead of `multi`.** Shares the `man` prefix with `manual`; attribute sites are scanned, and the two classes must not blur.
- **A kind-generic `emit` with a kindless manifest variant.** Surrenders the element kind ADR-0112 deliberately gave `ReplyContract::Stream(KindId)`; the manifest would stop answering "what does this handler send back," which is the introspection the class exists to provide.
- **Chained emissions** (inherit the hosting dispatch's chain). Turns settlement into accidental backpressure, makes the chain home depend on producer timing, and fuses n bounded transactions into one long-lived one on the guest side while the counterparty keeps them separate — the asymmetry ADR-0133 removed.
- **A streaming return type (`-> Stream<R>`).** Rejected in ADR-0109 and again in ADR-0112: it competes with the topic layer and has no emission mechanic for the cross-dispatch case.
- **Requiring the signature to spell `Single` too.** The attribute already states the class and the macro pins attribute ↔ signature consistent; forcing ~200 unchanged signatures to restate the default adds churn without information.
