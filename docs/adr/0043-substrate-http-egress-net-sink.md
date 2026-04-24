# ADR-0043: Substrate HTTP egress via net sink

- **Status:** Proposed
- **Date:** 2026-04-24

## Context

Components today have no way to talk to the network. The substrate owns I/O (ADR-0041), GPU (render sink), audio (ADR-0039), and the wasm boundary — but there is no sink for outbound HTTP, and wasm components have no path to sockets of their own. That's fine for the pure-compute + local-filesystem surface the substrate has been to date, but three concrete forcing functions now push against it:

- **Asset generation pipeline.** The user's image-authoring pipeline moves into the monorepo (prompts + outputs staged through git/GH, generation done via an external image API). Running it on the headless chassis as components requires calling an HTTP API from inside a wasm module — something no shape of today's mail surface supports.
- **Third-party content APIs more broadly.** LLM calls, image APIs, speech synthesis, translation, map tiles, weather — the shape of "Claude-authored game tools" routinely wants to talk to remote services. Today that's only doable outside the substrate, breaking the "everything the engine needs runs on it" model.
- **GitHub / git-forge integration.** ADR-0034's hub direction wants the coordination plane to observe and act on PRs, issues, and CI state. Even with `gh` shelling out as a v1 stopgap (ADR discussion, pre-draft), eventually a hub-resident component will want structured GitHub API access — which is just HTTPS with auth.

Giving components a socket primitive of their own is a non-starter, for the same reason `std::fs` was rejected in ADR-0041: hands arbitrary network reach to untrusted code, defeats the "substrate owns I/O" invariant (ADR-0002), and wasm32 has no socket primitives at all. The path forward is a substrate-mediated HTTP sink — mail-shaped, consistent with the render / audio / io sinks already in place.

Permissioning (capability declarations in a wasm custom section, gated at `load_component`) is the next ADR after this one, not this one. The user's explicit direction is **HTTP first, capabilities next**. V1 ships the transport with a blunt env-var allowlist as a stopgap; the capabilities work layers on top without re-litigating the sink shape.

This ADR decides: transport (mail on a `"net"` sink), operation set (HTTP fetch only, v1), backend (`ureq`, blocking, on the sink dispatch thread), response/body caps, stopgap allowlist, and chassis coverage.

## Decision

### 1. Transport: mail on a `"net"` sink

One request kind and one reply kind, routed through the existing ADR-0013 reply-to-sender path. The substrate owns a `"net"` mailbox (short name per the recipient-name convention); components mail `aether.net.fetch`, the sink dispatches through the HTTP backend, the reply comes back to the originating sender.

```rust
aether.net.fetch : {
    url: String,
    method: HttpMethod,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    timeout_ms: Option<u32>,
}

enum HttpMethod { Get, Post, Put, Delete, Patch, Head, Options }

aether.net.fetch_result : Ok  { url: String, status: u16, headers: Vec<(String, String)>, body: Vec<u8> }
                        | Err { url: String, error: NetError }

enum NetError {
    InvalidUrl(String),
    Timeout,
    BodyTooLarge,
    AllowlistDenied,
    Disabled,           // AETHER_NET_DISABLE=1
    AdapterError(String),
}
```

