# TCP listeners and sessions

`aether.tcp` represents framed TCP ownership as actors. The singleton
capability handles bind/connect control; instanced listener and session actors
own individual resources and lineage names.

## Actor topology

```text
aether.tcp (singleton control plane)
  ├─ TcpListenerActor/<name>
  │    ├─ TcpSessionActor/<connection>
  │    └─ TcpSessionActor/<connection>
  └─ outbound TcpSessionActor/<connection>
```

The control actor does not become a global byte pump. An instance owns each
listener/session lifetime, making close, monitoring, and recipient routing
explicit.

## Control operations

| Kind | Meaning |
|---|---|
| `aether.tcp.bind_listener` | bind an address and create a listener actor |
| `aether.tcp.unbind_listener` | stop one owned listener |
| `aether.tcp.list_listeners` | inspect active listener instances |
| `aether.tcp.connect` | establish an outbound connection and session actor |

Bind/connect results carry success or a bounded error. Readiness notifications
separate actor creation from a socket being usable. A connect timeout or bind
failure must resolve the initiating request; it must not leave a permanent
settlement hold.

## Session data contract

TCP is a byte stream, but Aether's session surface is framed. Reader sidecars
reassemble length-prefixed frames and notify the session actor that data is
ready. Consumers receive `aether.tcp.session_data`; writes use
`aether.tcp.session_write`. `session_close` requests cooperative local shutdown.
`session_closed` reports peer EOF, a read error, or frame rejection to the
configured consumer; a local `session_close` does not synthesize that event
today.

The framing body uses Aether's canonical wire format where the higher protocol
calls for typed frames. ADR-0118 supersedes old references to postcard in
earlier RPC/TCP decisions.

Do not treat one OS `read` as one message or assume a `write` is delivered as
one peer read. Reassembly and full-write loops are native responsibilities.

## Consumer binding

Route helpers bind a session to the actor that owns the application protocol.
Wasm and native extensions expose the same conceptual operations while keeping
raw sockets in native state. The session actor stamps and routes readiness,
data, and close events; the consumer should not derive sibling mailbox ids by
hand.

Listener/session names live under the engine's lineage. They are not globally
unique across engines and should be discovered from result/notification data,
not guessed from hashes.

## Concurrency boundary

Blocking accept/read loops and outbound connect run on sidecar threads. The
sidecars wake actors and hand results across standard-library channels, which
are currently unbounded; frame limits and kernel buffers do not turn those
handoffs into a general bounded-queue contract. Actors retain state ownership
and serialize control transitions. Session writes are the current exception to
the sidecar rule: `session_write` calls the socket's full-write loop on the actor
dispatcher and can briefly block there under kernel backpressure. Closing must
make all of these converge:

- socket shutdown;
- sidecar exit or detach;
- registry/monitor cleanup;
- no duplicate terminal notification on paths that report one;
- settlement holds released.

Never join an indefinitely blocked socket thread on the dispatcher.

## Security boundary

TCP is lower level than HTTP. The capability supplies framing and resource
lifetime, not application authentication or request validation. A consumer
must define:

- allowed bind/connect addresses;
- frame-size and rate limits;
- handshake/authentication before privileged messages;
- idle and shutdown timeouts;
- behavior on malformed or unknown frames.

The [player-session tier](player-sessions.md) is an example of adding trusted
identity and pacing above TCP rather than exposing the simulation directly to a
raw connection.

## Change route

- Public kinds/helpers: `crates/aether-tcp/src/{kinds,route}.rs`
- Control runtime: `crates/aether-tcp/src/runtime.rs`
- Listener actor: `crates/aether-tcp/src/listener/`
- Session actor: `crates/aether-tcp/src/session/`
- Configuration: `crates/aether-tcp/src/config.rs`
- Decisions: ADR-0079 (instanced actors), ADR-0118 (wire); earlier hub/RPC
  framing context in ADR-0072 is amended by ADR-0118
