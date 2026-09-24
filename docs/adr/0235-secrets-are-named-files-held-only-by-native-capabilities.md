# ADR-0235: Secrets are named files, held only by native capabilities

- **Status:** Provisional
- **Date:** 2026-09-23

Supersedes [ADR-0159](0159-guest-hosted-provider-capabilities.md) §5 (API keys
ride init-config bytes) and resolves its parked "secret-reference headers"
item. Satisfies [ADR-0234](0234-a-muse-turn-is-one-sampled-program.md)
decision 6. Amends [ADR-0043](0043-substrate-http-egress-net-sink.md) (the
egress cap gains a host secret table) and
[ADR-0090](0090-application-configuration.md) (the `Config` derive gains the
`secrets` field hint). Builds on
[ADR-0162](0162-config-channels-are-addressed-never-ambient.md) (argv is the
addressed machine channel; a forked child's environment is constructed).

## Context

The engine has no way to supply or hold a secret.

- `aether-anthropic` carries its Messages API key as the `api_key` field of its
  init-config kind `aether.anthropic.config`
  (`crates/aether-anthropic/src/component/config.rs`,
  `AnthropicComponentConfig`). The key sits in the loader's config bytes, in the
  wasm component's memory, and in every `aether.http.fetch` the component
  builds, as an `x-api-key` header (`AnthropicComponent::messages_headers`).
  ADR-0159 §5 records this as an interim posture and parks "secret-reference
  headers" as the future hardening.
- The `Config` derive (`crates/aether-derive/src/config.rs`) cannot mark a knob
  secret, and `--print-config` prints every env value verbatim
  (`crates/aether-substrate/src/config/dump.rs`, `leaf_row`). A key moved onto
  a knob would print.
- ADR-0162 already treats the forked child's environment as a leak surface:
  the hub builds each child environment from an allowlist
  (`crates/aether-fleet/src/child_env.rs`, `ALLOWLISTED_NAMES`) rather than
  handing its own down.
- ADR-0234's single Muse turn needs a bearer key for one HTTPS host. Its
  decision 6 says the key never enters a kind, the journal, a recorded
  closure, the bundle, or mail the program builds, and leaves how the key is
  supplied, held, and attached to this decision.
- The Bloomery already draws this boundary for its own workers: ADR-0150,
  ADR-0152, and ADR-0194 keep secrets on the host and out of every worker.

A secret that becomes data leaks through every surface data travels: mail,
traces, the journal, config dumps, logs, argv in `ps`, and the environment of
every forked child. This ADR records one engine-wide way to supply, hold, and
use a secret, and names the established practice each part follows.

## Decision

### 1. Only a native capability holds a secret

Actors never hold, read, or name a secret value. No kind field, mail payload,
init-config byte, journal record, recorded closure, argv flag, env var, log
line, or config dump carries one. No ctx verb, host function, or kind returns
one.

This follows the boundary ADR-0150, ADR-0152, and ADR-0194 draw, where the host
holds secrets and workers never do, and the egress-gateway injection pattern,
where the proxy attaches the secret and the client never sees it.

### 2. Secrets are named files in one secrets directory

Each secret is one file. The file name is the secret's name and the file bytes
are its value. This is the systemd credentials layout (`LoadCredential=`,
`$CREDENTIALS_DIRECTORY`), the Docker secrets layout (`/run/secrets/<name>`),
and the Kubernetes secret-volume layout. Secret managers such as a Vault agent
render into the same file shape, so the engine needs no manager client.

Where the directory comes from:

- Only `--secrets-dir <path>` names it (`ChassisMeta.secrets_dir` in
  `crates/aether-chassis/src/cli.rs`, resolved by `SecretsDir::locate`). It is a
  source-selecting flag beside `--config`, so every chassis root carries it.
  The engine reads no environment variable for it.
- A systemd unit passes `--secrets-dir %d` on its `ExecStart=` line. systemd
  expands `%d` to the unit's credentials directory, the path it exports as
  `$CREDENTIALS_DIRECTORY`.
- When the flag is absent, the engine has no secrets, and a binding that names
  one fails boot.
- The path must be absolute and must name a directory.

What the loader (`SecretsDir::read_secret`) accepts:

- A secret name (`SecretName`) matches `[A-Za-z0-9][A-Za-z0-9._-]{0,63}`. It
  holds no path separator and cannot start with a dot, so no name escapes the
  directory.
- The file is a regular file of at most 65,536 bytes. The loader follows
  symlinks, because Kubernetes mounts secrets as symlinks.
- The file must not be writable by group or others, following ssh's
  `StrictModes` refusal of a tamperable key file. It may be readable by others,
  because Docker and Kubernetes mount secrets 0444 and 0644 by default.
- One trailing `\n` or `\r\n` is trimmed, because `echo` and editors add one.
  What remains must be non-empty UTF-8 with no control characters, which also
  closes header injection.
- Every refusal is a boot error (`SecretError`) naming the key, the secret, the
  path, and the rule, never the content.

Files are chosen over environment variables. The twelve-factor app keeps config
in the environment, but an environment variable is inherited by every child
process unless each fork scrubs it, same-user processes can read it through
`/proc/<pid>/environ`, and crash reports and debug dumps capture it. The
systemd `Environment=` documentation, the Docker secrets documentation, and the
OWASP Secrets Management Cheat Sheet all advise against secrets in the
environment.

### 3. Config names a secret, never its value

A consumer declares the secrets it uses in its own config knob, as a comma list
of `<key>=<secret-name>` whose key follows the consumer's own grammar. This
follows Kubernetes `secretKeyRef`, docker-compose `secrets:`, and GitHub Actions
`secrets.NAME`: the reference is ordinary config, and the value is supplied
elsewhere. The knob holds only names, so its `--print-config` row is safe.

- The knob's type is `aether_substrate::config::SecretRefs`. Its parse
  (`parse_secret_refs`, `FromStr`, `Deserialize`) validates every name, so a
  garbage value is a hard boot error (ADR-0090 §4). It has no `Serialize`.
- The derive hint `#[config(secrets)]` wires that parse on the env, file, and
  argv sides, emits an empty default, and emits a `ConfigMember::resolve` that
  binds the refs to the source stack's directory (`ConfigSources::secrets_dir`,
  `SecretRefs::bind`).
