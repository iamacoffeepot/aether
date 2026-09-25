# Workspace imports and runs

> **Governing ADR:** [ADR-0237](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0237-workspaces-run-steps-over-trees.md)
> (workspaces run steps over trees), decisions 2, 3, 4, and 8. The actor
> answers `Import` and `Run`.

The `aether.workspace` actor is the Bloomery engine's only route to a container.
It talks to the Docker Engine API through a small client it owns privately, and
everything it produces goes into the journal as trees and blobs. No program,
component, or operator addresses the daemon directly, and there is no general
Docker actor.

`Import` pulls a digest-pinned image and decodes its filesystem into a stored
tree. An environment's base and toolchain layers enter the journal this way
before any run can use them. `Run` runs steps over a stored tree in a stored
environment, each in its own container, and stores what they produce.

## The contract

| Request | Fields | Reply | Arms |
|---|---|---|---|
| `aether.workspace.import` | `image: ImageRef` | `aether.workspace.import_result` | `Ok { tree: Ref<Tree> }`, `Failed { detail: Detail }` |
| `aether.workspace.run` | `tree`, `environment`, `mounts`, `steps`, `scratch`, `network` | `aether.workspace.run_result` | `Ok(Outcome)`, `Refused(Refusal)`, `Exhausted(Resource)`, `Failed { detail: Detail }` |

`ImageRef` is `<repository>@sha256:<64 lowercase hex>`, validated on
construction and on decode. It never carries a tag, because a tag can move and
an imported tree must be a function of the request that named it.

The actor is a root singleton at `aether.workspace`, so a program binding can
name it in `depends(...)` (ADR-0230). Import is operator mail, reachable over RPC
like the journal writes: it can make the daemon pull any digest-pinned image.
The actor records no event. An operator publishes the tree under a head
(`Publish` / `MoveHead`), or the environment merge program consumes it.

## What an import does

The whole sequence runs on a worker thread through the ADR-0093
hold-until-resolve dispatch, so the caller's settlement chain stays held until
the reply lands and no dispatcher thread blocks on the daemon or the journal:

1. `POST /images/create?fromImage=<ref>` pulls the image and reads its progress
   stream to the end. An `error` object in that stream fails the import, even
   though the status was 200.
2. `GET /images/<ref>/json` must list the ref in `RepoDigests`.
3. `POST /containers/create` creates a container from the ref, labelled
   `aether.workspace=import`. It is never started.
4. `GET /containers/{id}/export` streams the container's filesystem through the
   tar decoder into one journal batch, one copy buffer at a time.
5. `DELETE /containers/{id}?force=true&v=true` runs on every path once the
   container exists.
6. The batch commits only when the decode and the removal both succeeded.

A failed pull, an unlisted digest, an export the decoder refuses, a failed
removal, or a failed commit all answer `Failed { detail }`. No row is committed
and no container is left behind. Blob files a failed decode already wrote are
harmless orphans named by their content (ADR-0220). The pulled image stays in
the daemon's cache.

Importing the same image twice gives the same tree digest, and the journal's
artifact row count does not grow: every blob and tree is content-addressed, and
the commit inserts only absent rows.

## What a run does

A `Run` names everything by digest: the tree written at `/work`, the
`Environment` whose root is the whole visible filesystem, read-only mount
trees, 1 to 64 steps (a tool name from the environment's table, argv, extra
variables, and optional stdin), the scratch paths under `/work` left out of the
output, and whether the network is on. It names no cores, memory, or deadline;
those are the executor's.

The whole sequence runs on the worker thread, held like an import:

1. **Resolve.** Open one journal batch and load the environment, the tree,
   every mount tree, and every stdin blob. Then check the tree's
   `rust-toolchain.toml`, when it has one: its channel must be the
   environment's `provides.rust` channel, and its components and targets a
   subset. Then resolve each step's tool through the environment's `tools`
   table to a `Node::Executable` in the root, walking one directory per
   segment. These checks read only the journal. Last, `GET /info` maps the
   daemon's architecture and OS to a target triple (`<arch>-unknown-linux-gnu`
   on Linux), which must equal the environment's `platform`.
