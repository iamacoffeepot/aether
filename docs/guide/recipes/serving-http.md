# Serving HTTP from a component

**Class: recompile.** You're writing a wasm component that handles inbound
HTTP requests — `cargo` plus the pre-flight loop. The `aether.http.server`
capability (ADR-0108) binds the listening socket; you write the handler that
receives `aether.http.server.request` and replies
`aether.http.server.response`.

## 1. Configure the server

The HTTP server is opt-in (off by default). Set `AETHER_HTTP_SERVER_ENABLED=1`
to turn it on and bind the listening socket:

```sh
AETHER_HTTP_SERVER_ENABLED=1 \
AETHER_HTTP_SERVER_BIND_ADDR=127.0.0.1:8080 \
cargo run -p aether-chassis-headless --bin aether-headless
```

`AETHER_HTTP_SERVER_BIND_ADDR` defaults to `127.0.0.1:8080`; use port `0` to
let the OS pick a free port. The server has no default-handler config knob: a
component receives requests by claiming a route (ADR-0130), and a handler that
wants *every* request registers the `/` catch-all from its `wire` hook —
`register_route_self { prefix: "/" }` (shown in §3, and in §Claiming routes
for narrower prefixes). The registration is runtime-bound, so the handler can
load or reload without restarting the server, and a request matching no route
is answered `503`.

### Over MCP (`spawn_substrate`)

`spawn_substrate` forwards its `args` array to the substrate's argv, with no
env field — so the MCP-spawn path configures the server with flags instead of
the env vars above. The `#[derive(Config)]` on `HttpServerConfig` is tagged
`cli_prefix = "http-server"` (ADR-0090), which mints one flag per field —
`enabled` becomes the bare presence flag `--http-server-enabled`, and every
other field becomes `--http-server-<field>=<value>`:

```jsonc
// spawn_substrate (omit selector for the stored default headless binary)
{
  "args": [
    "--http-server-enabled",
    "--http-server-bind-addr=127.0.0.1:8080"
  ]
}
```

To use a non-default chassis binary, call `upload_binary` first and pass the
returned registry selector. `spawn_substrate` does not accept `binary_path`.

## 2. Set up the crate

The http server cap needs **no marker feature** — unlike `render` / `audio` /
`text` / `ui`, `aether_http` and its kinds are always-on, so a
default-features-off wasm build sees them with no extra feature wiring:

```toml
# crates/my-http-component/Cargo.toml
[package]
name = "my-http-component"
version.workspace = true
edition.workspace = true

[lib]
crate-type = ["cdylib"]

[dependencies]
aether-actor = { path = "../aether-actor" }
aether-http = { path = "../aether-http", default-features = false }
```

## 3. Write the handler

A handler is a wasm component with one `#[handler::single]` for
`aether.http.server.request` that returns `HttpRouterResult`: every route holder
covers the `HttpRouter` protocol, whose one row replies
`aether.http.server.router_result`. A buffered answer is its `Response` variant,
an `aether.http.server.response` with a status code, optional headers, and a
byte body. The server writes the formatted
HTTP/1.1 response to the client socket. On HTTP/1.1 the connection is kept alive
by default and serves the next request on the same socket; a client that sends
`Connection: close` (and HTTP/1.0, which closes by default) terminates it, and
an idle kept-alive connection is closed after `keep_alive_timeout_millis`.

```rust
use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_http::HttpServerCapability;
use aether_http::kinds::{HttpRouterResult, HttpServerRequest, HttpServerResponse, RegisterRouteSelf};

pub struct Web;

#[actor(root, depends(HttpServerCapability))]
impl WasmActor for Web {
    const NAMESPACE: &'static str = "web";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Web)
    }

    // Claim the `/` catch-all so every request dispatches here. Register a
    // narrower prefix instead (see §Claiming routes) to own just one path
    // family and leave the rest to other handlers.
    fn wire(&mut self, ctx: &mut WireCtx<'_, '_>) {
        ctx.send::<HttpServerCapability>(&RegisterRouteSelf {
            prefix: "/".to_string(),
            method: None,
            shared: false,
        });
    }

    #[handler::single]
    fn on_request(&mut self, _ctx: &mut WasmCtx<'_>, req: HttpServerRequest) -> HttpRouterResult {
        let (status, body): (u16, &[u8]) = match req.path.as_str() {
            "/" => (200, b"hello"),
            _ => (404, b"not found"),
        };
        HttpRouterResult::Response(HttpServerResponse {
            status,
            headers: Vec::new(),
            body: body.to_vec(),
        })
    }
}

aether_actor::export!(public = [Web]);
```

