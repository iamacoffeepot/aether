# Content-generation capabilities

Aether exposes long-running provider calls as native actors so guest code can
request text, images, or music without owning credentials, host networking, or
subprocesses. The shipped provider namespaces are `aether.anthropic` and
`aether.gemini`.

These capabilities are native-only runtime code. Their public kinds remain the
mail contract; availability still depends on chassis composition and
configuration.

## Shipped operations

| Namespace/kind | Purpose | Output shape |
|---|---|---|
| `aether.anthropic.messages.send` | Anthropic Messages API | text/model/usage or typed error |
| `aether.anthropic.cli.send` | local Claude CLI adapter | text/usage or typed error |
| `aether.gemini.nanobanana.generate` | model-validated image generation | staged PNG path plus metadata |
| `aether.gemini.lyria.generate` | Lyria music generation | staged WAV paths plus usage |

Each request carries a caller-supplied `request_id` echoed by its result. This
is application correlation in addition to Aether's mail correlation.

ADR-0050 contains older/deferred provider discussion. Current code is the
authority for which adapters ship: do not infer an OpenAI capability from that
ADR's historical text.

## Blocking work without blocking the actor

Provider API and CLI calls can take seconds or minutes. A handler submits them
through `NativeCtx::dispatch_blocking` rather than performing the call on the
single-threaded dispatcher.

`TaskQueue` adds a per-capability concurrency bound:

```text
request accepted
  ├─ slot free → dispatch blocking work, hold settlement open
  └─ at limit  → capture this request's hold + reply target, queue it

completion
  → resolve result
  → hand the freed slot to the next queued request
```

The queue lives in actor state without a mutex; actor dispatch is the mutual
exclusion. Queued work keeps its own settlement chain held from acceptance, so
an operator awaiting the root does not see a false early settlement.

## Output staging

Binary media does not ride inline in reply mail. Successful Gemini generation
writes under a configured staging root and returns relative paths such as
`gen/<uuid>.png` or `gen/<uuid>.wav` — never a literal `save://` address. The
filesystem policy and staging root are resolved at chassis boot.

Treat the returned path as an engine-side artifact reference:

- it is not automatically a path on the MCP client's machine;
- with the default staging root, a later consumer can read it through
  `aether.fs` using the `save` namespace and the returned relative path;
- `AETHER_GEN_DIR`/`--gen-dir` can place staging outside the save root, in which
  case retrieval or egress is deployment-specific rather than an `aether.fs`
  `save`-namespace guarantee;
- cleanup and retention policy belong to the configured staging root;
- never interpolate a generated path into a shell command.

## Configuration and credentials

Each provider has independent enable/disable, credential, concurrency, and
timeout configuration. Missing credentials or explicit disablement select a
disabled adapter that returns a bounded error rather than hanging.

Credentials are host configuration. They must not appear in guest config, mail
logs, generated outputs, or guide examples. Use `--print-config`/resolved config
inspection for non-secret shape, and redact secret values in diagnostics.

The Anthropic CLI adapter additionally crosses a subprocess trust boundary. It
uses a fixed adapter contract; guest prompts are data, not shell fragments.

## Provider validation and errors

Validate before paid work when possible. Gemini image requests check model ids,
aspect ratio, image size, reference counts, and required fields against the
selected model. Music requests validate mutually exclusive options. Typed error
families distinguish retryable rate limits from authorization, content policy,
unsupported inputs, unknown models, and adapter failure.

Callers should branch on the error enum, not substring-match display text.
Retries need their own budget and must respect reported retry timing; settlement
timeouts are not permission to launch duplicate paid requests blindly.

## Operator timeouts and evidence

The MCP traced-send default is deliberately longer than provider defaults, but
it still has a hard ceiling. If a tool times out, the current timeout response
does not expose a partial trace tree. Before retrying:

1. inspect provider actor logs and `actor_cost`;
2. determine whether the engine is still alive;
3. use application request ids to detect a late result;
4. avoid duplicating a potentially still-running paid call.

See [Inspection and debugging](../operating/inspect-and-debug.md).

## Testing

Provider logic should be tested through adapters/stubs, validation functions,
queue behavior, and staging helpers. CI must not require live credentials or
make paid network calls. Useful boundaries include:

- request → adapter shape;
- typed provider error mapping;
- queue hold/reply ownership under overflow;
- staged file extension/content and failure cleanup;
- disabled/missing-credential behavior.

## Change route

- Anthropic kinds/adapters/runtime: `crates/aether-capabilities/src/anthropic/`
- Gemini kinds/adapters/runtime: `crates/aether-capabilities/src/gemini/`
- Shared queue, staging, transport: `crates/aether-capabilities/src/shared/contentgen/`
- Settlement primitive: `crates/aether-substrate/src/actor/native/`
- Configuration: [Configuration](configuration.md)
- Decision: ADR-0050, interpreted against current code; ADR-0093 for blocking dispatch