- The consumer loads the values once, in `init`, with
  `SecretRefs::load(&self) -> Result<Secrets, SecretError>`. A consumer loads
  only the names its own config binds, which is least privilege at the
  consumer.

### 4. The in-process type is an in-tree `Secret`

`aether_substrate::config::Secret` owns a UTF-8 byte buffer and has the
properties of the ecosystem's standard secret wrappers, `secrecy` and `zeroize`,
written in place so no secret-handling crate enters the dependency graph:

- `Debug` prints `Secret(<redacted>)`, and there is no `Display`.
- Reading the value takes an explicit `expose(&self) -> &str`, so every read is
  one call a grep finds.
- There is no `Clone`, because no consumer needs one.
- There is no `Serialize`, `Deserialize`, `Schema`, or `Kind`. A type that
  cannot keep its invariant across serialization is not exportable.
- `Drop` wipes the whole allocation, spare capacity included, one byte at a
  time with `core::ptr::write_volatile`, then issues
  `compiler_fence(Ordering::SeqCst)` so the optimizer cannot remove the writes
  as dead stores. This is the technique `zeroize` uses.
- The loader wraps its buffer in a `Secret` before a byte lands in it, so a
  refused value is wiped too, and it sizes the buffer from the file's metadata
  so the read never reallocates and leaves a stale copy.

`Secrets` is the loaded collection of `(key, SecretName, Secret)` entries; a
consumer moves each `Secret` out of it into its own table.

### 5. `--print-config` shows presence and source, never a value

The dump ends with a `SECRETS` section: the directory and that it came from
`--secrets-dir`, then each validly named file with status `set` or
`invalid: <rule>` (`SecretsDir::describe`). The status comes from the loader's
own checks, and each loaded `Secret` is dropped, and wiped, at once. Without a
directory the section says so and names the flag, including the systemd
`--secrets-dir %d` form. This follows `systemd-creds list`, `docker secret ls`,
and `kubectl describe secret`, which report names and metadata, never data.

### 6. HTTP secrets are tied to an exact HTTPS host and injected by `aether.http`

`HttpConfig` gains `secrets: SecretRefs` (`--http-secrets`,
`AETHER_HTTP_SECRETS`). Each key is `<host>/<header-name>`, or `<host>/bearer`,
which means `Authorization: Bearer <value>` under RFC 6750. Examples:
`api.anthropic.com/x-api-key=anthropic` and `api.muse.example/bearer=muse`.
Keying secrets by host is the long-standing convention: `.netrc` `machine`
entries, git credential helpers keyed by URL, Docker `config.json` `auths`, npm
`//host/:_authToken`, and cargo per-registry tokens.

A host has exactly one secret per header. A second binding for the same host
and header is refused at boot, with an error naming the host, the header, and
both secret names — nothing in the engine holds a secret that is never sent.
The caller does not choose a key: `Fetch` gains no field that names a secret.

The boot checks run in the cap's `init` (`build_http_adapter`,
`HostSecrets::bind` in `crates/aether-http/src/client/secrets.rs`): a bound host
missing from the allowlist, a header name that is not an HTTP token, and a
duplicate host and header pair are each a boot error. A disabled cap loads
nothing.

The injection rules (`UreqHttpAdapter::fetch_once`, `request_headers`):

- The host's bound headers are attached after the allowlist and
  `require_https` checks, on every hop, and only when the hop's host exactly
  equals a bound host.
- A plain-`http` request to a bound host is refused with
  `HttpError::InvalidUrl`, so a secret never travels in cleartext.
- A caller-set header with a bound name, compared case-insensitively, is
  dropped, with a warning naming only the header and host.
- The injected `HeaderValue` is marked sensitive with
  `http::HeaderValue::set_sensitive`, as hyper and reqwest do for
  `Authorization`, so a debug print of the request shows `Sensitive`.
