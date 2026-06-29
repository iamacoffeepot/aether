# ADR-0050: Per-provider content-gen caps

- **Status:** Accepted (anthropic + gemini shipped; the openai amendment is deferred/unbuilt)
- **Date:** 2026-04-25
- **Revised:** 2026-05-19 (iamacoffeepot/aether#1001) — rewritten from a single `llm` sink to per-provider caps (`aether.anthropic`, `aether.gemini`) with per-API kinds, matching iamacoffeepot/aether#989. CLI is now a sibling kind; media outputs return file paths; retired broadcast-sink cost telemetry removed (issue #775).
- **Revised:** 2026-05-29 — added the `aether.openai` image cap (image→image `edit` + text→image `generate` via `gpt-image-1`) as a third instance of the per-provider / per-API pattern, in lieu of a separate ADR. A per-API ADR would fragment one decision across many docs; this ADR is the home for "how a provider API becomes a substrate cap," so a new provider amends it rather than spawning a sibling.

## Context

ADR-0046's content-generation pipeline pattern depends on a missing primitive: caps that call provider APIs. Pipelines that frame, distill, scrub, translate, compose text, or generate images and music dispatch those provider calls through the substrate, identical to how they'd dispatch a file write (ADR-0041) or an HTTPS fetch (ADR-0043). The DAG cluster (ADR-0045/0047/0048/0049) gives that dispatch caching + provenance for free; these caps close the loop by giving the pipeline its main paid-call dispatchers under the same primitives.

The original framing of this ADR (2026-04-25) was a single `"llm"` sink with a model-prefix adapter registry routing both text and image generation through one `aether.llm.complete` kind whose reply was `Ok { text, ... }`. Three things made that shape wrong:

- **Provider APIs are products, not interchangeable backends behind a model string.** Auth is per-provider (`ANTHROPIC_API_KEY` vs `GEMINI_API_KEY`), rate limits are sometimes per-provider, and each surface has quirks — Anthropic's `system` field is meaningless to a Gemini image request; Nano Banana's `aspect_ratio` is meaningless to an Anthropic messages request. Squeezing them through one kind forces `Option<everything>` schemas, the smell that says you're abstracting at the wrong layer.
- **Image generation can't be a text-completion reply.** Routing image gen through an `Ok { text }` reply is a real modeling bug — the reply shape can't carry a PNG. Media outputs are file paths, not text.
- **The original cost-telemetry surface no longer exists.** ADR-0050 published `aether.observation.llm_cost` to the broadcast sink every 30s. The broadcast sink and the entire `aether.observation.*` family retired in issue #775. That telemetry path is gone.

The deployment context for v1 is unchanged and load-bearing: **the user's Claude access is the local `claude` CLI driven by a subscription** — *not* direct Anthropic API access. Subscription billing means there's no API-key budget for routine development workflows; the CLI path has to be a first-class call surface, not a fallback. HTTP-based providers (Gemini for image + music generation, future API-budgeted Claude / OpenAI usage) coexist; the subprocess path is a peer, not a hidden routing detail.

This ADR commits to the per-provider cap mail surface, the per-cap adapter model, the v1 backends, the versioning policy, credential handling, and the observability surface — matching iamacoffeepot/aether#989, which implements it. Streaming, vision/multimodal, embeddings, and structured output are deferred to follow-up ADRs.

## Decision

The single `"llm"` sink is replaced by **one cap per provider, one kind per API.** The provider caps (`aether.anthropic` + `aether.gemini` shipped in v1; `aether.openai` added 2026-05-29):

- **`aether.anthropic`** — text completion via the official Messages API (HTTPS) *and* the local `claude` CLI subprocess, as explicit sibling kinds.
- **`aether.gemini`** — media generation only: image (Nano Banana) + music (Lyria). No text completion, no embeddings (the user defaults to Claude CLI for text; embeddings deferred until a use case appears).
- **`aether.openai`** — image generation only: image→image `edit` + text→image `generate` (`gpt-image-1`). Added 2026-05-29 for image→image transforms (depth / segmentation / stylized passes derived from a source frame). No text completion (Claude CLI is the text default).

Each cap owns provider-scoped state: auth, rate-limit budget, and its client / subprocess slot. Adding a provider (`aether.suno`, `aether.runway`, …) is a new cap; existing caps don't churn — as `aether.openai` itself demonstrates: it was added later with no change to the other two.

### 1. Mail surface

#### `aether.anthropic`

Two request kinds with identical input/output schemas, different routing — the caller picks the routing by picking the kind:

```rust
aether.anthropic.messages.send { request_id, model, messages, max_tokens?, temperature?, system? }
aether.anthropic.cli.send      { request_id, model, messages, max_tokens?, temperature?, system? }

aether.anthropic.messages.send_result : Ok  { request_id, text, model_used, usage }
                                      | Err { request_id, error: AnthropicError }
aether.anthropic.cli.send_result      : Ok  { request_id, text, model_used, usage }
                                      | Err { request_id, error: AnthropicError }
```

- `aether.anthropic.messages.send` — HTTPS to `api.anthropic.com` against the official **Messages API** (`/v1/messages`). Auth: `ANTHROPIC_API_KEY` env var, per-token billing. (This is the Messages API, *not* the deprecated text-completion `/v1/complete` endpoint — hence the reply field is `text: String`, never `completion`.)
- `aether.anthropic.cli.send` — spawns the local `claude` binary as a subprocess, pipes the request through stdin/stdout. No API key needed; it uses the user's subscription. Skips with `Err { error: AnthropicError::CliNotFound }` if `claude` is not on PATH.

CLI and Messages are **sibling kinds, not a hidden adapter choice.** A caller that wants the subscription rail sends `aether.anthropic.cli.send`; one with API budget sends `aether.anthropic.messages.send`. The "registry silently routes to CLI vs HTTP" model is gone — routing is the kind name, visible in `describe_kinds`.

```rust
struct Message { role: Role, content: String }   // role: User | Assistant
enum Role { User, Assistant }

struct Usage {
    input_tokens: u32,
    output_tokens: u32,
    wall_clock_millis: u32,
    cost_micros: Option<u64>,   // 1/1_000_000 USD; API reports it, CLI subscription leaves it None
}

enum AnthropicError {
    Overloaded,
    RateLimited { retry_after_millis: Option<u32> },
    ContextLengthExceeded { limit: u32 },
    Unauthorized,
    ContentPolicyRefused,
    CliNotFound,                // claude binary absent from PATH (cli.send only)
    UnknownModel { model: String, supported: Vec<String> },
    AdapterError(String),
}
```

`request_id` is a caller-supplied `u64` echoed on the reply (correlation by structured field, not FIFO position — the ADR-0041 convention). Single-shot completion only in v1; multi-turn is composable on top — a component that wants conversation state holds it locally and re-prompts with the assembled `messages`.

#### `aether.gemini`

Media-generation only. Two request kinds, each modeled on the actual Gemini API request/response shape for that endpoint (no `Option<everything>` at the cross-modality level). Auth for both: `GEMINI_API_KEY` env var. Reference images come in as file paths; the cap reads bytes before dispatching.

```rust
aether.gemini.nanobanana.generate {
    request_id: u64,
    model: String,                          // "gemini-2.5-flash-image",
                                            //   "gemini-3-pro-image-preview",
                                            //   "gemini-3.1-flash-image-preview" (NB2, default)
    prompt: String,
    aspect_ratio: AspectRatio,
    image_size: Option<ImageSize>,          // adapter enforces per-model
    thinking_level: Option<ThinkingLevel>,  // NB2 only; rejected on older models
    include_thoughts: Option<bool>,         // NB2 only
    object_reference_paths: Vec<String>,    // up to 10 (NB2) / 6 (NB Pro) / 0 (NB1)
    character_reference_paths: Vec<String>, // up to 4 (NB2) / 5 (NB Pro) / 0 (NB1)
    use_grounding: Option<bool>,            // NB2 only
}

aether.gemini.nanobanana.generate_result : Ok {
    request_id, output_path, model_used, usage,
    thought_signature: Option<String>,      // NB2 only; pass back unchanged for multi-turn
    grounding: Option<GroundingMetadata>,   // when use_grounding=true
} | Err { request_id, error: GeminiError }

aether.gemini.lyria.generate {
    request_id, model, prompt, duration_s, /* surveyed at implementation */
}
aether.gemini.lyria.generate_result : Ok  { request_id, output_path, model_used, usage }
                                   | Err { request_id, error: GeminiError }
```

**Media outputs are file paths, not text.** Each result carries `output_path: String` — the cap stages the generated bytes to `save://gen/<uuid>.png` (image) or `save://gen/<uuid>.wav` (audio) and returns the path; the caller resolves via `aether.fs` or directly. This is the fix for the original ADR's modeling bug: an image cannot ride an `Ok { text }` reply, and Lyria/music was never modeled at all.

`AspectRatio` is a typed enum (union of all model-supported ratios — `ASPECT_RATIO_1_1`, `ASPECT_RATIO_16_9`, … plus NB2-only extreme ratios like `ASPECT_RATIO_1_4`). `ImageSize` covers `S512` / `K1` / `K2` / `K4`. The adapter validates per-model and rejects unsupported combinations before any HTTP dispatch (`GeminiError::AspectRatioNotSupportedByModel`, `ImageSizeNotSupportedByModel`). `seed` and `negative_prompt` are deliberately absent — they don't exist in the Gemini Image API surface as of 2026-05. The Lyria request/response surface is surveyed at implementation time (same approach as the Nano Banana survey: model the kind on what exists, don't invent params); `output` is audio bytes staged to `save://gen/<uuid>.wav`, `duration_s: u32` bounded by the API's max-duration constraint at the cap layer.

