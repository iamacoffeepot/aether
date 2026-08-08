# ADR-0158: Per-sender bounded async egress dispatch

- **Status:** Accepted (shipped — bounded per-sender async egress in `crates/aether-http/src/client/egress.rs`)
- **Date:** 2026-07-21

## Context

The `aether.http` client runs one fetch at a time on the cap's dispatch thread and every other fetch waits behind it. `on_fetch` (`crates/aether-http/src/client/runtime.rs:118`, a `#[handler::single]`) runs the whole `ureq` call — DNS, connect, TLS, request, response read, and the hand-rolled redirect follow loop — inline before it returns the `FetchResult`. A slow remote stalls every subsequent HTTP request for up to the full timeout. ADR-0043 recorded this as a known consequence ("head-of-line blocking on the sink thread … network latency makes the pain more acute") and named the fix as deferred follow-up work: "multi-threaded sink dispatch … likely a worker pool scoped per sink with a small depth (2–4 threads)" (`docs/adr/0043-substrate-http-egress-net-sink.md:154`). **This ADR is that deferred follow-up.**

Two pieces that did not exist when ADR-0043 shipped now make the conversion a composition rather than a new mechanism:

- **ADR-0093's hold-until-resolve dispatch.** `NativeCtx::dispatch_blocking` (`crates/aether-substrate/src/actor/native/ctx.rs:370`) offloads a blocking closure onto an umbrella-aware worker, eagerly acquires a `SettlementHold` on the current chain root before the handler returns, and routes the worker's output back as `TaskDone<Output>` to a `#[handler(task)]` completion handler that re-replies and drops the hold. Its cap-level companion, `TaskQueue` (`crates/aether-substrate/src/actor/native/task_queue.rs`), adds the one thing the primitive deliberately leaves to the cap: a concurrency bound plus a pending queue. Over the bound, `TaskQueue::submit` captures the chain context immediately — `ctx.acquire_settlement_hold()` on the current root plus `ctx.reply_target()` — and buffers a thunk that replays the work through `ctx.dispatch_blocking_resumed(hold, reply_to, work)` when a slot frees, so a queued request keeps its own chain held from accept through its eventual re-reply.

- **The per-sender keying precedent.** `aether.audio` keys its per-sender gain trim on the envelope's reply target: `sender_gains: HashMap<MailboxId, f32>` (`crates/aether-audio/src/runtime/synth.rs:57`), inserted and looked up under `sender_mailbox_id(ctx.reply_target())` (`crates/aether-audio/src/runtime/mod.rs:91`), which reads the sender's `MailboxId` from a `SourceAddr::EngineMailbox` and collapses every other source to `MailboxId(0)`. The keying that isolates one sender's audio state isolates one sender's egress budget the same way.

The composition of the two — a per-sender `(in_flight, pending)` state over the `TaskQueue` bound-and-hold machinery — is the substance of this ADR.

**Why the bound belongs at the cap edge, keyed per sender.** The substrate is a general application host, so the fetch stream is many concurrent senders rather than one serial caller, and a single noisy sender must not consume the whole egress budget. A per-sender bound gives that fairness. The keying choice is also load-bearing for the wasm HTTP provider arc (ADR-0159): a wasm guest cannot express a `SettlementHold`. ADR-0139 §4 records that guest-side request contexts "hold nothing open engine-side (no settlement hold)" — precisely so their eviction is memory hygiene rather than a correctness event. A guest-side pending queue therefore reintroduces the premature-settlement window ADR-0093 closes: `send_mail_traced` would observe the request's chain settled before the queued fetch's reply dispatches. Bounding at the cap edge keeps every queued request engine-side under the cap's own `SettlementHold`s, settlement-correct by construction, and leaves the guest holding nothing.

