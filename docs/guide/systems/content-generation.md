# Content-generation capabilities

Aether's pattern for long-running provider calls — text, images, or music — is
a wasm guest component a substrate loads on demand (ADR-0159). The component
runs the pure request/response logic but holds no key and owns no socket,
subprocess, or disk: it reaches the network through `aether.http`, a local CLI
through `aether.process`, and artifact staging through `aether.fs`, addressing
each edge capability by mail.

No provider component currently ships in the workspace. The earlier ones left
the workspace unused, and git history holds them; a new provider starts from the
current actor API and the edges below.

Provider access is opt-in. The default chassis composition carries no provider
component; a workload that wants one uploads and loads it like any other
component (see [Components](components.md)). A pure-rendering or CI substrate
links none of the provider machinery at boot.

## Edges a provider mails

Each request/reply flow is the ADR-0139 `send_with_context` / `take_context`
two-handler shape. The request handler builds the provider request body,
stashes the caller's reply handle as a context, and dispatches one edge request;
the reply handler recovers the context, parses the edge reply, and replies the
provider's own `_result` kind to the original caller.

- **HTTP APIs** ride `aether.http.fetch`. The component sets no key header:
  `aether.http` attaches the operator's secret binding for that host to the
  fetch (ADR-0235). Egress is bounded per-sender at the `aether.http` edge
  (ADR-0158), so the component queues nothing itself. See [HTTP](http.md).
- **A local CLI backend** rides `aether.process.run`. The process allowlist is
  deny-by-default and must admit the binary; an allowlist that omits it yields a
  typed refusal rather than a hang. Invocation is argv-array only, so guest
  prompts stay data rather than shell fragments. No API key rides this path.
- **Artifact staging** (a provider returning binary media) rides
  `aether.fs.write` to the `save` namespace; the reply carries the staged path.

A large response can approach the `aether.http` body cap
(`AETHER_HTTP_MAX_BODY_BYTES`, 16 MB default) and the RPC frame budget
(`AETHER_MAX_FRAME_SIZE`), since the artifact bytes ride the fetch reply and then
mail; raise those knobs for multi-megabyte payloads.

## Output staging

Binary media does not ride inline in reply mail. A generating provider writes
under its configured staging directory and returns relative paths such as
`gen/<uuid>.png` — never a literal `save://` address. Treat the returned path as
an engine-side artifact reference:

- it is not automatically a path on the MCP client's machine;
- with a staging directory under the `save` namespace, a later consumer reads it
  through `aether.fs` using the `save` namespace and the returned relative path;
- cleanup and retention policy belong to the configured staging directory;
- never interpolate a generated path into a shell command.

## Keys and secrets

A provider key is not component configuration (ADR-0235, superseding ADR-0159
§5). The operator puts it in a named file under the engine's `--secrets-dir`
and binds it on `aether.http` as `--http-secrets <host>/<header>=<secret-name>`;
the http cap reads it once at boot and attaches it to each HTTPS fetch to that
exact host. The component never holds, reads, or names it, so the plaintext key
is never in guest memory, a kind, mail, or the journal. With no binding the
vendor answers 401, which a provider should surface as a typed authorization
error. See the [*Supplying secrets*](../recipes/supplying-secrets.md) recipe.

## Change route

- Edge capabilities a provider mails: `crates/aether-http/`, `crates/aether-process/`, `crates/aether-fs/`
- Decision: ADR-0159 (guest-hosted providers), ADR-0050 (kind vocabulary), ADR-0139 (reply correlation), ADR-0158 (egress bound), ADR-0235 (secrets)
- Configuration: [Configuration](configuration.md)