```rust
enum GeminiError {
    RateLimited { retry_after_millis: Option<u32> },
    ContentPolicyRefused,
    Unauthorized,
    UnknownModel { model: String, supported: Vec<String> },
    AspectRatioNotSupportedByModel { model: String, aspect_ratio: AspectRatio, supported: Vec<AspectRatio> },
    ImageSizeNotSupportedByModel { model: String, image_size: ImageSize, supported: Vec<ImageSize> },
    MissingRequiredField { model: String, field: String },
    AdapterError(String),
}
```

Per-provider error types carry provider-specific failure shapes instead of a generic `LlmError` that loses information.

#### `aether.openai`

Image generation only — added 2026-05-29 for the image→image transform use case (depth maps, segmentation maps, stylized renders derived from a source frame), which GPT's image model handles markedly better than text-to-image-only backends. Two request kinds, modeled on the OpenAI Images API. Auth: `OPENAI_API_KEY` env var. Source images come in as file paths; the cap reads bytes before dispatch (the same convention as Gemini's reference paths).

```rust
aether.openai.image.edit {                  // the load-bearing kind: image(s) + prompt -> image
    request_id: u64,
    model: String,                          // "gpt-image-1" (newer models ride this field, §4)
    image_paths: Vec<String>,               // source frame(s); read to bytes before dispatch
    mask_path: Option<String>,              // optional inpainting mask
    prompt: String,
    size: Option<ImageSize>,                // adapter enforces per-model
    quality: Option<String>,                // low | medium | high
    input_fidelity: Option<InputFidelity>,  // how strictly to preserve the source structure
    n: Option<u32>,
}

aether.openai.image.generate {              // text -> image sibling
    request_id, model, prompt, size?, quality?, n?,
}

aether.openai.image.edit_result
aether.openai.image.generate_result :
    Ok  { request_id, output_paths: Vec<String>, model_used, usage }
  | Err { request_id, error: OpenaiError }
```

