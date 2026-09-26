# Workspace imports and runs

> **Governing ADR:** [ADR-0237](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0237-workspaces-run-steps-over-trees.md)
> (workspaces run steps over trees), decisions 2, 3, 4, 8, and 9. The actor
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
those are the executor's (see [Provisioning](#provisioning)).

Once the run is admitted, the whole sequence runs on the worker thread, held
like an import:

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
   and `GET …/stats?stream=true` opens beside it: a second thread keeps the
   peak memory the samples report until the wait ends, for the estimate
   only, so a failed or dropped stats stream never changes the answer. The
   wait's read timeout is the run's remaining deadline. `inspect`
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
| CPU | `CpusetCpus` = the allotment's pinned cores; `NanoCpus` = their count × 10^9 |
| Memory, processes | `Memory` = `MemorySwap` = the allotment's memory; `PidsLimit` = the fixed pids limit |
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

## Provisioning

The actor alone chooses each run's cores, memory, and deadline, and admits it
against its host (ADR-0237 decision 9). No program can ask for resources.

**Budget.** `AETHER_WORKSPACE_CPUSET` lists the cores the actor may pin runs
to, and `AETHER_WORKSPACE_BUDGET_MEMORY_BYTES` the memory it may reserve at
once. What the host keeps for itself is the cores left out of the list and the
memory left out of the budget.

**Allotment.** Each run gets `run_cores` pinned cores (at most the list's
count), a memory limit for each step's container, and one deadline its steps
share, from the first step's container create to the last step's exit.

- A run key the actor has not seen gets `default_memory_bytes` and
  `default_deadline_millis`.
- A seen key gets its estimate times `headroom_percent`, floored at 256 MiB
  and 60 seconds.
- Every allotment, defaults included, is clamped: memory to the budget, and
  the deadline to `max_deadline_millis`. So an allotment larger than the whole
  budget still fits an idle host, and a hung step holds its cores for at most
  that long per attempt.

**Run key.** The estimate is kept per run key: sha256 over a fixed domain tag,
then the environment digest, the step count, and for each step in order its
tool name, its args, and its env entries (keys and values, in the order
given), every count and string length-prefixed. The tree, the mounts, the
scratch paths, the network, and every step's stdin are not in it, so two runs
doing the same work over different inputs share an estimate.

**Estimate.** An `Ok` run, whatever its exit codes, records its peak memory
(the highest stats sample of any step, less the reclaimable file cache) and
its wall time. The first observation seeds the key's estimate; later ones
blend in at weight 1/4. `Refused` and `Failed` change nothing.

**After exhaustion.** `Exhausted(Memory)` makes the key's next allotment twice
the memory that ran out, and `Exhausted(Time)` twice the deadline that passed,
so a reactor's retry receives more without asking. Both stay clamped: memory
to the budget, and the deadline to `max_deadline_millis`, so repeated timeouts
stop growing at 4 hours by default. Whether to keep retrying at the ceiling is
reactor policy.

**Admission.** Strict FIFO. A run starts only when no run waits ahead of it
and its allotment fits the free cores and free memory; it is pinned to the
lowest-numbered free cores. Otherwise it waits, its caller's settlement chain
held, and is never dropped or refused for load. Each completion releases the
finished run's cores and memory and starts runs from the front while the
front fits, computing the front's allotment from the estimate as it is then.
A small run behind a large waiting one waits too, so the large one is never
starved.

**Executor-local.** The budget, the queue, and the estimates live only in the
actor's memory. Nothing is written to the journal or placed in a result, and
a restart empties the estimates.

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
| `AETHER_WORKSPACE_TLS_CA_FILE` | `--workspace-tls-ca-file` | none |
| `AETHER_WORKSPACE_TLS_CERT_FILE` | `--workspace-tls-cert-file` | none |
| `AETHER_WORKSPACE_TLS_KEY_FILE` | `--workspace-tls-key-file` | none |
| `AETHER_WORKSPACE_MAX_IN_FLIGHT` | `--workspace-max-in-flight` | 1 |
| `AETHER_WORKSPACE_IMPORT_MAX_ENTRIES` | `--workspace-import-max-entries` | 1,000,000 |
| `AETHER_WORKSPACE_IMPORT_MAX_BYTES` | `--workspace-import-max-bytes` | 8 GiB |
| `AETHER_WORKSPACE_CPUSET` | `--workspace-cpuset` | `0` |
| `AETHER_WORKSPACE_BUDGET_MEMORY_BYTES` | `--workspace-budget-memory-bytes` | 8 GiB |
| `AETHER_WORKSPACE_RUN_CORES` | `--workspace-run-cores` | 4 |
| `AETHER_WORKSPACE_DEFAULT_MEMORY_BYTES` | `--workspace-default-memory-bytes` | 8 GiB |
| `AETHER_WORKSPACE_DEFAULT_DEADLINE_MILLIS` | `--workspace-default-deadline-millis` | 1,800,000 (30 minutes) |
| `AETHER_WORKSPACE_MAX_DEADLINE_MILLIS` | `--workspace-max-deadline-millis` | 14,400,000 (4 hours) |
| `AETHER_WORKSPACE_HEADROOM_PERCENT` | `--workspace-headroom-percent` | 150 |
| `AETHER_WORKSPACE_PIDS_LIMIT` | `--workspace-pids-limit` | 4,096 |
| `AETHER_WORKSPACE_OUTPUT_MAX_ENTRIES` | `--workspace-output-max-entries` | 1,000,000 |
| `AETHER_WORKSPACE_OUTPUT_MAX_BYTES` | `--workspace-output-max-bytes` | 8 GiB |

