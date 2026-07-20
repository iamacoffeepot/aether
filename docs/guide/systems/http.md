# HTTP egress

> **Governing ADR:** [ADR-0043](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0043-substrate-http-egress-net-sink.md)
> (substrate HTTP egress). The contract — one request kind, one reply kind, an
> echo-correlated reply, a deny-by-default allowlist enforced on every
> redirect hop — is **stable**. One backend ships today (a blocking `ureq`
> adapter); the trait is built to take more.

An actor never opens a socket. Outbound network is a capability like every
other: a component's reach is exactly the mailboxes it can address, and the
network is one of them — mail `aether.http.fetch` to the `aether.http` mailbox
and handle the `aether.http.fetch_result` that comes back. The bytes cross the
mail boundary; the actor holds no connection, no client, no socket of its own.

When you drive the engine over MCP, `aether.http` is how a run reaches a remote
service — a content API, a forge endpoint, anything that speaks HTTP — without an
out-of-band process. When you author a component, it's a request/reply exchange
like any other sink: send a `Fetch`, handle the `FetchResult`.

## Why it exists

Handing a component a socket would defeat the split the whole engine is built
on. The substrate owns I/O so that a loaded actor's reach is bounded by the
mailboxes it can address; a raw socket hands arbitrary network access to
untrusted code and that boundary is gone. Portability pushes the same way — a
wasm guest has no socket primitives at all, so network access *has* to be a
request the substrate services on the guest's behalf. Routing it through mail
keeps egress identical in shape to the file, render, and audio sinks: one mental
model, one boundary, network reach gated behind the mail wall.

A component reaches this adapter only by mailing `aether.http`, and the
deployer controls which hosts pass its allowlist. That check is not limited to
the initial URL: the adapter runs its own bounded redirect-follow loop and
re-applies the allowlist (and the HTTPS requirement) to every hop, so a 3xx
that points at a host outside the allowlist fails with the same
`AllowlistDenied` a direct request to that host would.

## What it does

**One mailbox, one operation.** Everything addresses the `aether.http` mailbox.
A single request kind pairs with a single reply kind:

| Request | Fields | Reply | `Ok` carries |
|---|---|---|---|
| `aether.http.fetch` | `url`, `method`, `headers`, `body`, `timeout_ms` | `aether.http.fetch_result` | `status`, `headers`, `body` |

`Fetch` is the request: `url` (String), `method` (an `HttpMethod` enum —
`Get` / `Post` / `Put` / `Delete` / `Patch` / `Head` / `Options`), `headers` (a
list of name/value `HttpHeader` pairs), `body` (raw bytes), and `timeout_ms`
(`Option<u32>` — `None` uses the chassis default). The method is an enum on the
wire rather than a string, so `"get"` / `"GET"` / `"Get"` can't disagree across
guests; the substrate maps each variant to its canonical HTTP verb.

`FetchResult` is the reply — an `Ok` / `Err` enum. `Ok` carries the HTTP
`status`, the response `headers`, and the response `body`. `Err` carries an
`HttpError`. Both arms echo the originating `url`.

**Replies correlate by the echoed URL.** A handler dispatches on the reply
*kind*, which on its own erases *which* request a given reply answers — so both
arms echo the request's `url` to restore that. A caller matches a reply to its
request on the kind plus that echoed URL, with no correlation id field on the
kind itself. The request `body` is deliberately not echoed: correlation needs
the identity of the request, not its contents, so a multi-megabyte upload
produces a small reply. A caller that fires the same URL twice back-to-back (a
non-idempotent POST, say) leans on the per-source correlation id the substrate
already threads through replies rather than a per-kind field.

**`HttpError` is one of six shapes.** `InvalidUrl(String)` (unparseable URL,
no host, or — with HTTPS required — an `http://` scheme on the initial URL or
a redirect hop), `Timeout`
(the request exceeded its deadline), `BodyTooLarge` (request or response body
over the cap), `AllowlistDenied` (the initial host, or a redirect hop's host,
is not on the allowlist),
`Disabled` (egress is turned off chassis-wide), and `AdapterError(String)` (the catchall, preserving
backend detail like a DNS failure or TLS handshake error as free-form text). The
first five are precise enough to branch on — `Timeout` → reconcile effects and
retry only when the request is idempotent, `AllowlistDenied` → a config issue,
`BodyTooLarge` → use a smaller/bounded response, `Disabled` → surface to the
operator; the sixth keeps the long tail addressable without schema churn.

