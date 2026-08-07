# ADR-0136: HTTP Route Target Sets

- **Status:** Accepted (shipped — HTTP route target sets and `#[router(shared)]` in `crates/aether-http/src/server/runtime/routing.rs`)
- **Date:** 2026-07-05

Amends **ADR-0130** (HTTP route registration): a route's target grows from one mailbox to a set of member mailboxes that N handler instances opt into together, with per-request selection spreading load across live members. Registration, conflict, and dead-target semantics otherwise stand. Builds on the sharded dispatch of **ADR-0135**.

## Context

ADR-0130 keys each route to exactly one `MailboxId`, and rejects a second claim of a held `(prefix, method)` key — the accidental-claim guard: two components cannot silently fight over a path family. That single mailbox is a single-threaded actor, and once ADR-0135 sharded the dispatch side, it became the measured throughput ceiling: the `spike/http-stress` sweep (issue 2610) walls at ~31k req/s on one handler regardless of shard count — ~31µs of serialized handler-actor time per request — while a measure-branch probe spreading requests across 4 handler instances moved the same workload to 57k. The substrate has cheap instanced actors (ADR-0079); what is missing is a way for a route to name more than one of them.

The guard is worth keeping. The failure it prevents — two unrelated components each believing they own `/api` — is silent misrouting, the worst kind. Spreading must therefore be something instances opt into *together*, not something a second registration gets by default.

## Decision

### 1. Registration opt-in: `shared` on the registration kinds

`RegisterRoute` and `RegisterRouteSelf` gain a `shared: bool` field (a positional wire append, tolerated pre-1.0 with every consumer in-repo and recompiled in lockstep — the `HttpServerRequest.peer_addr` precedent; the ADR-0131 route macro emits `shared: false`).

- `shared: false` is exactly today's semantics: an exclusive claim, conflict `Err` when the key is held by anyone else, idempotent kind-updating re-claim by the same mailbox.
- `shared: true` joins the key's member set — accepted only when every existing member is shared and the dispatch kind matches. A shared registration against an exclusively-held key, an exclusive registration against a shared set, and a kind mismatch on join are all conflict `Err`s. Re-registering an existing membership is an idempotent `Ok`.

`Route.mailbox` becomes `members: Vec<MailboxId>` plus the set's `shared` marker. Unregistering removes one membership; the empty set drops the route; `UnregisterRoutesAll` strips the mailbox from every set (so a dropped component leaves every set it joined, ADR-0130's drop fan-out unchanged).

### 2. Selection: per-shard round-robin over live members

At dispatch each shard picks the next member by a shard-local round-robin cursor, skipping members that fail the same registry liveness validation a single routed mailbox gets today; a set with no live member answers `503`, the unchanged surface. Round-robin over connection-affinity because buffered requests carry no per-connection ordering contract (keep-alive serves one request at a time per connection either way) and a cursor balances uneven per-connection rates; per-shard over global because a shared cursor would be a new cross-shard contention point for marginal balance.

### 3. Sessions pin their member

A session-establishing dispatch — a response-stream open (ADR-0128) or a websocket handshake (ADR-0129) — selects a member once, and the session's subsequent legs ride the existing handler carry (`PendingRequest.handler`, stream/websocket state): credit grants, inbound chunks, and teardown all address the member that opened the session. No new machinery; the pinning is what those tables already do.

## Consequences

- **Positive** — a path family scales across handler instances by having each instance register `shared: true`; the measured single-handler wall (~31k on the stress harness) lifts to the multi-instance number (57k in the 4-member probe) with no benchmark hacks.
- **Positive** — the accidental-claim guard survives intact: unrelated components still cannot split a path family by accident; every mixed or mismatched claim is a loud `Err`.
- **Negative** — a wire-shape change to two registration kinds (in-repo lockstep only).
- **Negative** — request → handler assignment becomes nondeterministic across members for buffered traffic; a handler that assumed it saw *every* request on its path (e.g. for in-actor counters) must not register shared. The field being opt-in is the mitigation.
- **Neutral** — selection policy is deliberately minimal (round-robin, live-skip). Weighted, least-loaded, or affinity policies would be new fields on the registration kinds if ever measured to matter.

## Alternatives considered

- **Connection-affine selection.** No ordering promise exists to keep; balances worse under uneven connection rates; costs a hash per request for it.
- **A separate `register_route_member` kind family.** More kinds for the same information; the `shared` field is self-describing at the registration site and keeps the explicit/`_self` symmetry.
- **Silent joining (no opt-in).** Deletes ADR-0130's accidental-claim guard; a config typo becomes silent load-splitting between unrelated handlers.
- **Weighted / random selection.** No measured need; round-robin is deterministic under test.