The fetch path itself is contention-free under parallel dispatch. `UreqHttpAdapter` holds a cheaply-cloneable `ureq::Agent` behind an `Arc<dyn HttpAdapter>` and touches no mutable cap state mid-request; the adapter doc already notes it "would parallelise cleanly behind a multi-thread dispatcher later" (`crates/aether-http/src/client/runtime.rs:136`). The redirect re-validation (`classify_redirect`, issue #3463) is pure and reads only the adapter's immutable allowlist, so it stays correct under concurrency without change.

Nothing in the repository sends `aether.http.fetch` today — the only in-tree callers are the cap's own tests. There are no compatibility constraints and no migration.

## Decision

### 1. Fetch dispatches through the ADR-0093 hold-until-resolve primitive

`on_fetch` stops running the `ureq` call inline. It clones the `Arc<dyn HttpAdapter>` into a closure, submits that closure to a per-sender bounded queue held in cap state, and returns immediately. The worker runs the fetch off-thread, shapes the `FetchResult` from the adapter response, and a `#[handler(task)]` completion handler resolves it back to the originating caller through the carried reply target, then frees the slot. The adapter, its redirect loop, and its validation are unchanged — only the thread the fetch runs on moves.

### 2. The bound is per sender, keyed by the sender's `MailboxId`

Cap state holds a per-sender table, `MailboxId -> PerSenderEgress { in_flight, pending }`, composed over the ADR-0093 `TaskQueue` bound-and-hold machinery. The key is `sender_mailbox_id(ctx.reply_target())`, the same helper and the same `SourceAddr::EngineMailbox` read `aether.audio` uses. A component sender keys on its own engine mailbox id; sessions and substrate-internal pushes collapse to `MailboxId(0)` and share one bucket, so the per-sender bound also throttles the aggregate of MCP-session-driven fetches — acceptable, because those callers are the harness rather than untrusted guests, and the global ceiling below still protects the host.

A queued fetch holds its chain from accept, exactly as `TaskQueue::submit` already does: `acquire_settlement_hold()` on the current root plus `reply_target()` are captured at accept time and replayed through `dispatch_blocking_resumed` when a slot frees.

### 3. A global ceiling composes with the per-sender bound

Both bounds apply, and a fetch dispatches only when it clears both: its sender is under the per-sender budget **and** the total in-flight count across all senders is under the global ceiling. Otherwise it queues, holding its chain. The two bounds answer different questions and neither subsumes the other — the per-sender bound is **fairness** (one sender cannot spend more than its share, so it cannot starve its peers), and the global ceiling is **protection** (`N` senders times a per-sender budget is otherwise an unbounded native thread and socket count, so a fan-out of distinct senders cannot exhaust the host). A per-sender bound alone leaves the host unprotected against many senders; a global ceiling alone lets one sender monopolize it. Composing them is the recommended and decided shape.

Draining preserves per-sender order and stays fair at the ceiling: within one sender the pending queue is first-in-first-out, and when a freed global slot can admit work, admission rotates across the senders that have pending requests rather than always favoring the sender whose completion freed the slot. The rotation keeps a busy sender from recapturing every freed global slot ahead of a waiting peer.

### 4. Default budgets

- **Per-sender budget: 4** concurrent fetches. This matches `TaskQueue::DEFAULT_MAX_IN_FLIGHT` (`task_queue.rs:39`) and lands inside the "small depth (2–4 threads)" ADR-0043 forecast.
- **Global ceiling: 32** concurrent fetches across all senders — eight senders at full per-sender budget before the ceiling engages, which bounds the worst-case native worker-thread and socket count while leaving generous headroom for realistic fan-out.

Both are operator knobs on `HttpConfig` through the ADR-0090 derive-`Config` path (`crates/aether-http/src/client/config.rs`), alongside the existing `AETHER_HTTP_*` fields: `AETHER_HTTP_MAX_IN_FLIGHT_PER_SENDER` and `AETHER_HTTP_MAX_IN_FLIGHT_TOTAL`. A per-sender value of 0 clamps to 1, following `TaskQueue::new`'s existing clamp, so a zero budget cannot wedge a sender's queue forever.

### 5. Per-sender entries reclaim on idle; queued holds cannot leak

A per-sender entry is created lazily on a sender's first submit and removed the moment it drains fully idle — `in_flight == 0` and `pending` empty. A `SettlementHold` exists only while a request is in flight or buffered pending a slot, so an entry that holds anything is never idle, and idle-reclamation therefore can never drop a hold on the floor. The table's size is bounded by the count of senders with live or queued work, which the global ceiling and the per-sender budgets already bound, rather than by cumulative request volume.

This entry lifecycle is deliberately unlike ADR-0139 §4's guest-side request-context table, which evicts drop-oldest under a fixed capacity. That table can discard an entry safely because a context "holds nothing open engine-side" — its worst case is the actor's own heap. A per-sender egress entry holds engine-side `SettlementHold`s, so dropping a non-idle one would either leak the hold (its chain never settles) or release it early (its chain settles before the queued fetch replies — the premature-settlement bug ADR-0093 exists to prevent). Idle-reclamation, never drop-oldest, is the eviction policy the hold semantics require.

### 6. A caller-minted `request_id` on `Fetch`, echoed on `FetchResult`

`Fetch` gains a caller-minted `request_id: u64`, and both `FetchResult::Ok` and `FetchResult::Err` echo it, mirroring `MessagesSend.request_id` / `MessagesSendResult` in `aether-anthropic` (`crates/aether-anthropic/src/kinds.rs:68`) field-for-field. Under concurrency the existing `url` echo is an ambiguous correlator: a caller that fires two requests to the same URL — a retry, or a non-idempotent `POST` to one endpoint — receives two replies indistinguishable by `url`. A caller-minted id in the payload disambiguates them, and it does so in the reply body itself, which is what a cross-wire or MCP-driven caller reads — the `send_mail` tool projects the reply payload as JSON and does not surface the ADR-0139 envelope correlation. This is the same reason the content-gen caps carry `request_id` in the payload even though ADR-0139 envelope correlation exists: those caps are MCP-driven, and the in-payload id is the correlator their callers can see. The SDK-side envelope correlation (`ctx.in_reply_to()`, ADR-0139) remains available and unaffected for native and guest callers that prefer it.

`url` stays on both reply arms as an informational echo for log and MCP-caller readability, demoted from correlation key the same way ADR-0139 §5 demotes the `fs` cap's echoed `namespace` / `path`. Because there are zero in-repo `Fetch` senders, adding a required field is a free wire change carrying no migration.

### 7. Non-goals

- **Guest-reachable `dispatch_blocking_p32`.** ADR-0093 §7 defers the FFI superset that would let a wasm guest drive the same hold-until-resolve dispatch. Making one bounded-dispatch system serve both native caps and wasm guests is a desirable follow-up — it would let a guest express engine-held pending work directly — but it is out of this arc's scope. This ADR bounds at the native cap edge, which is exactly what keeps the guest holding nothing (§ Context) and is sufficient for the wasm HTTP provider (ADR-0159).
- **Egress streaming.** Chunked request and response bodies via byte handles stay parked where ADR-0043 left them ("streaming request/response bodies via byte handles … parked, not committed"). This ADR changes the dispatch model, not the buffered request-response body shape.

### 8. The load-bearing property for ADR-0159

Stated plainly for the wasm HTTP provider arc: **a guest must never queue pending egress work guest-side.** A guest cannot hold a `SettlementHold` open, so a guest-side pending queue makes the request's chain settle before the queued reply dispatches, reopening the premature-settlement window (ADR-0139 §4). Bounding per sender at the `aether.http` cap edge keeps every queued request engine-side under the cap's own holds, so it is settlement-correct by construction. ADR-0159's wasm provider therefore bounds at this edge and adds no guest-side scheduling primitive.

## Consequences

### Positive

- **Head-of-line blocking is gone.** A slow remote occupies one worker slot for its own sender instead of stalling the cap's whole dispatch thread. Concurrent fetches from independent senders proceed independently.
- **The mechanism is reused, not invented.** The hold lifecycle, the eager-acquire-before-return that closes the premature-settlement window, the pending-with-held-chain buffering, and the completion routing all come from ADR-0093's `TaskQueue`. This ADR adds a per-sender key over an existing bound and a global counter — a small, testable delta over battle-tested machinery.
- **Fairness and protection are both explicit.** No single sender starves its peers, and no fan-out of senders exhausts the host's thread and socket budget. The two bounds are separately tunable operator knobs.
- **Concurrent correlation is unambiguous.** The caller-minted `request_id` distinguishes concurrent same-URL replies in the reply payload, for native, guest, and MCP-driven callers alike.
- **The wasm provider arc is unblocked correctly.** Bounding at the edge gives ADR-0159 a settlement-correct foundation with no guest-side scheduling primitive to design or prove.

### Negative

- **Cap state and a completion handler are new surface.** The cap gains a per-sender table plus a `#[handler(task)]` completion handler, where today it has a single synchronous handler. The per-sender-with-global composition is more moving parts than one flat queue, and the drain-fairness rotation is the subtlest piece.
- **The resolve-or-leak invariant is a runtime guard, not a compile-time proof.** Inherited from ADR-0093: a dropped `TaskDone` `debug_assert`s rather than failing to compile. Idle-reclamation narrows the per-sender leak surface but cannot statically prove every hold is eventually resolved.
- **Sessions share the `MailboxId(0)` bucket.** All non-component senders — including concurrent MCP-driven fetches — contend for one per-sender budget. This is acceptable for a harness-facing caller and the global ceiling still bounds the host, but it is coarser isolation than a component sender gets.
- **Worst-case native thread count rises.** Up to the global ceiling of blocking workers can run at once, against one dispatch thread today. This is the intended cost of removing head-of-line blocking; the ceiling bounds it.

### Neutral / forward

- **No chassis-coverage change.** Desktop and headless keep the full client; the hub keeps none. The bounding is internal to the cap.
- **The `request_id` addition is a two-field kind change** in `aether-http`, with no schema churn elsewhere and no migration (zero senders).
- **Generalizes if a second egress cap appears.** The per-sender-over-`TaskQueue` composition is not HTTP-specific; a future outbound cap with the same many-senders shape can reuse it.
- **Status stays Proposed** until the implementation lands, per the project's ADR-accepted-after-implementation convention.

## Alternatives considered

- **A single global worker pool with no per-sender key** (the literal ADR-0043 forecast — "a worker pool scoped per sink with a small depth"). Rejected: it fixes head-of-line blocking but gives no fairness, so one sender saturating the pool starves every other sender, and it offers the wasm arc no per-sender isolation to build on. The per-sender key is cheap over the same machinery and is what ADR-0159 needs.
- **Per-sender bound with no global ceiling.** Rejected: fair but unprotected — a fan-out of `N` distinct senders each at its per-sender budget is an unbounded native thread and socket count. The global ceiling is the host's protection and composes at negligible cost.
- **Global ceiling with no per-sender bound.** Rejected: protected but unfair — one sender can consume the entire ceiling and starve its peers. Fairness is the reason to key per sender at all.
- **Drop-oldest eviction on the per-sender table** (mirroring ADR-0139 §4's context table). Rejected: a per-sender entry holds engine-side `SettlementHold`s, so dropping a non-idle entry leaks or prematurely releases a hold. Idle-reclamation is the only eviction the hold semantics permit.
- **Guest-side pending queue** (bound the fetch stream inside the wasm component). Rejected on the load-bearing property in § Context: a guest cannot hold a chain open, so a guest-side queue reopens the premature-settlement window ADR-0093 closes. Bounding at the edge is the settlement-correct placement.
- **Envelope correlation alone, no payload `request_id`** (rely on ADR-0139's `ctx.in_reply_to()`). Rejected as the sole correlator: envelope correlation is invisible to a cross-wire or MCP-driven caller reading the projected reply payload, which is where concurrent same-URL replies must be told apart. The payload id mirrors the established `MessagesSend` precedent; the envelope path stays available for SDK callers that prefer it.
- **`spawn_inherit` / `spawn_detached` instead of hold-until-resolve.** Rejected for the same reasons ADR-0093 rejected them for content-gen: `spawn_inherit`'s hold dies with the worker thread and reopens the premature-settlement window, and `spawn_detached` holds nothing at all. Reply-in-a-later-turn is exactly the shape `dispatch_blocking` exists to serve.
