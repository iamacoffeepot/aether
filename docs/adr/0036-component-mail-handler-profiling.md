# ADR-0036: Per-handler latency sampling and the `profile_component` MCP tool

- **Status:** Proposed
- **Date:** 2026-04-20

## Context

Claude sits in the harness as the primary author of components. The iteration loop is: write code, `load_component`, drive it via `send_mail`, observe behavior via `receive_mail` / `capture_frame` / `engine_logs`, revise. Behavior is well-covered by the existing surface; *performance* is not. Today, a Claude session has no way to answer "is this handler slow, and which one" without adding ad-hoc `tracing` spans to the substrate or timing from the outside.

Mail-driven components make this question tractable. Everything a component does is triggered by a `deliver` call against a specific `(mailbox_id, kind_id)` pair. Wall-clock duration of that call is already a strong signal — "your Tick handler averages 140µs, 10× your MouseMove handler" is often exactly what the loop needs to decide what to optimize. The substrate already owns the call boundary; adding a pair of `Instant::now()` readings and a bounded ring is a handful of lines.

Tier 2 (sampled stack flame graphs via wasmtime epoch interruption) and Tier 3 (guest-declared `span!` annotations) were considered alongside this and deferred — both solve intra-handler questions this ADR does not. "Which handler" is the common case and is worth answering first with the minimum viable instrumentation.

## Decision

The substrate records per-delivery wall-clock duration per `(mailbox_id, kind_id)` in a bounded ring, and exposes aggregated stats through a new MCP tool `profile_component(engine_id, mailbox_id, reset?)`. Request and reply ride two new control-plane kinds (`aether.control.profile_component` and `aether.control.profile_component_result`) on the same await-reply mechanism `capture_frame` and `load_component` already use.

### 1. Measurement boundary

One sample is the wall-clock duration of one `deliver` call against a component instance — `Instant::now()` taken just before the host-to-guest call into `receive_p32`, paired with `Instant::elapsed()` immediately after guest return. The window *includes* every nested host-fn call the guest makes synchronously during deliver (`send_mail_p32`, `reply_mail_p32`, `save_state_p32`). The window *excludes* host-side decode of the incoming mail (which happens before entry), queue wait before dispatch (a different question), and any mail the handler sends that triggers downstream handlers (those get their own samples on their own mailboxes).

Only successful deliveries are sampled. A trap or decode failure is an error path, not a "how fast is this handler" signal — those continue to surface through `engine_logs` (ADR-0023). Lifecycle callbacks (`init`, `on_replace`, `on_drop`) are not sampled; they are one-shot and a different question.

### 2. Storage

Each component instance owns a `HashMap<u64, RingBuffer<u64>>`, keyed by `kind_id`, lazily allocated on first sample. Each ring holds up to 1024 recent sample durations in nanoseconds. On overflow the oldest sample is evicted silently. Ring size is fixed (not per-kind configurable); 1024 × 16 bytes (u64 sample + u64 unix-ms timestamp for windowing) = 16 KiB per `(mailbox × kind)` bucket, bounded by the number of kinds the component declares in its `#[handlers]` manifest (ADR-0033).

Samples are **reset on `replace_component`**. The code changed — old samples are stats for a different program. Samples are dropped when the component is dropped.

### 3. Per-deliver cost

Two `Instant::now()` calls (~30–50ns each on macOS/Linux), one HashMap lookup (~20ns amortized), one ring write (~10ns). Total overhead ~100ns per deliver. For a Tick subscriber at 60Hz this is 6µs/sec of instrumentation cost per component — below the noise floor of any real handler. Always-on; no opt-in flag.

### 4. Control-plane wire

Two new kinds registered in the substrate's core kind vocabulary (same slot as `capture_frame`, `load_component`, etc.):

```rust
// request
struct ProfileComponentRequest {
    mailbox_id: u64,
    reset: bool,  // if true, clear all rings for this mailbox after reading
}

// reply
struct ProfileComponentResult {
    mailbox_id: u64,
    handlers: Vec<HandlerStats>,
    error: Option<String>,  // populated iff mailbox_id is unknown
}

struct HandlerStats {
    kind_name: String,     // from labels sidecar (ADR-0032)
    kind_id: u64,
    calls: u64,            // number of samples in the ring (≤ 1024)
    total_observed: u64,   // total calls seen, including evicted (for "is my ring full?")
    min_ns: u64,
    max_ns: u64,
    mean_ns: u64,
    p50_ns: u64,
    p95_ns: u64,
    p99_ns: u64,
    first_sample_ms_ago: u64,  // age of oldest sample in the ring
    last_sample_ms_ago: u64,   // age of most recent sample
}
```