Output PNG bytes stage to `save://gen/<uuid>.png`; `output_paths` is a `Vec` because a request may ask for `n > 1`. **`gpt-image-1` returns base64 inline, not a URL** — the cap decodes the b64 payload and stages it (no separate download step, unlike a URL-returning API).

`input_fidelity` is OpenAI-specific and load-bearing for this cap's purpose: it controls how faithfully the model preserves the source image's structure. For the image→image transform use case — a depth or segmentation map must register pixel-for-pixel with the input frame — a high-fidelity setting is the lever against the "reimagine the scene" drift that misaligns the output, so the field is first-class rather than buried in an options bag. Exact `size` / `quality` value sets are surveyed at implementation against the live API (the same "model the kind on what exists, don't invent params" discipline as the Nano Banana / Lyria surfaces).

```rust
enum OpenaiError {
    RateLimited { retry_after_millis: Option<u32> },
    ContentPolicyRefused,
    Unauthorized,
    UnknownModel { model: String, supported: Vec<String> },
    UnsupportedParam { model: String, field: String, reason: String },
    AdapterError(String),
}
```

Dispatch (spawn-and-die, §2), versioning (§4), capability gating (§6), observability (§7), and handle-store integration (§9) are inherited from the per-provider pattern unchanged — the point of the pattern is that a new provider adds a mail surface and an adapter, nothing more.

