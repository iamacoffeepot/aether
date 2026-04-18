# ADR-0023: Substrate log capture and the `engine_logs` MCP tool

- **Status:** Proposed
- **Date:** 2026-04-17

## Context

ADR-0009 established the hub as the supervisor for substrate processes spawned via `spawn_substrate`. The hub forks the binary with `AETHER_HUB_URL` injected, owns the child for its lifetime, and exposes `terminate_substrate` for clean shutdown. Substrates can also be started independently (`AETHER_HUB_URL=127.0.0.1:8889 cargo run -p aether-substrate`) and connect to the hub on their own — that's the development workflow today.

Across both spawn modes, the substrate emits log output the agent currently can't see:

- **`eprintln!` from ADR-0015 trap containment.** When a hook traps or `Component::deliver` panics, the substrate logs the trap message. Without log access, a trap is invisible — the agent sees no observation mail, no reply-to-sender, no signal that the deliver failed.
- **Substrate-side `tracing` / `log` output.** Bring-up issues post-handshake (wgpu init failure, shader compile error, scheduler complaints) come through the standard logging infra. Today they go to whichever tty the substrate was launched in.
- **Component panics surfaced through the substrate.** When a wasm guest panics, wasmtime returns a trap; ADR-0015's containment logs it. Same `eprintln!` path as above.

The naive fix is hub-side pipe capture: when the hub spawns a substrate it can attach `Stdio::piped()` and read line-by-line. That works for hub-spawned engines but doesn't work for externally connected ones — the hub doesn't own their stdio. Splitting the design into "hub captures pipes for spawned, externally connected get nothing" leaves the dev workflow uncovered, and "hub captures pipes for spawned, externally connected use a different mechanism" carries two parallel paths.

The alternative is to flip the capture point: the substrate installs its own tracing subscriber and forwards entries over the engine ↔ hub wire. The hub becomes a sink, not a reader. Same per-engine ring buffer, same MCP tool surface, but uniform across both spawn modes.

The cost is small and lives in one place: `aether-substrate` adds a tracing layer plus a flush task, and the hub-protocol crate gains one frame variant. Foreign substrates that want log visibility opt in by doing the same; foreign substrates that don't simply return empty `engine_logs` results — not an error, just an empty drain.

## Decision

The substrate installs a tracing-subscriber capture layer that buffers formatted log entries and periodically forwards them to the hub via a new `EngineToHub::LogBatch` frame. The hub stores entries in a bounded per-engine ring buffer. A new MCP tool `engine_logs(engine_id, max?, level?, since?)` returns recent entries with cursor-based pagination.

### 1. Capture in the substrate

