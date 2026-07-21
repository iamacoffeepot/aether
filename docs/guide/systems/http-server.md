# HTTP server and typed routes

`aether.http.server` turns inbound HTTP into actor mail. It is the mirror of
[HTTP egress](http.md): the native capability owns sockets and protocol policy,
while guest actors own application routing and responses.

## Request path

```text
TCP accept / HTTP reader
  → route lookup
  → one dispatch shard owns connection + in-flight state
  → typed or raw handler mailbox
  → reply opens buffered response, stream, or websocket
  → reader sidecar writes buffered bytes; stream writer handles chunks/websocket
```

Accept, socket reads, normal buffered-response writes, and streamed body writes
perform their blocking I/O on sidecars. Dispatch shards keep mutable
connection/stream state in actors and return events to the capability. There
are current dispatcher-thread exceptions: streamed-response and websocket
response heads, plus some canned status responses, call blocking
`write_all`/`flush` through the shard's socket half. Those writes are small but
can still stall under peer backpressure, so do not treat all server writes as
off-dispatch. Sharding changes throughput topology, not the public kinds.

## Buffered contract

The baseline handler receives `aether.http.server.request`, containing the
method, path, query, headers, body, and peer address. It has no request-id
field; dispatch identity and reply correlation ride the mail envelope. The
handler replies with `aether.http.server.response`, containing status, headers,
and a complete body.

The configured `max_request_bytes` bounds a **buffered request** body, and
`max_header_bytes` bounds its request line plus headers. There is currently no
HTTP-server-specific size cap for a buffered response body: the handler returns
a complete `Vec<u8>`, and the renderer allocates and writes it in full (subject
only to broader mail/guest-memory constraints). Use buffered responses for
small JSON, HTML, and assets, and use response streaming when response memory
needs an explicit bounded window. Large or unbounded uploads require the
structural streaming opt-in described below; `max_request_bytes` applies to the
buffered path, not that credit-paced path.

## Route registration

Handlers declare interest by mail during wiring and unregister during teardown.
A route key includes the method and path pattern. Registration results surface
invalid patterns and ownership conflicts explicitly.

Two target modes exist:

- **exclusive** (default): one live claimant owns the route;
- **shared**: multiple instanced handlers opt into one member set and the
  server spreads requests across live members.

Shared and exclusive claimants cannot silently mix for the same key. Dead
members are removed; lifecycle cleanup is part of route ownership, not a caller
convention.

`#[http::router]` derives registration from typed `#[route]` methods.
`#[http::router(shared)]` opts every generated claim into the shared set. Use
shared routes with replicated instances only when handler state and external
effects tolerate per-request distribution.

## Typed authoring

The typed layer parses a raw request into route parameters, query/body/header
extractors, and a handler-specific context. A route method returns a type that
can be rendered into the server reply contract.

Treat the macros as the public authoring surface and the raw kinds as the
portable protocol. When debugging expansion or adding an extractor, read both
`aether-http`'s `typed.rs` and `aether-http-derive`; behavior may be generated
rather than visible in the handler source.

The worked examples are split across [Serving HTTP from a component](../recipes/serving-http.md).
Verify every operational tool argument against the live MCP schema before
copying an old recipe.

## Response streaming

A handler opts into streaming by replying with
`aether.http.server.response_stream_open`. That initial correlated reply
declares status and headers. The capability grants a bounded number of chunks
through `stream_credit`; the handler sends at most that many
`response_chunk` messages before waiting for more credit, then sends
`response_stream_end`.

`ResponseStream` captures both the dispatching counterparty and `stream_id` from
the credit message. Later handler methods use that durable handle. They do not
assume the singleton server mailbox is the correct return address—sharded
dispatch means the actual counterparty matters.

Each chunk is a detached short chain. Settlement of the opening request does
not remain held for the full lifetime of a download.

## Request streaming

Request streaming is a structural handler opt-in, not the default for every
upload. After route selection, the capability checks whether that handler's
accept set includes `aether.http.server.request_stream_open`; a handler that
only accepts the ordinary request kind receives a buffered body. For an opted-in
handler, the capability sends `request_stream_open`, then chunks only as the
handler grants `request_credit`. The handler learns the stream id at open;
`request_stream_end` marks the last body bytes and retains the reply correlation
needed for the response.

Backpressure is bidirectional and explicit. Sending over credit is a protocol
violation that can tear down the stream; ignoring credit can park the producer.

## Websocket upgrade

An ordinary HTTP upgrade request reaches the selected route. The handler may
reply with `websocket.accept`; after that, complete de-fragmented messages and
close events use an explicit `stream_id` in both directions.

`WebSocketStream` is reply-derived like response streams: it remembers the
actual dispatch counterparty. An unknown or already-torn-down id is not rerouted
to another connection.

## Trust and limits

The native capability is the protocol firewall. Current bounds and remaining
responsibilities include:

- request header and buffered-body sizes, plus websocket frame/message sizes;
- buffered and streaming in-flight counts;
- route pattern complexity;
- response credit and writer queues;
- an explicit buffered-response body cap if a deployment needs one—the server
  does not currently provide that configuration;
- idle/read/write lifetimes;
- malformed requests and websocket frames.

Guest handlers are trusted to obey the mail-level credit protocol but do not
receive raw socket ownership. A route handler should still validate
application authorization, content type, and input shape; native parsing is not
application permission.

## Failure model

| Symptom | First evidence |
|---|---|
| Port does not bind | resolved server config and `aether.http.server` logs |
| 404/route miss | registration result, handler wiring, method/path pattern |
| Registration conflict | exclusive/shared claimant set and live instances |
| Handler runs but peer hangs | reply class, correlation, stream-open/end path |
| Stream stalls | credit ownership and actor logs/cost on handler/shard |
| Websocket closes | upgrade response, frame limits, explicit stream id |

Do not debug inbound failures through the egress `aether.http.fetch` actor; the
two capabilities share shapes but not runtime ownership.

## Change route and decisions

- Public kinds: `crates/aether-http/src/kinds.rs`
- Typed authoring and handles: `crates/aether-http/src/{typed,stream}.rs`
- Runtime: `crates/aether-http/src/server/`
- Derives: `crates/aether-http-derive/src/lib.rs`
- Integration boundary: `crates/aether-http/tests/http_serving.rs`
- ADR-0108, ADR-0128–ADR-0136 (all accepted; read amendments in order)
