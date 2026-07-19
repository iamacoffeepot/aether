# ADR-0154: Typed Router Path Templates and Deferred Routes

- **Status:** Proposed
- **Date:** 2026-07-19

Amends **ADR-0131** (the typed route-authoring surface): the route macro grows from a `(prefix, method)` dispatcher that must reply synchronously into one that owns the whole route tree — nested path templates with captures, and routes that answer a downstream reply instead of returning inline. Builds on the guest-side extraction principle of **ADR-0130**, the handler classes of **ADR-0134**, and the kind-typed request contexts of **ADR-0139**; takes the data-phase reasoning of **ADR-0133** as the reason a deferred reply cannot simply ride the request chain.

## Context

ADR-0131 gave guests a typed way to author HTTP routes: `#[http::route(<Method>, "<prefix>")]` methods under `#[http::router]`, each minting a request-shaped route kind and compiling down to a `#[handler::single]` glue method that returns an `HttpServerResponse`. It deliberately covers exactly one route shape — a static prefix, a method filter, a synchronous buffered reply. Two things it cannot express are exactly the two things a real REST control surface needs, and the first in-repo consumer to need both — the Bloomery REST api (`crates/aether-bloomery-host/src/api/runtime.rs`, ADR-0149) — hand-rolled its own router rather than use the macro at all.

**1. No nested paths or path parameters.** The macro routes on `(static prefix, method)`. Bloomery's surface is `/drafts/{id}`, `/drafts/{id}/seal`, `/blooms/{id}/supersede`, `/blooms/{id}/answer` — nested sub-resources keyed by a path parameter. Even registering the prefix `/drafts` through the macro, the handler would still hand-parse the remaining segments, so bloomery skipped the macro and wrote one `match (method, segments.as_slice())` over the whole tree. The registry is not the blocker: ADR-0130 already keys routes by prefix and forecloses cap-side field extraction — "the wire payload for a routed kind is always request-shaped, so parsing a request into domain values is guest-side." Segment matching and capture binding therefore belong in guest-side glue the macro can generate; the cap's route table need not change.

**2. No deferred (reply-later) route.** The macro hard-requires `-> HttpServerResponse` and emits `#[handler::single]`, which replies the instant the method returns (`crates/aether-capabilities-derive/src/lib.rs`, the response-return check and the emitted glue). A streaming route already escapes this by keeping "the raw `#[handler]` surface." But most of bloomery's routes are neither synchronous nor streaming: they forward a mail to a peer capability (the control core, the store, the artifacts cap, the signing cap) and answer only when *that* reply lands. An HTTP handler cannot block on a mail reply, so bloomery took the inbound reply obligation across the async boundary (`take_inbound`), dispatched a detached mail, keyed a pending-correlation table by the dispatch's correlation id, recovered the guard on the downstream reply via `reply_target().correlation_id`, answered through it, and armed a settlement subscription to `504` a chain that settles without ever replying.

That pending-map-keyed-on-echoed-correlation pattern is precisely the hand-roll **ADR-0139** was written to retire. ADR-0139 shipped `send_with_context(&request, context)` / `ctx.take_context::<C>()` / `in_reply_to()` on both `WasmCtx` and `NativeCtx`, moving the pending table into `aether-actor` with an exact correlation key, kind-typed contexts, hot-reload through `save_state`, and bounded eviction — and migrated the fs / audio / text / behavior / kit consumers onto it. Bloomery's api predates that migration and never adopted it, so it still carries the exact defects ADR-0139 names. The one thing ADR-0139 deliberately left out is the piece an HTTP request needs on top of the correlation: reclamation tied to the request's *settlement* (the `504` net), which its consequences flagged as "available as an opt-in later rung."

The substrate is a general application host, and concurrent request/reply demux over an HTTP surface is the inner loop of server-shaped work. Bloomery is the first HTTP consumer to need the full REST shape; it will not be the last. Leaving the router at "synchronous prefix only" pushes every such consumer back to a hand-rolled dispatch table and a hand-rolled correlation table — the two things ADRs 0131 and 0139 respectively already decided should be owned centrally.

## Decision

The route macro owns the whole route tree. Two additions, each a self-contained rung; neither changes the cap's route registry or the wire.

### 1. Path templates with captures

`#[http::route(<Method>, "/drafts/{id}/seal")]` accepts `{name}` capture segments after the claimed static head. The macro:

- registers the static head of the template (`/drafts`) as the claimed prefix with the cap, exactly as today — the ADR-0130 `(prefix, method)` registration, conflict, and dead-target semantics are untouched;
- compiles every route that shares a claimed prefix into **one** generated dispatcher that switches on `(method, remaining segments)` and binds captures — the macro generates the `match (method, segments)` bloomery wrote by hand;
- binds each capture through the ADR-0131 `FromRequest` machinery (a `PathParam`-style extractor), so a malformed capture becomes the same guest-side `400` an ordinary `FromRequest` failure does, and the routed method receives typed parameters.

The capture matching runs entirely guest-side in generated glue. The cap does **not** gain a routing trie or any knowledge of sub-paths — the load-bearing invariant from ADR-0130 that keeps route shape out of the capability.

### 2. Deferred routes

A route may answer a downstream reply instead of returning inline. A deferred route is authored as a pair:

