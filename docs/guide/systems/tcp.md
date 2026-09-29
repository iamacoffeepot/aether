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
| `aether.tcp.bind_listener_self` | bind with the sender as consumer |
| `aether.tcp.unbind_listener` | stop one owned listener |
| `aether.tcp.list_listeners` | inspect active listener instances |
| `aether.tcp.connect` | establish an outbound connection and session actor |
| `aether.tcp.connect_self` | connect with the sender as consumer |

Bind/connect results carry success or a bounded error. Readiness notifications
separate actor creation from a socket being usable. A connect timeout or bind
failure must resolve the initiating request; it must not leave a permanent
settlement hold.

Connect, bind, and unbind answer later than the handler turn that receives
them, through typed held replies (ADR-0243). Each handler holds its reply as a
`Held<R>` and returns `Pending<R>`, so `describe_handlers` names its reply
kind. The `Held` waits in the capability's state, keyed by the work that
answers it:

- A connect waits under its connect id until the dial sidecar reports, then
  until the staged session's birth completes.
- A bind waits under its listener name until the staged listener's birth
  completes and the capability has installed its monitor.
- An unbind waits on the listener's entry until the listener's close notice
  arrives.

A staged birth carries only its key (`aether.tcp.session_spawn_key` or
`aether.tcp.listener_spawn_key`) into its task completion, which looks up the
`Held` and answers it. An early failure answers the `Held` at once. When the
capability closes, the ledger settles every reply it still holds.

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

A consumer covers the `TcpConsumer` protocol (`aether_tcp::TcpConsumer`): it
handles `session_data` and `session_closed`, both silently. Each session holds
its consumer as a `ProtocolRef<TcpConsumer>`, so its fan-out compiles only for
those two kinds.

A consumer actor binds itself to its sessions with a `_self` kind:
`ctx.send::<TcpCapability>(&BindListenerSelf { .. })` or
`ctx.send::<TcpCapability>(&ConnectSelf { .. })`. The capability takes the
consumer from the proven sender, so the actor never names its own position. It
casts the sender to `TcpConsumer` once, at receipt (ADR-0231 §4), and replies
`Err` without binding or dialing when the sender's published rows do not
cover the protocol, or when the mail has no actor sender. A component that
binds itself can check its coverage at compile time with
`TcpConsumer: CoveredBy<Self>`, as the `tcp_load_probe` fixture does.

An agent, or a capability binding a different actor, names that actor in the
`consumer` field of `bind_listener` or `connect`. The field is a
`ProtocolPath<TcpConsumer>`: in code an `ActorPath<R>` narrowed with
`.narrow::<TcpConsumer>()`, and over MCP the canonical `path` a component load
returns. The path must be canonical; a short `aether.component/:name` path is
refused. Its decode proves that the live route at the path publishes both
silent rows, and a path that does not is refused at decode: the mail is logged
at warn and gets no reply. A route that left between decode and receipt gets
`Err`. Sessions and listeners are addressed as
`aether.tcp/aether.tcp.session:<session_name>` and
`aether.tcp/aether.tcp.listener:<listener_name>`, from the names the results
return.

Each session delivers `session_data` and `session_closed` as itself, so the
host stamps the session as the envelope sender. The consumer writes or closes
through that sender: `ctx.sender()`, then
`ctx.send_to(session, &SessionWrite { .. })` or `&SessionClose {}`. Accepted
and outbound sessions are addressed the same way. There are no route helpers,
and the consumer never derives a session's mailbox from its name. Raw sockets
stay in native state.

That consumer-to-session write is still an erased shape on `main` that #6895
retires: it goes through `ctx.sender()`'s `ErasedActorRef`, and a guest has no
cast or typed resolve yet to prove the session with. Under the design rules, a
reference that is sent through is a typed proof
([R-0039](../contributing/design-rules.md#r-0039)).

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

Put trusted identity and pacing in a session tier above TCP rather than
exposing an application actor directly to a raw connection.

## Change route

- Public kinds: `crates/aether-tcp/src/kinds.rs`
- Control runtime: `crates/aether-tcp/src/runtime.rs`
- Listener actor: `crates/aether-tcp/src/listener/`
- Session actor: `crates/aether-tcp/src/session/`
- Configuration: `crates/aether-tcp/src/config.rs`
- Decisions: ADR-0079 (instanced actors), ADR-0118 (wire); earlier hub/RPC
  framing context in ADR-0072 is amended by ADR-0118
