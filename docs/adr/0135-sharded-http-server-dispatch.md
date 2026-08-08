# ADR-0135: Sharded HTTP Server Dispatch

- **Status:** Accepted (shipped — sharded HTTP server dispatch in `crates/aether-http/src/server/shard/`)
- **Date:** 2026-07-05

Restructures the `aether.http.server` capability's request path for throughput. Amends **ADR-0108** (the single-actor dispatch topology of §5, the trust-cap enforcement seat of §6) and **ADR-0128** (which actor answers the reader's streaming decision in §4). The streaming, websocket, and routing wire contracts (ADR-0128/0129/0130/0132/0133) are unchanged — this ADR moves work between actors and threads; it does not change any kind, any handler-visible semantics, or any external mail surface.

## Context

A closed-loop stress harness (`spike/http-stress`, issue 2610) measured the cap's sustained throughput for a trivial keep-alive handler at ~8,600 req/s on an ~11-worker machine, flat from concurrency 8 through 256 while p50 latency grew linearly to 29 ms — the signature of a single serialization point clearing ~115 µs per request. The mail scheduler beneath sustains 1.6–5.6 M mails/s on the same machine; at 3–5 mails per request the mail layer alone could back 400–500 k req/s. The HTTP path reaches ~2 % of that.

The serialization point is the cap actor itself. `HttpServerCapability` is one actor, and per buffered keep-alive request it executes, in sequence on its single dispatch:

1. **The streaming decision round trip** (ADR-0128 §4): the reader posts `RequestHeadParsed` and parks; the cap resolves the route, reads the handler's accept-set, and signals `ReaderControl::Buffered` back — one scheduler wake, one full route resolution, and an OS park/wake pair, spent answering "buffer or stream?".
2. **Dispatch**: the reader posts `RequestParsed`; the cap resolves the route a second time, looks the fallback handler up by name, allocates the peer-address string, encodes the request kind, sends it, subscribes settlement, and records the in-flight entry.
3. **Reply interception**: the `#[fallback]` matches the correlation, decodes the response, renders the HTTP bytes, and performs the response `write_all` + `flush` — a socket syscall on the dispatch critical path — then signals `Resume` (a second park/wake pair).

Each of the reader's posts also fires its own `HttpInboundReady` wake mail, so a request costs ~5 mails, two of them wakes. Per-connection reader threads already parallelize the parse, and the handler already runs on the worker pool; every request is then funneled through this one actor, so the multi-core scheduler is never fed in parallel. The `native` and `wasm` benchmark handlers track within 1 %, confirming the cost sits in this plumbing, not the handler.

Two structural facts shape the fix. First, everything requests contend on is connection-affine: the write half, the control channel, the in-flight entry, the response stream, and the websocket state all belong to one connection, and HTTP/1.1 keep-alive serves one request at a time per connection — so partitioning by connection partitions the whole state machine with no cross-shard coordination. Second, the substrate already has the parallel primitive: instanced native actors (ADR-0079, `ctx.spawn_child`), scheduled independently on the work-stealing pool.

## Decision

The cap splits into a **supervisor** (the existing `HttpServerCapability` identity) and **N dispatch shards** (a new `Instanced` native actor), with connections assigned to shards at accept. Three further changes shrink each shard's per-request work: the reader makes the request-path decision itself (the **reader fast path**), the reader writes the buffered response bytes (**reader-written responses**), and the sidecar wake mail coalesces. In detail:

### 1. Supervisor + N connection-affine dispatch shards