`HttpRouterResult` names the three answers the server accepts: `Response` (a
buffered `HttpServerResponse`), `Stream` (an `HttpResponseStreamOpen`, see
"Streaming a response" below), and `WebSocket` (a `WebSocketAccept`). One
handler can return any of them per request (see "Mixing buffered and streamed
routes" below), and returning any other kind is a compile error.
The `register_route_self` registration casts the sender to `HttpRouter`; a
component with no `aether.http.server.request` handler replying
`HttpRouterResult` is answered `register_route_result::Err`. A handler may omit
its actor, and the macro types the ctx by it — `WasmCtx<'_>` reads as
`WasmCtx<'_, Self>`, reaching only the actors the component declares with
`depends(R)`. Spell `WasmCtx<'_, Erased>` for the untyped view.

The component registers at `web`, its `NAMESPACE` const: a singleton guest is
named by its own namespace
([ADR-0241](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0241-code-is-published-not-loaded.md)
§5). Its `wire` hook
claims the `/` catch-all, so every request the server can't match to a more
specific route dispatches here. `req.peer_addr` carries the connecting
client's address (`ip:port`, IPv6 bracketed) for logging, rate-limiting, or
allowlisting.

## 4. Load the handler

`load_component` resolves against the hub's content-addressed component
registry (ADR-0116), so stage the compiled wasm with `upload_component` first:

```sh
cargo build --target wasm32-unknown-unknown -p my-http-component
```

The artifact is normally
`target/wasm32-unknown-unknown/debug/my_http_component.wasm`. Rebuild it after
every handler change; an upload selector continues to name the bytes that were
actually uploaded, not whatever source is now on disk.

```jsonc
// upload_component
{
  "staged_path": "/path/to/my_http_component.wasm"
}
// → { "hash": "<hash>", "name": null }
```

Then load it by selector over the MCP harness once the substrate is up:

```jsonc
// load_component
{
  "engine_id": "<engine>",
  "selector": "<hash-or-name>"
}
```

`load_component` replies with the component's registered address
(`web`). After that, any inbound HTTP request
on the bound port routes to your handler.

## 5. Send a request

From a shell, or from any HTTP client that speaks HTTP/1.1:

```sh
curl http://127.0.0.1:8080/
# → hello
```

The server reads the request, dispatches `aether.http.server.request` to the
handler mailbox, waits for the `aether.http.server.router_result` reply, and writes
the formatted response to the client. The server adds the `Connection` header
(`keep-alive` on a persistent HTTP/1.1 connection, `close` otherwise) and an
appropriate `Content-Length` header; your handler sets the status code,
optional extra headers, and the body.

## Claiming routes

Several components can each own a path family on the same server (ADR-0130).
A component claims a prefix from its `wire` hook by mailing
`aether.http.server.register_route_self` to the server capability; the server
then dispatches matching requests to that component directly. A request
matching no route is answered `503`, unless some component claimed the `/`
catch-all (as the §3 handler does) — then everything unmatched goes there.

```rust
use aether_http::HttpServerCapability;
use aether_http::kinds::RegisterRouteSelf;

// In an `#[actor(depends(HttpServerCapability))]` block.
fn wire(&mut self, ctx: &mut WireCtx<'_, '_>) {
    ctx.send::<HttpServerCapability>(&RegisterRouteSelf {
        prefix: "/api".to_string(),
        method: None,                        // or Some(HttpMethod::Get)
        shared: false,                       // true joins an ADR-0136 member set
    });
}
```

Matching is by path segment: `/api` claims `/api` and everything under
`/api/…`, and leaves `/apiary` alone; `/` claims everything as a catch-all.
When prefixes overlap, the longest match wins, and a route filtered to one
method beats a method-agnostic route at the same prefix. A prefix already
claimed by another component is answered
`aether.http.server.register_route_result::Err` — first claimant keeps it.
Routes follow the component: the route holds a proof of the registrant that
survives `replace_component`, and it is released automatically when the
component drops, or explicitly via
`aether.http.server.unregister_route_self`. External callers (an MCP session,
a test) use the `register_route` / `unregister_route` forms, which name the
handler by its canonical path.

Every route dispatches as `aether.http.server.request` to the holder's one
request handler, so a component serving several prefixes tells them apart by
`req.path` (or through the typed surface below, which does that for it).

### Registering a route for another actor

`register_route_self` resolves the registrant from the sender's in-process
`Source`; an MCP session or a test has no such source, so it uses the named
form instead — `register_route` / `unregister_route`, which name the handler
by its canonical actor path. `RegisterRoute` carries `prefix` (`String`),
`method` (`Option<HttpMethod>` — a bare variant string like `"Get"`, or `null`
to match every method; the seven variants are `Get`, `Post`, `Put`, `Delete`,
`Patch`, `Head`, `Options`), `handler` (the path text), and `shared` (the
ADR-0136 member-set flag — `false` claims the prefix exclusively, `true` joins
the round-robin set on it).

The `handler` path must be canonical — the `path` a `load_component` reply
returns, `api` for a singleton component whose `NAMESPACE` is `api`. The named actor has to take `aether.http.server.request` and reply
`HttpRouterResult`: the path is `ProtocolPath<HttpRouter>`, so a path whose
live route does not publish that row is refused when the mail is
decoded — logged at warn, with no `register_route_result` reply at all, rather
than accepted and then answering `502` on every request. In Rust the same path
is written `ActorPath::<Handler>::root().narrow::<HttpRouter>()`, which will
not compile unless `Handler` has the row. Every route holder has it, including
a streaming, websocket, or deferred handler and a `#[http::router]` actor.

```jsonc
// send_mail → aether.http.server  (kind: aether.http.server.register_route)
{
  "prefix": "/api",
  "method": "Get",
  "handler": "api",
  "shared": false
}
```

The reply is `aether.http.server.register_route_result` — `"Ok"` or
`{ "Err": { "error": "…" } }` — the same shape `register_route_self` replies,
which is *why* the named form exists: an external caller (an MCP session, a
test) has no in-process `Source` to resolve, so `register_route_self` always
answers it `Err`.

Releasing the route mirrors the registration, keeping `method` so a
method-specific route and a method-agnostic route at the same prefix release
independently. Its `handler` is a plain actor path rather than a protocol one
— a release needs only the identity the route table is keyed by — so any
spelling the engine resolves to the handler works, an ADR-0166 short path
included:

```jsonc
// send_mail → aether.http.server  (kind: aether.http.server.unregister_route)
{
  "prefix": "/api",
  "method": "Get",
  "handler": "api"
}
```

## Typed route authoring

The typed surface writes that whole registration for you (ADR-0131). Put
`#[http::router]` on the actor's impl block, above `#[actor]`, and
`#[http::route(<Method|any>, "<prefix>")]` on a method; the macros inject the
`register_route_self` send into `wire` and emit the router's one
`#[handler::single]` for `aether.http.server.request`, which picks the route and
answers `HttpRouterResult::Response` with what the method returns. A routed method takes an
`http::Ctx<'_, C>` — the transport ctx (`WasmCtx` here) plus the request and
matched route, dereffing to the ctx so mail sends read as usual — and returns
`HttpServerResponse`. The actor must declare `depends(HttpServerCapability)`
on its `#[actor]` attribute, because the injected registration mails the
server; without it, the actor fails to compile at `#[http::router]`:

```rust
use aether_http as http;
use aether_http::HttpServerCapability;
use aether_http::kinds::{HttpServerRequest, HttpServerResponse};

#[http::router]
#[actor(root, depends(HttpServerCapability))]
impl WasmActor for ApiHandler {
    const NAMESPACE: &'static str = "api";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(ApiHandler)
    }

    #[http::route(Get, "/api/users")]
    fn list_users(&mut self, ctx: http::Ctx<'_, WasmCtx<'_>>) -> HttpServerResponse {
        HttpServerResponse {
            status: 200,
            headers: Vec::new(),
            body: format!("path: {}", ctx.request().path).into_bytes(),
        }
    }
}
```

To parse a request into domain values, add parameters that implement
`http::FromRequest`. Each runs in declaration order before the method body; the
first one that returns `Err` becomes the reply — the boundary where a malformed
request turns into a `400` instead of ad-hoc parsing inside the handler:

```rust
struct UserId(u64);

impl http::FromRequest for UserId {
    fn from_request(request: &HttpServerRequest) -> Result<Self, HttpServerResponse> {
        request
            .path
            .rsplit('/')
            .next()
            .and_then(|seg| seg.parse().ok())
            .map(UserId)
            .ok_or(HttpServerResponse {
                status: 400,
                headers: Vec::new(),
                body: b"expected a numeric user id".to_vec(),
            })
    }
}

#[http::route(Get, "/api/users")]
fn get_user(&mut self, _ctx: http::Ctx<'_, WasmCtx<'_>>, id: UserId) -> HttpServerResponse {
    // `id` is already parsed; a bad id never reaches here.
    HttpServerResponse { status: 200, headers: Vec::new(), body: format!("user {}", id.0).into_bytes() }
}
```

An `HttpServerRequest` parameter hands the method the whole request (the
identity extractor). The same surface serves native actors — a routed method on
an `impl NativeActor` takes `http::Ctx<'_, NativeCtx<'_>>` with a
`state: &mut YourState` first parameter, and the macros write the native `wire`.

The router types a route's ctx by its actor: `http::Ctx<'_, WasmCtx<'_>>` reads
as `http::Ctx<'_, WasmCtx<'_, Self>>`. A route therefore reaches the peers its
actor declares with `depends(..)` through the flat verbs — `ctx.send::<R>(..)`
and its siblings, on both transports — and a route that sends to `R` needs
`depends(R)` like any other handler.

Drop to the raw `register_route_self` surface above for a streaming route
(`HttpRouterResult::Stream`) — the typed surface returns `HttpServerResponse`,
so a streamed response keeps its own hand-written request handler.

A typed route returns `HttpServerResponse` and answers at once. A handler that
forwards to a peer and answers when the peer replies is a hand-written
`HttpServerRequest` handler that holds its reply (ADR-0243): it returns
`Pending<HttpRouterResult>` from `ctx.hold::<HttpRouterResult>()`, forwards
with `send_with_context`, carrying the `Held` in the request context, and
answers from the peer's reply handler, where `take_context` hands the `Held`
back. The peer is a declared dependency (`depends(.., Peer)`), and the held
reply is native, so this handler is a native actor's:

```rust
#[aether_data::kind(name = "my_app.forward_context")]
struct ForwardContext {
    held: Held<HttpRouterResult>,
}

#[handler::single]
fn on_request(_state: &mut State, ctx: &mut NativeCtx<'_>, request: HttpServerRequest) -> Pending<HttpRouterResult> {
    let (pending, held) = ctx.hold::<HttpRouterResult>();
    let _ = ctx.send_with_context::<Peer>(&Ask { path: request.path }, ForwardContext { held });
    pending
}

#[handler::single]
fn on_answer(_state: &mut State, ctx: &mut NativeCtx<'_>, answer: Answer) {
    if let Some(ForwardContext { held }) = ctx.take_context::<ForwardContext>() {
        held.answer(ctx, &HttpRouterResult::Response(answer.into_response()));
    }
}
```

### Scaling one handler to N instances

`#[http::router(shared)]` — the bare ident `shared` in place of no argument —
registers every route on the impl `shared: true` instead of the default
exclusive claim. Load N instances of a component written this way and they
join one round-robin member set on their shared prefixes, so "scale this
handler to 4" is one attribute plus a `replicas: 4` on the load spec, with no
hand-written `register_route_self` sends. Any argument other than the bare
`shared` ident is a compile error naming the two accepted forms (no argument,
or `shared`).

## What happens when the handler doesn't reply

A request handler always answers: it returns its `HttpRouterResult`, or holds it
and answers later. A router that closes while it still holds a reply answers
`502 Bad Gateway` at once (`router closed before answering`). A chain that
settles with no answer, such as a streamed upload whose `request_stream_end`
handler replies nothing, triggers the server's own `502` safety net. If the
answer takes longer than `AETHER_HTTP_SERVER_REQUEST_TIMEOUT_MILLIS` (default
30 000 ms), as a held reply whose peer never replies does, the server sends
`504 Gateway Timeout`. A request matching no route (nothing has
claimed it — e.g. no handler loaded yet) returns `503 Service Unavailable`.

## Adding response headers

Pass a `Vec<HttpHeader>` in the returned `HttpServerResponse`:

```rust
use aether_http::kinds::HttpHeader;

HttpServerResponse {
    status: 200,
    headers: vec![HttpHeader {
        name: "content-type".to_string(),
        value: "application/json".to_string(),
    }],
    body: br#"{"ok":true}"#.to_vec(),
}
```

The server sends these after its own `Connection` and `Content-Length`
headers.

## Streaming a response

A handler that serves a large download or a long-lived event stream returns
`HttpRouterResult::Stream` in place of `HttpRouterResult::Response`, then emits the body
across many `HttpResponseChunk` mails and terminates with
`HttpResponseStreamEnd` (ADR-0128). The server renders the response as chunked
transfer-encoding, so the whole body never resides in memory at once.

The pace is a windowed credit protocol. When the handler opens a stream, the
server grants it an initial credit window (`AETHER_HTTP_SERVER_RESPONSE_STREAM_WINDOW`,
default 16) as an `HttpStreamCredit` mail, and replenishes one credit each time
its per-connection writer thread drains a chunk to the socket. A chunk consumes
one credit; when credit reaches zero the handler pauses until the next
`HttpStreamCredit` arrives. So a slow client blocks the writer thread, not the
scheduler, and the handler cannot outrun the socket.

The data phase answers whoever dispatched to the handler — the same invariant the
`HttpResponseStreamOpen` reply already honours (ADR-0133). The handler captures a
`ResponseStream` handle from its first credit mail — the counterparty that paced
the stream, plus the `stream_id` — and emits every chunk through it. The stream
flows back to that counterparty, so a test mock or a middleware forwarding in
front of the server receives it exactly as the real server does. Each send is a
detached chain root, so a chunk settles on its own causal chain instead of the
credit grant that triggered it. The handler reads the proven sender of the credit
mail with `ctx.sender()` and hands it to `from_credit`, so the credit handler is
an ordinary `#[handler::single]`:

```rust
use aether_actor::{WasmCtx, WasmInitCtx};
use aether_http::ResponseStream;
use aether_http::kinds::{
    HttpResponseStreamOpen, HttpRouterResult, HttpServerRequest, HttpStreamCredit,
};

pub struct Feed {
    // The stream this handler is feeding, captured from the first credit mail.
    stream: Option<ResponseStream>,
    next: u32,
    done: bool,
}

#[actor(root)]
impl WasmActor for Feed {
    const NAMESPACE: &'static str = "feed";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Feed { stream: None, next: 0, done: false })
    }

    // Open the stream. The body arrives later, one chunk per unit of credit.
    #[handler::single]
    fn on_request(&mut self, _ctx: &mut WasmCtx<'_>, _req: HttpServerRequest) -> HttpRouterResult {
        self.next = 0;
        self.done = false;
        HttpRouterResult::Stream(HttpResponseStreamOpen { status: 200, headers: Vec::new() })
    }

    // Spend the granted credit, then terminate once the body is exhausted.
    // The first credit mail arms the stream handle — its counterparty is
    // whoever paced the stream, and every chunk flows back through it.
    #[handler::single]
    fn on_credit(&mut self, ctx: &mut WasmCtx<'_>, credit: HttpStreamCredit) {
        let stream = match self.stream {
            Some(stream) => stream,
            None => {
                let Some(sender) = ctx.sender() else {
                    return;
                };
                *self.stream.insert(ResponseStream::from_credit(sender, &credit))
            }
        };
        let mut budget = credit.credit;
        while budget > 0 && self.next < 100 {
            stream.chunk(ctx, format!("line {}\n", self.next).into_bytes());
            self.next += 1;
            budget -= 1;
        }
        if self.next >= 100 && !self.done {
            stream.end(ctx);
            self.done = true;
        }
    }
}
```

The buffered `HttpRouterResult::Response` path is unchanged — a handler that
returns it gets a single `Content-Length`-framed response exactly as before. Streaming is
purely opt-in per reply.

## Mixing buffered and streamed routes

"Stream one route, buffer the rest" is a single handler choosing between two
`HttpRouterResult` variants per request, returning whichever one the request
calls for:

```rust
use aether_actor::WasmCtx;
use aether_http::kinds::{HttpResponseStreamOpen, HttpRouterResult, HttpServerRequest, HttpServerResponse};

#[handler::single]
fn on_request(&mut self, _ctx: &mut WasmCtx<'_>, req: HttpServerRequest) -> HttpRouterResult {
    match req.path.as_str() {
        "/download" => HttpRouterResult::Stream(HttpResponseStreamOpen {
            status: 200,
            headers: Vec::new(),
        }),
        _ => HttpRouterResult::Response(HttpServerResponse {
            status: 404,
            headers: Vec::new(),
            body: b"not found".to_vec(),
        }),
    }
}
```

## Verify against current code

This recipe names the env keys and kind names live in the source. Before
following it, confirm `AETHER_HTTP_SERVER_ENABLED`, `HttpServerRequest`,
`HttpServerResponse`, `HttpRouterResult`, `HttpServerConfig`, the `--http-server-*` argv flags
(`cli_prefix = "http-server"` on `HttpServerConfig`), `RegisterRoute` /
`UnregisterRoute` / `HttpRouter` / `HttpMethod`, the `http::{router, route,
FromRequest, Ctx}` authoring surface, and `http::ResponseStream` still exist
where named — grep the crates, and if a name has drifted, fix the recipe as
part of your work.
