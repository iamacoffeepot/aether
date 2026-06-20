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
use aether_actor::{BootError, WasmActor, WasmCtx, OutboundReply, Resolver, actor};
use aether_kinds::{HttpServerRequest, HttpServerResponse};

pub struct Web;

#[actor]
impl WasmActor for Web {
    const NAMESPACE: &'static str = "web";

    fn init<C: Resolver>(_ctx: &mut C) -> Result<Self, BootError> {
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
address you put in `AETHER_HTTP_SERVER_HANDLER_MAILBOX`.

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

## Verify against current code

This recipe names the env keys and kind names live in the source. Before
following it, confirm `AETHER_HTTP_SERVER_ENABLED`, `HttpServerRequest`,
`HttpServerResponse`, and `HttpServerConfig` still exist where named — grep the
crates, and if a name has drifted, fix the recipe as part of your work.