### 2. Adapter model

There is no global model-routing registry. Each cap holds its own adapter, and routing to an API is the kind name, not a `model → adapter` lookup. The adapter is the **compat layer between the caller-stable kind and the underlying HTTP / subprocess wire**, holding a `model → ApiShape` dispatch table:

```rust
trait GeminiAdapter: Send + Sync {
    fn nanobanana_generate(&self, req: &NanobananaGenerate) -> Result<NanobananaResult, GeminiError>;
    fn lyria_generate(&self, req: &LyriaGenerate) -> Result<LyriaResult, GeminiError>;
}

trait AnthropicAdapter: Send + Sync {
    fn messages_send(&self, req: &MessagesSend) -> Result<MessagesResult, AnthropicError>;
    fn cli_send(&self, req: &CliSend) -> Result<MessagesResult, AnthropicError>;
}

trait OpenaiAdapter: Send + Sync {
    fn image_edit(&self, req: &OpenaiImageEdit) -> Result<OpenaiImageResult, OpenaiError>;
    fn image_generate(&self, req: &OpenaiImageGenerate) -> Result<OpenaiImageResult, OpenaiError>;
}
```

Each adapter dispatches by `model: String` through a `model → ApiShape` table; per-shape request/response constructors translate to/from the underlying wire. Unknown model → `…Error::UnknownModel { model, supported }`. Adding a known model that speaks an existing shape is a one-line table entry — no kind change, no caller change.

#### Dispatch: cap-local spawn-and-die with a per-cap concurrency bound

Each cap is a single-threaded actor — one OS thread per actor (ADR-0038) — so request intake is already serialized on the mail queue. Long-tail provider calls (multi-second image generation, a `claude` subprocess) must not block that intake, but the adapter call itself is blocking (`ureq` for HTTPS, a child process for the CLI — the same blocking-client posture as `aether.http`). The cap reconciles the two with **cap-local spawn-and-die**:

- For each request the actor takes off its mail queue, if `in_flight < max_in_flight` it spawns **one ephemeral OS thread** to run the blocking call and increments `in_flight`; otherwise it pushes the request onto a `pending: VecDeque`.
- The ephemeral thread does the blocking HTTPS / subprocess call, sends the result back as a reply mail through the `Mailer` loopback (correlated to the request by `request_id`), and **dies**. It touches **no actor state** — its only end-of-life action is sending the reply, the "alert" that the call finished.
- When that reply lands back **on the actor thread**, the actor decrements `in_flight`, applies any stateful bookkeeping (rate-limit accounting), and pops + spawns the next `pending` request if the queue is non-empty.

There is **no `Semaphore` and no `Mutex`.** Because the actor is single-threaded, the concurrency bound is just an `in_flight: usize` counter plus a `pending` queue living in the actor's lock-free state (consistent with the actor-state-no-locks discipline). The "permit release" *is* the reply mail; the single-threaded actor is the mutual exclusion. The ephemeral-thread-per-call shape matches the existing `aether.tcp` cap (ADR-0043), which spawns a sidecar OS thread and routes its result back through the same `Mailer` loopback.

`max_in_flight` is per-cap and configurable. It **doubles as rate-limit throttling**: the paid provider endpoints (Anthropic Messages, Gemini) are rate-limited, so bounding the number of concurrent in-flight calls is correct behaviour, not just resource hygiene — the bound keeps the cap from issuing a burst the provider would reject.

`save://gen/` is the default output namespace for binary outputs. Configurable via `AETHER_GEN_DIR`; the resolved path lands in the reply kind.

### 3. v1 backends

**`aether.anthropic`** — two backend paths under one cap:

