# ADR-0060: Guest-side logging via mail sink

- **Status:** Proposed
- **Date:** 2026-04-27

## Context

Wasm guests have no path to the substrate's `engine_logs` ring (ADR-0023).
The `tracing` subscriber installed by `aether-substrate-core::log_capture`
lives host-side and only sees events emitted from native code. Inside the
wasm sandbox, `tracing::warn!` invokes a subscriber that doesn't exist;
the macro is effectively a no-op. The host-fn surface
(`crates/aether-substrate-core/src/host_fns.rs`) currently exposes five
imports — `send_mail_p32`, `reply_mail_p32`, `save_state_p32`,
`wait_reply_p32`, `prev_correlation_p32` — none of which carry log
records.

This is not a hypothetical gap:

- **Issue 317** asks for `tracing::warn!` on DSL parse / mesh failures
  in `aether-mesh-editor-component`. The proposed fix as drafted
  silently no-ops because the subscriber doesn't exist on the wasm
  side.
- **`aether-static-mesh-component/src/lib.rs:25`** literally documents
  the gap: *"tracing is parked until the SDK exposes a logging
  facility."*
- A black-screen capture from the mesh editor's iteration loop is
  currently indistinguishable from a subscription failure or a wrong
  sink name. The legibility cost compounds with every silent
  swallow site across in-repo components.

The shape question — host fn vs mail vs hybrid — was settled by the
existing system's pattern. The five host fns above exist for things that
**cannot** be expressed as mail: bootstrapping mail itself
(`send_mail_p32`), blocking the guest thread (`wait_reply_p32`), and
substrate-internal state (`save_state_p32`, `prev_correlation_p32`).
Every other primitive in the engine — `DrawTriangle`, `aether.audio.*`,
`aether.io.*`, `aether.camera`, `aether.tick`, `aether.control.*` — is
mail. Logging *can* be expressed as mail; making it a sixth host fn
would be the odd one out, not mail-as-log.

Three forces tilt the same direction:

- **Volume.** Per-tick `DrawTriangle` already moves orders of magnitude
  more bytes through the mail path than any plausible log volume. The
  DSL mesh editor replays its full triangle cache every tick. If that
  works, debug-tier logs at <100/sec/component are noise.
- **Chassis flexibility.** Mail is universal substrate infrastructure
  (ADR-0035 puts it in `aether-substrate-core`, every chassis carries
  it). A sink is per-chassis: desktop / headless wire it to
  `tracing::event!`; hub doesn't and bubbles to its parent (ADR-0037);
  a future alternate chassis could write to a file or forward to a
  remote collector. Host-fn locks every chassis into the same log
  behavior at the ABI level.
- **ABI minimalism.** The host-fn count is the substrate's permanent
  ABI commitment. Five is the current line; six widens it forever.

`tracing::Subscriber.enabled()` is the lever that makes the perf
question moot. The macro consults `enabled()` *before* formatting the
message, allocating a `String`, or doing anything else. A subscriber
that returns `false` for trace and debug events at compile time
short-circuits the entire pipeline at the `tracing::trace!` call site —
no FFI, no mail, vtable-call-and-out. The mail path only runs for
events the component cares about.

## Decision

Add guest-side logging as mail, with a substrate-owned sink:

### Wire shape

```rust
// aether-kinds/src/lib.rs
#[derive(Kind, Schema, serde::Serialize, serde::Deserialize, Debug, Clone)]
#[kind(name = "aether.log")]
pub struct LogEvent {
    /// 0 = trace, 1 = debug, 2 = info, 3 = warn, 4 = error.
    pub level: u8,
    /// Module-style target. Substrate-side EnvFilter matches against
    /// this. Defaults to the guest's crate name; overridable per
    /// `tracing::event!` call site.
    pub target: alloc::string::String,
    /// Pre-formatted message. The guest does the `format_args!` work;
    /// structured fields collapse into the message body for v1 in
    /// fields-first form, matching `tracing-subscriber`'s default fmt
    /// layer (`tracing::warn!(error = %e, count = 3, "parse failed")`
    /// becomes `"error=<Display of e> count=3 parse failed"`).
    /// Capped at 4096 bytes by the SDK; oversize messages are
    /// truncated with a `" [truncated]"` suffix before send.
    pub message: alloc::string::String,
}
```

