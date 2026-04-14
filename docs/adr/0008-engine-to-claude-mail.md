# ADR-0008: Engine-to-Claude mail (observation path)

- **Status:** Accepted
- **Date:** 2026-04-14

## Context

ADR-0006 shipped a uni-directional hub: Claude → hub → engine. It was a deliberate V0 scope cut, with this note on the wall:

> Users who want to "see the game Claude is playing" will hit this wall immediately; it's a deliberate V0 scope cut, not a permanent limit.

That wall is now the next thing in the way. A Claude driving the engine can send input but cannot receive anything back from the engine beyond "frame delivered by the hub." The harness is credible as a remote control and not yet as a harness — Claude-as-player cannot observe, Claude-as-engineer cannot assert, Claude-as-designer cannot iterate. Every interesting use case needs an engine → Claude channel.

Forces at play:

- **The mail system is the answer.** ADR-0002's "Claude is just another mail sender" has a dual: Claude is just another mail *recipient*. Engine components should send observation mail the same way they send any other mail — a kind, a payload, a recipient name. No parallel "events" or "notifications" system.
- **Sessions are ephemeral.** A Claude instance is an MCP session (ADR-0006). Sessions come and go as Claude Code connects and disconnects. They don't fit the registry-at-init model kinds use (ADR-0005) and they don't fit the engine-UUID model (ADR-0006) either — there's no long-lived identity to key off.
- **Components shouldn't know about sessions.** A WASM component writing observation mail should not deal with MCP session ids, session lifecycles, or which Claudes are connected. Session management is a transport concern that belongs at the hub. Components think in mail recipients and kinds.
- **Addressing is usually targeted, sometimes broadcast.** "Reply to whoever sent me this command" is the common case. "Tell all Claudes attached about a world event" is the less common but real case. A design that only does broadcast forces every reply into a fan-out; a design that only does targeted can't express the world-event case.
- **The wire is already bidirectional.** ADR-0006's heartbeats flow both ways. Adding engine-originated mail frames is a new variant on an existing channel, not new infrastructure.

## Decision

Engines send mail to Claudes through the hub, using the existing mail abstraction. Sessions are hidden from components — the component API exposes a well-known recipient name for broadcast and an opaque sender handle for reply-to-sender. The hub translates between the component's abstraction and the session-level routing it already owns.

### 1. Component-facing API

Two ways for a component to address a Claude:

