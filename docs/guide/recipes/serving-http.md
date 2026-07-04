# Serving HTTP from a component

**Class: recompile.** You're writing a wasm component that handles inbound
HTTP requests — `cargo` plus the pre-flight loop. The `aether.http.server`
capability (ADR-0108) binds the listening socket; you write the handler that
receives `aether.http.server.request` and replies
`aether.http.server.response`.

## 1. Configure the server

The HTTP server is opt-in (off by default). Set `AETHER_HTTP_SERVER_ENABLED=1`
to turn it on, and point it at the component mailbox your handler will register
at:

```sh
AETHER_HTTP_SERVER_ENABLED=1 \
AETHER_HTTP_SERVER_BIND_ADDR=127.0.0.1:8080 \
AETHER_HTTP_SERVER_HANDLER_MAILBOX=aether.component/aether.embedded:web \
cargo run -p aether-substrate-bundle --bin aether-substrate-headless
```

`AETHER_HTTP_SERVER_BIND_ADDR` defaults to `127.0.0.1:8080`; use port `0` to
let the OS pick a free port. `AETHER_HTTP_SERVER_HANDLER_MAILBOX` is the late-
bound mailbox name (ADR-0108 §3): the server resolves it at dispatch time, so
the handler component can load or reload without restarting the server.

## 2. Write the handler

A handler is a wasm component with one `#[handler]` for
`aether.http.server.request`. It replies `aether.http.server.response` with a
status code, optional headers, and a byte body. The server writes the formatted
HTTP/1.1 response to the client socket and closes the connection.

```rust
use aether_actor::{ActorInitError, WasmActor, WasmCtx, OutboundReply, Resolver, actor};
use aether_kinds::{HttpServerRequest, HttpServerResponse};

pub struct Web;

#[actor]
impl WasmActor for Web {
    const NAMESPACE: &'static str = "web";

    fn init<C: Resolver>(_ctx: &mut C) -> Result<Self, ActorInitError> {
        Ok(Web)
    }

    #[handler]
    fn on_request(&mut self, ctx: &mut WasmCtx<'_>, req: HttpServerRequest) {
        let (status, body): (u16, &[u8]) = match req.path.as_str() {
            "/" => (200, b"hello"),
            _ => (404, b"not found"),
        };
        ctx.reply(&HttpServerResponse {
            status,
            headers: Vec::new(),
            body: body.to_vec(),
        });
    }
}

aether_actor::export!(Web);
```

The component registers at `aether.component/aether.embedded:web` (its
`NAMESPACE` const rendered through the ADR-0099 lineage), which is the same
address you put in `AETHER_HTTP_SERVER_HANDLER_MAILBOX`. `req.peer_addr`
carries the connecting client's address (`ip:port`, IPv6 bracketed) for
logging, rate-limiting, or allowlisting.

## 3. Load the handler

Load the handler component with `load_component` over the MCP harness once the
substrate is up:

```jsonc
// load_component
{
  "engine_id": "<engine>",
  "binary_path": "/path/to/web.wasm"
}
```

`load_component` replies `LoadResult.Ok` with the registered mailbox name
(`aether.component/aether.embedded:web`). After that, any inbound HTTP request
on the bound port routes to your handler.

## 4. Send a request

From a shell, or from any HTTP client that speaks HTTP/1.1:

```sh
curl http://127.0.0.1:8080/
# → hello
```

The server reads the request, dispatches `aether.http.server.request` to the
handler mailbox, waits for the `aether.http.server.response` reply, and writes
the formatted response to the client. The server adds `Connection: close` and
an appropriate `Content-Length` header; your handler sets the status code,
optional extra headers, and the body.

## What happens when the handler doesn't reply

If the handler receives the request but returns without calling `ctx.reply`, the
settled chain triggers the `502 Bad Gateway` safety net. If the handler takes
longer than `AETHER_HTTP_SERVER_REQUEST_TIMEOUT_MILLIS` (default 30 000 ms), the
server sends `504 Gateway Timeout`. A missing handler mailbox (nothing loaded
yet) returns `503 Service Unavailable`.

## Adding response headers

Pass a `Vec<HttpHeader>` in the reply:

```rust
use aether_kinds::HttpHeader;

ctx.reply(&HttpServerResponse {
    status: 200,
    headers: vec![HttpHeader {
        name: "content-type".to_string(),
        value: "application/json".to_string(),
    }],
    body: br#"{"ok":true}"#.to_vec(),
});
```

The server sends these after its own `Connection: close` and `Content-Length`
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
scheduler, and the handler cannot outrun the socket:

```rust
use aether_capabilities::http::HttpServerCapability;
use aether_capabilities::http::kinds::{
    HttpResponseChunk, HttpResponseStreamEnd, HttpResponseStreamOpen, HttpStreamCredit,
};

pub struct Feed {
    stream_id: u64,
    next: u32,
    done: bool,
}

#[actor]
impl WasmActor for Feed {
    const NAMESPACE: &'static str = "feed";

    fn init<C: Resolver>(_ctx: &mut C) -> Result<Self, ActorInitError> {
        Ok(Feed { stream_id: 0, next: 0, done: false })
    }

    // Open the stream. The body arrives later, one chunk per unit of credit.
    #[handler]
    fn on_request(&mut self, _ctx: &mut WasmCtx<'_>, _req: HttpServerRequest) -> HttpResponseStreamOpen {
        self.next = 0;
        self.done = false;
        HttpResponseStreamOpen { status: 200, headers: Vec::new() }
    }

    // Spend the granted credit, then terminate once the body is exhausted.
    // The handler learns its `stream_id` from the first credit mail.
    #[handler]
    fn on_credit(&mut self, ctx: &mut WasmCtx<'_>, credit: HttpStreamCredit) {
        self.stream_id = credit.stream_id;
        let mut budget = credit.credit;
        while budget > 0 && self.next < 100 {
            ctx.actor::<HttpServerCapability>().send(&HttpResponseChunk {
                stream_id: self.stream_id,
                body: format!("line {}\n", self.next).into_bytes(),
            });
            self.next += 1;
            budget -= 1;
        }
        if self.next >= 100 && !self.done {
            ctx.actor::<HttpServerCapability>().send(&HttpResponseStreamEnd {
                stream_id: self.stream_id,
            });
            self.done = true;
        }
    }
}
```

The buffered `HttpServerResponse` path is unchanged — a handler that replies it
gets a single `Content-Length`-framed response exactly as before. Streaming is
purely opt-in per reply.

## Verify against current code

This recipe names the env keys and kind names live in the source. Before
following it, confirm `AETHER_HTTP_SERVER_ENABLED`, `HttpServerRequest`,
`HttpServerResponse`, and `HttpServerConfig` still exist where named — grep the
crates, and if a name has drifted, fix the recipe as part of your work.