- **Messages API** (`aether.anthropic.messages.send`). HTTPS to `api.anthropic.com/v1/messages`. Loaded when `ANTHROPIC_API_KEY` is set; per-token billing; reports full `Usage` including `cost_micros` from the API pricing table. Absent in the user's default workflow.
- **CLI subprocess** (`aether.anthropic.cli.send`). Runs the local `claude` binary as a child process per request, request piped through stdin, `text` captured from stdout, stderr to the actor log ring. The load-bearing v1 path: the user runs Claude via subscription exercised through the CLI, no API key configured. Validated empirically in `spikes/prompt-pipeline-spike/` — five experiment runs across Haiku / Sonnet / Opus profiles, content-addressed caching, no failures; the spike's `src/claude.rs` is the reference. `Usage` from the CLI reports `wall_clock_millis` only; `input_tokens` / `output_tokens` are `0` and `cost_micros` is `None` (the CLI's text-output mode doesn't surface tokens, and subscription billing isn't per-call). If `claude` isn't on PATH, `cli.send` replies `Err { error: CliNotFound }`.

Both kinds share the cap's rate-limit budget tracker when the user is on one Anthropic account — at the provider level they aren't separate buckets, though today CLI uses subscription quota and Messages uses per-token billing, so the two paths don't actually interact.

**`aether.gemini`** — two backend kinds under one cap, both HTTPS to `generativelanguage.googleapis.com`, auth `GEMINI_API_KEY`:

- **Nano Banana** (`aether.gemini.nanobanana.generate`) — image generation. The spike's `src/gemini.rs` validated this against `gemini-3.1-flash-image-preview`. Output PNG bytes stage to `save://gen/<uuid>.png`.
- **Lyria** (`aether.gemini.lyria.generate`) — music generation. Output audio bytes stage to `save://gen/<uuid>.wav`. Request/response surface surveyed at implementation.

Gemini is media-only here. The original ADR's "Gemini does text + vision under the `llm` sink" framing is dropped: text defaults to the Claude CLI; multimodal grading is a deferred follow-up ADR (the Spike B shape is pre-validated but out of scope for these caps).

**`aether.openai`** — image generation, HTTPS to `api.openai.com/v1/images/{generations,edits}`, auth `OPENAI_API_KEY`. `gpt-image-1`: `aether.openai.image.edit` is the primary kind (image→image — source frame + prompt → derived image), `aether.openai.image.generate` the text→image sibling. The API returns base64 PNG; the cap decodes and stages to `save://gen/<uuid>.png`. Added 2026-05-29 for image→image transforms (depth / segmentation / stylized passes) after the GPT image model proved markedly better than alternatives at producing pixel-registered derived images from stylized frames.

Local-model / enterprise-gateway / video / additional-music caps are deferred — each is a new cap when a use case arrives, not a wire change to the existing ones.

### 4. Versioning policy

Two cases, both falling out of "one cap per provider, one kind per API." This is semver-by-kind: a compatible change rides a field, a breaking change gets a new kind.