- **Broadcast:** send mail to the well-known recipient name `"hub.claude.broadcast"`. Fan-out to all attached sessions is the hub's problem; the component just sends. Zero sessions attached is not an error — the mail is dropped with a status, same as any other undeliverable address.
- **Reply to sender:** mail received *from* the hub carries an opaque sender context (a token the component can't introspect). Passing that token back as the recipient of an outbound mail routes the reply to the originating session. The token is valid for as long as the hub says it is — if the session disconnected between receipt and reply, the mail is dropped with an undeliverable status.

Components never see session ids, session lifecycles, or "is this Claude still here?" questions. If a session concept is needed later (e.g., "is this the same Claude that sent me that earlier message?"), it gets added as an opaque equality token, not a raw id.

### 2. Wire protocol — engine → hub

Extend `EngineToHub` with a mail frame:

```rust
pub enum EngineToHub {
    Hello { .. },
    Heartbeat,
    Mail {
        address: ClaudeAddress,
        kind_name: String,
        payload: Vec<u8>,
    },
    // ...
}

pub enum ClaudeAddress {
    /// Reply to a specific session. The token was handed to the engine
    /// on a prior inbound mail frame and is opaque to the engine.
    Session(SessionToken),
    /// Fan-out to every currently attached session.
    Broadcast,
}
```

`SessionToken` is a hub-minted value (a UUID, a signed id, whichever the hub prefers). The engine treats it as bytes. The hub validates it on receipt and rejects unknown/expired tokens with a per-mail undeliverable status (same shape as `send_mail`'s per-item status from ADR-0007).

### 3. Wire protocol — hub → engine

The existing `HubToEngine::Mail` grows a `sender` field:

```rust
pub struct MailFrame {
    pub recipient_name: String,
    pub kind_name: String,
    pub payload: Vec<u8>,
    pub sender: SessionToken, // NEW — the hub's routing handle for the origin session
}
```

The substrate carries the sender through to the component when mail is dispatched. If the component chooses to reply, it addresses the reply with that token. The engine never synthesizes tokens — it only echoes ones the hub gave it.

### 4. Claude-facing surface

The hub delivers engine-originated mail to Claude sessions via an MCP resource or notification channel — detail deferred to implementation (rmcp gives us both; the right choice depends on delivery semantics we want, e.g., at-most-once vs queued). The key point is that on the MCP side, each session sees only mail addressed to *it* (either by session token from a reply, or by broadcast). Fan-out happens hub-side.

A `receive_mail(since_cursor?)` tool (or equivalent MCP resource) gives Claude a way to pull observation mail. Agent-polled for V0; push-style notifications are additive later.

### 5. Lifecycle and failure modes

- **Session disconnect.** A `SessionToken` becomes invalid when the session disconnects. Subsequent engine replies to that token are dropped with `sessionGone` status. The engine-side substrate does not learn about individual session disconnects — it only sees per-mail delivery results.
- **Broadcast to zero sessions.** Delivered to zero recipients, status `noRecipients`. Not an error on the component side.
- **Hub restart.** All tokens become invalid. Engines reconnect (ADR-0006 mechanism); any tokens they still hold are now stale and will fail on next reply attempt. Components should not persist tokens across mail receipts in expectation of long-term validity — the token's lifetime is bounded by the session's.
- **Large payloads.** Same framing rules as ADR-0006's existing mail. No new size policy here.

## Consequences

### Positive

- **Closes the observation wall.** Screenshots, state dumps, "Claude, here's what happened" — all become ordinary mail with a kind and a payload. No separate events API, no schema for "observations," no new abstractions.
- **Components stay simple.** The component API is still "send mail to a name" and "receive mail with a sender." Sessions are a transport detail. A component written before this ADR can start emitting observation mail by adding a single send to `"hub.claude.broadcast"`.
- **Reply-to-sender is symmetric with how agents already think.** A Claude sends a command; the engine's response comes back to that Claude. The token flow makes this routing work without the component ever knowing who the Claude is. The common case is free; the uncommon case (broadcast) is one well-known name away.
- **Direction-asymmetric addressing is honest about what's stable.** Hub → engine uses `(engine_id, recipient_name)` because engines have stable ids and components have stable names. Engine → hub uses `ClaudeAddress` because sessions don't have stable ids and broadcast is a first-class intent. Forcing symmetry here would have meant giving sessions fake long-lived identities.
- **Schema-driven encoding (ADR-0007) applies in both directions.** Engine-originated observation kinds declared at handshake can be rendered as structured params on the Claude side — Claude reads `{entity_id: 7, pos: {x: 1.0, y: 2.0}}` rather than raw bytes. No new encoder needed; the descriptor is the descriptor.

### Negative

- **Hub learns yet another responsibility.** It now maintains session-token state, translates between component-level abstractions (`Broadcast`, reply-via-token) and session-level routing, and handles fan-out. Dumb-forwarder it is not — this is the third chunk of logic after the kind registry (ADR-0007) and the routing table (ADR-0006).
- **Tokens are a capability-ish concept.** A component holding a `SessionToken` can send to that session until it disconnects. In V0 this is harmless (localhost, trusted engine). In any multi-tenant future, token validity/scoping becomes a real concern. Worth naming now so it's not a surprise later.
- **Claude-side delivery is a polling vs push decision we haven't made.** Deferred to implementation, but deferred is not decided. MCP resources with notifications is the rmcp-native path; a `receive_mail` tool with a cursor is simpler. The choice affects latency and the shape of "which mails has this session already seen."
- **Second bidirectional channel on the engine wire.** Heartbeats were bidirectional but trivial. Mail in both directions means the substrate TCP reader and writer are now both carrying meaningful traffic, and backpressure on the engine → hub direction is real. V0 can ignore it; a busy observation stream will force the question.

### Neutral

- **Not all engine→Claude traffic is mail.** Screenshots, for example, might eventually want a different path (binary blobs, lower-priority queue). This ADR doesn't foreclose that — the mail path is the default, additional channels are additive.
- **Ack semantics stay weak.** Same as ADR-0006: the hub tells the engine "delivered to session" or "session gone," not "Claude read it and did something." Execution-level acks are a layer above.
- **Token format is the hub's choice.** Engines treat it as opaque bytes. Swapping UUIDs for signed tokens later is a hub-internal change.

## Alternatives considered

- **Session ids in the component API.** Components send to `Session(id)` directly and track which sessions are alive. Rejected: leaks transport state into components, forces every component to care about session lifecycle, and tangles the substrate with hub-internal identifiers. The abstraction cost is low and worth paying.
- **Broadcast-only, no reply-to-sender.** Every engine → Claude mail fan-outs to all attached sessions; Claude filters client-side. Rejected: reply-to-sender is the common case. Making it the uncommon case inverts the cost model (every reply becomes an N-way fan-out and a client-side filter) and invites races when multiple Claudes are driving.
- **Stable Claude identities across sessions.** Mint a long-lived id per Claude client and route mail to that id. Rejected for V0: sessions are what MCP gives us, adding a layer on top is premature without a concrete need. Stateful cross-session identity can be a follow-on.
- **Engine → hub mail uses `(engine_id, recipient_name)` too.** Symmetric wire format. Rejected: sessions don't have stable ids and there's no meaningful "recipient name" on a Claude — the session *is* the address. Forcing symmetry loses information.
- **Observation as a parallel non-mail channel.** A dedicated "events" frame type alongside mail, with its own kind registry. Rejected: doubles the concept count, forks the schema-driven encoding work from ADR-0007, and re-litigates "Claude is just another mail peer." Mail already has the shape we need.
- **Pub-sub / topic subscriptions for broadcast.** Claudes subscribe to topics; engine publishes to topics; hub routes. Rejected at V0: `"hub.claude.broadcast"` is a degenerate single-topic pub-sub and covers the current need. If per-component or per-kind topics become real, the well-known name scales into a namespace (`hub.claude.broadcast/<topic>`) without changing the wire.

## Follow-up work

- **`EngineToHub::Mail` + `SessionToken`** in `aether-hub-protocol`. Postcard-framed, same wire as ADR-0006.
- **`sender: SessionToken` on `HubToEngine::Mail`** — breaking change to the existing mail frame, but ADR-0006 is V0 and has no external consumers.
- **Hub session routing** — token mint/validate, fan-out for `Broadcast`, undeliverable statuses for expired tokens.
- **Component-facing reply primitive** — substrate carries `sender` through the mail dispatch path to components, and accepts it as a recipient on outbound sends.
- **`"hub.claude.broadcast"` well-known name** — reserved recipient, rejected for components that try to claim it locally.
- **MCP delivery surface** — `receive_mail` tool or MCP resource with notifications. Decide based on what rmcp makes cheap.
- **Parked, not committed:** stable cross-session Claude identity, per-topic pub-sub, separate observation channels for large blobs (screenshots), at-least-once delivery semantics, token scoping / capability model. Each is additive and covered by the baseline.