2. **Environment image.** The daemon must hold
   `aether-workspace-environment:<environment hex>` labelled
   `aether.workspace.environment=<hex>`. When it holds no such image, the root
   tree streams as a canonical tar to `POST /images/create?fromSrc=-`, which
   applies the label, and the image is inspected again. The image has no `Env`
   of its own; every variable is constructed per step. It stays after the run
   as a rebuildable derivative of the journal.
3. **Volumes.** One daemon-named volume for `/work`, shared by every step's
   container, and one per mount, each labelled `aether.workspace=run`. Each
   mount's tree streams into its volume through a helper container that is
   created and never started. The run tree streams into the first step's
   container at `/work` before it starts.
4. **Steps.** Each step gets its own container under the sandbox pins below.
   Stdin, when set, streams through a hijacked `attach`. The container starts,
   and the wait's read timeout is the run's remaining deadline. `inspect`
   gives the exit code; each output is one counting read of the demultiplexed
   log stream, which fixes the blob's length, then one writing read into the
   journal, so no log is held whole. The steps stop after the first non-zero
   exit.
5. **Output.** `GET …/archive?path=/work` on the last step's container decodes
   under the canonical rules and the output bounds. The `work` entry is the
   output, minus each scratch path; a scratch tmpfs comes back as an empty
   directory, so only its ancestors are rebuilt. Mount paths are never read
   back.
6. **Finish.** Every container and volume is removed on every path. Only then,
   and only for `Ok`, does the batch commit, before the reply, so a result
   that cites the output tree names rows that exist.

Running the same `Run` twice gives the same `RunResult` digest: the result
carries no container id, volume name, duration, host name, or timestamp, and
every blob and tree is content-addressed.

### The sandbox pins

| Hidden input | Pinned as |
|---|---|
| Command | `Cmd` = `/` + the tool's path, then `args`; no `Entrypoint`, no shell |
| Environment variables | `Environment::env`, overlaid by `Step::env`, overlaid by `SOURCE_DATE_EPOCH=315532800` |
| Working directory, user, hostname | `/work`, `0:0`, `workspace` |
| Filesystem | read-only root from the environment image; `/work` on the run's volume; a tmpfs (`rw,exec`) at each `/work/<scratch>`; mounts read-only; volumes never seeded from the image (`NoCopy`) |
| Network | `NetworkMode none` unless `Network::On` |
| Memory, processes | `Memory` = `MemorySwap` = the allotment; `PidsLimit` |
| Privilege | `CapDrop ALL`, `no-new-privileges` |
| Logs | the `local` driver, one file, rotation size set explicitly, so a daemon default cannot truncate or rewrite output |

The root is read-only, so a tool that writes to `/tmp` needs `TMPDIR` pointed
under `/work`, at a scratch path when its files must not reach the output. Each
scratch path is its own tmpfs, so a tool that renames files between a scratch
path and the rest of `/work` fails there (`EXDEV`); keep such a tool's staging
directory and its destination on the same side.

### Answers

| Answer | When |
|---|---|
| `Ok(Outcome)` | The steps ran. `steps` holds one `StepOutcome` per step that ran, each with `exit_code: Some(code)`, the stored stdout and stderr, and the `ToolRecord` naming the executable blob that ran; `tree` is `/work` minus scratch. A non-zero exit is an outcome. Docker reports a signal death as 128 + n, which cannot be told apart from `exit(128 + n)`, so this backend always answers `Some`. |
| `Refused(InputMissing(digest))` | The journal lacks the environment, the tree, a mount tree, a stdin blob, or anything they cite. |
| `Refused(ToolchainMismatch)` | The tree's `rust-toolchain.toml` asks for a channel, component, or target the environment does not provide. |
| `Refused(UnknownTool(name))` | A step's tool is not in the table, or its path does not hold an executable. |
| `Refused(PlatformMismatch)` | The environment's platform is not the daemon's. |
| `Refused(EnvironmentUnavailable)` | The daemon answered but could not produce the environment image, or the image's label names another environment. Never a mid-run failure. |
| `Exhausted(Time)` | A step was still running at the deadline; it is killed. |
| `Exhausted(Memory)` | The kernel killed a step for memory (`OOMKilled`). |
| `Failed { detail }` | The executor failed after accepting the run: a daemon or transport error, an output over the decode bounds, a `/work` no tree can represent (a FIFO, a device, an absolute symlink, a name the kinds refuse), an unreadable `rust-toolchain.toml`, a journal I/O failure, or a failed removal. `detail` names the call or the path. |