- **Compatible-shape model versions** (most transitions — a new model name, maybe new optional fields). The `model: String` field carries the version. New optional fields land as `Option<...>` on the existing kind; old callers don't set them. The `model` field does *not* fork request shape (`Option<everything>` smell) — it selects an underlying model that speaks the same request/response shape.
- **Shape-breaking changes** (rename, removed field, changed reply-enum semantics, a new reply variant the existing enum can't fit cleanly). A new sibling kind ships alongside the old one — e.g. `aether.gemini.nanobanana_v3.generate` next to `aether.gemini.nanobanana.generate`. The old kind stays callable as long as the underlying API is; the caller migrates when ready. Design for it; don't preemptively split.

**Empirical backing.** Nano Banana shipped three model releases in 18 months — `gemini-2.5-flash-image` → `gemini-3-pro-image-preview` → `gemini-3.1-flash-image-preview` (Aug 2025 → May 2026) — with *zero* shape-breaks. Every change was additive: new optional fields, new aspect-ratio enum values, model-specific value ranges enforced by the adapter. The pattern absorbs vendor evolution cleanly; no release has triggered the new-kind path yet.

**The adapter is the compat layer; the kind is the caller-stable contract.** Three classes of vendor change, three handling patterns:

1. **New compatible model** (same fields, same response). One-line addition to the `model → ApiShape` table. No kind change; the caller bumps `model: String` and nothing else.
2. **New optional field on an existing kind** (a newer model gains `style_strength`; older models ignore it). Add `Option<...>` to the kind. The adapter enforces per-model required-ness at dispatch time — e.g. an unset value required by the selected model errors `GeminiError::MissingRequiredField` before any HTTP call.
3. **Required field added / renamed / removed for a new model, or a broken reply shape.** The adapter holds two request constructors (one per `ApiShape`) and dispatches by model; the kind absorbs the union as `Option<...>`. If the *reply* shape breaks, force a new kind — reply-enum bloat with stale `_` arms is what burns callers.

Each adapter has fixture-replay unit tests (`tests/fixtures/nanobanana_v2_response.json`, etc.) that lock the vendor wire shape we built against. When the vendor changes the wire silently, the fixture-replay test fails and we update the adapter — caller code stays untouched. Same pattern in `AnthropicAdapter`.

### 5. Configuration

Per-provider env vars (v1 — same precedence-stack-deferred posture as ADR-0041's TOML/CLI):

- **`ANTHROPIC_API_KEY`** — auth for `aether.anthropic.messages.send`. Read once at startup. Absent in the user's default workflow (subscription / CLI only).
- **`GEMINI_API_KEY`** — auth for both `aether.gemini` kinds. Read once at startup.
- **`OPENAI_API_KEY`** — auth for both `aether.openai` kinds. Read once at startup. A cap whose key is unset still loads but replies `Err { error: Unauthorized }`.
- **`AETHER_GEN_DIR`** — overrides the `save://gen/` binary-output namespace.

The `aether.anthropic.cli.send` path needs no key — it relies on the `claude` binary being on PATH. The startup log emits which caps and backends initialized at INFO level; a cap whose required key is unset still loads but replies `Err { error: Unauthorized }` to API-mode requests (CLI-mode requests are unaffected).

A future ADR can add a TOML config layer (matching ADR-0041's deferral). v1 stays env-only.

### 6. Capability gating

When ADR-0044's capability system unparks, `anthropic` and `gemini` are top-level capabilities. A component without the cap can't dispatch to it (the same way components without `net` can't fetch today). Single-grant per cap in v1 — no per-model or per-kind gradations (a future ADR if cost-control workflows demand them). Pre-ADR-0044, all components on a substrate can dispatch; the trust model is "the substrate's owner trusted these components when they loaded them."

### 7. Observability

Per-request entries land in the per-actor log ring (ADR-0081):

- **DEBUG** — request submitted: `request_id`, `model`, kind, prompt length / param summary, sender mailbox.
- **INFO** — request completed: `request_id`, `model_used`, `usage`, and (for media) `output_path`.
- **WARN** — adapter error, retry, ignored unsupported parameter, per-model validation rejection.
- **ERROR** — irrecoverable failure, with the provider error variant.

**Per-call cost rides the reply, not a broadcast.** Each completion's reply carries a `usage` field (`input_tokens`, `output_tokens`, `wall_clock_millis`, `cost_micros`) — the minimum cost surface, available to the caller without polling. There is no `aether.observation.llm_cost` 30-second broadcast: the broadcast sink and the entire `aether.observation.*` family retired in issue #775, so there is no fan-out target. A future user-space observer (TCP / websocket / session-targeted mail) is the path forward for engine-out cost fan-out if a long-running session wants a near-real-time roll-up; until then, callers aggregate the reply-carried `usage` themselves.

### 8. Chassis coverage

All three caps are **chassis-owned**, like `aether.fs` (ADR-0041), net (ADR-0043), and audio (ADR-0039):

- **Desktop** — all three caps. `aether.anthropic` with the CLI path by default; API paths active when keys are set. `aether.gemini` active when `GEMINI_API_KEY` is set; `aether.openai` active when `OPENAI_API_KEY` is set.
- **Headless** — all three caps, identical semantics. Headless content-gen workloads (CI runners, batch sculpting) are a primary target.
- **Hub** — none of the caps. Mail to `aether.anthropic` / `aether.gemini` / `aether.openai` warn-drops as unknown mailbox, identical to the `aether.fs` behaviour on hub chassis. The hub coordinates substrate children; it hosts no workload components in v1.

Components needing provider access live on a desktop or headless chassis. A component on one chassis cannot dispatch through another chassis's cap directly. If a deployment grows multiple substrate children that share a single Claude subscription and concurrent CLI calls hit subscription rate limits, the right answer is a hub-routed cap dispatched through ADR-0037 bubbling — wire-additive when needed, not v1 work.

### 9. Handle-store integration

Provider calls are not auto-persisted as content-addressed handles. The reply lands as regular mail to the sender; provider-call sources are ephemeral monotonic handles (per ADR-0045 §3), not content-addressed — `temperature > 0` makes every call a fresh observation, and even `temperature = 0` can differ across model versions. A caller wanting "compute once, reuse forever" wraps the call in a transform (ADR-0048): the transform's content-address keys on the source handle id, so two pipelines wiring the same source handle into the same transform skip the second compute. This is the auto-cascade property `project_unify_workflows_under_aether` depends on, and ADR-0046's Frame / Distill stages do exactly this.

## Testing (cost-bounded CI)

Provider integration tests are the only place in the DAG-handles tree that touches paid external services; the test posture keeps CI cost at zero:

- **Stub adapters by default.** `StubAnthropicAdapter` and `StubGeminiAdapter` return canned responses without hitting the network. CI runs these smokes by default — send a kind, assert the reply (stub Nano Banana returns a fixed PNG path that exists and decodes; stub Lyria a fixed WAV path).
- **Real-API tests are `#[ignore]`.** `#[ignore = "needs ANTHROPIC_API_KEY"]` / `#[ignore = "needs GEMINI_API_KEY"]` tests call the live APIs with tiny requests when keys are present; not run in CI. Devs validate locally.
- **CLI path test.** A test that spawns the `claude` binary if it's on PATH, asserting a graceful `Err { error: CliNotFound }` skip otherwise — the user's "no API budget" rail.
- **Per-model validation tests.** For each known `model`, send an unsupported field combo and assert the adapter returns the right `GeminiError` variant *before* any HTTP dispatch. Plus an unknown-model test asserting `UnknownModel { model, supported }`.
- **Fixture-replay tests** lock the vendor wire shape (see §4).

## Consequences

### Positive

- **Pipelines have substrate-level provider dispatch under the DAG primitives.** ADR-0046's stages dispatch through well-defined per-provider caps instead of bespoke per-pipeline subprocess / HTTP management.
- **Per-provider state is owned where it belongs.** Auth, rate-limit budget, and client live on the cap, not smeared across a shared registry. Adding a provider is a new cap; existing caps don't churn.
- **CLI is a first-class, visible call surface.** The subscription rail is a kind the caller picks (`aether.anthropic.cli.send`), surfaced in `describe_kinds`, not a hidden routing detail.
- **Media outputs model correctly.** Image / music generation reply with a file path the caller resolves through `aether.fs`, not a text-shaped reply that can't carry bytes.
- **Versioning is bounded.** Compatible model changes ride the `model` field; shape-breaks get a new kind; the adapter absorbs vendor evolution with fixture-replay coverage. Empirically, three Nano Banana releases needed zero kind changes.
- **Mail-shaped surface lets the Claude harness submit calls directly.** A harness session can mail `aether.anthropic.cli.send` or `aether.gemini.nanobanana.generate` via MCP `send_mail` and observe the reply.

### Negative

- **CLI path has limited usage telemetry.** `claude` text mode doesn't report token counts and subscription billing isn't per-call, so `cli.send` reports `wall_clock_millis` only and `cost_micros: None`. Fine-grained cost accounting needs the API path.
- **Per-substrate cap set, not per-component.** All components on a substrate share each cap's auth and budget. Per-component overrides are a future complication without a forcing function.
- **No streaming, vision, or embeddings in v1.** Buffered replies only; deferred to follow-up ADRs.
- **Credential management is env-var only.** No rotation, no per-component keys. Acceptable for v1.

### Neutral

- **Mail-shape uniformity.** Each cap follows the request-kind / reply-kind / structured-field / error-variant shape of `aether.fs`, net, audio, render. No new substrate primitives — the cap actor and the per-actor mpsc dispatch (ADR-0038) compose.
- **Costs charged to the substrate's auth.** Whoever owns the env-var key (or the subscription) pays. Per-component billing is deferred.
- **No substrate-side rate limiting.** Provider rate-limit replies surface as `…Error::RateLimited`; the caller decides whether to retry. The cap's internal budget tracker is a coarse hook, not a real limiter.

## Alternatives considered

- **Leave ADR-0050 stale, mark iamacoffeepot/aether#989 authoritative.** Same problem as the ADR-0047 drift case — agents read the ADR and get misled, and the image-as-text modeling gap is a bug a reader would inherit. Rejected; this is why the ADR is rewritten rather than annotated.
- **One omnibus `aether.content_gen` cap with `Mode` and `Provider` fields.** Forces `Option<everything>` on every input — Nano Banana's `aspect_ratio` is meaningless to an Anthropic messages request; Anthropic's `system` is meaningless to a Gemini request. Schemas balloon and `describe_kinds` becomes useless. Rejected.
- **Per-modality caps (`aether.llm.completion`, `aether.image.generate`, …) with provider as a field.** Closer to right, but still pushes provider quirks into shared kinds, and versioning across providers tangles (a Nano Banana field bump ripples into every image-gen caller). Rejected after the iamacoffeepot/aether#989 discussion in favour of per-provider grouping.
- **One cap per API (`aether.gemini.nanobanana` as its own mailbox, separate from `aether.gemini.lyria`).** Splits provider-scoped state — auth keys and rate-limit budgets would have to coordinate across mailboxes. Rejected; one cap per provider, kinds per API is the right grain.
- **CLI as a hidden adapter behind a single completion kind** (the original ADR's model). Makes the subscription-vs-API choice an opaque routing detail instead of a visible kind the caller picks. Rejected for sibling kinds (`aether.anthropic.cli.send` / `aether.anthropic.messages.send`).
- **Provider calls as transform-shaped operations rather than caps.** Transforms are pure (ADR-0048 §3); a provider call is not (different replies for the same inputs, remote state, cost side effects). The cap is the right abstraction; a transform *wrapper* gives content-addressing at the wrapper layer (see §9).
- **Caps as guest components, not substrate caps.** API-key handling in wasm is worse security, and there's per-call wasm round-trip latency. The existing private `image-gen` crate already shows the substrate-cap pattern fits. Rejected on security + perf.
- **Specialized provider-only chassis / substrate-core caps.** Components frequently want provider access *and* other capabilities at once, so splitting across chassis costs a hop per call; and not every deployment wants provider machinery loaded (a CI mesh-dispatch runner shouldn't). Chassis-owned caps keep composition local — the same reasoning that makes `aether.fs`, net, and audio chassis-owned.

## Follow-up work

- **Implementation**: iamacoffeepot/aether#989 — `aether.anthropic` (messages + cli) and `aether.gemini` (nanobanana + lyria) caps, kind modules, per-cap adapters with `model → ApiShape` tables, stub + fixture + `#[ignore]` real-API tests, desktop + headless chassis registration.
- **Implementation (`aether.openai`)**: `gpt-image-1` image `edit` (primary) + `generate` kinds in `aether-kinds`, the `aether.openai` cap + `OpenaiAdapter` (`model → ApiShape` table, base64 → stage), `StubOpenaiAdapter` + fixture-replay + `#[ignore]` real-API tests, desktop + headless registration. Reuses the `contentgen/{dispatch,shared,staging}` infra unchanged.
- **Parked, future ADR**: streaming completion (`aether.anthropic.messages.stream`).
- **Near-term follow-up ADR (Spike B has validated the shape)**: multimodal / vision completion — image inputs via `Ref<Image>` (ADR-0045 handle refs), lifting the spike's `parts: [text, inlineData…]` body shape.
- **Parked, future ADR**: structured output (response schema, tool use).
- **Parked, future ADR**: embeddings cap.
- **Parked, future ADR**: additional provider caps (`aether.suno`, `aether.runway`, …) as use cases arrive.
- **Parked, future ADR**: per-component / per-model capability gradations; credential vault; TOML config layer (matches ADR-0041's deferral).
- **Parked, future generalization — global named thread-pool registry.** The cap-local `in_flight` counter + `pending` queue (§2) is sufficient until backpressure has to be shared *across* caps. When multiple caps want shared, configurable, bounded pools — or when CPU-bound vs I/O-bound work needs centralized sizing — the generalization is a substrate-level registry of named pools selected by a `ThreadPool` type (it qualifies *which* pool a unit of work runs on: an HTTP pool vs a CPU/transform pool — explicitly **not** named `PoolKey`). This relates to ADR-0048's transform compute pool, the CPU-bound sibling; both the I/O-bound provider-call pool here and that transform pool would eventually be named `ThreadPool`s under one registry. Out of 0.4 scope: the per-cap counter is enough until cross-cap backpressure forces the generalization.