Percentiles are computed on the reply side over the ring's current contents — small enough (≤1024) that sort-and-index is fine. `total_observed - calls` tells the caller how many older samples were evicted; `first_sample_ms_ago` says how far back the ring reaches.

### 5. MCP tool surface

```jsonc
mcp__aether-hub__profile_component(
  engine_id: Uuid,       // required
  mailbox_id: u64,       // required
  reset: bool?,          // default false
)
```

Response mirrors `ProfileComponentResult` above, with `mailbox_id` echoed and `handlers` sorted by `total_ns` descending so the hot handlers surface first.

The hub forwards the request as `aether.control.profile_component`, awaits the `aether.control.profile_component_result` reply via the same pending-replies mechanism `capture_frame` and `load_component` use, and returns the decoded stats inline. Rejects with a clear error if the substrate doesn't reply within `timeout_ms` (default 3000).

Typical loop:

1. Claude loads a component, mails work at it for a few seconds.
2. Calls `profile_component(engine_id, mailbox_id)`.
3. Reads back "Tick: p95=340µs, MouseMove: p95=12µs" — decides which handler to optimize.
4. Edits code, `replace_component`, repeats. (Replace resets the rings.)

### 6. What this ADR does *not* do

- **No intra-handler sampling.** If a handler is slow and Claude wants to know *which line* inside it is slow, that's Tier 2 (sampled stack flame graph via wasmtime epoch interruption) or Tier 3 (guest-declared `span!` annotations). Separate ADRs if they're ever warranted.
- **No host-side latency breakdown.** Mail queue wait, decode cost, scheduler overhead — none surfaced here. The question this ADR answers is guest-side.
- **No cross-component aggregation.** One call, one mailbox. If Claude loads five components and wants a global hot list, call five times. Fine for now.
- **No historical retention beyond the ring.** 1024 most recent samples per kind. A long-running component with busy kinds loses older samples silently (flagged via `total_observed - calls`). Post-mortem stats after component drop are out of scope.
- **No streaming / push.** Poll-on-demand only. A `receive_mail`-style async observation channel for profiling data could come later; not needed for the tight iteration loop.

## Consequences

### Positive

- **Closes the perf-feedback loop for Claude-authored components.** The iteration cycle (write → load → drive → read stats → edit → replace) becomes self-contained without leaving the MCP surface.
- **Structured output, Claude-readable.** Stats come back as numbers and names, not a rendered flame graph — directly usable in the reasoning loop. Sort-by-total-ns means the hot kind is always first.
- **Negligible runtime overhead.** ~100ns per deliver, bounded memory per component. Always-on without a "start profiling" ceremony.
- **Reuses the existing await-reply plumbing.** Same pattern as `capture_frame`, `load_component`, `replace_component`. No new infrastructure in the hub — one more control-plane kind and a handler.
- **Preserves labels-sidecar semantics.** Kind names in the response come from the substrate's `aether.kinds.labels` section (ADR-0032); Claude doesn't need a separate `describe_kinds` roundtrip.
- **Doesn't touch the guest surface.** No FFI changes, no guest SDK work. Components don't opt in and can't opt out — the instrumentation is entirely substrate-side, at the deliver boundary.

### Negative

- **Only answers "which handler".** If the answer is "your Tick handler, and it's spending 90% of its time in one function", this ADR stops short. User revisits with Tier 2 when and if that gap bites.
- **Ring size is fixed.** A component dominated by one high-frequency kind may see samples from a slow-but-rare kind evicted before Claude reads them. Mitigation: caller reads with `reset: true` after each test run. If this becomes friction, configurable ring size is a small follow-on.
- **One more control-plane kind pair** (`profile_component`, `profile_component_result`) in the substrate's vocabulary. Trivial on the wire; one more entry in the kind manifest.
- **Samples don't survive component drop.** Drop a component, lose the stats. `replace_component` also resets. Acceptable — stats are about the current code, not history — but worth stating.

