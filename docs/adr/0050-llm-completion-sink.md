# ADR-0050: LLM completion sink

- **Status:** Proposed
- **Date:** 2026-04-25

## Context

The substrate today ships five sinks: render, camera, audio, io (ADR-0041), and net (ADR-0043). Each owns a peripheral or boundary the substrate's components shouldn't touch directly. ADR-0046's content-generation pipeline pattern names a sixth required sink: an LLM completion endpoint. Pipelines that frame, distill, scrub, translate, or compose text via Claude / GPT / local model dispatch their LLM calls through the substrate, identical to how they'd dispatch a file write or an HTTPS fetch.

`spikes/prompt-pipeline-spike/` validated the call shape against `claude -p` as a subprocess. Each call composes a prompt, dispatches via `Command::new("claude").arg("-p").arg(prompt).args(["--model", model])`, captures stdout, and content-addresses the cache by `(prompt, model, template-hash)`. The shape held across five experimental runs and worked uniformly across Haiku / Sonnet / Opus model variants. The data-flow shape — request with `(prompt, model)`, reply with `text`, optional cost / latency fields — is well-understood. What's open is the *substrate-side* engineering: mail kinds, adapter dispatch, credential management, capability gating, observability.

The deployment context for v1 is **Claude via subscription, dispatched through the local `claude` CLI** — *not* direct Anthropic API access. Subscription billing means there is no API key budget for routine development workflows; every Claude call has to flow through the CLI. HTTP-based providers (Gemini for image generation, future API-budgeted Claude / OpenAI usage) coexist as separate adapter shapes, but the subprocess adapter is the load-bearing v1 path, not a peer of HTTP.

Two design pressures:

- **Adapter neutrality across mechanism.** Different providers expose different surfaces. The user's Claude access is CLI-only (subscription, no API budget). Gemini is HTTP-only (no CLI). Future providers (OpenAI, Ollama, enterprise gateway) will be HTTP. The sink contract abstracts the mechanism behind a `model` string dispatched through an adapter registry, parallel to ADR-0041's `AdapterRegistry`. v1 ships subprocess as the default, HTTP-Gemini as a loadable adapter for image-gen workflows, and HTTP-Anthropic as an optional adapter for deployments that have configured API access.
- **Cost observability.** LLM calls are the most expensive routine operation in any content pipeline. The substrate is the right place to log per-call cost — model, input tokens, output tokens, wall-clock — because the substrate sees every dispatch. Application-side cost tracking would have to instrument every component. Subscription-billed CLI calls can't surface per-call cost (the CLI doesn't know subscription pricing); HTTP adapters can, since the API responses include token counts.

This ADR commits to the LLM sink mail surface, the adapter model, the v1 backends, credential and capability handling, and the observability surface. Streaming, vision/multimodal, and structured output are deferred to follow-up ADRs.

## Decision

### 1. Mail surface

The substrate exposes one new sink, `"llm"`, with three request kinds and three corresponding reply kinds:

```rust
aether.llm.complete         { model, prompt, max_tokens?, temperature?, system?, stop_sequences? }
aether.llm.list_models      { /* empty */ }
aether.llm.cancel           { request_id }

aether.llm.complete_result  : Ok  { text, model, usage, request_id }
                            | Err { error: LlmError, request_id }
aether.llm.list_models_result : Ok  { models: Vec<ModelInfo> }
                              | Err { error: String }
aether.llm.cancel_result    : Ok  { cancelled: bool }
                            | Err { error: String }
```

Concrete shapes:

