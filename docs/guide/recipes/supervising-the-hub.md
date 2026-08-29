# Supervising the hub with systemd

**Class:** drive-only (nothing here rebuilds aether beyond the hub binary
itself). The hub is a thin coordinator with no durable state of its own, so its
supervision is declarative: a checked-in systemd user unit, one uncommitted
environment file per host, and journald. Read
[Engine fleet](../operating/engine-fleet.md) for what the hub serves once it is
up, and [Harness lifecycle and fleet-wide mutations](../operating/harness-lifecycle.md)
for what a restart costs the fleet.

This unit is a sibling of the coordinator's
([Supervising the coordinator with systemd](supervising-the-coordinator.md)),
not a child of anything. The development tunnel (`aether-tunnel`, ADR-0089)
forks and supervises its own hub for the duration of an MCP session; that hub is
part of the harness. A hub under this unit is the standing fleet coordinator on
the host, and nothing sits above it. Run one or the other on a host, not both on
the same port.

Two files in the repository carry the whole arrangement:

| File | Role |
|---|---|
| `scripts/aether-hub.service` | The unit. Generic — no path, port, or cadence is written into it. |
| `scripts/hub.env.example` | The template for the host's values. Copy, edit, never commit. |

## 1. Build the hub

```bash
cargo build --release -p aether-chassis-hub --bin aether-hub
```

The unit executes a built binary rather than `cargo run`, so a restart is
immediate and a new build takes effect when you say so.

Build the chassis binaries the hub will hand out too — at minimum
`aether-headless`, and `aether-desktop` where the host can render — since the
environment file names them for the store bootstrap in the next step:

```bash
cargo build --release -p aether-chassis-headless --bin aether-headless
```

## 2. Fill in the environment file

```bash
mkdir -p ~/.config/aether
cp scripts/hub.env.example ~/.config/aether/hub.env
$EDITOR ~/.config/aether/hub.env
```

Every value the hub resolves from its environment lives here: the binary, the
RPC port, the two store roots, the startup binary bootstrap, the log filter, and
the tuning knobs for engine liveness, spawn patience, and automatic restart. The
template documents each one at its compiled default and cites the file and line
where it is read. Three of them decide whether the arrangement holds up:

- **`AETHER_FLEET_STORE_ROOT`** must be set. Every spawn materializes the
  resolved substrate binary to `<root>/<engine-id>/substrate`, a full copy of a
  chassis binary per engine, and nothing removes that directory when the engine
  dies. The unit's `ExecStartPre` reaps this root at every start and refuses to
  guess one, so the root belongs to exactly one hub.
- **`AETHER_BINARY_STORE_DIR`** should name a directory *outside* that root.
  This is the content-addressed store of uploaded binaries and components
  (ADR-0115 / ADR-0116) and is meant to outlive a restart; the reap must never
  reach it.
- **`AETHER_BINARY_BOOTSTRAP`** names the chassis binaries ingested into that
  store at startup, comma-separated absolute paths, each named by its file stem.
  Without at least one, a `spawn_substrate` with no selector — or with a `name`
  selector — has nothing to resolve in a freshly started hub until something
  uploads a binary. Ingest is content-addressed and idempotent, so listing the
  same paths at every start costs nothing.

One knob is worth knowing about before you leave the hub unattended:

- **`AETHER_HUB_RESTART_ON_CRASH=true`** makes the engines cap re-fork a crashed
  or evicted engine from the recipe it was spawned with, under a burst limit
  (`AETHER_HUB_RESTART_BURST_LIMIT`) counted over a rolling window
  (`AETHER_HUB_RESTART_BURST_WINDOW_SECS`) — the same start-limit shape this
  unit uses on the hub itself. Off by default: the cap's contract is that a
  death is terminal and the caller re-spawns. A deliberate `terminate_substrate`
  is never restarted whatever this says.

  The successor engine gets a **new** engine id, so an observer holding the old
  one learns the engine is gone from the recently-died ring exactly as it always
  has. Turn this on when the fleet is long-lived and unattended; leave it off
  when a caller is driving spawns and expects to see a death.

The environment file holds no credential, and the hub asks for none — it reads
no token, no webhook, and no repository credential of any kind. `HUB_BIN`, the
one variable in the file that is not a config knob, deliberately carries no
`AETHER_` prefix: the hub sweeps its own environment at boot and warns on every
`AETHER_*` variable no registered knob claims, so a wrapper variable under that
prefix would log a spurious warning at every start. That same sweep is why a
typo in this file surfaces in the journal instead of being silently ignored.

