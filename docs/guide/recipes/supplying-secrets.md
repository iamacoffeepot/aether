# Supplying secrets

**Class:** drive-only. Nothing here rebuilds aether: you put secret files in a
directory, point an engine at it with `--secrets-dir`, and bind each secret by
name in the config of the capability that uses it. The design and its reasons
are [ADR-0235](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0235-secrets-are-named-files-held-only-by-native-capabilities.md).

The rules to carry:

- A secret is **one file**: the file name is the secret's name, the file bytes
  are its value. This is the systemd credentials, Docker secrets, and
  Kubernetes secret-volume layout.
- Config holds **names, never values**. A capability binds a secret with
  `<key>=<secret-name>` in its own knob; the value stays in the file.
- Only a native capability ever holds the value. No actor, kind, mail,
  init-config, journal record, argv, environment variable, log line, or config
  dump carries it.
- Values are read **once, at boot**. Rotate by replacing the file and
  restarting.

## 1. Make the directory and a secret file

```sh
install -d -m 0700 /etc/aether/secrets
# Write the value without echoing it into your shell history:
install -m 0600 /dev/stdin /etc/aether/secrets/anthropic <<'EOF'
<paste the key here>
EOF
```

The loader's rules, each a boot error naming the secret, path, and rule — never
the content:

- **Name:** `[A-Za-z0-9][A-Za-z0-9._-]{0,63}` — no `/`, no leading dot. Files
  that don't match (`.hidden`, Kubernetes' `..data`) are simply not secrets.
- **File:** a regular file (symlinks are followed) of at most 65,536 bytes.
- **Mode:** not writable by group or others. Readable by others is fine
  (Docker and Kubernetes mount 0444 / 0644).
- **Value:** one trailing `\n` or `\r\n` is trimmed (the one `echo` and editors
  add); what remains must be non-empty UTF-8 with no control characters.

## 2. Point the engine at it

Every chassis binary takes `--secrets-dir <path>`. The path must be absolute
and name a directory. There is no environment-variable form, and without the
flag the engine has no secrets — any binding that names one fails boot.

```sh
aether-headless --secrets-dir /etc/aether/secrets ...
```

## 3. Bind a secret on `aether.http`

The http cap attaches a bound secret as a request header for one exact HTTPS
host. The knob is `--http-secrets` (`AETHER_HTTP_SECRETS`, `[http] secrets` in a
config file): a comma list of `<host>/<header-name>=<secret-name>`, or
`<host>/bearer=<secret-name>` for `Authorization: Bearer <value>`.

For the Anthropic Messages API (the `aether.anthropic` component sends no key
of its own):

```sh
aether-headless \
  --secrets-dir /etc/aether/secrets \
  --http-allowlist api.anthropic.com \
  --http-secrets api.anthropic.com/x-api-key=anthropic
```

For a bearer-token API:

```sh
  --http-allowlist api.muse.example \
  --http-secrets api.muse.example/bearer=muse
```

What the binding guarantees:

- The header rides only a request whose host **exactly** equals the bound host
  (`sub.api.example.com` and a redirect to another host get nothing), and only
  over HTTPS: a plain-`http` request to a bound host is refused with
  `InvalidUrl` before anything is dialed.
- A caller-set header of the same name is dropped (with a warning naming only
  the header and host) and replaced by the bound value.
- **One secret per host and header.** A second binding for the same host and
  header — `api.example.com/x-api-key=a,api.example.com/X-Api-Key=b`, or a
  `bearer` beside an `authorization` — fails boot, naming the host, header, and
  both secret names. Choosing between keys per session is future work
  (invocation context, #6598).
- A bound host missing from `--http-allowlist`, or a header name that is not an
  HTTP token, fails boot.

## 4. Check it with `--print-config`

`--print-config` ends with a `SECRETS` section — the directory, where it came
from, and each secret's status, never a value:

```text
SECRETS
dir: /etc/aether/secrets (from --secrets-dir)
  anthropic  set
  muse       invalid: writable by group or others
```

Without the flag it reads `dir: none (pass --secrets-dir <path>; …)`. The
`AETHER_HTTP_SECRETS` row in the knob table shows the names each binding uses.
(That row, like every knob row, resolves from the environment only; a value
passed as `--http-secrets` shows its default there.)

## 5. Under a supervisor

**systemd.** Load the secret into the unit with `LoadCredential=` and pass
systemd's credentials directory with the `%d` specifier — the same path systemd
exports as `$CREDENTIALS_DIRECTORY`, which aether does not read:

```ini
[Service]
LoadCredential=anthropic:/etc/aether/secrets/anthropic
ExecStart=/usr/local/bin/aether-headless --secrets-dir %d \
  --http-allowlist api.anthropic.com \
  --http-secrets api.anthropic.com/x-api-key=anthropic
```

**Docker.** Secrets mount under `/run/secrets` by default:

```sh
docker run --secret anthropic ... aether-headless --secrets-dir /run/secrets ...
```

**Kubernetes.** Mount a secret volume and point the flag at the mount; the
loader follows the symlinks Kubernetes creates and skips its `..data` entries:

```yaml
volumeMounts:
  - { name: aether-secrets, mountPath: /var/run/aether-secrets, readOnly: true }
volumes:
  - { name: aether-secrets, secret: { secretName: aether } }
# args: ["--secrets-dir", "/var/run/aether-secrets", ...]
```

## 6. A hub-spawned engine

A substrate the hub forks gets a path, never a value, through
`spawn_substrate.args` (the ADR-0162 argv channel):

```text
spawn_substrate(args=["--secrets-dir", "/etc/aether/secrets/engine-a",
                      "--http-allowlist", "api.anthropic.com",
                      "--http-secrets", "api.anthropic.com/x-api-key=anthropic"])
```

The hub never forwards its own directory, and nothing about secrets rides the
forked environment. Give each engine its own directory holding only what it
needs.

## 7. Rotate

Replace the file (same name, same mode) and restart the engine that reads it:
`curl -fsS -X POST http://127.0.0.1:8890/admin/restart-hub` for the hub, or a
respawn for a hub child. There is no hot reload; a running engine keeps the
value it read at boot.

## Adding a new consumer

A native capability that needs a secret declares a `SecretRefs` field with the
`#[config(secrets)]` hint on its config struct, loads it once in `init` with
`config.secrets.load()?`, and moves each `Secret` into its own table. Read the
value only through `expose()`, never log it, and never put it in a kind. The
http cap (`crates/aether-http/src/client/secrets.rs`) is the worked example.
