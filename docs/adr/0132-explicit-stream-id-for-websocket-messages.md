# ADR-0132: Explicit Stream Id for Websocket Messages

- **Status:** Accepted (shipped — explicit `stream_id` on websocket messages in `crates/aether-http/src/server/runtime/websocket.rs`)
- **Date:** 2026-07-04

Amends **ADR-0129** (HTTP server websocket upgrade): replaces its causal-chain-root routing for data-phase websocket mail with an explicit `stream_id` carried on the wire, the correlation posture **ADR-0128** established for mid-session mail on short chains.

## Context

ADR-0129 upgraded a connection into a bidirectional message mode but left the data-phase kinds without a connection identity. Its §Decision states: "An outbound `WebSocketMessage` carries no connection id: it rides the inbound message's causal chain … and the cap routes it to the originating connection by that root." Concretely, the cap dispatches each inbound message on a fresh root, records `root correlation_id → ConnId` in a per-message `ws_message_conn` table, subscribes to settlement to reclaim the entry, and the outbound handlers (`on_websocket_message` / `on_websocket_close`) look the sender's chain root up in that table.

That mechanism forecloses the uses the same ADR names as motivation:

1. **Server push is impossible.** An outbound message routes only while its chain root is a live inbound message's root. A handler sending from a `Tick`, a peer actor's mail, or any other chain misses the table and the message is silently dropped — `on_websocket_message` no-ops on an unknown root. Chat (pushing user A's message to user B's socket) and live telemetry (pushing on a timer) cannot be expressed. The only workaround is pinning an inbound chain open, which is exactly the connection-long chain ADR-0129 §Context rejects for trace-ring growth.
2. **Inbound messages carry no connection identity.** `WebSocketMessage` is `{ binary, data }`, so a handler serving several connections cannot tell their messages apart or keep per-connection state.
3. **External callers are foreclosed.** The routing key never appears on the wire, so an external session (MCP `send_mail`) can never send to a websocket peer. ADR-0130 solved the same reachability gap for route registration with explicit-`mailbox` kind variants.
4. **Per-message bookkeeping.** Every inbound message costs a table insert, a settlement-registry subscription, and a `Settled` mail back to the cap, purely to garbage-collect a routing entry. Connection routing is per-connection state being paid for per-message.

ADR-0128 faced the same correlation problem — mid-stream mails on per-chunk chains, where envelope correlation cannot tie a mail back to its connection — and resolved it with an explicit `stream_id` field on every chunk / credit / terminator kind. ADR-0129 reused that credit protocol unchanged, so an upgraded connection already *has* a `stream_id`: the cap mints one at accept (`accept_websocket`), keys the writer-thread stream by it, and the handler already learns it from the initial `HttpStreamCredit` grant sent at upgrade. The id exists at both ends; the data-phase kinds just cannot carry it.

## Decision

Carry the connection's `stream_id` on every data-phase websocket kind, and route by it.

- `WebSocketMessage` gains `stream_id: u64`, both directions. Inbound, the cap stamps the connection's stream id on each dispatched message, so a handler can key per-connection state and knows where a message came from. Outbound, the handler names the target connection explicitly; the cap resolves it through the `streams` table exactly as `on_response_chunk` resolves an ADR-0128 body chunk.
- `WebSocketClose` gains `stream_id: u64`, both directions, with the same semantics.
- `WebSocketAccept` / decline are unchanged: the upgrade handshake is genuinely request/response, and the one-shot correlation-echoed reply remains its shape. The handler learns its `stream_id` from the initial credit grant immediately after accepting, before any peer traffic, so push is available from the moment the connection upgrades.
- The chain-root machinery is deleted: the `ws_message_conn` table, its per-message settlement subscription, the `Settled`-handler branch that reclaimed entries, and the connection-teardown sweep over the table. Outbound websocket mail no longer needs to ride any particular causal chain — a send from a `Tick` handler, a peer actor's chain, or an external `Call` routes identically.
- Sender trust matches ADR-0128's chunk posture: the cap routes by the payload's `stream_id` without validating the sender against the connection's handler mailbox. Stream ids are process-internal (allocated from the monotonic counter shared with response streams), the bind is loopback-default, and typed `#[handler]`s cannot read the envelope source today. Tightening this is a shared follow-on with ADR-0128, deferred deliberately (§Consequences).

The wire shape of two ADR-0129 kinds changes (one added `u64` field each). Pre-1.0, with the echo fixture as the only consumer, there is no compatibility burden; the fixture moves with the kinds in the same change.

## Consequences

- **Positive** — server-initiated push works: a handler can send to any connection it holds a `stream_id` for, at any time, from any chain. Multi-connection handlers can distinguish and address their peers. External sessions can drive a websocket peer over the wire. These restore the "chat, live telemetry" capability ADR-0129 claims.
- **Positive** — routing state becomes per-connection instead of per-message: the cap's existing `streams` table is the single routing surface, and the per-message settlement subscription disappears. Correlation is legible on the wire and in traces — the `stream_id` is visible in every envelope rather than implicit in chain ancestry.
- **Positive** — the correlation vocabulary is uniform across the server's long-lived surfaces: response streams, request streams, credit, and now websocket messages all carry an explicit `stream_id`.
- **Neutral / cost** — eight bytes per message on the wire; two kind schemas change shape; the handler must thread its `stream_id` where previously an in-chain echo needed no state (the fixture change is a two-line diff).
- **Negative** — an outbound message no longer proves it was caused by inbound traffic; a buggy handler can send to a stale `stream_id` and the cap must drop it (unknown-stream sends are ignored, matching `push_chunk`'s posture on a torn-down stream). This is the same failure class ADR-0128 accepted.
- **Follow-on** — sender validation for stream-addressed mail (websocket messages and ADR-0128 chunks alike) if the trust regime tightens beyond loopback; inbound websocket flow control remains the streamed-request-bodies composition ADR-0129 deferred, unaffected here.

## Alternatives considered

- **Keep chain-root routing, add a separate explicit-id push kind.** Rejected: two routing mechanisms for one message type, with the implicit one still silently dropping mis-chained sends — the defect this ADR removes would remain reachable.
- **Reply-based data phase.** Rejected: the reply handle is one-shot per inbound mail and websocket messages are not paired — one inbound message can produce zero, one, or many outbound messages plus a close. The same constraint made ADR-0128's chunks sends rather than replies.
- **A connection-long causal chain as the routing key.** Rejected by ADR-0129 already: it defeats settlement-aware trace-ring eviction over a multi-minute session (ADR-0106).
- **A dedicated `connection_id` distinct from `stream_id`.** Rejected: the cap already mints and hands out a per-connection `stream_id` through the credit protocol; a second id would name the same thing twice and force handlers to correlate two keys.
- **Validate outbound senders against the connection's handler now.** Deferred rather than rejected: typed `#[handler]`s cannot read the host-stamped envelope source today, ADR-0128's chunk path carries the same posture, and the v1 regime is loopback-default — the tightening is named as a shared follow-on instead of blocking this fix.