`url` is echoed on every reply (ADR-0041's correlation pattern — echo the originating identifier + reply kind name is enough for most callers to match reply-to-request). `body` on the request echoes nothing back — same decision as `Write` in ADR-0041: a multi-MB upload should not round-trip its bytes. Components that need strict per-op correlation (same URL fired twice back-to-back, non-idempotent POST) already have it: ADR-0042 tags every `ReplyTo` with a per-component correlation id, and `prev_correlation_p32` exposes it to the guest. No per-kind correlation field needed.

`HttpMethod` is an enum, not a string. Strings on the wire invite "is `get` ≠ `GET` ≠ `Get`" surprises; enums get schema-validated at the hub's decode step and the guest SDK gets exhaustive matching for free.

`NetError` uses four typed variants for errors agents routinely need to branch on (timeout → retry, allowlist denied → config issue, body-too-large → chunk the response, disabled → surface to operator) plus an `AdapterError(String)` catchall for everything else — same shape as `IoError::AdapterError` in ADR-0041.

### 2. Backend: `ureq`, blocking, on the sink dispatch thread

The sink dispatches synchronously on one thread, one request at a time. Same model as the `io` sink in ADR-0041. Concretely:

- **Backend**: `ureq` (blocking HTTP client, rustls TLS, no tokio). Chosen over `reqwest` because `reqwest`'s default shape is async-first and drags tokio into the substrate; the blocking feature works but layers a runtime underneath the blocking façade. `ureq` is blocking by design and has no async runtime.
- **TLS**: rustls via `ureq`'s default. No cert pinning in v1; system roots are trusted.
- **Redirects**: ureq's default (follow up to N redirects). Not exposed to callers in v1; `max_redirects` is a later addition if needed.
- **Concurrency**: one request at a time on the sink dispatch thread. The second request queues. This is the same constraint as the `io` sink, and will hurt sooner than it did there (network latency is 10-100× disk), but shipping two concurrency models at once is worse than shipping one now and upgrading both together later. Multi-threaded sink dispatch is a separate ADR.

### 3. Body size cap

Response bodies over `AETHER_NET_MAX_BODY_BYTES` (default 16MB) produce `NetError::BodyTooLarge` without allocating the full body. `ureq`'s `Response::into_reader()` is read up to the cap + 1 byte; exceeding the cap aborts with the typed error. Request bodies are also capped at the same limit — a component attempting to POST a 100MB payload gets rejected before the wire touches it.

Streaming (chunked reads / chunked sends) is deferred. It ties to the CachedBytes / byte-handle work in an open design thread; the streaming reply shape almost certainly wants handles rather than inline `Vec<u8>`. V1 is buffered, request-response only.

### 4. Timeouts

Default timeout: 30 seconds. Per-request override via `timeout_ms: Option<u32>` on the fetch request. Env override for the default: `AETHER_NET_TIMEOUT_MS`. Exceeded timeouts produce `NetError::Timeout`.

Timeout applies to the whole request — connect + TLS + request send + response receive. Per-phase timeouts (connect-only, read-only) are deferred; the fetch shape doesn't preclude adding optional `connect_timeout_ms` later without breaking existing callers.

### 5. Stopgap allowlist (pre-capabilities)

Until the capabilities ADR lands, network egress is gated by a blunt substrate-level allowlist:

- `AETHER_NET_ALLOWLIST=host1,host2,host3` — comma-separated hostnames. Exact match; `*.example.com` wildcards are deliberately not parsed to keep the stopgap dumb. An empty or unset value means **no egress** (fetches return `NetError::AllowlistDenied`).
- `AETHER_NET_DISABLE=1` — skip backend construction entirely; every fetch returns `NetError::Disabled`. Useful for CI, test fixtures, and chassis builds that shouldn't touch the network.
- `AETHER_NET_REQUIRE_HTTPS=1` — reject `http://` URLs with `NetError::InvalidUrl("http scheme not allowed")`. Default off for v1 (the allowlist is the primary gate); defaulting on once capabilities land is plausible.

This is the opposite default from most HTTP libraries: v1 ships deny-by-default, not allow-all. The capabilities ADR will replace this with per-component declarations (each component names the hosts it expects to reach; operator sees the list at `load_component`; substrate enforces), at which point the env allowlist becomes a chassis-wide override or goes away entirely. The stopgap is intentionally crude — it exists so we don't ship an unrestricted network API during the capabilities gap, not because it's a good long-term shape.

### 6. Request-header policy

Headers pass through verbatim with two exceptions:

- `Host` in request headers is stripped and logged at warn. The substrate derives `Host` from the URL; letting components override it would bypass the allowlist (component requests allowlisted host A with `Host: B` header, TLS SNI says A, adapter receives the request, server vhost routes to B). Rejecting is simpler than validating.
- `User-Agent` is injected if not set by the caller (default: `aether/<version>`). Components that want a specific UA set it themselves; components that don't get a sensible default instead of whatever `ureq` picks.

`Content-Length` is managed by `ureq` and ignored if present in headers. All other headers (including `Authorization`, `Content-Type`, `Accept`) pass through unchanged.

### 7. Chassis coverage

- **Desktop chassis**: full net sink.
- **Headless chassis**: full net sink (the asset pipeline is the forcing function, and it runs headless). This matters: `io` and `net` are the two sinks that *must* work on headless, because content-authoring workloads run there.
- **Hub chassis**: no net sink in v1. The hub is a coordination plane, not an egress host. Mail to `"net"` on a hub substrate gets `NetError::AdapterError("net sink unavailable on hub chassis")` so callers fail loud instead of silent-dropping. Once hub-resident components become a thing (ADR-0034 Phase 2+) and the GitHub-integration use case becomes concrete, wiring the sink on the hub chassis is trivial — it's the same adapter code.

### 8. Boot-time overrides (summary)

- `AETHER_NET_DISABLE=1` — disable net sink entirely (nop-like behaviour; every fetch replies `Err::Disabled`).
- `AETHER_NET_ALLOWLIST=host1,host2,...` — comma-separated allowlist. Empty/unset = deny all.
- `AETHER_NET_MAX_BODY_BYTES=<n>` — response/request body cap. Default 16MB.
- `AETHER_NET_TIMEOUT_MS=<n>` — default per-request timeout. Default 30000.
- `AETHER_NET_REQUIRE_HTTPS=1` — reject `http://` URLs. Default off.

## Consequences

### Positive

- **Asset pipeline unblocked.** The image-authoring pipeline can run on the headless chassis with components for each stage (read prompt → call image API → write output), driven through MCP the same way any other component is driven. No out-of-band bin, no cross-process plumbing.
- **Consistent with existing sinks.** `net` joins `io`, `audio`, `render`, `camera` under one model: substrate-owned mailbox, mail in, reply out via the ADR-0013 path. Component authors do not learn a new shape.
- **Deny-by-default is the correct stopgap.** A net sink that requires an allowlist to reach anything is strictly safer than one that defaults open. The ergonomic cost is low (one env var per chassis-run); the security cost of getting it wrong (a component silently exfiltrating to an attacker-controlled host before capabilities land) is meaningful.
- **Synchronous-on-sink-thread matches `io`.** Two sinks with the same concurrency model is one model. When the multi-thread dispatch ADR lands, both sinks graduate together — no split-brain where half the substrate is sync-on-thread and half is pooled.
- **Structured errors where they matter.** Four typed `NetError` variants cover the branches agents actually care about (timeout, allowlist, oversize, disabled); `AdapterError(String)` keeps the long tail addressable without schema churn.

### Negative

- **Head-of-line blocking on the sink thread.** A slow remote (DNS timeout on a dead host, large download) stalls every subsequent fetch for up to the timeout. Already true for `io`, but network latency makes the pain more acute. Multi-threaded sink dispatch is the fix; deferring it keeps this ADR scoped but puts real friction on workloads with any parallelism.
- **Buffered bodies are wasteful at scale.** A 15MB image API response allocates ~15MB on the adapter side, postcard-encodes it, copies into component memory. Three copies minimum, same problem ADR-0041 has for large reads. Streaming via byte handles is the right fix; it's not in v1.
- **The stopgap allowlist is crude.** Host-level, exact-match, chassis-wide. A component that legitimately needs `api.openai.com` also gets `github.com` if `github.com` is in the allowlist. Per-component scoping is the capabilities ADR's job; living with chassis-wide until then is the tradeoff for shipping HTTP now.
- **Config surface grows again.** Five new env vars on day one. Every one is well-bounded, but ADR-0041 already added three, and the total "where does the substrate look for knobs" surface is now ~dozen env vars. TOML consolidation (parked in ADR-0041) gets more attractive with each addition.
- **`ureq` adds a dep with rustls + `webpki-roots`.** Not trivial — rustls pulls in ring or aws-lc-rs for crypto, and `webpki-roots` is ~150KB of baked-in CA certs. Acceptable cost for HTTPS; flagged because every substrate binary grows by the delta.

### Neutral

- **Host fn surface unchanged.** Mail-based, no new `_p32` imports. The net sink is a mailbox like any other — FFI is exactly what ships today.
- **Guest SDK unchanged in shape.** `Sink<K>` + `ctx.send(&net_sink, &Fetch { .. })` and a `#[handler]` for `FetchResult` work identically to every other reply-shaped sink. A sugar helper (`ctx.fetch(url) -> FetchResult` via ADR-0042's sync wait) is worth adding once the primitive lands but is not load-bearing for v1.
- **Kind manifest grows by two.** `aether.net.fetch` + `aether.net.fetch_result` land in `aether-kinds`. Data change, not a structural one.
- **Correlation is already handled.** Per ADR-0042, `ReplyTo` carries a per-component correlation id and `prev_correlation_p32` exposes it to guests. Fetch replies use the same machinery — no `correlation_id` field on the kind itself.

## Alternatives considered

- **Generic socket primitive (TCP/UDP).** Reject: 10× the surface area (connection lifecycle, read/write streams, protocol-agnostic framing) for use cases that are 100% HTTP today. If non-HTTP protocols become load-bearing (gRPC bidi-stream, MQTT, game-server UDP), a separate sink with a different wire makes more sense than overloading `net.fetch` with sub-protocols.
- **`reqwest` backend.** Reject: async-first, drags tokio as a transitive dep even with the blocking feature. `ureq` is blocking-native and produces a smaller dep graph. Reopen if HTTP/2 or QUIC performance becomes load-bearing.
- **Allow-by-default with opt-in denylist.** Reject: inverts the security default in the wrong direction for the capability gap. Deny-by-default with an explicit allowlist means the operator has to say "yes" to each host; the cost is trivial and the failure mode (fetch denied, clear error) is loud rather than silent.
- **Stream responses as byte handles (no inline `body` field).** Reject for v1: the byte-handle / CachedBytes design is still open (mid-discussion pre-ADR). Shipping `net` with an unfinished streaming shape invites two migrations (inline → handle, then handle-v1 → handle-v2); shipping inline now and handle-based streaming as a follow-up ADR is cleaner.
- **Fine-grained permissioning as part of this ADR.** Reject: user explicitly scoped as "HTTP first, capabilities next." A joint ADR would conflate transport decisions with sandbox policy; separating them means the capabilities ADR can cover all sinks (io, net, and future ones) uniformly rather than re-deciding per-sink.
- **Substrate-side retry / backoff policy.** Reject: components that want retry compose it in user-space (wrap `fetch` in a helper, handle `Timeout` with exponential backoff). Putting retry in the substrate forces every caller to live with whichever policy we pick; leaving it to user-space means `ctx.fetch_with_retry` is one helper crate away.
- **Named HTTP clients / connection pools per component.** Reject: adds state the substrate has to track, no clear win for the forcing functions. `ureq` manages connection reuse under the hood; explicit pooling is a later optimisation if profiling shows it matters.
- **JSON sugar (`FetchJson` kind that encodes/decodes JSON in the substrate).** Reject: component authors can serde_json in user-space. Substrate stays bytes-transparent; JSON is one of many possible body encodings and doesn't deserve wire privilege.

## Follow-up work

- **PR**: substrate-side — define the `net` sink, wire `ureq`-backed fetch dispatch on desktop + headless chassis, add the five env-var knobs, reject unknown allowlist hosts with `AllowlistDenied`, hub chassis returns the "unavailable" adapter error.
- **PR**: kinds — add `aether.net.fetch` + `aether.net.fetch_result` to `aether-kinds`, plus `HttpMethod` and `NetError`.
- **PR**: guest SDK ergonomics — `ctx.fetch(url, ...)` helper that wraps the send + `wait_reply_p32` (ADR-0042) so component authors don't hand-roll the envelope for the common request-response case.
- **ADR (next)**: capabilities in the wasm custom section. Replaces the env-var allowlist with per-component declarations; `load_component` surfaces requested capabilities to the operator; substrate enforces at mail dispatch. Covers `net` (hosts), `io` (namespaces), and any future sinks uniformly.
- **ADR (deferred)**: multi-threaded sink dispatch. Lifts head-of-line blocking on `net` and `io` together. Likely a worker pool scoped per sink with a small depth (2–4 threads), not a general-purpose executor.
- **Parked, not committed**: streaming request/response bodies via byte handles (ties to the CachedBytes design thread).
- **Parked, not committed**: HTTP/2 / HTTP/3 support. `ureq` is HTTP/1.1-only; reopen if a forcing function (gRPC, long-lived streams) demands it.
- **Parked, not committed**: per-phase timeouts (connect / read / write) as distinct fields on the fetch request.
- **Parked, not committed**: websocket / SSE support. Both are long-lived streams, both want a different reply shape than request-response fetch; a sibling kind or a separate sink.
- **Parked, not committed**: GitHub-specific API wrapper component. Uses `net` underneath; ships as user-space, not substrate.