`LogEvent` is postcard-encoded (variable-size fields, not
`#[repr(C)]`), addressed to the substrate-owned mailbox
`aether.sink.log`.

### Sink behavior

`aether.sink.log` is registered at chassis boot — desktop and
headless wire it to a handler that:

1. Decodes the postcard `LogEvent`.
2. Maps `level` to a `tracing::Level`.
3. Checks the chassis's existing `EnvFilter` (`AETHER_LOG_FILTER`)
   against the decoded `target` and level. Filtered events return
   without further work.
4. Emits a `tracing::event!` at the matched level with the message
   and target. The existing `log_capture` subscriber records the event
   into the ring, where MCP `engine_logs` reads it.

The hub chassis does not register the sink. The shipped chassis
(desktop, headless) all do, so guest log mail rarely bubbles in
practice — the bubble-up path is the same one any unhandled mail
would take (ADR-0037), not a load-bearing log-aggregation feature.
A standalone substrate with no parent and no local sink warn-drops
the mail as unknown mailbox.

### SDK side

`aether-component` implements `tracing::Subscriber` for a small
`MailSubscriber` type and installs it as the global default during
the SDK's `init` walker:

```rust
// In aether-component, called once per component before user init().
let _ = tracing::subscriber::set_global_default(MailSubscriber::new());
```

`MailSubscriber::enabled` short-circuits below the configured
max level (default `Level::INFO` for v1). For events that pass:

1. Format the message inline. `tracing::field::Visit` walks the
   event's fields and `Display`-formats them into the message string
   in fields-first order — `key1=val1 key2=val2 message_body`. This
   matches `tracing-subscriber`'s default fmt layer so a reader of
   `engine_logs` sees the same shape as native tracing output.
2. Truncate the message to 4096 bytes if needed, suffixing
   `" [truncated]"`. Bounds the per-mail upper size so a misbehaving
   component can't queue megabyte frames.
3. Build a `LogEvent { level, target, message }`.
4. `send_mail` it to `aether.sink.log` (the recipient id is a
   compile-time const — `mailbox_id_from_name("aether.sink.log")`
   per ADR-0029).

Existing `tracing::warn!(...)` / `error!(...)` / `info!(...)` calls in
guest code now Just Work. The `tracing` (not `tracing-subscriber`)
facade is `no_std`-friendly; the SDK's `MailSubscriber` is hand-rolled
and does not pull `tracing-subscriber`.

The `aether-static-mesh-component/src/lib.rs:25` "tracing is parked"
comment becomes a no-op delete. Issue 317's mesh-editor warn-on-parse
becomes the `tracing::warn!(...)` form the issue body originally
proposed, now actually reaching `engine_logs`.

### Default level and per-component filter

For v1, `MailSubscriber`'s max level is hardcoded to `Level::INFO`.
Trace and debug events are dropped at `enabled()` with no FFI cost.
Components author at info / warn / error and get them in `engine_logs`
unconditionally.

Per-component dynamic filter — pushing `AETHER_LOG_FILTER`'s
per-target levels down to the guest at load time via a control mail
(`aether.control.set_log_level { level }`) — is deferred. The wire
kind and host-side derivation can land later without breaking
the v1 contract.

## Consequences

- **`engine_logs` becomes complete.** A black-screen capture from the
  mesh editor now shows the parse error inline. Black box → grey box.
- **Static-mesh, mesh-editor, and any future component can log.** The
  parked-tracing comment in static-mesh deletes; mesh-editor's silent
  `let Ok(...) else { return; }` swallows convert to `tracing::warn!`
  forms.
- **No new host-fn surface.** ABI commitment stays at 5. Future
  guest-targeted needs (e.g. trace context propagation, remote
  collectors) compose by adding kinds and sinks rather than imports.
- **Chassis-flexible by construction.** A future chassis variant
  routing logs to a file, to an OpenTelemetry collector, or to a
  custom in-process supervisor is a sink-wiring change, not an ABI
  change. Today's shipped chassis (desktop, headless) both wire to
  the same `tracing::event!` path; the flexibility is latent until
  someone takes it.
- **Subscriber dep added to `aether-component`.** The `tracing` facade
  (not `tracing-subscriber`). Manageable; `tracing` is widely used
  and `no_std`-compatible at the facade tier.