```rust
struct CompleteRequest {
    model: String,                       // adapter-routed (e.g., "haiku", "sonnet", "opus", "gpt-4")
    prompt: String,                      // utf-8, single user message
    max_tokens: Option<u32>,             // adapter-default if None
    temperature: Option<f32>,            // 0.0..=2.0; adapter-default if None
    system: Option<String>,              // optional system message
    stop_sequences: Option<Vec<String>>, // optional stop tokens
}

struct CompleteResult {
    text: String,
    model: String,                       // canonical model name the adapter actually used
    usage: Usage,
    request_id: u64,                     // for correlation with cancel and engine_logs
}

struct Usage {
    input_tokens: u32,
    output_tokens: u32,
    wall_clock_ms: u32,
    cost_micros: Option<u64>,           // 1/1_000_000 USD; adapter-reported, None if unknown
}

enum LlmError {
    UnknownModel(String),
    Unauthorized,                        // missing or invalid credentials
    RateLimited { retry_after_ms: Option<u32> },
    ContextLengthExceeded { limit: u32 },
    AdapterError(String),                // catchall for backend-specific failures
    Cancelled,
    Timeout,
}

struct ModelInfo {
    name: String,                        // canonical name (e.g., "haiku")
    aliases: Vec<String>,                // adapter-known aliases ("claude-haiku-4-5", etc.)
    adapter: String,                     // which backend serves this model
    context_window: u32,                 // input token cap
    max_output_tokens: u32,
    supports_system_message: bool,
}
```

Single-shot completion only in v1. Conversation/multi-turn is composable on top: a component that wants conversation state holds it locally and re-prompts with the assembled history. Streaming, structured outputs (JSON schema), tool use, and vision are deferred (see §8).

`request_id` is a substrate-assigned monotonic counter. The reply echoes it; cancel takes it. Same shape ADR-0041 used for the namespace+path echo (correlation by structured field, not by FIFO position).

### 2. Adapter registry

Parallel to ADR-0041's `AdapterRegistry<Namespace>`:

```rust
struct LlmAdapterRegistry {
    adapters: HashMap<String, Box<dyn LlmAdapter>>,
    model_routes: HashMap<String, String>,  // model_name -> adapter_name
}

trait LlmAdapter: Send + Sync {
    fn complete(&self, req: &CompleteRequest) -> Result<CompleteResult, LlmError>;
    fn list_models(&self) -> Vec<ModelInfo>;
    fn cancel(&self, request_id: u64) -> bool;
}
```

The substrate boots with adapters loaded from config (§4) and a model-routing table built from each adapter's `list_models()`. A `complete` request looks up `model_routes[req.model]`, dispatches to the named adapter, and returns the result. Lookup miss → `LlmError::UnknownModel(req.model)`.

Adapter calls are *not* dispatched on the actor-per-component thread. The substrate owns a small thread pool (default 4 threads, configurable via `AETHER_LLM_CONCURRENCY`) for in-flight LLM calls; the sink dispatch enqueues the request and returns the reply when the adapter completes. This matches ADR-0043 (net) — long-tail outbound calls don't block per-component scheduling.

Per-call timeout: default 120s, overridable via `AETHER_LLM_TIMEOUT_MS`. Exceeding it surfaces as `LlmError::Timeout`. The adapter is expected to honor cancel signals — subprocess adapters kill the child; HTTP adapters drop the connection.

### 3. v1 adapters

**Subprocess adapter for Claude** (`claude_cli`). Mandatory v1, default. Runs `claude -p <prompt> --model <model> --max-turns 1 --output-format text` as a child process per request. Stdout is captured as the `text` field; stderr goes to engine_logs. The adapter expects `claude` on PATH at substrate startup; if absent, the substrate logs a warn and the adapter is marked unavailable — `complete` requests routed to a Claude model return `LlmError::AdapterError("claude CLI not found on PATH")`. This is the load-bearing path: the user runs Claude via subscription, the subscription is exercised through the CLI, no API key is configured.

Model names recognized: `haiku`, `sonnet`, `opus`, plus passthrough of fully-qualified IDs (`claude-haiku-4-5`, etc.). The adapter passes the user-supplied `model` string to the CLI verbatim; routing happens in the CLI.

This adapter validated empirically in `spikes/prompt-pipeline-spike/` — five experiment runs across multiple model profiles, content-addressed caching, no failures. The spike's `src/claude.rs` is the reference implementation.