`Exhausted` and `Failed` are faults about the attempt, never results: the
`Workspace` program binding (#6711) ends the invocation on either, and the
program never sees them. Nothing from a failed or exhausted run is committed,
so retrying it is safe.

## Userland rules and bounds

The export decodes under `aether_bloomery_tar::Rules::userland`: the canonical
tree rules plus the two an imported userland needs.

- An absolute symlink target is rewritten to the relative target that resolves
  the same inside the tree: `usr/bin/cc -> /etc/alternatives/cc` becomes
  `usr/bin/cc -> ../../etc/alternatives/cc`.
- A device node under `dev/` is dropped, because the container runtime supplies
  `/dev`. A device anywhere else is refused.

Owner, group, times, and every mode bit except owner-exec are dropped. The
decoder bounds the import by an entry budget and a byte budget from the actor's
config; an export over either one fails the import.

## Configuration

`WorkspaceConfig` resolves at boot through the ADR-0090 derive path (argv >
env > file > default). The actor never reads `DOCKER_HOST` or any other
environment variable of its own.

| Knob | Flag | Default |
|---|---|---|
| `AETHER_WORKSPACE_ENDPOINT` | `--workspace-endpoint` | `unix:///var/run/docker.sock` |
| `AETHER_WORKSPACE_MAX_IN_FLIGHT` | `--workspace-max-in-flight` | 1 |
| `AETHER_WORKSPACE_IMPORT_MAX_ENTRIES` | `--workspace-import-max-entries` | 1,000,000 |
| `AETHER_WORKSPACE_IMPORT_MAX_BYTES` | `--workspace-import-max-bytes` | 8 GiB |
| `AETHER_WORKSPACE_RUN_DEADLINE_MILLIS` | `--workspace-run-deadline-millis` | 1,800,000 (30 minutes) |
| `AETHER_WORKSPACE_MEMORY_LIMIT_BYTES` | `--workspace-memory-limit-bytes` | 8 GiB |
| `AETHER_WORKSPACE_PIDS_LIMIT` | `--workspace-pids-limit` | 4,096 |
| `AETHER_WORKSPACE_OUTPUT_MAX_ENTRIES` | `--workspace-output-max-entries` | 1,000,000 |
| `AETHER_WORKSPACE_OUTPUT_MAX_BYTES` | `--workspace-output-max-bytes` | 8 GiB |

- Only `unix://` with an absolute socket path is accepted, and only on Unix. Any
  other scheme refuses boot naming `AETHER_WORKSPACE_ENDPOINT`. TCP with TLS and
  the Windows named pipe follow in #6721.
- `init` does not dial the daemon, so an engine boots without one; the first
  import that cannot connect answers `Failed`.
- Imports and runs share `max_in_flight`: past it they queue and are never
  dropped. The default of 1 also keeps two runs from building the same
  environment image at once.
- A zero import bound, output bound, deadline, memory limit, or pids limit
  refuses boot naming its key.
- The five run knobs are one fixed allotment every run gets. The deadline
  covers a run's steps together, from the first step's container create to
  the last step's exit; memory and pids apply to each step's container.
  Executor provisioning (#6710) replaces them with a host budget, per-program
  estimates, and FIFO admission.

## Composition

The Bloomery chassis composes the actor beside the component host and HTTP
egress, over the artifact store of the journal it opened. The store is handed
out only by that journal (`Journal::artifact_store`), and only the chassis's own
boot sets it, so no embedder can compose the workspace over a root the engine
did not open. `aether.process` is not composed on the Bloomery chassis.

`--describe` and `--print-config` compose the chassis to list the actor and its
knobs without a store and never boot it. A boot that reaches the actor without a
store is refused, naming the missing journal store.
