# Running a Muse session

**Class:** drive-only. Nothing here rebuilds aether's engine: you start one
long-lived Bloomery engine under the hub, bind the Muse bundle on it once, and
then drive any number of sessions with five `cargo xtask muse` verbs. Each verb
prints a few `key=value` lines and keeps the long waits and the file bytes to
itself, so a caller sequences them and reads only those lines.

A session is the `muse.session` reactor loop in `crates/aether-bloomery-muse`
(ADR-0234): `muse.session.open` starts one on a tree, the loop runs turns and
the tool calls they ask for, and every rest is recorded as a `muse.session`
whose head is named by the session key. The verbs speak the engine's own
journal and driver mail over its RPC port: `aether.bloomery.journal.stage`,
`publish`, `read_events`, `watch_head`, and `read_artifacts` to the unit's
journal owner, and `aether.bloomery.driver.call` to the driver.

## 1. Start a Bloomery engine under the hub

The engine needs egress to the model vendor and a credential for it. Put the
key in a secret file and bind it on `aether.http` exactly as
[Supplying secrets](supplying-secrets.md) describes (its sections 6 and 8 cover
a hub-spawned engine and the Bloomery engine). Upload the `aether-bloomery`
binary with `upload_binary`, then fork it with `spawn_substrate`, passing the
engine's flags through `args`:

```text
spawn_substrate(selector=<the uploaded aether-bloomery>,
                args=["--bloomery-units", "primary=<journal directory>",
                      "--secrets-dir", "<directory holding only this engine's secrets>",
                      "--http-allowlist", "api.muse.example",
                      "--http-secrets", "api.muse.example/bearer=muse"])
```

Keep the `rpc_port` the reply reports (`list_engines` shows it again later):
every verb below takes it as `--rpc-port`, and `--unit` is the unit key the
`--bloomery-units` entry names (`primary` here). Each verb dials
`127.0.0.1:<rpc-port>`, so run them on the host the engine runs on.

## 2. Bind the bundle, once

Build the bundle with `cargo xtask build-wasm`, then:

```sh
cargo xtask muse bind --rpc-port <port> --unit primary --bundle <path to aether_bloomery_muse.wasm>
```

```text
bound bundle=<digest> set=<digest>
```

`bind` stages the bundle and, in one fenced publish, moves the `muse` head to
it and the reactor-set root (`core.reactors`) to a set holding `muse` beside
every member the bound set already holds. A rebind of the same bundle prints
`unchanged` and publishes nothing. Rebind after rebuilding the bundle.

## 3. Open a session

```sh
cargo xtask muse open --rpc-port <port> --unit primary \
  --commit <revision> --brief brief.md --seeds seeds.txt \
  --endpoint https://api.muse.example/v1/responses --model <model> \
  --effort medium --max-output-tokens 16000 --max-turns 40 --input-limit 184220
```

```text
tree=<digest>
session=<key>
after=<seq>
```

- `--commit` imports the files the commit tracks, exactly as
  `cargo xtask import-commit` does (see
  [Workspace](../systems/workspace.md#importing-a-source-tree)), in the
  repository around the working directory. `--tree <digest>` names a tree
  already in the journal instead.
- `--brief` is a file holding the first user message.
- `--seeds` is optional: a file naming one tree path per line, blank lines
  skipped. Each is read with `tree.read` before the first turn, so the model
  starts with those files in view.
- The settings have no defaults. `--effort` is `low`, `medium`, or `high`.
  `--input-limit` is the most input tokens a turn may be billed for before the
  session rests `context-full`; a value of 0 is refused.
- Every bound tool is offered: `tree.list`, `tree.read`, `tree.grep`,
  `tree.edit`, `tree.write`, and `muse.echo`.

The call key is derived from the open's input digest, so running the same
`open` again (after a lost reply, say) prints the same session instead of
opening a second one. Keep `session=` and `after=` for the next step.

## 4. Wait for it to rest

```sh
cargo xtask muse wait --rpc-port <port> --unit primary --session <key> --after <seq>
```

```text
rested completed turns=7
from=<tree digest> to=<tree digest>
usage input=184220 cached=151040 output=9311 reasoning=4096
<the final assistant message>
```

`wait` reads the journal from `--after` and blocks on its head until the
session's head moves to its next record. It counts only the session's own
entries: an entry belongs to the session when its cause chain reaches the
session's open run, or a continue run that names the session, so twenty
sessions on one engine are each followed by their own `wait`.

- `rested` is `completed`, `declined`, `incomplete`, `turn-limit`,
  `context-full`, or `failed: <why>`. The final message follows only for `completed`.
- `turns` counts the session's `muse.turn` runs read, and `usage` sums the
  token counts they reported.
- `from` is the tree the activation started on (the open's tree, or the tree
  the continued record held) and `to` the tree its record holds.
- A fault or failed reaction in the session's chain is printed after the rest
  and exits non-zero naming it. A second one, or a failed head move, means the
  session cannot record its rest; `wait` exits non-zero at once instead of
  blocking.

## 5. Export the changes

```sh
cargo xtask muse export --rpc-port <port> --unit primary --from <from> --to <to> --into <checkout>
```

```text
M scripts/run.sh
A src/new.rs
D src/old.rs
```

`export` walks both trees through batched artifact reads, writes every added
or changed file with its execute bit and every symlink, and removes every
deleted entry, printing `A|M|D <path>` in path order. Run it on a clean
checkout of the commit the session opened on, and `git diff` there is the
patch; `git add -A && git diff --cached` includes the added files.

## 6. Continue

```sh
cargo xtask muse continue --rpc-port <port> --unit primary --session <key> \
  --message next.md --max-turns 20
```

```text
after=<seq>
```

`continue` finds the session's latest record (its head's last move), stages the
message, and calls `muse.session.continue`; pass the printed `after=` to the
next `wait`. Without `--message` the conversation is resent as it stands,
which recovers a session that rested failed, on a vendor 504 for instance, or
incomplete with no output. `--max-output-tokens` optionally replaces the
session's output budget from that turn on.

The session keeps working on the tree its record holds, so edits made in the
checkout between rounds are not visible to it. Feed back compile errors or CI
failures as the message text. Each `wait` prints its round's `from` and `to`,
so exporting them in turn into the same checkout brings it along round by
round.
