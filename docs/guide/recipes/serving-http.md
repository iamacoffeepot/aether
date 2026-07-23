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
cargo run -p aether-chassis-headless --bin aether-substrate-headless
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
`aether.http.server.request`. It replies `aether.http.server.response` with a
status code, optional headers, and a byte body. The server writes the formatted
HTTP/1.1 response to the client socket. On HTTP/1.1 the connection is kept alive
by default and serves the next request on the same socket; a client that sends
`Connection: close` (and HTTP/1.0, which closes by default) terminates it, and
an idle kept-alive connection is closed after `keep_alive_timeout_millis`.

```rust
use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_http::HttpServerCapability;
use aether_http::kinds::{HttpServerRequest, HttpServerResponse, RegisterRouteSelf};
use aether_data::Kind as _;

pub struct Web;

#[actor]
impl WasmActor for Web {
    const NAMESPACE: &'static str = "web";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Web)
    }

    // Claim the `/` catch-all so every request dispatches here. Register a
    // narrower prefix instead (see §Claiming routes) to own just one path
    // family and leave the rest to other handlers.
    fn wire(&mut self, ctx: &mut WireCtx<'_, '_>) {
        ctx.actor::<HttpServerCapability>().send(&RegisterRouteSelf {
            prefix: "/".to_string(),
            method: None,
            kind: HttpServerRequest::ID,
            shared: false,
        });
    }

    #[handler::single]
    fn on_request(&mut self, _ctx: &mut WasmCtx<'_>, req: HttpServerRequest) -> HttpServerResponse {
        let (status, body): (u16, &[u8]) = match req.path.as_str() {
            "/" => (200, b"hello"),
            _ => (404, b"not found"),
        };
        HttpServerResponse {
            status,
            headers: Vec::new(),
            body: body.to_vec(),
        }
    }
}

aether_actor::export!(Web);
```

`#[handler::single]` replies by *returning* its kind, as above.
`#[handler::manual]` opts into the `Manual` ctx (`WasmCtx<'_, Manual>`)
whose `ctx.reply(&…)` sends the reply explicitly — reach for it when one handler
needs to reply one of *several* kinds (see "Mixing buffered and streamed routes"
below), since a single return type can't express that choice.

The component registers at `aether.component/aether.embedded:web` (its
`NAMESPACE` const rendered through the ADR-0099 lineage). Its `wire` hook
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

`load_component` replies `LoadResult.Ok` with the registered mailbox name
(`aether.component/aether.embedded:web`). After that, any inbound HTTP request
on the bound port routes to your handler.

## 5. Send a request

From a shell, or from any HTTP client that speaks HTTP/1.1:

```sh
curl http://127.0.0.1:8080/
# → hello
```

The server reads the request, dispatches `aether.http.server.request` to the
handler mailbox, waits for the `aether.http.server.response` reply, and writes
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
use aether_http::kinds::{HttpServerRequest, RegisterRouteSelf};
use aether_data::Kind as _;

