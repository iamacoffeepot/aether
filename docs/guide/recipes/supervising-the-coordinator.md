# Supervising the coordinator with systemd

**Class:** drive-only (nothing here rebuilds aether beyond the coordinator
binary itself). The coordinator is journal-backed and restart-safe, so its
supervision is declarative: a checked-in systemd user unit, one uncommitted
environment file per host, and journald. Read
[Driving a bloom over the REST control API](bloomery-rest-api.md) for what the
coordinator serves once it is up.

Two files in the repository carry the whole arrangement:

| File | Role |
|---|---|
| `scripts/bloomery.service` | The unit. Generic — no path, port, or cadence is written into it. |
| `scripts/bloomery.env.example` | The template for the host's values. Copy, edit, never commit. |

## 1. Build the coordinator

```bash
cargo build --release -p aether-chassis-bloomery --bin bloomery
```

The unit executes a built binary rather than `cargo run`, so a restart is
immediate and a new build takes effect when you say so.

## 2. Fill in the environment file

```bash
mkdir -p ~/.config/bloomery
cp scripts/bloomery.env.example ~/.config/bloomery/bloomery.env
$EDITOR ~/.config/bloomery/bloomery.env
```

Every value the coordinator resolves from its environment lives here: the
checkout it runs in, the binary, the durable store path, the artifacts root, the
local-worktree base, the lane scratch root, the poll interval, the HTTP port,
and the cargo target directory lanes inherit. The template documents each one at
its default. Two of them decide whether the arrangement holds up:

- **`AETHER_STORE_PATH`** must name a file. The compiled default is `:memory:`,
  and a coordinator that restarts onto an in-memory journal has forgotten every
  bloom it was driving.
- **`AETHER_LANE_SCRATCH`** should name a roomy volume other than the root
  filesystem. A model lane's child builds throwaway cargo target directories
  there, and a full root filesystem fails every later lane before it produces a
  byte of evidence.

One more knob is worth setting the first time you leave the coordinator
unattended:

- **`AETHER_BLOOMERY_NOTIFY_WEBHOOK_FILE`** names a host-local file whose
  contents are a Discord-compatible incoming webhook URL. With it set the
  coordinator posts one plain-text message per loud transition — a bloom
  sealed, landed, or superseded; a member wedged, parked, or waiting on a
  surface amendment; a host fault; a spend quiesce — so a stop reaches you
  without anyone opening the board. Unset, the notification reactor mounts
  disabled and logs one line saying so; nothing else changes.

  It is a *path* rather than the URL because the URL is a credential: anything
  passed on the command line or in the environment is readable from a process
  listing. Give the file mode `600` and keep it outside the checkout. The URL
  never reaches log output at any level.

The environment file holds no credential of its own. The unit mints
`GITHUB_TOKEN` from `gh auth token`
inside its `ExecStart` wrapper at every start, so the token lives in the
coordinator's environment for exactly as long as the coordinator does, and a
rotated one is picked up by a restart. The unit file states that choice and the
alternative it was picked over in a comment beside the line that implements it.

## 3. Install and enable the unit

```bash
mkdir -p ~/.config/systemd/user
cp scripts/bloomery.service ~/.config/systemd/user/bloomery.service
systemctl --user daemon-reload
systemctl --user enable --now bloomery
```

On a headless host, allow the user manager to run without a login session, or
the coordinator stops when you disconnect and never starts at boot:

```bash
loginctl enable-linger "$USER"
```

## 4. Operate it

| To… | Run |
|---|---|
| restart the coordinator | `systemctl --user restart bloomery` |
| read its logs | `journalctl --user -u bloomery` |
| follow them live | `journalctl --user -u bloomery -f` |
| read one past run | `journalctl --user -u bloomery --since "1 hour ago"` |
| stop it, and have it stay stopped | `systemctl --user stop bloomery` |
| see liveness, uptime, and how it last exited | `systemctl --user status bloomery` |

Logs go to journald, so the numbered `coordinatorN.log` convention is retired:
there is no log file to increment, and no host where the newest number is the
one to read. Old numbered files on a host are inert once the unit is running and
can be deleted.

`Restart=on-failure` draws the line the operator cares about. A crash comes
back on its own after five seconds. A `systemctl --user stop` stays stopped, and
so does a clean shutdown, since the coordinator handles `SIGTERM` and exits `0`.
Five failed starts inside five minutes stop the unit in a `failed` state rather
than looping forever, so a coordinator that cannot come up is visible in
`status` instead of quietly cycling.

## What a restart does to work in flight

Boot reconciles what outlives the process: the orders the store still holds
outstanding, and the scratch worktrees under the configured local-worktree base.
An outstanding order whose directories survived is re-adopted, and an attempt
that finished while the coordinator was down still admits from the
`evidence.json` its run left behind. A sealed dispatch deadline is persisted
beside its order, so a restart does not renew a lane's allowance.

The lane child process does not survive. Stopping the unit kills its whole
control group, so a lane still running goes down with the coordinator rather
than outliving it as an orphaned build. Its order is re-adopted at the next
boot, rides to the deadline its bloom sealed, and is then recorded as an
ordinary failure that retry and wedge handling take from there. The
`ExecStartPre` reap clears the build tree it left in the scratch root before the
new process starts.

## Troubleshooting

| Symptom in `systemctl --user status bloomery` | Cause |
|---|---|
| `bloomery.env: No such file or directory` | Step 2 was skipped, or the copy landed somewhere other than `~/.config/bloomery/`. |
| `ExecStartPre` fails complaining that `AETHER_LANE_SCRATCH` is not set | The environment file names no scratch root, and the reap refuses to guess one. |
| Start fails immediately, journal shows a `gh` error | The host's `gh` is not authenticated, or `gh` is not on the `PATH` the environment file sets. |
| Unit runs, journal warns about missing connection knobs | `AETHER_GITHUB_OWNER` / `AETHER_GITHUB_REPO` are unset — the coordinator boots, but the mirror has no repository. |
| A lane fails with a linker or `No space left on device` error | The scratch volume filled. Check `AETHER_LANE_SCRATCH` names the roomy disk, and lower `AETHER_BLOOMERY_MAX_CONCURRENT_LANES`. |