### Neutral

- **Scope is every deliver, not opt-in kinds.** Instrumenting every deliver is simpler than a filter list and the overhead is below the noise floor. If a specific kind is somehow hot enough that even the `Instant::now()` pair matters, that's its own problem and a flag can be added.
- **Tool count grows by one** in CLAUDE.md (ten → eleven). Update the harness section.
- **Reply uses the standard pending-reply queue mechanism.** Same pattern as `capture_frame` — one session can't have two profile requests in flight against the same substrate simultaneously. Not a real constraint; these responses return in milliseconds.

## Alternatives considered

- **Tier 2 directly: sampled stack flame graph.** Wasmtime's epoch interruption + `WasmBacktrace` + name-section symbolization gives a real flame graph without guest changes. Rejected as the first thing to ship — bigger substrate lift (sampling thread, epoch config, aggregation, folded-stack encoding, percentile-free data model), and the "which handler" question is almost always what Claude actually wants. Tier 2 becomes a separate ADR when a concrete component needs intra-handler detail.
- **Tier 3: guest-declared `span!` annotations.** SDK macro emits enter/exit mail around user-defined regions. Rejected as the first thing to ship — puts instrumentation burden on Claude-as-author, misses anything unannotated, complements Tier 1 rather than replacing it.
- **Wasmtime jitdump / perfmap integration.** Turn it on in engine config, capture with Linux `perf` or macOS Instruments. Rejected outright: output isn't reachable from an MCP tool response, so the Claude-in-harness loop can't close on it. Still available to a human dev tracking down the same question out-of-band.
- **Stream samples as broadcast observation mail.** Every deliver pushes a `frame_stats`-style event. Rejected: floods the observation channel, forces Claude to aggregate client-side, fights with the existing `frame_stats` path. Substrate-local aggregation is the right side.
- **Opt-in start/stop profiling window.** `start_profiling(mailbox)` / `stop_profiling(mailbox)` ceremony. Rejected: the overhead is already negligible; the ceremony costs more in friction than the instrumentation costs at runtime. Always-on is simpler.
- **Always return full sample list, aggregate client-side.** Send 1024 u64s per kind back to Claude and let it compute percentiles. Rejected: wastes tool-response tokens on raw numbers Claude has to fold back into stats anyway. Aggregation is ~20 lines substrate-side and the reply is ~200 bytes per kind instead of ~8 KiB.
- **Persist stats past `replace_component`.** Carry ring contents across swaps. Rejected: the code changed, so the samples are about a different program. Clean slate is the honest behavior.

## Follow-up work

- **`aether-substrate-core`** (assuming ADR-0035 has landed; otherwise `aether-substrate`): add per-component `HashMap<u64, RingBuffer<u64>>` keyed by kind id; wrap the `deliver` call site in the `Instant::now()` pair; reset on `replace`; drop on `drop`.
- **Control plane**: register `aether.control.profile_component` + `aether.control.profile_component_result` kinds; add `handle_profile_component` in `control.rs` that computes percentiles over the current rings and emits the reply.
- **`aether-hub`**: add `profile_component` MCP tool; wire the await-reply queue the same way `capture_frame` and `load_component` do; decode the reply via the kind descriptor (ADR-0020) and return inline as structured JSON.
- **CLAUDE.md**: add `profile_component` to the MCP harness tool list.
- **Tests**: load a component with two kinds of mixed cost; drive it with a known number of mails; assert reported `calls` matches dispatch count, percentiles are sane, and the `reset` flag clears state. Cover the `replace_component` reset and the unknown-`mailbox_id` error path.
- **Parked, not committed:**
  - Configurable ring size per component or per kind.
  - Tier 2 sampled-stack flame graph (separate ADR when intra-handler detail is actually needed).
  - Tier 3 guest-declared `span!` annotations (separate ADR).
  - Host-side latency breakdown (queue wait, decode time).
  - Cross-component aggregation / "engine-wide hot list".
  - Streaming / observation-mail variant for live dashboards.
  - Historical retention across `replace_component` or drop (would need a substrate-side archive; parked without a use case).