- **The request route** returns an `http::Outcome` (`Reply(HttpServerResponse)` for the synchronous case — the existing `-> HttpServerResponse` stays valid and is sugar for `Reply` — or `Deferred`). It defers by calling `ctx.defer(target, &request)`, which the http typed surface implements as: take the inbound reply obligation, `send_with_context` the request to `target` (ADR-0139), store the held obligation's key as the context, arm a settlement subscription for the `504` net, and return `Outcome::Deferred`. The macro emits a `manual`-class glue (ADR-0134) for a deferred route so the dispatch does not auto-reply.
- **The reply route** — `#[http::reply] fn on_x(&mut self, ctx, reply: R) -> HttpServerResponse` — is generated glue keyed on `in_reply_to()`: it calls `take_context` to recover the held HTTP obligation, runs the body to map the domain reply `R` into a response, and answers through the recovered obligation rather than the ambient reply target. A downstream chain that settles without a reply fires the armed subscription and answers `504`.

This is bloomery's exact structure for a one-request-one-reply route — a request route that forwards and a typed reply route that maps and answers — with the correlation wiring, the obligation table, and the `504` net generated instead of hand-written. The author writes only the two domain halves.

**Out of scope: N-way scatter/gather.** A route that fans one request into N downstream replies, joins them, and answers only when all N complete is a quorum barrier, not a deferred reply — bloomery's seal (`POST /drafts/{id}/seal`) is the one such route, holding one obligation across N member-signature verifies (`PendingSeal.remaining`) with a fail-closed teardown, then chaining to a second single-reply `Admit` defer. `take_context` is take-once and `#[http::reply]` maps one reply to one response, so neither expresses the join; ADR-0139 likewise scoped N-reply correlation out. The deferred route deliberately does not try to absorb this: the seal keeps its explicit join (its single-reply `Admit` tail migrates to a deferred route like any other), and a general gather primitive — `defer_all` plus a quorum join — is a separate future decision an ADR of its own would make if a second N-join consumer appears. Forcing seal through a not-yet-designed primitive on the strength of one consumer is the leaky abstraction this avoids.

### 3. The obligation table lives in the HTTP server surface

The held-obligation table and the settlement `504` net live in the `aether-capabilities` HTTP surface (alongside `http::typed`), not in `aether-actor` next to the ADR-0139 context table. The correlation half is ADR-0139's — `defer` is a thin composition over `send_with_context`, and the reply route over `take_context`. Only the HTTP-specific halves (holding the request's reply obligation open, and the settlement-tied `504` reclamation ADR-0139 foreshadowed) are new, and they are HTTP semantics, so they stay in the HTTP surface. This is the one genuinely open fork in the design; it is recorded here as a decision rather than left to the implementation.

## Consequences

- **Positive** — the Bloomery api's routing and its one-request-one-reply correlation collapse into the macro: the hand-rolled `match (method, segments)`, the `pending` / `verifying` maps, the `take_inbound` plumbing, and the `504` net all go. What stays is domain logic, not routing boilerplate — the reply-to-response mappers, the pre-seal shaping ceilings, and the seal's N-verify quorum join (the barrier itself; its single-reply `Admit` tail migrates like any other deferred route). Every future HTTP consumer gets the full REST surface — nested paths, path params, deferred replies — from the macro, so the next single-reply control surface is not another hand-roll.
- **Positive** — bloomery's api migrates off the pre-ADR-0139 correlation pattern in the same move, closing the one consumer that ADR-0139's sweep missed.
- **Neutral** — additive and opt-in. Existing synchronous routes keep returning `HttpServerResponse` unchanged; raw streaming handlers keep the raw `#[handler]` surface; the cap's route registry, the wire, and the registration kinds are untouched.
- **Negative** — the route macro grows materially: a template parser, a per-prefix dispatcher generator, and the deferred-route/reply-route pairing. Complexity moves from each consumer into one reviewed place, which is the trade this ADR is making, but the macro is now a larger surface to get right.
- **Negative** — the `504` net is a settlement subscription per in-flight deferred request. This is the cost ADR-0139 named when it left settlement-tied reclamation as an opt-in; a deferred HTTP route is the case that justifies paying it, because a hung request must not hang the socket forever.
- **Follow-on** — two shippable rungs, one issue/PR each: (1) path templates + captures; (2) deferred routes + the obligation/`504` table. Rung 2 depends on rung 1's per-prefix dispatcher generation, so they land in order rather than in parallel. Bloomery's api migration is a third PR behind both, and its landing is what retires the hand-rolled router.

## Alternatives considered

- **Cap-side routing trie with path parameters.** Rejected — it moves domain path shape into the capability and contradicts ADR-0130's decision that field extraction is guest-side. The cap would have to grow sub-prefix matching and capture semantics that today are (correctly) the guest's.
- **Closure-continuation defer** (`ctx.defer(target, &req, |reply| response)`). Rejected — the downstream reply arrives as a *separate* dispatch, so a wasm actor cannot hold the mapping closure across it. The paired typed reply route is the form that composes with the actor dispatch model and with ADR-0139's take-once `take_context`.
- **The obligation table in `aether-actor`** next to the ADR-0139 context table. Rejected — the `504` settlement net is HTTP-specific; generalizing it into the actor SDK would drag HTTP request-lifecycle semantics into a layer that has no HTTP. The correlation half is already general (ADR-0139); only the HTTP halves are new, and they belong with HTTP.
- **Leave it hand-rolled per consumer.** Rejected — the substrate is a general application host and concurrent request demux is server-shaped work's inner loop (ADR-0139's own framing); the consumer count grows from bloomery, and each hand-roll re-decides dispatch and correlation policy the macro and ADR-0139 already own.