The supervisor keeps the listener, the accept thread, the route table, the external mail surface (`register_route` / `unregister_route` / `_self` / `_all`), and the boot handle. On its first sidecar wake it spawns `dispatch_shards` shard children (config knob, default = the pool's worker count), each carrying its own inbound mpsc + wake-mail sink — the same sidecar shape the cap already uses, replicated per shard.

Accepted connections flow accept thread → supervisor (as today) → shard: the supervisor assigns each new connection round-robin and posts the socket to the shard's sink. From that point the connection lives entirely in its shard: the shard spawns the reader (whose `WakeSink` targets the shard), owns the `ConnState`, the in-flight table, the response-stream and websocket tables, dispatches requests, and intercepts replies — today's state machine, unchanged, over a 1/N slice of the connections. Handler replies and stream-phase mails arrive at the shard because the shard is the dispatch source (ADR-0133's handles and the reply correlation both follow the dispatching mailbox). Settlement subscriptions name the shard's mailbox. Per-request work never touches the supervisor.

`max_connections` stays one global ceiling: a shared atomic live count, incremented by the supervisor at assignment (it writes the `503` refusal itself past the ceiling) and decremented by the owning shard on connection close.

The route table becomes shared state — `Arc<RwLock<…>>`, written only by the supervisor's registration handlers, read by shards and readers. Registration mutates it in place, so a route change is visible to the next request everywhere at once; the granularity of "when does a route change take effect" moves from next-drained-event to next-request-head, which no contract promises otherwise.

### 2. Reader fast path: the request-path decision moves to the reader

The reader already holds `Arc<Mailer>`; with the shared route table it can resolve the route, validate the mailbox, and read the handler's accept-set itself — the registries are `RwLock`-backed and thread-safe. The ADR-0128 §4 head round trip collapses for the common case:

- **Buffered request to a live handler** (the fast path): the reader decides locally, reads the body, encodes the `HttpServerRequest` payload itself (the peer string is captured once at accept), and posts a single `RequestParsed` event carrying the encoded payload plus the resolved `(handler, kind, method, keep_alive)`. The shard's dispatch is then: send, subscribe settlement, record in-flight. One wake, zero route resolutions, zero encodes on the shard.
- **Rejects** (`400`/`411`/`413`/`431`/`501`/`503`): the reader writes the canned response on its own full-duplex clone — the same socket it already writes `100 Continue` on, and no response can be in flight at that point — and posts `ReaderClosed` so the shard reaps the table entry. Reject traffic never wakes the shard for a write.
- **Streaming bodies and websocket upgrades** keep today's `RequestHeadParsed` round trip verbatim: the shard still seats the stream/upgrade decision, mints stream ids, and seeds credit (ADR-0128/0129 unchanged). These are the rare, session-establishing cases; the round trip is the right shape for them.

The shard trusts the reader's validation; a handler that dies in the microseconds between the reader's check and the send is caught by the existing settlement `502` net, the same net that catches it today between dispatch and delivery.

### 3. Reader-written responses

Reply interception on the shard renders the response bytes but no longer writes them. It sends `ReaderControl::Respond { bytes, resume }` down the connection's control channel — the reader is already parked there awaiting exactly this deadline — removes the in-flight entry, and moves on. The reader writes the bytes: on `resume` it loops into the next request (keep-alive); otherwise it exits and posts `ReaderClosed`, which runs the shard's normal close path. `Resume` without bytes remains for the stream-finish path (ADR-0128), whose head and body already go out on the writer thread.

This removes the response syscall from the shard's dispatch and, as a corollary, removes the last head-of-line blocking hazard: a peer that stalls its receive window can now only stall its own reader thread, never the dispatch actor. The shard's `write_half` remains for the paths that genuinely write off-cycle (the websocket `101`, canned statuses on connections whose reader is mid-body).

### 4. Wake coalescing

`WakeSink::post` fires one `HttpInboundReady` per event today, and the drain loop already tolerates spurious wakes. Each sink gains a dirty flag (an atomic): a post only fires the wake mail if the flag was clear, and the drain handler clears it before draining. A burst of events costs one wake instead of one per event, cutting steady-state mail volume per request roughly in half and keeping the aggregate mail rate at 500 k req/s (~3 mails/request ≈ 1.5 M mails/s) inside the scheduler's measured envelope.

### Cost accounting

Per buffered keep-alive request, the serialized shard work becomes: one mpsc recv, one `send_envelope_detached`, one settlement subscription, one in-flight insert; on reply, one correlation lookup, one decode+render, one control send, one in-flight remove. Estimated 5–15 µs against today's ~115 µs, across N independent shards — 60–200 k req/s per shard, 400 k+ aggregate at 8 shards, with the scheduler and reader threads absorbing the parallel share. The stress spike re-measures after each stage; the estimates decide nothing, the harness does.

## Consequences

- **Positive — the dispatch ceiling scales with cores.** Throughput for trivial-handler workloads is bounded by shard count × per-shard cost instead of one actor's 115 µs; both factors are now improvable independently and measurable per stage.
- **Positive — stall isolation.** A slow or stalled peer blocks its own reader thread only, never the server's dispatch.
- **Positive — each stage lands separately.** Sharding, the fast path, reader-written responses, and wake coalescing are four independently shippable, independently measurable changes over one architecture.
- **Negative — more moving state.** N shard sidecars (mpsc + wake sink + tables) instead of one; the route table and live-connection count become shared (lock/atomic) state below the mail layer, alongside the existing sidecar mpsc precedent. Reader threads take read locks on the route table per request — a contention point to watch in measurement, with a snapshot-per-reader fallback if it shows.
- **Negative — the reply's next actor ceiling is the handler.** All requests still dispatch to one handler mailbox; at ~2 µs/request budget the single handler actor becomes the binding ceiling near a few hundred k req/s. Spreading one route across a set of handler mailboxes is deliberately out of this ADR — it is a routing feature (an ADR-0130 amendment) tracked separately in the issue's plan.
- **Neutral — introspection.** `actor_cost` / `actor_logs` address shards individually (instanced mailboxes under the supervisor's lineage), which is finer-grained than today's single-mailbox view.

## Alternatives considered

- **Per-request sharding (round-robin requests, not connections).** HTTP/1.1 responses on one connection must be written in request order, and keep-alive serves one request at a time per connection anyway — connection affinity gets the same parallelism with none of the reordering machinery.
- **Reader-direct dispatch (readers send to handlers, shards keep only reply state).** Requires the in-flight table to be written by reader threads and read by the shard — shared mutable actor state, against the single-threaded dispatch model. Deferred: if per-shard cost still binds after this ADR's stages, that is the next seam, and it needs its own design for the correlation table.
- **`SO_REUSEPORT` multi-listener accept sharding.** Accepts are amortized away by keep-alive in the measured workloads; the single accept thread was never the bottleneck.
- **Async-runtime rewrite of the cap's I/O.** Replaces the substrate's thread + mail architecture wholesale for one capability; the measured gap is dispatch serialization, not thread count.
- **Head-decision cache on the single actor (no shards).** Cutting the 115 µs to ~15 µs on one actor caps at ~60–70 k req/s — under the issue's own 100 k floor, and it forecloses none of this ADR's work, so it is strictly dominated by doing both.