`aether-substrate` registers a `tracing_subscriber::Layer` that captures every event matching a runtime filter. Filter defaults to `INFO+`; overridable via the `AETHER_LOG_FILTER` env var using the standard `EnvFilter` syntax (`AETHER_LOG_FILTER=aether_substrate=debug,wgpu=warn`). The capture is additive — console output (whatever the host's existing subscriber writes) is unaffected.

Each captured event becomes a `LogEntry`:

```rust
pub struct LogEntry {
    pub timestamp_unix_ms: u64,
    pub level: LogLevel,        // Trace | Debug | Info | Warn | Error
    pub target: String,         // module path, e.g. "aether_substrate::scheduler"
    pub message: String,        // already-formatted event message
    pub sequence: u64,          // monotonic per substrate, starts at 0 each boot
}
```

Messages longer than 16 KiB are truncated at capture time with a `...[truncated]` marker — a runaway component dumping a megabyte of log per event shouldn't blow the buffer in one go.

### 2. Local buffering and forwarding

The capture pushes entries onto a bounded ring (default 2,000 entries / 2 MiB) inside the substrate. A small async task drains the ring and sends batches via the existing engine ↔ hub channel:

- **Batch trigger:** flush when 100 entries are queued OR 250 ms have passed since the last flush, whichever first.
- **Frame:** new `EngineToHub::LogBatch(Vec<LogEntry>)` variant in `aether-hub-protocol`.
- **Backpressure:** if the channel is full, drop the oldest queued entries to keep the ring bounded; record a `LogEntry { level: Warn, target: "aether_substrate::log_capture", message: "dropped N entries" }` so the loss is observable.

A momentary disconnect (network blip, hub restart) doesn't immediately drop entries — they sit in the ring waiting for reconnect (handled by ADR-0006's V1 reconnect story; today a disconnect is terminal and the buffer's contents are lost with the substrate).

### 3. Hub storage

The hub keeps a per-engine ring buffer (default 2,000 entries / 2 MiB), appending each `LogBatch` frame and evicting oldest when capped. Eviction is silent at append time; readers see it via `truncated_before` on the response (§4).

The buffer survives engine exit until hub shutdown — post-mortem inspection ("why did the substrate crash?") is the most valuable case for these logs. A long-running hub with many spawn cycles accumulates buffers; for now this is acceptable (each is bounded). GC for stale buffers is parked.

### 4. MCP tool surface

```jsonc
mcp__aether-hub__engine_logs(
  engine_id: Uuid,                                 // required
  max: u32?,                                       // default 100, max 1000
  level: "trace"|"debug"|"info"|"warn"|"error"?,   // minimum level; default "trace"
  since: u64?,                                     // sequence number; entries with sequence > since
)
```

Response:

```jsonc
{
  "engine_id": "...",
  "entries": [
    {
      "timestamp_unix_ms": 1713379200123,
      "level": "error",
      "target": "aether_substrate::component",
      "message": "trap in deliver: unreachable",
      "sequence": 47
    }
  ],
  "next_since": 47,
  "truncated_before": null
}
```

`since` enables cursor-based polling without re-receiving entries. `truncated_before` flags when the hub-side ring evicted entries the caller hadn't seen — a signal to poll more often or accept the gap. `level` filters server-side so an agent watching for errors doesn't pull a megabyte of debug output.

### 5. Migrating substrate-side `eprintln!` to tracing

ADR-0015's trap containment currently logs via `eprintln!`. Once this ADR ships, those callsites migrate to `tracing::error!` so the capture picks them up — otherwise the headline motivation (trap visibility) doesn't actually work. Same migration applies to any other substrate-internal `eprintln!`/`println!` callsites; small surface today, but worth catching them all in the same change to avoid drift.

### 6. What this ADR does *not* do

- **No pre-handshake capture.** A substrate that crashes before it connects to the hub has no engine_id, can't be queried via `engine_logs`, and its early stderr (panic messages, init failures) is gone with the process. Out of scope; pre-handshake debugging stays a "look at the terminal" workflow.
- **No wasm-guest stdout capture.** Components' `println!` / `eprintln!` goes through wasi-stdout to the substrate's inherited stdio. Routing that through tracing is doable (custom wasi-stdout writer) but out of scope here; the host-fn host-side trap log covers the panic case, which is the load-bearing one.
- **No streaming / long-poll.** `engine_logs` is a poll, not a subscription. Cursor-based pagination keeps it cheap; SSE / long-poll is a follow-on if friction shows up.
- **No structured event fields.** The capture preserves `target` and the formatted `message` string. Tracing's structured fields (`?value`, `%value`) are flattened into the message via the default formatter; structured passthrough is parked.
- **No content filtering beyond level.** Substring/regex filters live agent-side. Server-side content filtering is easy to add later if log volumes warrant it.

## Consequences

### Positive

- **Uniform across spawn modes.** Hub-spawned and externally connected substrates expose logs through the same MCP tool with the same shape. The dev workflow (manual `cargo run -p aether-substrate` + agent driving via MCP) becomes self-contained.
- **Trap visibility.** Once `eprintln!` migrates to `tracing::error!`, every ADR-0015 containment event flows to `engine_logs`. The "deliver failed silently" failure mode gets a diagnostic surface.
- **Bring-up errors are debuggable** post-handshake. `spawn_substrate` succeeds, the renderer fails to init wgpu, the agent polls logs and sees the wgpu error.
- **Lifetime survives engine exit.** When a substrate crashes, the hub-side buffer keeps the last N entries for post-mortem.
- **Bounded by construction.** Substrate-side ring (2 MiB), hub-side ring (2 MiB per engine), per-line cap (16 KiB). A misbehaving subscriber can't OOM either side.
- **Symmetric with the rest of the architecture.** Engine pushes a frame; hub stores it; agents drain via MCP. Same pattern as observation mail (ADR-0008).

### Negative

- **Substrate-side dependency on the capture infra.** Every substrate binary that wants log visibility installs the tracing layer + flush task. For our `aether-substrate` that's a one-time addition; for hypothetical foreign substrates it's an opt-in cost. Substrates that skip it return empty `engine_logs` results.
- **Pre-handshake panics aren't captured.** A substrate that panics before connecting never registers an engine_id. Acceptable: there's nothing to query against anyway. Mitigated by reading the substrate's terminal directly during early bring-up.
- **Wasm-guest `println!` not captured.** Component-side stdio goes through wasi-stdout to the substrate's inherited stdio, not through the tracing capture. Component panics surface (via the host-side trap log) but `println!` debugging from inside the guest still requires looking at the terminal. Parked.
- **Capture has cost.** Every event matching the filter does a format + clone + enqueue. At INFO+ this is negligible; cranking the filter to TRACE in a hot path could become noticeable. Acceptable: the filter is the throttle.
- **One more frame type on the wire.** `EngineToHub::LogBatch` is additive; no existing frame changes shape.

### Neutral

- **Hub-side surface largely unchanged.** Per-engine buffers, MCP tool with `engine_id`-keyed lookup. The mechanics of how entries get there flip from "read pipes" to "decode frames"; the storage and serving paths stay the same.
- **No special-casing for hub-spawned vs externally connected** at the hub. Either kind of substrate either forwards logs or doesn't; the tool returns whatever's in the buffer.
- **Tool count grows from 6 to 7 in CLAUDE.md.** Update docs.

## Alternatives considered

- **Hub-side pipe capture (original draft).** Hub attaches `Stdio::piped()` to spawned substrates and reads line-by-line. Zero substrate-side cost. Rejected because it only covers hub-spawned — the dev workflow and the ADR-0006 "external engines" path get nothing — and the cost of substrate-side capture is small enough that uniformity wins.
- **Hybrid: pipe capture for spawned, substrate forwarding for external.** Both paths feed the same per-engine ring buffer. Rejected: two mechanisms doing the same job, with subtle differences (pipe capture catches pre-handshake stderr, substrate capture doesn't), and double-capture for hub-spawned (both paths active) creates dedup work.
- **Stream logs as observation mail.** Make log entries a broadcast mail kind. Rejected: pollutes the observation channel with engine-internal noise, and a substrate that's failing pre-handshake can't broadcast (it isn't connected yet — same constraint as the chosen design, but with worse channel discipline).
- **Filesystem logging the agent reads via the host.** Sidesteps the protocol entirely. Rejected: assumes agent + hub on the same host, pushes log management onto the agent, and doesn't compose with the MCP-as-the-only-surface stance.
- **Forward to syslog / journald.** Outsources retention and querying to the host. Rejected: cross-platform fiddly, not agent-readable through MCP, doesn't solve the access problem.
- **Defer until logs are demonstrably needed.** ADR-0009's stance. Rejected now because traps from ADR-0015 are the canonical "something failed and you need to know why" signal, and they're invisible without this.

## Follow-up work

- **`aether-hub-protocol`**: add `LogEntry`, `LogLevel`, and `EngineToHub::LogBatch(Vec<LogEntry>)` frame variant.
- **`aether-substrate`**: install the tracing-subscriber capture layer (additive over existing console output); spawn the flush task with the documented batch trigger; wire the hub channel sender through to it.
- **`aether-substrate`**: migrate every `eprintln!` / `println!` callsite (ADR-0015 trap containment is the headline) to `tracing::error!` / `tracing::info!` etc. so capture picks them up. Audit and convert all in one pass to avoid silent gaps.
- **`aether-hub`**: per-engine ring buffer; append on `LogBatch` receipt; serve `engine_logs` MCP tool with the documented shape; retain buffer past engine exit.
- **CLAUDE.md**: add `engine_logs` to the MCP harness section. Note the `AETHER_LOG_FILTER` env var.
- **Tests**: substrate emits known events at multiple levels; agent drains via `engine_logs`; assert level filter, sequence + `next_since` cursor, `truncated_before` on overflow. Externally connected substrate end-to-end (start substrate manually, spawn an agent session, drain logs).
- **Parked, not committed:**
  - GC for stale per-engine buffers in long-running hubs.
  - Long-poll / SSE streaming variant of `engine_logs`.
  - Server-side substring / regex filtering.
  - Structured tracing field passthrough (today fields are flattened into the message string).
  - Wasi-stdout capture for wasm guests (custom writer routing through the same tracing capture).
  - Pre-handshake log capture (would require a side channel; out of scope).