To see what the hub actually resolved, ask the binary rather than reading the
file back:

```bash
aether-hub --print-config   # source-resolved value of every knob
aether-hub --describe       # linked caps and build provenance
```

## 3. Install and enable the unit

```bash
mkdir -p ~/.config/systemd/user
cp scripts/aether-hub.service ~/.config/systemd/user/aether-hub.service
systemctl --user daemon-reload
systemctl --user enable --now aether-hub
```

On a headless host, allow the user manager to run without a login session, or
the hub stops when you disconnect and never starts at boot:

```bash
loginctl enable-linger "$USER"
```

## 4. Operate it

| To… | Run |
|---|---|
| restart the hub | `systemctl --user restart aether-hub` |
| read its logs | `journalctl --user -u aether-hub` |
| follow them live | `journalctl --user -u aether-hub -f` |
| read one past run | `journalctl --user -u aether-hub --since "1 hour ago"` |
| stop it, and have it stay stopped | `systemctl --user stop aether-hub` |
| see liveness, uptime, and how it last exited | `systemctl --user status aether-hub` |

Logs go to journald. A hub started this way is not the tunnel's hub, so the
tunnel's out-of-band `POST /admin/restart-hub` does not reach it; `systemctl
--user restart aether-hub` is the restart.

`Restart=on-failure` draws the line the operator cares about. A crash comes back
on its own after five seconds. A `systemctl --user stop` stays stopped, and so
does a clean shutdown, since the hub handles `SIGTERM` and exits `0`. Five
failed starts inside five minutes stop the unit in a `failed` state rather than
looping forever, so a hub that cannot come up is visible in `status` instead of
quietly cycling.

## What a restart does to work in flight

Nothing survives it. The hub holds its fleet table in memory, so a restart
destroys every live engine and every `engine_id` an observer was holding; the
process-local id sequence resets, so an id that reappears later is not proof of
engine continuity. Reacquire the fleet with `list_engines` after a restart
rather than reusing an id captured before it. Any client connected over RPC —
`aether-mcp` included — must re-dial.

The substrate children do not survive either, and that is deliberate on both
sides. Tearing down, the hub terminates and reaps each substrate it forked, so a
clean stop never orphans one. Stopping the unit also kills the whole control
group, which is the backstop for the case where the hub cannot do it itself: a
forked substrate runs in its own process group, which a signal to the hub alone
would not reach, but the cgroup holds it either way. The `ExecStartPre` reap then
clears the materialized binaries those engines left under the store root before
the new process starts.

What does outlive a restart is the content-addressed store: uploaded binaries
and components stay in `AETHER_BINARY_STORE_DIR`, and the startup bootstrap
re-ingests the named chassis binaries idempotently, so selectors resolve again
as soon as the hub is up.

## Troubleshooting

| Symptom in `systemctl --user status aether-hub` | Cause |
|---|---|
| `hub.env: No such file or directory` | Step 2 was skipped, or the copy landed somewhere other than `~/.config/aether/`. |
| `ExecStartPre` fails complaining that `AETHER_FLEET_STORE_ROOT` is not set | The environment file names no store root, and the reap refuses to guess one. |
| Start fails immediately with `HUB_BIN: parameter null or not set` | The environment file was copied but not edited, or step 1 never produced the binary at the path it names. |
| Start fails, journal names an address already in use | Another hub — often a development tunnel's — already holds `AETHER_RPC_PORT`. |
| Unit runs, journal warns about an unknown `AETHER_` env var | A key in the environment file is misspelled or stale; the hub ignores it. Check it against `aether-hub --print-config`. |
| Unit runs, but `spawn_substrate` cannot resolve `default` | `AETHER_BINARY_BOOTSTRAP` is unset or names paths that do not exist, so the store has no named chassis binary. |
| An engine dies at spawn and the journal names a connect budget | The host is slow enough that a cold substrate start exceeds `AETHER_HUB_PROXY_CONNECT_BUDGET_SECS`. Raise it, or spawn a release build. |
| Engines are evicted while apparently healthy | The heartbeat is too tight for the host. Raise `AETHER_HUB_HEARTBEAT_INTERVAL_SECS` or `AETHER_HUB_HEARTBEAT_MISS_LIMIT`. |