- Injection happens inside the hop and never enters the caller's header list,
  so a cross-host redirect hop never carries it. The Fetch standard and
  `requests` likewise drop `Authorization` when a redirect changes host.
- `aether-http` keeps `#![forbid(unsafe_code)]`: it reads the value through
  `expose()` and builds the bearer form through `Secret::with_prefix`, which
  writes the joined value straight into a new `Secret`.

### 7. Least privilege

A consumer loads only the names it binds. An operator gives each engine its own
directory holding only what that engine needs, as systemd gives each unit its
own credentials directory. Any actor on an engine whose fetch reaches a bound
host gets the header attached, though it never sees the value. Every actor on
an engine is loaded by its operator, so this ADR accepts that and defers
per-actor scoping until a multi-tenant engine needs it.

### 8. Rotation is by restart

A secret is read once, at boot. To rotate, replace the file and restart the
engine: `POST /admin/restart-hub` for the hub, or a respawn for a hub child.
This follows systemd, where a unit's credentials are fixed for its lifetime and
a restart picks up new ones, and twelve-factor disposability. Re-reading per
request would put file I/O and partial-write races on the request path, and
watch-and-reload adds machinery no consumer needs yet. Actors never see values,
so either could be added later without an API change.

### 9. A forked substrate receives a path, never a value

A hub-spawned engine gets its directory as `--secrets-dir <path>` in
`spawn_substrate.args`, the ADR-0162 argv machine channel; a path in `ps`
exposes nothing. The hub never forwards its own directory. The engine reads no
environment variable for its directory, so nothing about secrets is inherited
through the ADR-0162 child environment. A value never rides argv, which is
world-readable in `ps` and `/proc/<pid>/cmdline`.

### 10. What changes in prior ADRs

This ADR supersedes ADR-0159 §5 and resolves its parked "secret-reference
headers" item differently than it sketched: the operator binds a secret by
host, and the component never names one. ADR-0234 decision 6 is satisfied by
decision 6 here. ADR-0043's egress gains the host secret table, and ADR-0090's
`Config` derive gains the `secrets` hint.

## Consequences

- **Actors lose no capability.** A component that set an auth header itself now
  relies on the operator's binding; `aether-anthropic` drops its `api_key` field
  and its synchronous no-key `Unauthorized` gate, and a missing binding surfaces
  as the vendor's 401, still mapped to `Unauthorized`.
- **The `aether.anthropic.config` schema breaks.** Old config bytes carrying
  `api_key` no longer decode; the break is loud, by design.
- **A request to a bound host over plain `http` now fails** with `InvalidUrl`.
- **Rotation needs a restart** (decision 8).
- **The wipe has a boundary.** It covers the `Secret`'s own buffer. It does not
  reach copies the OS makes (swap, a core dump) or the per-request header copies
  the HTTP stack builds to put the value on the wire.
- **Per-session key choice waits on invocation context (#6598):** typed context
  kinds, context requirements declared by the API type, `run` unable to read
  context, and defaults layered invocation over invoker over engine config.
  That work relaxes the one-secret-per-host-and-header refusal when it adds
  selection.
- **Follow-on:** the Bloomery mirror's `GITHUB_TOKEN` still rides the fork
  environment allowlist (`crates/aether-fleet/src/child_env.rs`); moving it onto
  a named secret file is its own slice.

## Alternatives considered

- **Environment variables such as `ANTHROPIC_API_KEY` (twelve-factor).**
  Children inherit them, `/proc` exposes them, dumps capture them, and the
  env-driven `--print-config` prints them. ADR-0162 already strips unknown keys
  from hub children, so a hub-spawned engine would never receive one.
- **Keep keys in init-config bytes (ADR-0159 §5).** The value is mail-borne data
  inside guest memory and inside every fetch the component builds.
- **A secret name on the fetch request, so a caller picks its key per
  session.** Deferred to invocation context (#6598), not decided here. Until it
  lands, a host has one secret per header, a second binding is refused, and
  `Fetch` stays unchanged.
- **Actor-named secret references (ADR-0159's parked "secret-reference
  headers").** Every actor would get a handle it could aim at any secret.
- **The `secrecy` and `zeroize` crates.** The ecosystem's standard form, but the
  owner chose an in-tree type to avoid supply-chain risk; the properties that
  matter take a few dozen lines.
- **A `*_FILE` path per knob (the Docker official-image convention).** Each
  binding would carry a full path. Names plus one directory keep config the
  same across hosts and give the dump one place to look.
- **An OS keychain or a secret-manager client.** Keychains are
  platform-specific and have no headless story; managers already render files,
  which decision 2 consumes.
- **An init-context verb that loads any named secret.** An ambient door for
  every native cap. Binding through the cap's own config knob keeps each
  consumer's names declared and printed.
- **Reading `$CREDENTIALS_DIRECTORY` when `--secrets-dir` is absent.** An
  environment read and its lint suppression; `--secrets-dir %d` gives systemd
  units the same directory with no ambient input.
- **Holding a later binding for the same host and header unused.** Nothing
  should hold a secret that is never sent, so the second binding is refused
  until #6598 adds selection.
- **Re-read per request, or hot reload.** See decision 8.