**Every hop is gated by a deny-by-default allowlist.** Before issuing a
request — the initial one or any redirect hop — the adapter parses the URL and
checks its host by exact string match; there is no wildcard parsing. An empty
or unset allowlist denies every host, and a miss returns `AllowlistDenied`
before that hop's request touches the wire. Redirects are not left to the
`ureq` backend's default follower: the adapter disables automatic following
and runs its own loop, bounded by a fixed redirect budget, re-checking the
allowlist and the `AETHER_HTTP_REQUIRE_HTTPS` scheme requirement on every
`Location` before following it. A redirect to a denied host or an `http://`
hop under `require_https` fails with the same error a direct request would;
a redirect that stays within the allowlist follows to its final response.
Credential-bearing headers (`Authorization`, `Cookie`, `Proxy-Authorization`)
are dropped when a hop crosses to a different host, mirroring the protection
`ureq`'s own follower gives. The knobs that set the allowlist — and the
response cap, timeout, and HTTPS requirement — are the worked `HttpConfig`
example on the [Configuration](configuration.md#adding-a-knob) page:

- `AETHER_HTTP_ALLOWLIST` — comma-separated hostnames allowed for a request.
  Empty or unset denies every request; the check runs on the initial URL and
  every redirect hop.
- `AETHER_HTTP_DISABLE` — turn egress off entirely; every fetch replies
  `HttpError::Disabled`. Useful for CI and fixtures that shouldn't touch the
  network.
- `AETHER_HTTP_REQUIRE_HTTPS` — reject an initial `http://` URL with
  `InvalidUrl`. Off by default; the allowlist is the primary gate.
- `AETHER_HTTP_MAX_BODY_BYTES` — cap on request *and* response body bytes.
  Default 16 MB. An oversize body on either side returns `BodyTooLarge`.
- `AETHER_HTTP_TIMEOUT_MS` — default per-request timeout. Default 30 s. A
  `Fetch` with a `timeout_ms` overrides it per request.

**Headers pass through, with two exceptions.** A caller-set `Host` header is
stripped and logged at warn — the substrate derives `Host` from the URL, and
letting a component override it would route the server-side vhost past the
allowlisted host the TLS handshake actually reached. `User-Agent` is injected as
`aether/<version>` when the caller doesn't set one. Every other header —
`Authorization`, `Content-Type`, `Accept` — passes through unchanged.

**One request at a time—and a known exception to the normal blocking rule.** The
adapter is backed by blocking `ureq` and currently runs on the capability's
dispatcher thread: a long fetch holds that actor's line until it finishes or
times out. The same limitation exists in the file sink. Do not copy this shape
into new capabilities; use the sanctioned offload/settlement primitives from
[Concurrency and blocking](concurrency.md). Moving these legacy sinks off the
dispatcher is a separate architectural correction.

The cap is wired on the desktop and headless chassis. The in-process substrate-harness
chassis omits it, so `aether.http` is not a registered mailbox there and mail to
it warn-drops like any unaddressed name.

### Where this page's authority ends: parallel egress paths

`aether.http` is the general-purpose egress mailbox, and the `AETHER_HTTP_*`
knobs gate it. They do **not** gate the engine's entire outbound surface. The
provider capabilities — `aether.anthropic` and `aether.gemini` — are parallel
egress paths registered alongside `aether.http`. Each carries its own HTTP
client and its own provider-specific configuration, and each dials out directly
rather than routing through `aether.http`. The allowlist, disable flag, body cap,
and timeout on this page apply to `aether.http` alone; a deployer locking down
egress reckons with the provider caps' own configuration separately. Those caps
are content generation, a subject of their own — out of scope here, named so you
know which mailbox this page governs and which it does not.

## How to use it

**From a component.** Address the cap by type. The facade lifts the two common
verbs to a typed call:

```rust
ctx.actor::<HttpCapability>().get("https://api.example.com/v1/status");
ctx.actor::<HttpCapability>().post("https://api.example.com/v1/ingest", &body);
```

These are fire-and-forget and use the chassis default timeout. The result
arrives later as its own mail, which you receive like any other kind:

```rust
#[handler::single]
fn on_fetch_result(&mut self, ctx: &mut WasmCtx<'_>, result: FetchResult) {
    match result {
        FetchResult::Ok { url, status, body, .. } => { /* url tells you which fetch */ }
        FetchResult::Err { url, error } => { /* branch on error */ }
    }
}
```

Because both arms echo `url`, a component with several fetches outstanding tells
them apart by the echoed URL — match it against whatever state you were waiting
to fill. For a request that needs custom headers, a method beyond GET/POST, a
body on a non-POST, or a per-request timeout, send the `Fetch` kind directly:
`ctx.actor::<HttpCapability>().send(&Fetch { url, method, headers, body, timeout_ms })`.
The kinds live with the capability in
`aether-http/src/kinds.rs` (ADR-0121).

**From an agent over MCP.** `send_mail` rides settlement and hands back the
correlated reply, so a fetch is a single call: mail `aether.http.fetch` to
`aether.http` and the `FetchResult` comes back with it — no polling. The host
in the initial URL has to be on the chassis allowlist (`AETHER_HTTP_ALLOWLIST` at
`spawn_substrate` time) or the reply is `AllowlistDenied`. `describe_kinds`
carries the exact param schema for `Fetch` if you need it.

## How to extend or reuse it

The seam is the backend trait. A new backend is an implementation of
`HttpAdapter` — one method, `fetch`, taking a validated request and returning the
response or a typed `HttpError`. The adapter owns allowlist enforcement (on the
initial URL and every redirect hop), URL validation, redirect behavior, the
body cap, and timeout application; the cap just moves bytes between the wire
and the adapter. The
`ureq` adapter is the one that ships and is
the reference for what an adapter is expected to enforce. The mail surface
doesn't change when the backend does — a component sending `Fetch` is untouched.

The allowlist is a chassis-wide stopgap ahead of per-component capability
declarations. A component that legitimately needs one host also reaches every
other host on the list. Narrowing that to per-component scope is future
capabilities work, tracked in ADR-0043's follow-ups. Streaming bodies (chunked
reads and sends, rather than the buffered `Vec<u8>` today) are deferred there
too — they tie to the byte-handle design.

## Where to read more

- The transport decision, the backend choice, the body cap and timeout
  rationale, and the stopgap allowlist —
  [ADR-0043](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0043-substrate-http-egress-net-sink.md).
- The `AETHER_HTTP_*` knobs as a worked `Config` derive, and how a per-spawn
  layer reaches them — [Configuration](configuration.md).
- Why a single `send_mail` returns the fetch's reply — the settlement contract on
  [Tracing & settlement](tracing-and-settlement.md), and the tool surface on
  [The MCP harness](../mcp-harness.md).
- How a component receives a reply kind in a `#[handler::<class>]` —
  [Components & lifecycle](components.md).