- `unix://<absolute path>` dials the daemon's socket, on Unix only.
- `tcp://<host>:<port>` dials a daemon anywhere, always over mutual TLS. The
  port is required, and the host is a DNS name or an IP literal (IPv6 in
  brackets) that the daemon's certificate must name. All three TLS files are
  required: the CA file is the only trust root, with no platform store and no
  bundled roots, and the certificate and key files are the client certificate
  the daemon checks. There is no plaintext TCP, since the daemon socket is
  root-equivalent. `init` reads the three files once; one that cannot be read,
  holds no certificate or key, or that rustls refuses fails boot naming its
  key.
- Any other scheme refuses boot naming `AETHER_WORKSPACE_ENDPOINT`, and so does
  a `tcp://` address without a port. A missing TLS file beside `tcp://`, or a
  TLS file set beside any other scheme, refuses boot naming that file's key.
  The Windows named pipe is not supported yet (#6775).
- `init` does not dial the daemon, so an engine boots without one; the first
  import that cannot connect answers `Failed`.
- `max_in_flight` bounds imports only: past it they queue and are never
  dropped. Runs are admitted against the budget instead (see
  [Provisioning](#provisioning)).
- Runs can overlap, so two first runs of one environment may both import its
  image. That is idempotent: both import the same content under the same tag
  and label, and each run inspects the label before use.
- A `cpuset` that does not parse (an empty list, a reversed range, a value
  that is not a number, an index above 1023) refuses boot naming
  `AETHER_WORKSPACE_CPUSET`, and a headroom below 100 names
  `AETHER_WORKSPACE_HEADROOM_PERCENT`.
- A zero import bound, output bound, budget memory, run cores, default
  memory, default deadline, maximum deadline, or pids limit refuses boot
  naming its key.
- The pids limit and the output bounds are the same for every run; memory and
  pids apply to each step's container.

## Composition

The Bloomery chassis composes the actor beside the component host and HTTP
egress, over the artifact store of the journal it opened. The store is handed
out only by that journal (`Journal::artifact_store`), and only the chassis's own
boot sets it, so no embedder can compose the workspace over a root the engine
did not open. `aether.process` is not composed on the Bloomery chassis.

`--describe` and `--print-config` compose the chassis to list the actor and its
knobs without a store and never boot it. A boot that reaches the actor without a
store is refused, naming the missing journal store.

## Building an environment

An environment starts from two imported trees, the distro userland and the
Rust toolchain (ADR-0237 decision 3). No upstream image carries what the build
needs, so a checked-in recipe in `scripts/bloomery/environment/` builds both
and publishes them where the daemon can pull them by digest.

| File | What it holds |
|---|---|
| `base.Dockerfile` | Debian slim pinned by digest, plus the packages the workspace's build scripts need, each with a one-line reason |
| `toolchain.Dockerfile` | the official `rust` slim image of the channel `rust-toolchain.toml` names, pinned by digest, plus that file's components and targets |
| `publish.sh` | builds both images, pushes them to a loopback registry, and prints the two references |
| `check.sh` | proves the base's package list with `cargo check --workspace --locked` |

Run the steps on the host whose daemon the actor dials.

1. **Publish.** `scripts/bloomery/environment/publish.sh` starts a
   digest-pinned `registry:2` container bound to `127.0.0.1` unless one is
   already running, builds both images, pushes them, and prints:

   ```text
   base=localhost:5000/aether-env/base@sha256:<digest>
   toolchain=localhost:5000/aether-env/toolchain@sha256:<digest>
   ```

   A rerun reuses the registry and prints the same references when nothing
   was rebuilt. `AETHER_ENV_REGISTRY_PORT` picks the port (default 5000).
   `publish.sh --stop` removes the registry container; its named volume keeps
   what was pushed.
2. **Check.** `scripts/bloomery/environment/check.sh` runs `publish.sh` and
   checks the references it prints:
   - `cargo fetch --locked` runs in the toolchain image, so the base carries
     no network tooling or CA certificates;
   - a throwaway image adds only the toolchain directory to the base;
   - `cargo check --workspace --locked --offline` runs in it with no network,
     the repository mounted read-only, and the target directory on a tmpfs.

   A missing `-dev` package fails a crate's build script here, before any
   import: without `libasound2-dev`, `alsa-sys` fails. Add the package to
   `base.Dockerfile` with its reason and run the check again.
3. **Import.** Mail `aether.workspace.import { image }` to `aether.workspace`
   on a Bloomery engine, once per reference. Each answers `Ok { tree }`, and a
   second import of the same reference answers the same tree.
4. **Merge.** Stage an `environment.merge.input` (`MergeInput { base,
   toolchain }`, the two imported trees) and send
   `aether.bloomery.driver.call` for the program `environment.merge`, in the
   `aether-bloomery-workspace-programs` bundle bound under the
   `Head<OpaqueBytes>` named `workspace-programs`. The Pure program answers a
   `Transition` whose result is a stored `aether.workspace.environment`:
   - `root` is the base tree with the toolchain image's one directory
     `usr/local/rustup/toolchains/<channel>-<triple>` cited at the same path.
     Every subtree the merge does not touch keeps its digest; only the
     directories on the path to the toolchain, `dev`, and the root are new.
   - Docker's placeholders are dropped: the empty `.dockerenv` goes, and `dev`
     becomes an empty directory, where the runtime supplies `/dev`. A
     non-empty `.dockerenv` or `dev` entry refuses, naming its path, because
     the merge cannot tell it from userland content.
   - `platform`, `provides` and `tools` come from entry names alone, so the
     program reads no file blob. Each installed component leaves a
     `lib/rustlib/manifest-<component>[-<target>]` file: the host triple is
     the `X` of the `manifest-rustc-X` the directory name ends with, the
     targets are every `manifest-rust-std-X`, and every other manifest is a
     component, with the host suffix and a trailing `-preview` removed
     (`clippy-preview` is `clippy`). `tools` is every executable in the
     toolchain's `bin`.
   - `env` holds only what every step shares: `PATH`, led by the toolchain's
     `bin`, and `LANG=C.UTF-8`. The root is read-only, so `CARGO_HOME`,
     `CARGO_TARGET_DIR` and `TMPDIR` belong to each step.

   The input cites both trees, so the driver checks every member of both into
   the engine blob store for the call, deduplicated by digest. That closure
   must fit `--bloomery-closure-limit-bytes`, whose default is the 4 GiB
   ceiling; the program itself loads only the few directories it walks.
5. **Publish.** Programs write no journal record, so the caller moves the
   head. Send `aether.bloomery.journal.publish` with no artifacts and one
   `RecordedHeadMove` of the head `(aether.workspace.environment,
   <platform>)`, such as `x86_64-unknown-linux-gnu`, to the transition's
   result, under the journal fence. The head is named by the platform because
   the name exists only at run time, and one head per platform lets a caller
   ask for the environment its executor runs. A caller reads it back with
   `Heads::binding(&RecordedHead::new(Environment::ID, platform)?)` and cites
   it in a `Run` as `Ref<Environment>`.

What the imported trees hold:

- In the base tree, `usr/bin/cc` is the relative symlink
  `../../etc/alternatives/cc`, and `dev/` holds no device node. It holds only
  what Docker creates in every container: an empty `console` file and empty
  `pts` and `shm` directories, beside an empty `.dockerenv` at the root.
- The toolchain tree holds the toolchain at
  `usr/local/rustup/toolchains/<channel>-<triple>`, which the merge selects,
  so no rustup proxy from `usr/local/cargo` enters an environment.

Pins are image digests; a tag beside one only names it for the reader.

- To move the channel, edit `rust-toolchain.toml` and repin
  `toolchain.Dockerfile` to that channel's `rust:<channel>-slim-<release>`
  digest. The build fails while the two disagree, because the pinned image
  would end up holding a second toolchain.
- Keep the base on the Debian release the toolchain image is built on, so
  both trees share one libc.
- `apt-get` output changes from day to day, so a rebuilt base can have a new
  digest. An environment records the imported tree digest, not the recipe.
