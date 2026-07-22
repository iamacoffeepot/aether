# Content-generation capabilities

Aether exposes long-running provider calls — text, images, or music — as wasm
guest components a substrate loads on demand. A loaded provider component holds
its own credentials and runs the pure request/response logic, but owns no
socket, subprocess, or disk: it reaches the network through `aether.http`, the
`claude` CLI through `aether.process`, and artifact staging through `aether.fs`,
addressing each edge capability by mail (ADR-0159). The shipped provider
namespaces are `aether.anthropic` and `aether.gemini`.

Provider access is opt-in. The default chassis composition carries neither
component; a workload that wants one uploads and loads it. A pure-rendering or
CI substrate links none of the provider machinery at boot.

## Loading a provider

Upload the component wasm to the hub's content store, then load it — either at
spawn time through a boot manifest or afterward with `load_component`. The API
key and per-request tuning ride init-config bytes (ADR-0090 §5), so the raw key
never touches process env or the wire beyond the component's own `Config`:

```text
upload_component(staged_path, name)             # aether_anthropic / aether_gemini
spawn_substrate(components=[{selector, config_path}])   # boot-manifest load
  # or, on a running engine:
load_component(engine_id, selector, config_path)
```

`config_path` points at the component's init-config bytes — for anthropic the
API key, timeout, and CLI-binary override; for gemini the API key, disable flag,
timeout, and the namespace-relative staging directory the chassis used to
resolve for the native cap. A loaded component registers at
`aether.component/aether.embedded:aether.anthropic` (or `:aether.gemini`); mail
its request kinds to that lineage address.

## Shipped operations

| Namespace/kind | Purpose | Output shape |
|---|---|---|
| `aether.anthropic.messages.send` | Anthropic Messages API | text/model/usage or typed error |
| `aether.anthropic.cli.send` | local Claude CLI adapter | text/usage or typed error |
| `aether.gemini.nanobanana.generate` | model-validated image generation | staged PNG path plus metadata |
| `aether.gemini.lyria.generate` | Lyria music generation | staged WAV paths plus usage |

Each request carries a caller-supplied `request_id` echoed by its result. This
is application correlation in addition to Aether's mail correlation. The wire
kinds are byte-identical to what the retired native caps handled, so a caller
changes only the recipient address (a loaded-component lineage instead of a
chassis mailbox) and reads back the same reply kind. `describe_kinds` shows the
same kinds once the component is loaded.

## How a request reaches its provider

Each request/reply flow is the ADR-0139 `send_with_context` / `take_context`
two-handler shape. The request handler builds the provider request body with the
ported pure logic, stashes the caller's reply handle as a context, and dispatches
one edge request; the reply handler recovers the context, runs the ported
parser, and replies the provider `_result` kind to the original caller.

- **Messages API and Gemini HTTPS** ride `aether.http.fetch`. The component sets
  the `x-api-key` / auth header from its init-config and feeds the `FetchResult`
  body to the parser. Egress is bounded per-sender at the `aether.http` edge
  (ADR-0158), so the component queues nothing itself — no false early settlement.
- **The `claude` CLI backend** rides `aether.process.run`. The allowlist must
  admit `claude`; an allowlist that omits it yields the graceful `CliNotFound`
  skip the kind already models. No API key rides this path.
- **Gemini artifact staging** rides `aether.fs.write` to the `save` namespace at
  `gen/<uuid>.{png,wav}`; the reply carries the staged path. Reference images for
  Nano Banana ride `aether.fs.read`, one read per referenced path before the
  fetch.

A large render or long clip can approach the `aether.http` body cap
(`AETHER_HTTP_MAX_BODY_BYTES`, 16 MB default) and the RPC frame budget
(`AETHER_MAX_FRAME_SIZE`), since the artifact bytes ride the fetch reply and then
mail; raise those knobs for multi-megabyte payloads.

## Output staging

Binary media does not ride inline in reply mail. Successful Gemini generation
writes under the component's configured staging directory and returns relative
paths such as `gen/<uuid>.png` or `gen/<uuid>.wav` — never a literal `save://`
address. Treat the returned path as an engine-side artifact reference:

- it is not automatically a path on the MCP client's machine;
- with a staging directory under the `save` namespace, a later consumer reads it
  through `aether.fs` using the `save` namespace and the returned relative path;
- a staging directory outside the save root makes retrieval or egress
  deployment-specific rather than an `aether.fs` `save`-namespace guarantee;
- cleanup and retention policy belong to the configured staging directory;
- never interpolate a generated path into a shell command.

## Configuration and credentials

Each component receives independent enable/disable, credential, concurrency, and
timeout configuration through its init-config bytes. Missing credentials or
explicit disablement short-circuit every request to a bounded `Unauthorized`
error rather than hanging.

The interim security posture (ADR-0159 §5) places the raw key inside the
component's memory and onto the `x-api-key` header of each fetch: the trust model
is "the substrate owner trusted this component when they loaded it with this
key." Secret-reference headers, so the plaintext key never enters guest memory,
are the named future hardening. Credentials remain host configuration — they must
not appear in mail logs, generated outputs, or guide examples.

The Anthropic CLI adapter additionally crosses a subprocess trust boundary
through `aether.process`, whose deny-by-default allowlist and argv-array-only
invocation keep guest prompts as data, not shell fragments.

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

1. inspect the loaded component's actor logs and `actor_cost`;
2. determine whether the engine is still alive;
3. use application request ids to detect a late result;
4. avoid duplicating a potentially still-running paid call.

See [Inspection and debugging](../operating/inspect-and-debug.md).

## Testing

Provider logic is tested through its pure functions and a harness end-to-end
against the edge caps, never through live credentials or paid network calls. Each
provider crate carries fixture-replay unit tests over the ported request builders
and response parsers, plus a `SubstrateHarness` scenario that loads the built
component wasm and composes `aether.http` / `aether.process` with empty
allowlists to drive the deterministic refusal paths. Useful boundaries include:

- request → provider body shape;
- typed provider error mapping from a `FetchResult` / process refusal;
- the two-handler context recovery and reply routing;
- staged file extension/content;
- disabled/missing-credential behavior.

## Change route

- Anthropic kinds + guest component: `crates/aether-anthropic/src/`
- Gemini kinds + guest component + pure provider logic: `crates/aether-gemini/src/`
- Edge capabilities the components mail: `crates/aether-http/`, `crates/aether-process/`, `crates/aether-fs/`
- Decision: ADR-0159 (guest-hosted providers), ADR-0050 (kind vocabulary), ADR-0139 (reply correlation), ADR-0158 (egress bound)
- Configuration: [Configuration](configuration.md)