- **Per-call cost is dominated by `enabled()`.** For events at or
  above the configured level the cost is `format!` + postcard encode +
  send_mail FFI — comparable to a `DrawTriangle` send. For events below
  the level, the cost is one vtable call. Same shape `tracing` already
  has on native targets.
- **Production builds can opt into compile-time level elimination.**
  `tracing` exposes `STATIC_MAX_LEVEL` checked at the macro-expansion
  site, controllable via cargo features (`tracing/release_max_level_info`,
  `release_max_level_warn`, etc.). Component crates that want
  truly-zero-cost trace/debug skipping in release wasm should set the
  feature in their own `Cargo.toml` — neither the SDK nor the substrate
  needs to know. The runtime `LevelFilter` from `MailSubscriber` is the
  v1 default; the cargo feature is the production tightening for
  components that benchmark out a hot-path cost.
- **Bootstrap is fine.** `MailSubscriber` is installed during the SDK
  init walker before user `init()` runs. The walker already emits
  `aether.control.subscribe_input` mail, so the mail path is
  proven-working from the same call site. Logging from inside `init`
  works.
- **Structured fields are lossy in v1.** A `tracing::warn!(error = %e,
  count = 3, "...")` becomes a single message string. Adequate for
  human / Claude consumption; insufficient for programmatic field
  access. Strictly additive to fix later (a separate kind, or a
  `fields: Vec<(String, String)>` field on `LogEvent`).
- **Forces a forward decision on log volume backpressure.** If a
  component log-storms, the mail path queues and the log sink actor
  serializes. Drop-on-overflow vs block-the-emitter is a sink-handler
  policy choice; the v1 sink can drop with a substrate-side
  `warn!("dropped log mail under load")` if it ever becomes pressure.
  Not a v1 issue, but flagged for the implementation phase.
- **Throughput escape hatch tracked separately.** If the mail path
  becomes a measured bottleneck — not a hypothetical one — issue 326
  captures the forcing-function review (batched send within a
  dispatch first; host-fn fallback only if batching also doesn't
  close the gap).

## Alternatives considered

**Host-fn `aether::log_p32(level, target_ptr, target_len, msg_ptr,
msg_len) -> u32`.** Synchronous, no queue, lowest per-call latency —
direct FFI into a substrate-side `tracing::event!`. Rejected because
the existing host-fn principle is *only* for things mail can't express,
and logs can. Adopting host-fn here would break the pattern, lock every
chassis into the same log behavior at the ABI level, and add a sixth
permanent ABI commitment to save microseconds per call that don't
matter at the volume tracing is filtered to. The hot-path argument
(trace-tier in tight loops) is fully addressed by `Subscriber.enabled()`
short-circuiting before any FFI happens.

**Reply-on-failure / fail-via-mail-result.** The mesh editor's
`set_text` mail returns a `SetTextResult { ok, error }` reply; only
the sender of the request sees the failure. Rejected as the primary
mechanism: it does not surface async errors (`set_path` reads from
`aether.sink.io` then meshes on reply — the sender is long gone), it
does not feed `engine_logs` for human-eyes debugging, and it doesn't
generalize to non-request-shaped components. Reasonable as an addition
on top, not as a replacement.

**Hardcoded substrate-side filter only (no guest-side `enabled()`).**
Every `tracing::trace!` crosses FFI even when filtered. Rejected:
trace-tier in tight loops dominates wasm CPU; tracing's existing
`enabled()` mechanism makes guest-side filtering free at the call
site. The substrate-side filter still runs as a backstop for events
the guest's filter let through.

**Compile-time feature flag for log levels.** `cargo build --features
trace-logs` raises the level; default builds skip trace at compile
time. Rejected: defeats the dynamic-debug ergonomic that's the whole
point of `tracing`. Changing tier mid-session requires a rebuild and
a reload.

**Per-call structured fields encoded as postcard map.** Preserves
`tracing`'s field structure on the wire; substrate side reconstructs
a `tracing::Event` with fields. Parked, not rejected. The v1 wire
collapses fields into the message string, which is adequate for
human / Claude consumption. The parked option becomes attractive
only if a programmatic consumer (a structured-log indexer, a
log-driven test fixture) needs typed access to fields. Strictly
additive — adding a `fields` field to `LogEvent` is a new
`Kind::ID` (schema hash changes per ADR-0030), but the v1 sink
handler doesn't decode an old `LogEvent` differently from a new one
since they're distinct kinds.
