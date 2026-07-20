# RPC wire and engine routing

Aether RPC carries engine control and mail between processes. It is an internal
typed transport used by the hub, child substrates, and MCP coordinator—not a
general application replacement for HTTP or TCP actors.

## Connection contract

Peers exchange a framed `WireFrame` vocabulary. Normal clients begin with
`Hello`/`HelloAck`, which exchange the wire version and a `PeerKind` containing
a substrate or client name/version (plus a substrate's shallow kind list).
`PeerKind` does not carry an `EngineId` or parent/child relationship. More
importantly, the current server records `hello_received` but does not consult it
before dispatching `Call`: a peer can send an ordinary call before `Hello` and
the server accepts it. Treat the handshake as the current client convention and
version exchange, not an admission or authentication gate. Network access to
the RPC listener is therefore its own trust boundary. Frames cover call/result
routing, mail envelopes, replies, heartbeats, and protocol errors.

The body encoding is Aether's canonical wire format. ADR-0118 amends older ADRs
and comments that refer to postcard. Length-prefix and maximum-frame checks
remain part of the stream boundary.

## Addressing

An RPC mail envelope carries an optional engine selection plus mailbox/kind
identity and correlation metadata. `engine = None` means the current RPC
server's local actor registry, not specifically the engine-control capability;
a hub fleet operation uses that form with the `aether.engine` mailbox.
Per-engine operations use `Some(id)` and route through the hub's matching
`EngineProxy`.

```text
engine = none     → supplied mailbox in this RPC server's local registry
engine = some(id) → hub EngineServer → proxy for that child → child registry
```

Using `Some(id)` prevents a child mailbox from accidentally resolving in the
hub registry. It also makes an unknown/dead engine distinguishable from an
unknown actor inside a live engine.

## Heartbeats and failure

The proxy/server relationship uses ping/pong heartbeats and connection state.
Missed heartbeats can evict an engine; clean termination, crash, eviction, and
spawn failure are retained as different recently-dead reasons.

RPC failure is not proof that the child never performed an operation. If a
connection fails after dispatch, re-read fleet/component state before retrying
a consequential call such as spawn, upload, or replace.

## Schema and reply projection

RPC transports bytes and descriptors; `aether-mcp` adds the agent-facing JSON
layer:

- named kinds are schema-encoded against a per-engine cache;
- reverse names come from engine inventory;
- reply events and settlement are projected into bounded JSON;
- oversized byte leaves/responses spill to scratch files with summaries.

Do not add engine semantics only to the projection layer. A new operator tool
should rest on a real capability/RPC/mail contract that other clients and tests
can exercise.

## When to change RPC

Change the wire only for a cross-process responsibility. Ordinary new actor
kinds travel inside `MailEnvelope` and do not require a new `WireFrame` variant.

Wire changes need:

1. a compatibility decision and ADR when load-bearing;
2. encode/decode round trips and malformed/oversize tests;
3. hub, proxy, child, and MCP caller updates;
4. mixed-version behavior stated explicitly;
5. bounded allocation before trusting peer-provided lengths.

## Change route

- Frames and addresses: `crates/aether-rpc/src/wire.rs`
- Native server/session: `crates/aether-rpc/src/server/`
- Engine proxy/server: `crates/aether-engine/src/`
- MCP client/session: `crates/aether-mcp/src/{rpc.rs,tools/}`
- Stream framing: `crates/aether-codec/src/frame.rs`
- Decisions: ADR-0072 (amended), ADR-0074, ADR-0089, ADR-0118