`Usage` reporting from subprocess: `wall_clock_ms` measured by the substrate; `input_tokens` and `output_tokens` are `0` (the CLI's text-output mode doesn't surface them, and the subscription model means tokens aren't billed per-call anyway). `cost_micros` is `None` for the subprocess adapter — subscription billing isn't per-call. Operators wanting per-call cost data must use the HTTP-Anthropic adapter with API access.

**HTTP adapter for Gemini** (`http_gemini`). Loaded when `AETHER_LLM_GEMINI_API_KEY` is set. Dispatches against `generativelanguage.googleapis.com` via the substrate's net sink (ADR-0043). Returns full `Usage` including token counts. The spike's `src/gemini.rs` validated this against `gemini-3.1-flash-image-preview` (image gen) and `gemini-3-pro-preview` (text + vision); both shapes are reusable by the adapter. This is the API path for providers that don't have a CLI option.

Model names: `gemini-3.1-flash-image-preview`, `gemini-3-pro-preview`, etc. — passthrough of Gemini model IDs.

**HTTP adapter for Anthropic** (`http_anthropic`). Loaded only when `AETHER_LLM_ANTHROPIC_API_KEY` is set, which the user does not currently set. Documented for completeness — same shape as `http_gemini` against the Anthropic API, returns full `Usage` with `cost_micros` computed from the API pricing table. Useful for future deployments with API budget (CI runners, headless production workloads, multi-tenant deployments). The default path remains `claude_cli`; routing only flips to `http_anthropic` if the operator explicitly sets `AETHER_LLM_ROUTE_<model>=http_anthropic`.

All three adapters honor `max_tokens`, `temperature`, `system`, `stop_sequences`. HTTP adapters encode them into the request body per the provider's API spec; the subprocess adapter passes them as CLI flags where supported and warns to engine_logs on first ignore otherwise.

OpenAI / local-model / enterprise-gateway adapters are deferred to follow-up ADRs. The trait is forward-compatible — adding adapters doesn't change the `aether.llm.complete` wire format.

### 4. Configuration

The substrate reads adapter and routing config from environment variables (v1 — same precedence-stack-deferred as ADR-0041's TOML/CLI):

- **`AETHER_LLM_ADAPTERS`** — comma-separated adapter names to enable. Default: `claude_cli`. Adapters whose required env keys are unset are skipped (logged as a warn). Example: `claude_cli,http_gemini` enables Claude (subscription) + Gemini (API).
- **`AETHER_LLM_DEFAULT_MODEL`** — fallback model when a request omits or names an unknown one. Default: `haiku`.
- **`AETHER_LLM_CONCURRENCY`** — thread pool size for in-flight requests. Default: 4.
- **`AETHER_LLM_TIMEOUT_MS`** — per-call timeout. Default: 120000.
- **`AETHER_LLM_GEMINI_API_KEY`** — auth for the `http_gemini` adapter. Read once at startup.
- **`AETHER_LLM_ANTHROPIC_API_KEY`** — auth for the `http_anthropic` adapter. Optional; absent in the user's default workflow (subscription only).
- **`AETHER_LLM_ROUTE_<MODEL>=<adapter>`** — explicit routing override (e.g., `AETHER_LLM_ROUTE_haiku=http_anthropic` to force the API path for haiku even with the CLI adapter loaded). Without overrides, the substrate uses the first-loaded adapter that claims a given model — `claude_cli` wins for Claude models in v1.

Models that aren't claimed by any loaded adapter route to `LlmError::UnknownModel`. The startup log emits the resolved routing table at INFO level.

A future ADR can add a TOML config layer (matching ADR-0041's deferral) for richer per-deployment configuration. v1 stays env-only.

### 5. Capability gating

When ADR-0044's capability system unparks, `llm` is a top-level capability. Components without `llm` can't dispatch to the sink (the same way components without `net` can't dispatch fetches today). The capability is single-grant — a component either has it or doesn't, no per-model gradations in v1. Per-model capability gating (e.g., grant access to `haiku` but not `opus`) is a future ADR if cost-control workflows demand it.

In v1 (pre-ADR-0044), all components on a substrate can dispatch LLM calls. The trust model is "the substrate's owner trusted these components when they loaded them." Operators concerned about LLM cost from rogue components can either (a) not load untrusted components, (b) cap concurrency via `AETHER_LLM_CONCURRENCY=0` to disable the sink, or (c) set up ADR-0044 once it lands.

### 6. Observability

Per-request engine_logs entries (ADR-0023):

- **DEBUG** — request submitted, with `request_id`, `model`, `prompt.len()`, `max_tokens`, sender mailbox.
- **INFO** — request completed, with `request_id`, `model`, `usage` (tokens, ms, cost), reply text length.
- **WARN** — adapter error, retry, fallback to default model, ignored unsupported parameter.
- **ERROR** — request failed irrecoverably, with `LlmError` variant.

The hub MCP surface gains:

- **`mcp__aether-hub__llm_status(engine_id)`** — current in-flight count, queued count, per-adapter dispatched count since substrate boot, per-model dispatched count + total tokens + total cost. Useful for cost triage in long-running content-gen sessions.

Cost roll-up per substrate session is published as `aether.observation.llm_cost` to the broadcast sink every 30 seconds (cadence matching the `frame_stats` observation pattern from ADR-0008). Format: `{ session_micros: u64, calls: u32, by_model: Vec<(String, u64)> }`. Harness sessions watching the broadcast see cost accumulation in near-real-time without polling.

### 7. Cancel semantics

`aether.llm.cancel { request_id }` walks the in-flight pool, finds the matching request if present, signals the adapter to abort. Subprocess adapters SIGTERM the child; HTTP adapters drop the connection. The cancelled request's reply (`Err { error: LlmError::Cancelled, request_id }`) goes to the original sender if it hadn't replied yet. Idempotent: cancelling an already-completed or never-existed request returns `Ok { cancelled: false }`.

This matches ADR-0047's cancel semantics for DAGs: the substrate stops paying attention; the underlying work may complete server-side. Cost accounting still records calls that complete after cancel — the user paid for them.

### 8. Deferred capabilities

The following are intentionally not in v1; each is a future ADR:

- **Streaming.** A long completion (1000+ tokens) takes seconds to minutes. v1 buffers the full response before replying. Streaming would let the consumer process partial output. Likely ADR shape: a `aether.llm.stream { ... }` request kind that pushes `aether.llm.stream_chunk` mail to the sender as tokens arrive, terminated by `aether.llm.stream_end`. Defers because content-gen pipelines (the v1 customer) don't need streaming.
- **Vision / multimodal.** v1 is text-in, text-out. Image inputs (for grading rendered output, vision-LLM-based critique) are central to ADR-0046's Spike B. The follow-up ADR adds `aether.llm.complete_multimodal { ... }` with a `Vec<ImageInput>` field. Image inputs likely use `Ref<Image>` (ADR-0045 handle refs) so a generated image flows directly into a vision call without inlining bytes. **Spike B Phase 3 ran the multimodal grading workflow against Gemini and validated the call shape**: `parts: [text, inlineData, inlineData, ...]` in declared order, multiple images per request handled cleanly, response shape is identical to text-only completion (text candidates with `parts[].text`). The follow-up multimodal ADR can lift the spike's `gemini::generate_text(prompt, references, model)` shape directly. ADR effort is moderate (kind definitions + adapter dispatch path); the design surface is well-understood and ready for focused review.
- **Structured output / JSON schema.** Many providers support "respond in this JSON shape." v1 returns plain text and the consumer parses. The follow-up ADR adds a `response_schema: Option<JsonSchema>` field that the adapter passes through where supported.
- **Tool use / function calling.** Provider-side tool dispatch (the LLM emits a tool-call request, the substrate runs the tool, feeds the result back). Out of scope; the substrate's mail-shaped sink dispatch is already the substrate-side equivalent.
- **Embeddings.** A separate operation (input → vector, not input → text). Likely a separate sink (`aether.embed.compute`) or a separate kind on this sink. Defer until embedding-driven workflows appear.
- **Conversation history.** Multi-turn state. Components that want conversations build them locally; the sink stays single-shot.

### 9. Chassis coverage

The LLM sink is **chassis-owned**, like `io` (ADR-0041), `net` (ADR-0043), and `audio` (ADR-0039). Each chassis instance bootstraps its own adapter registry at startup; components on that chassis dispatch into the local sink. No cross-chassis routing in v1.

- **Desktop** — full LLM sink. `claude_cli` adapter loaded by default; HTTP adapters loadable when API keys are set.
- **Headless** — full LLM sink, identical semantics. Headless content-gen workloads (CI runners, batch sculpting) are a primary target.
- **Hub** — no LLM sink. Mail to `"llm"` warn-drops as unknown mailbox, identical to the io sink behaviour on hub chassis. The hub coordinates substrate children; it does not host workload components in v1, so it has no consumer for the sink.

Components needing LLM access live on a desktop or headless chassis. A component on one chassis cannot dispatch through another chassis's sink directly — that would require either explicit cross-substrate addressing or routing-by-bubbling (ADR-0037), neither of which are wired through the LLM sink in v1. If a deployment grows multiple substrate children that share a single Claude CLI subscription and concurrent calls hit subscription rate limits, the right answer is a **hub-routed adapter** (described under Alternatives) — wire-additive when needed, not v1 work.

### 10. Handle-store integration

LLM completions are not auto-persisted as content-addressed handles in v1. The reply lands as regular mail to the sender; if the sender wants to share the result across components or persist it across substrate restart, the sender wraps the call in a transform (ADR-0048) — the transform's content-addressed handle id captures `(prompt, model, params)` and the result rides through ADR-0049's persistent handle store.

Why not auto-handle the reply? Because the LLM sink doesn't know the inputs are *intended* to be cache keys. The same `(prompt, model)` requested twice intentionally (e.g., to sample variance) shouldn't dedup. A transform-wrapped call expresses the intent: "this is a memoized lookup, treat it as content-addressable." Direct sink calls express the alternative intent: "I want a fresh call each time, even if the inputs are identical."

ADR-0046's Frame and Distill stages naturally wrap the LLM call in a transform; the spike validated this pattern — content-addressed cache keyed on `(prompt, model, template-hash)` was a per-pipeline implementation, but the same shape moves into a `#[transform]` cleanly when ADR-0048 ships.

## Consequences

### Positive

- **Pipelines have a substrate-level LLM dispatch.** ADR-0046's Frame, Distill, Scrub, Translate, Compose stages all dispatch through one well-defined sink instead of bespoke per-pipeline subprocess management.
- **Adapter neutrality across mechanism.** v1 ships `claude_cli` (subscription, no API budget) and `http_gemini` (image gen + multimodal) — the two providers the spike actually exercised. `http_anthropic` slots in for deployments that have API access. Future providers (OpenAI, local Ollama, enterprise gateway) drop in under the same trait without wire churn.
- **Substrate-level observability.** Per-call wall-clock, model, prompt length, request id surface via engine_logs and broadcast observation. HTTP adapters add token-level usage and per-call cost in USD micros; subprocess (subscription) adds wall-clock only. A long-running content-gen session can see usage rolled up across all calls without per-component instrumentation.
- **Capability-ready.** When ADR-0044 unparks, `llm` is a top-level cap. No retrofit required.
- **Mail-shaped surface lets Claude harness submit LLM calls directly.** A harness session can mail `aether.llm.complete` via MCP `send_mail` and observe the reply in `receive_mail`. Useful for ad-hoc "what does Haiku say if I ask it X" without authoring a component.

### Negative

- **Subprocess adapter has limited usage telemetry.** `claude -p` text mode doesn't report token counts and subscription billing isn't per-call, so the subprocess adapter reports `wall_clock_ms` only and `cost_micros: None`. HTTP adapters (when configured) report tokens + cost. Pipelines that want fine-grained cost accounting need API access; subscription users get latency only.
- **Per-substrate adapter set, not per-component.** All components on a substrate share the same adapter registry and routing. A workflow that wants component-A on Haiku-via-subprocess and component-B on Opus-via-HTTP needs both adapters loaded and uses model-string routing per call. Acceptable; per-component adapter overrides are a future complication that doesn't pay off without a forcing function.
- **No streaming in v1.** A 30-second completion holds an in-flight slot for 30 seconds; the consumer waits for the full response. Acceptable for content-gen workloads (Frame outputs are short, Distill outputs even shorter); a chat-shaped consumer would need streaming.
- **No vision in v1 (but the design is unblocked).** ADR-0046's Spike B (image grading) needs vision inputs; it ran successfully against Gemini using the spike's own multimodal HTTP client. The follow-up ADR adding `complete_multimodal` to this sink can lift the validated shape directly. Spike B has unblocked the multimodal design rather than just forcing it.
- **Credential management is env-var only.** No rotation, no per-component API keys, no provider-specific auth flows. Acceptable for v1; a future ADR can add a credential vault if multi-tenant deployments emerge.

### Neutral

- **Mail-shape uniformity.** The LLM sink follows the same shape as io, net, audio, render — request kind, reply kind, structured fields, error variants. No new substrate primitives required; the sink trait, `AdapterRegistry`, and the actor-per-component dispatch (ADR-0038) all compose.
- **Costs charged to the substrate's auth, not the component's.** Whoever owns the API key (env var) pays for the call. v1 acceptable; per-component billing is a deeper concern (probably tied to capabilities) deferred.
- **No substrate-side rate limiting.** Adapter-side rate limit replies surface as `LlmError::RateLimited`; the consumer decides whether to retry. A substrate-side concurrency cap (`AETHER_LLM_CONCURRENCY`) provides a coarse rate-control hook but isn't a real limiter. Future ADR can add one if cost-runaway becomes a concern.

## Alternatives considered

- **HTTP-only (no subprocess adapter).** Cleaner — one adapter shape, full token telemetry, no PATH dependency. Rejected: the spike validated subprocess against `claude -p` specifically because it uses the user's existing CLI auth; requiring an API key for development workflows is friction. Both adapters earn their slot.
- **No adapter abstraction (single hardcoded backend).** Simpler to ship. Rejected: the user explicitly wants to experiment with cross-model behavior across providers (the spike's model variation matrix). Adapter neutrality is a v1 requirement, not a future-proofing exercise.
- **LLM as a transform-shaped operation rather than a sink.** Transforms are pure (ADR-0048 §3); LLM completion is not pure (different replies for the same inputs, depends on remote state, has cost side effects). Sink is the right abstraction. A transform wrapper around an LLM sink call gives the content-addressing benefit at the wrapper layer; ADR-0046's pipelines do exactly this.
- **Single sink kind with model-string routing vs separate kinds per provider.** Single kind with routing is simpler for callers (one mail kind to learn); per-provider kinds would let static type checking enforce model availability. Rejected: model availability changes per deployment (which adapters loaded), so static enforcement isn't possible anyway. Single kind it is.
- **Streaming in v1.** Useful for chat-shaped consumers. Rejected for v1 because content-gen pipelines (the actual customer) don't need it; Frame/Distill/Compose outputs are short enough that buffering is fine. Follow-up ADR adds streaming when a forcing function emerges.
- **Multimodal in v1.** Necessary for Spike B's grading workflow. Rejected for v1 timing — the multimodal surface is enough additional design to deserve its own ADR (image-input handle integration, vision model routing, response-shape differences). Spike B has now both forced and pre-validated the design; the follow-up ADR is near-term work, not deferred indefinitely.
- **Caching at the sink level.** The substrate could content-address LLM replies by `(prompt, model, params)` automatically. Rejected: same-prompt-same-model intentionally repeated (variance sampling, A/B comparison) shouldn't dedup; the consumer expresses caching intent by wrapping the call in a transform. Sink-level caching would be opt-out, transform-level caching is opt-in — the latter matches the substrate's "explicit is better than implicit" defaults elsewhere.
- **Hub-routed LLM dispatch (single coordinator).** Centralize all LLM calls at the hub: substrate-child components mail `aether.llm.complete`, which bubbles up via ADR-0037, the hub serves all completions from a single shared adapter registry. The pull is real — one Claude subscription is rate-limited as a unit, so multiple substrates each invoking `claude` concurrently can blow the limit; centralized dispatch can serialize / queue / throttle. Single CLI install, single credential surface, single observability stream. Rejected for v1 because (a) every LLM call would cost a hub round-trip even when the consumer is on the same machine, (b) headless-only deployments without a hub get nothing, and (c) the forcing function (multiple concurrent agent loops sharing one subscription) doesn't exist yet. The right shape when it does: a `bubble_to_hub` adapter loaded on the substrate-children, dispatched through ADR-0037's bubbling — wire-additive, the chassis-owned sink stays unchanged.
- **Specialized LLM-only chassis.** A new chassis kind whose only job is to expose the LLM sink, with components needing LLM access living there and others mailing across. Rejected: components frequently want LLM access *and* other capabilities simultaneously (a sculptor wants LLM + mesh-editor mail dispatch + frame capture). Splitting capabilities across chassis costs a hop per call. Chassis-owned sink keeps composition local.
- **LLM as a substrate-core capability.** Bake the sink directly into `aether-substrate-core` so it's not chassis-optional. Rejected: not all deployments want LLM (a CI test runner that just exercises mesh dispatch shouldn't load the adapter machinery). Same reason `io`, `net`, and `audio` are chassis-owned, not core.

## Follow-up work

- **PR**: kinds + schema-derive — `CompleteRequest`, `CompleteResult`, `Usage`, `LlmError`, `ModelInfo`, `CancelRequest`/`Result`, `ListModelsRequest`/`Result` in `aether-kinds`.
- **PR**: substrate sink — `LlmAdapter` trait, `LlmAdapterRegistry`, `claude_cli` subprocess adapter (lifting from `spikes/prompt-pipeline-spike/src/claude.rs`), `http_gemini` adapter (lifting from `spikes/prompt-pipeline-spike/src/gemini.rs`, dispatched through the net sink), thread pool integration, env-var config, capability gate stub for ADR-0044. `http_anthropic` adapter optional in this PR or follow-up; gated on operator API setup either way.
- **PR**: hub MCP — `llm_status` tool surfacing per-adapter / per-model dispatch counts and cost.
- **PR**: observation — `aether.observation.llm_cost` broadcast every 30s, on the same publisher as `frame_stats`.
- **Parked, future ADR**: streaming completion (`aether.llm.stream`).
- **Near-term follow-up ADR (Spike B has validated the shape)**: multimodal completion (`aether.llm.complete_multimodal` with `Vec<Ref<Image>>`). The wire shape, adapter dispatch path, and response handling are pre-validated by the spike's `gemini::generate_text(prompt, references, model)` worked example. Lift the spike's HTTP body shape (`parts: [text, inlineData...]`) into the adapter trait method.
- **Parked, future ADR**: structured output (response schema, tool use).
- **Parked, future ADR**: embeddings sink.
- **Parked, future ADR**: per-component / per-model capability gradations (cost-control workflows).
- **Parked, future ADR**: credential vault (multi-tenant deployments, key rotation).
- **Parked, future ADR**: TOML config layer for richer per-deployment configuration (matches ADR-0041's deferred TOML+CLI work).
