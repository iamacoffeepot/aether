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
settlement hold. Repeating a bind of the same address as the same consumer
returns the standing listener rather than creating one.

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

Every bind and connect names a consumer; there is no listener or session
without one. A consumer covers the `TcpConsumer` protocol
(`aether_tcp::TcpConsumer`): it
handles `session_data` and `session_closed`, both silently. Each session holds
its consumer as a `ProtocolRef<TcpConsumer>`, so its fan-out compiles only for
those two kinds.

A consumer actor binds itself to its sessions with a `_self` kind:
`ctx.send::<TcpCapability>(&BindListenerSelf { .. })` or
`ctx.send::<TcpCapability>(&ConnectSelf { .. })`. The capability takes the
consumer from the proven sender, so the actor never names its own position.
Both handlers require `TcpConsumer` of their sender (ADR-0231 §11), which
each states as its ctx's sender (`NativeCtx<'_, Self, TcpConsumer>`) and
reads as the proven `ProtocolRef<TcpConsumer>` from `ctx.sender()`: the send
builds only for an actor with silent handlers for `session_data` and
`session_closed`, and an actor that lacks one gets a build error naming it.
The engine casts the sender before the handler runs, so a `_self` kind that
arrives another way, such as a call relayed from MCP, is answered
`Err(Consumer(..))` naming the sender and the handler it lacks, and nothing
is bound or dialed. Mail with no actor sender is refused with no reply.

An agent, or a capability binding a different actor, names that actor in the
`consumer` field of `bind_listener` or `connect`. The field is required, a
`ProtocolPath<TcpConsumer>`: in code an `ActorPath<R>` narrowed with
`.narrow::<TcpConsumer>()`, and over MCP the canonical `path` a component load
returns. The path must be canonical; a short `root/:key` path is refused. Its decode proves that the route at the path, live or closed,
publishes both silent rows. A path no route has stood at, one whose route
does not publish them, and a consumer that has closed are all answered
`Err(Consumer(PathRefused { path, reason }))`, with nothing bound or dialed; a
dial or bind that fails otherwise is `Err(Failed { addr, error })`. Sessions and listeners are addressed as
`aether.tcp/aether.tcp.session:<session_name>` and
`aether.tcp/aether.tcp.listener:<listener_name>`, from the names the results
return.

Each session delivers `session_data` and `session_closed` as itself, so the
host stamps the session as the envelope sender. The consumer writes or closes
through that sender: `ctx.sender()`, cast as below, then
`ctx.send_to(session, &SessionWrite { .. })` or `&SessionClose {}`. Accepted
and outbound sessions are addressed the same way. There are no route helpers,
and the consumer never derives a session's mailbox from its name. Raw sockets
stay in native state.

An erased reference has no send verb (ADR-0231 §4), so the consumer casts
the session sender once, on the session's first delivery, to a protocol with
the `SessionWrite` / `SessionClose` rows it needs, keeps the
`ProtocolRef<P>` with its session state, and writes through that
([R-0044](../contributing/design-rules.md#r-0044)). A guest casts with
`WasmCtx::cast`; the tcp load probe fixture
(`crates/aether-test-fixtures-bundle/src/tcp_load_probe.rs`) is the worked
example.

Listener/session names live under the engine's lineage. They are not globally
unique across engines and should be discovered from result/notification data,
not guessed from hashes.

## Lifetimes

The capability monitors each consumer it has bound a listener or dialed a
session for (ADR-0079 §8), once per consumer however many it holds for it.
When the consumer closes, the capability mails `aether.tcp.close` to every
listener bound to it, the mail an unbind sends, and
`aether.tcp.session_close` to every session dialed for it. A consumer that
closes, by whatever exit, therefore leaves nothing bound or connected on its
behalf, and it need not unbind first.

The engine does not close an actor's children when the actor closes. A
listener therefore keeps an entry for each session it accepted and mails each
one `aether.tcp.session_close` as it closes, whether an unbind or its
consumer's close ended it.

- A listener lives until `aether.tcp.unbind_listener` names it, until the
  consumer it was bound for closes, or until the engine tears down.
- An accepted session lives until its peer closes or a read fails, a frame is
  rejected, a write fails, it is sent `aether.tcp.session_close`, or its
  listener closes.
- A dialed session lives until one of the same ends, with its consumer's
  close in the place of the listener's.
- A listener told to close while a session birth is still settling waits for
  that birth, so the session is closed with the rest. Connections that arrive
  while it waits are dropped.
- One consumer's close touches only what was bound to it: another consumer's
  listeners and sessions stay.
- A republish of the consumer keeps its mailbox, so it closes nothing: the
  listener and its sessions deliver to the successor.
- A session closed by `aether.tcp.session_close`, whoever sent it, sends no
  `session_closed`. Its peer sees the connection close.

A listener that closes because its consumer closed leaves
`aether.tcp.list_listeners` the way an unbound one does, and an unbind parked
on it at that moment is answered `Ok`. The capability keeps an entry for each
dialed session, and no kind lists sessions or closes one by name.

## Concurrency boundary

Blocking accept/read loops and outbound connect run on sidecar threads. The
sidecars wake actors and hand results across standard-library channels, which
are currently unbounded; frame limits and kernel buffers do not turn those
handoffs into a general bounded-queue contract. Actors retain state ownership
and serialize control transitions. Session writes are the current exception to
the sidecar rule: `session_write` calls the socket's full-write loop on the actor
dispatcher and can briefly block there under kernel backpressure. Closing, by
any exit in [Lifetimes](#lifetimes) including the consumer closing, must make
all of these converge:

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