fn wire(&mut self, ctx: &mut WireCtx<'_, '_>) {
    ctx.actor::<HttpServerCapability>().send(&RegisterRouteSelf {
        prefix: "/api".to_string(),
        method: None,                        // or Some(HttpMethod::Get)
        kind: HttpServerRequest::ID,
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
Routes follow the component: they survive `replace_component` (the mailbox id
is stable) and are released automatically when the component drops, or
explicitly via `aether.http.server.unregister_route_self`. External callers
(an MCP session, a test) use the `register_route` / `unregister_route` forms,
which name the handler mailbox explicitly.

The `kind` field names the kind the route's requests dispatch as.
`HttpServerRequest::ID` keeps the generic shape. Registering a route-specific
kind — a struct with `aether.http.server.request`'s fields under its own
`#[kind(name = …)]` — routes each prefix to its own `#[handler::single]`, with its own
`describe_component` entry and `actor_cost` row; the payload bytes are always
request-shaped, so the route kind decodes them directly.

### Registering a route for another mailbox

`register_route_self` resolves the registrant from the sender's in-process
`Source`; an MCP session or a test has no such source, so it uses the named
form instead — `register_route` / `unregister_route`, which take the target
`mailbox` explicitly. `RegisterRoute` carries `prefix`
(`String`), `method` (`Option<HttpMethod>` — a bare variant string like
`"Get"`, or `null` to match every method; the seven variants are `Get`,
`Post`, `Put`, `Delete`, `Patch`, `Head`, `Options`), `kind` (the route's
request `KindId`), and `mailbox` (the handler's `MailboxId`). Over the MCP
wire both tagged ids render as ADR-0064 strings — `knd-…` and `mbx-…` — so
the values below come from `describe_kinds` (the `kind` for
`aether.http.server.request`, or a route-specific kind's own id) and from
`load_component`'s `LoadResult.name` (the `mailbox`, once resolved through
`describe_component` or the same tagged form it's already returned in):

```jsonc
// send_mail → aether.http.server  (kind: aether.http.server.register_route)
{
  "prefix": "/api",
  "method": "Get",
  "kind": "knd-…",     // aether.http.server.request's id, from describe_kinds
  "mailbox": "mbx-…"   // the handler's mailbox, from load_component's LoadResult
}
```

The reply is `aether.http.server.register_route_result` — `"Ok"` or
`{ "Err": { "error": "…" } }` — the same shape `register_route_self` replies,
which is *why* the named form exists: an external caller (an MCP session, a
test) has no in-process `Source` to resolve, so `register_route_self` always
answers it `Err`.

Releasing the route mirrors the registration, dropping `kind` (a release
doesn't need it) and keeping `method` so a method-specific route and a
method-agnostic route at the same prefix release independently:

```jsonc
// send_mail → aether.http.server  (kind: aether.http.server.unregister_route)
{
  "prefix": "/api",
  "method": "Get",
  "mailbox": "mbx-…"
}
```

## Typed route authoring

The typed surface writes that whole registration for you (ADR-0131). Put
`#[http::router]` on the actor's impl block, above `#[actor]`, and
`#[http::route(<Method|any>, "<prefix>")]` on a method; the macros mint the
route's request-shaped kind, inject the `register_route_self` send into `wire`,
and turn the method into the route's `#[handler::single]`. A routed method takes an
`http::Ctx<'_, C>` — the transport ctx (`WasmCtx` here) plus the request and
matched route, dereffing to the ctx so mail sends read as usual — and returns
`HttpServerResponse`:

```rust
use aether_http as http;
use aether_http::kinds::{HttpServerRequest, HttpServerResponse};

#[http::router]
#[actor]
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

Drop to the raw `register_route_self` surface above for a streaming route
(`HttpResponseStreamOpen`) — the typed surface returns `HttpServerResponse`, so
a streamed response keeps its own hand-written `#[handler::single]`.

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

If the handler receives the request but returns without calling `ctx.reply`, the
settled chain triggers the `502 Bad Gateway` safety net. If the handler takes
longer than `AETHER_HTTP_SERVER_REQUEST_TIMEOUT_MILLIS` (default 30 000 ms), the
server sends `504 Gateway Timeout`. A request matching no route (nothing has
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

A handler that serves a large download or a long-lived event stream replies
`HttpResponseStreamOpen` in place of `HttpServerResponse`, then emits the body
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
credit grant that triggered it. Reading the dispatch's sender needs the `Manual`
ctx, so the credit handler is `#[handler::manual]`:

```rust
use aether_actor::{Manual, WasmCtx, WasmInitCtx};
use aether_http::ResponseStream;
use aether_http::kinds::{
    HttpResponseStreamOpen, HttpServerRequest, HttpStreamCredit,
};

pub struct Feed {
    // The stream this handler is feeding, captured from the first credit mail.
    stream: Option<ResponseStream>,
    next: u32,
    done: bool,
}

#[actor]
impl WasmActor for Feed {
    const NAMESPACE: &'static str = "feed";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Feed { stream: None, next: 0, done: false })
    }

    // Open the stream. The body arrives later, one chunk per unit of credit.
    #[handler::single]
    fn on_request(&mut self, _ctx: &mut WasmCtx<'_>, _req: HttpServerRequest) -> HttpResponseStreamOpen {
        self.next = 0;
        self.done = false;
        HttpResponseStreamOpen { status: 200, headers: Vec::new() }
    }

    // Spend the granted credit, then terminate once the body is exhausted.
    // The first credit mail arms the stream handle — its counterparty is
    // whoever paced the stream, and every chunk flows back through it.
    #[handler::manual]
    fn on_credit(&mut self, ctx: &mut WasmCtx<'_, Manual>, credit: HttpStreamCredit) {
        let stream = match self.stream {
            Some(stream) => stream,
            None => match ResponseStream::from_credit(ctx, &credit) {
                Some(stream) => *self.stream.insert(stream),
                None => return,
            },
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

The buffered `HttpServerResponse` path is unchanged — a handler that replies it
gets a single `Content-Length`-framed response exactly as before. Streaming is
purely opt-in per reply.

## Mixing buffered and streamed routes

"Stream one route, buffer the rest" is a single handler choosing between two
reply kinds per request — a `#[handler::single]`'s return type can only be one
kind, so this is exactly the case that needs `#[handler::manual]` and its
`Manual` ctx, whose `ctx.reply(&…)` sends the reply explicitly instead of
returning it:

```rust
use aether_actor::{Manual, OutboundReply, WasmCtx};
use aether_http::kinds::{HttpResponseStreamOpen, HttpServerRequest, HttpServerResponse};

#[handler::manual]
fn on_request(&mut self, ctx: &mut WasmCtx<'_, Manual>, req: HttpServerRequest) {
    match req.path.as_str() {
        "/download" => ctx.reply(&HttpResponseStreamOpen {
            status: 200,
            headers: Vec::new(),
        }),
        _ => ctx.reply(&HttpServerResponse {
            status: 404,
            headers: Vec::new(),
            body: b"not found".to_vec(),
        }),
    }
}
```

Use `#[handler::manual]` + `ctx.reply` when a single route chooses between
reply shapes; return from a `#[handler::single]` otherwise.

## Verify against current code

This recipe names the env keys and kind names live in the source. Before
following it, confirm `AETHER_HTTP_SERVER_ENABLED`, `HttpServerRequest`,
`HttpServerResponse`, `HttpServerConfig`, the `--http-server-*` argv flags
(`cli_prefix = "http-server"` on `HttpServerConfig`), `RegisterRoute` /
`UnregisterRoute` / `HttpMethod`, the `http::{router, route, FromRequest,
Ctx}` authoring surface, and `http::ResponseStream` still exist where named —
grep the crates, and if a name has drifted, fix the recipe as part of your
work.
