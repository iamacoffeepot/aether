# Workspace imports and runs

> **Governing ADR:** [ADR-0237](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0237-workspaces-run-steps-over-trees.md)
> (workspaces run steps over trees), decisions 2, 3, 4, 7, 8, and 9, and
> [ADR-0240](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0240-several-bloomery-journal-units-per-engine.md)
> D7 (one workspace per engine over a typed storage source). The actor answers
> `Import` and `Run`.

The `aether.bloomery.workspace` actor is the Bloomery engine's only route to a container.
It talks to the Docker Engine API through a small client it owns privately. It
holds no store: each request names its storage as a `source`, a unit's
journal, and the actor reads every input from it and stages everything it
produces to it as trees and blobs. One workspace serves every unit this way,
and each unit's runs stay in that unit's journal. No program, component, or
operator addresses the daemon directly, and there is no general Docker actor.

`Import` pulls a digest-pinned image and decodes its filesystem into a stored
tree. Only an environment's base and toolchain layers enter the journal this
way, before any run can use them: an image carries a bare, reusable layer and
never a source tree, which the operator stages from a commit instead
([Importing a source tree](#importing-a-source-tree)). `Run` runs steps over a
stored tree in a stored environment, each in its own container, and stores what
they produce.

## The contract

| Request | Fields | Reply | Arms |
|---|---|---|---|
| `aether.workspace.import` | `image: ImageRef`, `source: ProtocolPath<ArtifactStorage>` | `aether.workspace.import_result` | `Ok { tree: Ref<Tree> }`, `Err(ImportError)`: `Failed { detail: Detail }` or `Source(PathRefused)` |
| `aether.workspace.run` | `source: ProtocolPath<ArtifactStorage>`, `request: RunRequest` | `aether.workspace.run_result` | `Ok(Outcome)`, `Err(RunError)`: `Refused(Refusal)`, `Exhausted(Resource)`, or `Failed { detail: Detail }` |

`RunRequest` (`aether.workspace.run_request`) holds every field of a run but
its source: `tree`, `environment`, `mounts`, `steps`, `scratch`, and `network`.
It is what a program's `Workspace` call carries, so a program never names the
storage its run reads and writes (see [From a program](#from-a-program)).

`ImageRef` is `<repository>@sha256:<64 lowercase hex>`, validated on
construction and on decode. It never carries a tag, because a tag can move and
an imported tree must be a function of the request that named it.

### The source

`source` is the path of an actor that covers the `ArtifactStorage` protocol
(`read`, `read_closure`, and `stage`); on a Bloomery engine that is a unit's
journal owner, `aether.bloomery.journal:<unit key>`, written as
`ActorPath::<JournalActor>::instance(key).narrow::<ArtifactStorage>()`. The
mail that carries it decodes only when a route covering the protocol has stood
at the path; on receipt the actor proves the source live before anything is
queued. A source that fails either proof is answered naming the path and why,
`Err(Refused(SourceUnavailable(PathRefused { path, reason })))` for a run and
`Err(Source(PathRefused { path, reason }))` for an import, without a single
Engine API request.

Every read and stage of the request then goes through that source as mail. The
tar codec runs on a worker thread, which sends no mail, so each read or stage it
needs is a request its own actor sends for it and hands the answer back:

- **Reads, a window ahead.** A tree is read while it is written, in
  `ReadClosure` windows of at most 64 MiB requested ahead of the archive under
  the run's read budget (`AETHER_WORKSPACE_PREFETCH_BYTES`), which bounds what
  the run holds plus what it has asked for. Each directory the archive lists
  queues its subdirectories in the order the archive reaches them, and their
  closures are sent without waiting while the budget left covers a window, so
  the source reads the next windows while the worker encodes and uploads this
  one; each answer is shared blobs from the journal's read cache. A closure
  over its window reads that one directory with `ReadArtifact`, and its
  subdirectories are read the same way when the archive reaches it, so only
  the spine of oversized directories and the blobs directly inside them are
  left out. Those blobs are read as the archive reaches them, in batched
  `ReadArtifacts` of up to 64 MiB and 4,096 blobs: one names a blob and the
  later files of its directory not yet read, and the answer is held, charged
  to the budget, only until the archive takes it. Each blob leaves memory and
  the budget once the archive has written it. Every member is verified
  against its digest before it is trusted; a blob hashes as it streams into a
  container and fails the write at its end on a mismatch.
- **Writes, bounded batches.** Each blob and tree is staged as it is produced,
  in `Stage` batches of up to 64 MiB or 4,096 artifacts; a larger blob goes
  alone. One `Stage` is in flight while the next fills, and each is answered
  before the next is sent. Children are staged before the parents that cite
  them, so every citation in a batch names an artifact already stored or in the
  same batch. A request that ends in anything but its `Ok` leaves what it staged
  cited by nothing.

The actor is a root singleton at `aether.bloomery.workspace`, so a program binding can
name it in `depends(...)` (ADR-0230). Import is operator mail, reachable over RPC
like the journal writes: it can make the daemon pull any digest-pinned image.
The actor records no event. An operator publishes the tree under a head
(`Publish` / `MoveHead`), or the environment merge program consumes it.

## What an import does

The whole sequence runs on a worker thread through the ADR-0093
hold-until-resolve dispatch, so the caller's settlement chain stays held until
the reply lands and no dispatcher thread blocks on the daemon or the source:

1. `POST /images/create?fromImage=<ref>` pulls the image and reads its progress
   stream to the end. An `error` object in that stream fails the import, even
   though the status was 200.
2. `GET /images/<ref>/json` must list the ref in `RepoDigests`.
3. `POST /containers/create` creates a container from the ref, labelled
   `aether.workspace=import`. It is never started.
4. `GET /containers/{id}/export` streams the container's filesystem through the
   tar decoder, one copy buffer at a time, staging each blob and tree to the
   source as it decodes.
5. `DELETE /containers/{id}?force=true&v=true` runs on every path once the
   container exists.
6. When the decode and the removal both succeeded, the last stage is answered
   before the reply, so the tree the reply names is stored.

A failed pull, an unlisted digest, an export the decoder refuses, a failed
removal, or a refused stage all answer `Err(Failed { detail })`; a source that
did not prove answers `Err(Source(..))`. No container is left behind, and what a failed import
staged is cited by nothing: harmless content named by its digest. The pulled
image stays in the daemon's cache.

Importing the same image twice gives the same tree digest: every blob and tree
is content-addressed, and staging bytes already stored stores them once.

## What a run does

A `Run` names everything by digest: the tree written at `/work`, the
`Environment` whose root is the whole visible filesystem, read-only mount
trees, 1 to 64 steps (a tool name from the environment's table, argv, extra
variables, and optional stdin), the scratch paths under `/work` left out of the
output, and whether the network is on. It names no cores, memory, or deadline;
those are the executor's (see [Provisioning](#provisioning)).

Once the run is admitted, the whole sequence runs on the worker thread, held
like an import:

1. **Resolve.** Load the environment, the roots of the run tree and every
   mount tree, and every stdin blob, all from the source. A stored tree's
   members are stored whenever the tree is, so a root that loads means the
   tree is whole, and its members are read later, as they are written. Then check the tree's
   `rust-toolchain.toml`, when it has one: its channel must be the
   environment's `provides.rust` channel, and its components and targets a
   subset. Then resolve each step's tool through the environment's `tools`
   table to a `Node::Executable` in the root, walking one directory per
   segment, a few reads per tool. These checks read only the source. Last, `GET /info` maps the
   daemon's architecture and OS to a target triple (`<arch>-unknown-linux-gnu`
   on Linux), which must equal the environment's `platform`.
2. **Environment image.** The daemon must hold
   `aether-workspace-environment:<environment hex>` labelled
   `aether.workspace.environment=<hex>`. When it holds no such image, the root
   tree is read a window ahead and streams as a canonical tar to
   `POST /images/create?fromSrc=-`, which applies the label, and the image is
   inspected again. The image has no `Env`
   of its own; every variable is constructed per step. It stays after the run
   as a rebuildable derivative of the journal.
3. **Volumes.** One daemon-named volume for `/work`, shared by every step's
   container, and one per mount, each labelled `aether.workspace=run`. Each
   mount's tree streams into its volume through a helper container that is
   created and never started. The run tree streams into the first step's
   container at `/work` before it starts. With warm layers on, a warm run
   also gets its layer at cargo's target directory (see
   [Warm layers](#warm-layers)).
4. **Steps.** Each step gets its own container under the sandbox pins below.
   Stdin, when set, streams through a hijacked `attach`. The container starts,
   and `GET …/stats?stream=true` opens beside it: a second thread keeps the
   peak memory the samples report until the wait ends, for the estimate
   only, so a failed or dropped stats stream never changes the answer. The
   wait's read timeout is the run's remaining deadline. `inspect`
   gives the exit code; each output is one counting read of the demultiplexed
   log stream, which fixes the blob's length, then one writing read staged to
   the source. The steps stop after the first non-zero
   exit.
5. **Output.** `GET …/archive?path=/work` on the last step's container (on a
   warm run, a collector container holding only the `/work` volume) decodes
   under the canonical rules and the output bounds, staged to the source as it
   decodes. The `work` entry is the
   output, minus each scratch path; a scratch tmpfs comes back as an empty
   directory, so only its ancestors are rebuilt. Mount paths are never read
   back.
6. **Finish.** Every container and volume is removed on every path. Only then,
   and only for `Ok`, is the last stage answered, before the reply, so a
   result that cites the output tree names artifacts that are stored.

Running the same `Run` twice gives the same `RunResult` digest: the result
carries no container id, volume name, duration, host name, or timestamp, and
every blob and tree is content-addressed.

### Warm layers

With `AETHER_WORKSPACE_WARM_LAYERS` on, cargo's build output is kept between
runs as executor state (ADR-0237 decision 11). It never enters an output tree,
the journal, or a result, and cargo's own fingerprints decide what it reuses,
so a warm run answers what a cold run would.

A run is warm when its tree's root holds a `Cargo.lock` file and cargo's
target directory is one of its scratch paths: every step's environment names
the same `CARGO_TARGET_DIR` of the form `/work/<scratch>`, or none names it
and `target` is a scratch path. Every other run builds cold. The layer is
keyed by the unit (the run's `source` path), the run key (see
[Provisioning](#provisioning)), and the digest of the `Cargo.lock` blob, so a
changed lock or environment writes a new layer beside the old one.

| Case | What the run does |
|---|---|
| Miss | No pointer volume `aether-workspace-layer-<hex>`: the run builds into a fresh data volume labelled `aether.workspace.layer=<hex>` and `aether.workspace.layer.lock=<lock>`, mounted writable at the target directory. Once its steps ran to their exits, whatever the exit codes, it creates the pointer naming the data volume and labelled `aether.workspace.layer.tree=<tree>`, the hex digest of the run's tree, and the data volume stays. A run that ends any other way, or loses the pointer race, removes it. |
| Hit | The pointer and the data volume it names both carry the hex. The run creates an upper and a work volume of its own and a `local`-driver `overlay` volume whose `lowerdir` is the data volume's `Mountpoint` and whose `upperdir` and `workdir` are theirs, mounted writable at the target directory. All three are removed with the run, so no run sees another's writes, and the bottom layer is never written again. |
| Freshness | Every file reaches `/work` through the tar codec, which writes the canonical 1980 mtime, and cargo reuses a path crate's build when no source file is newer than it. So a hit uploads its tree through `encode_stamped` against the pointer's `aether.workspace.layer.tree`: every file, executable, or symlink that differs from that base tree carries the run's start in wall-clock seconds, and everything else, directories included, keeps the canonical mtime. Each run compares against the tree the layer was built over, never the previous run, because the layer's artifacts match that tree and no other: a file reverted to its base content is fresh again, and an edit made after any earlier run is still newer than the layer. A pointer with no tree label, written before the label existed, or a malformed one stamps every file, which is correct and only slower. A miss uploads the canonical stream: its layer holds no build output a stale mtime could fool. |
| Anything else | A pointer or data volume labelled for another layer, a data volume gone, or a mountpoint holding `,`, `:`, or `\`: the run builds cold and logs why. |

The daemon writes and removes every layer byte, so this works against a remote
daemon and needs no host directory. Layers are rebuildable: removing the
volumes labelled `aether.workspace.layer` costs only the next run's cold
build. The output archive of a step's container would carry the layer nested
under `/work`, so a warm run reads its output from a collector container that
mounts only the `/work` volume and never starts.

### The sandbox pins

| Hidden input | Pinned as |
|---|---|
| Command | `Cmd` = `/` + the tool's path, then `args`; no `Entrypoint`, no shell |
| Environment variables | `Environment::env`, overlaid by `Step::env`, overlaid by `SOURCE_DATE_EPOCH=315532800` |
| Working directory, user, hostname | `/work`, `0:0`, `workspace` |
| Filesystem | read-only root from the environment image; `/work` on the run's volume; a tmpfs (`rw,exec`) at each `/work/<scratch>` except a warm run's target directory, which mounts its layer writable; mounts read-only; volumes never seeded from the image (`NoCopy`) |
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
| `Err(Refused(InputMissing(digest)))` | The source lacks the environment, its root, the tree, a mount tree, or a stdin blob. A member missing below a root that loaded is a damaged source and answers `Failed`. |
| `Err(Refused(SourceUnavailable(refused)))` | The source did not prove: no journal has stood at the path, its route does not cover `ArtifactStorage`, or it is not live when the run is received. `refused` names the path and why. Answered before the run is queued. |
| `Err(Refused(ToolchainMismatch))` | The tree's `rust-toolchain.toml` asks for a channel, component, or target the environment does not provide. |
| `Err(Refused(UnknownTool(name)))` | A step's tool is not in the table, or its path does not hold an executable. |
| `Err(Refused(PlatformMismatch))` | The environment's platform is not the daemon's. |
| `Err(Refused(EnvironmentUnavailable))` | The daemon answered but could not produce the environment image, or the image's label names another environment. Never a mid-run failure. |
| `Err(Exhausted(Time))` | A step was still running at the deadline; it is killed. |
| `Err(Exhausted(Memory))` | The kernel killed a step for memory (`OOMKilled`). |
| `Err(Failed { detail })` | The executor failed after accepting the run: a daemon or transport error, an output over the decode bounds, a `/work` no tree can represent (a FIFO, a device, an absolute symlink, a name the kinds refuse), an unreadable `rust-toolchain.toml`, a read or stage the source refused, or a failed removal. `detail` names the failed call or the in-tree path and the class of failure (for example `reading the daemon's platform: connecting to the Docker daemon failed (entity not found)`), never a host path, a socket, a host name, the daemon's own message, or the source's, because the driver records it. The actor's log keeps the full text. |

`Exhausted` and `Failed` are faults about the attempt, never results: the
`Workspace` program binding ends the invocation on either, and the program
never sees them (see [From a program](#from-a-program)). Nothing a failed or
exhausted run staged is cited, so retrying it is safe.

## From a program

A Bloomery program reaches the actor through the trailing `Workspace` binding
(ADR-0237 decision 7), one of the closed set of program APIs beside `Http` and
`Process`. It is Sampled, so the program must declare `Mode::Sampled`; a
`Mode::Pure` program that takes it does not compile.

```rust
async fn run(input: Self::Input, env: &mut Env<Async>, mut workspace: Workspace) -> Result<Self::Result, Refusal> {
    let outcome = workspace.run(run).await?; // Result<Outcome, aether_bloomery_workspace::Refusal>
    // ...
}
```

`run` is a `RunRequest`, which names no source. `workspace.run(run).await` gives `Ok(Ok(outcome))` for an outcome, a non-zero
exit included, and `Ok(Err(refusal))` for the workspace's `Refused` answer. The
outer `Err` is the program's own `Refusal` for a call that broke: a reply that
is not a `RunResult`, or a send the invocation could not make.

The bundle's invocation declares no dependency. It sends the captured call to
its bundle root, which relays it to the driver that sent the `Invoke`, and the
driver maps `Workspace` to the workspace it holds (ADR-0240 D6). The driver
sends it as a `Run` whose `source` is its own unit's journal, written once from
the unit key it was born with, so a program's runs read and stage only in its
own unit (ADR-0240 D7, I-5). Each program's
record in the `aether.bloomery.programs` section lists the APIs its `run`
binds, so before a request's closure read or load the driver records a
`BundleUnavailable` fault for a program that binds an API with no provider in
its unit.

The binding never resolves on a fault. It ends the invocation instead, no
program code after the await runs, nothing the program staged is recorded, and
the driver records the fault, caused by the request's `Requested`:

| Run answer | Recorded fault |
|---|---|
| `Err(Exhausted(Time))` | `Fault { TimedOut }` |
| `Err(Exhausted(Memory))` | `Fault { ResourceExhausted }` |
| `Err(Failed { detail })` | `Fault { ExecutorFailed { reason } }`, with `reason` the same detail |

Step stdout and stderr are `Ref<OpaqueBytes>` values the actor stored, and the
output tree is a stored `Ref<Tree>`. A program cites them in its result without
reading them, or reads one through `Env<Async>::read`, which fetches it from
the journal on a miss.

## Provisioning

The actor alone chooses each run's cores, memory, and deadline, and admits it
against its host (ADR-0237 decision 9). No program can ask for resources.

**Budget.** `AETHER_WORKSPACE_CPUSET` lists the cores the actor may pin runs
to, and `AETHER_WORKSPACE_BUDGET_MEMORY_BYTES` the memory it may reserve at
once. What the host keeps for itself is the cores left out of the list and the
memory left out of the budget.

**Allotment.** Each run gets pinned cores, a memory limit for each step's
container, and one deadline its steps share, from the first step's container
create to the last step's exit.

- **Cores** are the executor's choice, with no knob: 8 to 16 per run, or the
  whole list when it holds fewer than 8. As a run starts it gets the free
  cores split evenly among the runs that want them now, itself included,
  clamped to that range, so a lone run gets 16 and four waiting runs get 8
  each. Measured on the build host, a leaf-edit clippy over warm layers took
  6.6 s on 32 cores alone, 6.6 s each as 2 runs on 16, 8.9 s each as 4 runs
  on 8, and 15.3 s each as 8 runs on 4: past 16 cores a run gains nothing,
  and below 8 it slows faster than running more at once pays back.

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

**Admission.** Runs wait in arrival order, and the front run starts as soon
as its allotment fits the free cores and free memory. While the front waits
it holds a reservation: the earliest time enough cores (8, or the whole list
when smaller) and its memory free up, found by releasing the running runs in
the order their deadlines end. A run behind it starts first only when it fits
the free budget now and its own deadline ends by that reservation. A run's
deadline is its estimate (a key never seen has the default deadline), and the
actor kills a run at its deadline, so a backfilled run never delays the
front, and a stream of short runs cannot starve it. Every run is pinned to the
lowest-numbered free cores. A run that cannot start waits, its caller's
settlement chain held, and is never dropped or refused for load. Each
completion releases the finished run's cores and memory, then starts the
front while it fits and any run that backfills past it, computing each
allotment from the estimate as it is then.

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
| `AETHER_WORKSPACE_DEFAULT_MEMORY_BYTES` | `--workspace-default-memory-bytes` | 8 GiB |
| `AETHER_WORKSPACE_DEFAULT_DEADLINE_MILLIS` | `--workspace-default-deadline-millis` | 1,800,000 (30 minutes) |
| `AETHER_WORKSPACE_MAX_DEADLINE_MILLIS` | `--workspace-max-deadline-millis` | 14,400,000 (4 hours) |
| `AETHER_WORKSPACE_HEADROOM_PERCENT` | `--workspace-headroom-percent` | 150 |
| `AETHER_WORKSPACE_PIDS_LIMIT` | `--workspace-pids-limit` | 4,096 |
| `AETHER_WORKSPACE_OUTPUT_MAX_ENTRIES` | `--workspace-output-max-entries` | 1,000,000 |
| `AETHER_WORKSPACE_OUTPUT_MAX_BYTES` | `--workspace-output-max-bytes` | 8 GiB |
| `AETHER_WORKSPACE_PREFETCH_BYTES` | `--workspace-prefetch-bytes` | 256 MiB |
| `AETHER_WORKSPACE_WARM_LAYERS` | `--workspace-warm-layers` (a presence flag) | off |

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
- `prefetch_bytes` bounds what one run holds or has requested from its source
  at once, each artifact counted as its payload plus its eight-byte kind prefix
  and each closure read in flight as its limit. A tree is read in windows of at
  most 64 MiB ahead of the archive, and each blob leaves the budget once
  written. Below 8 or above 4 GiB refuses boot naming
  `AETHER_WORKSPACE_PREFETCH_BYTES`.

## Composition

The Bloomery chassis composes the actor once, beside the component host and HTTP
egress, with no params: it holds no store and depends on no journal crate at
runtime. Each unit's journal owner covers `ArtifactStorage` and stays the only
writer of its root, so a request writes into a journal only by naming it as its
source. `aether.process` is not composed on the Bloomery chassis.

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
3. **Bind the bundle.** `environment.merge` runs from the
   `aether-bloomery-workspace-programs` bundle bound under the
   `Head<OpaqueBytes>` named `workspace-programs`, once per journal. Read the
   fence with `aether.bloomery.journal.read_head`, then send
   `aether.bloomery.journal.publish` to `aether.bloomery.journal:<key>`, the
   journal owner of the unit whose key `--bloomery-units` names, with
   one artifact, the bundle's built wasm as an `OpaqueBytes` artifact (the
   `OpaqueBytes` kind id, the file's bytes, no citations), and no moves. The
   `Committed` reply lists the artifact's digest. A second publish at the new
   head, with no artifacts, carries one `RecordedHeadMove` of the head
   `(OpaqueBytes, workspace-programs)` to that digest.
   `describe_kinds(names: ["aether.bloomery.journal.publish"], detail:
   "schema")` prints the shape.
4. **Load the bootstrap.** `upload_component` the built
   `aether_bloomery_bootstrap.wasm` (`crates/aether-bloomery-bootstrap`), then
   `load_component` it with the two references `publish.sh` printed and the
   two actors it mails as its config, here for the unit keyed `primary`:

   ```json
   {
     "base": "localhost:5000/aether-env/base@sha256:<digest>",
     "toolchain": "localhost:5000/aether-env/toolchain@sha256:<digest>",
     "journal": "aether.bloomery.journal:primary",
     "driver": "aether.bloomery.driver:driver"
   }
   ```

   The component declares `aether.bloomery.workspace` a dependency, so the load is
   refused on an engine without the workspace. A config missing a field is
   refused naming the field, as ``aether.bloomery.bootstrap.config has no `base` ``.
5. **Watch.** `actor_logs` on
   `aether.bloomery.bootstrap`. It logs one
   `info` line per step and ends with `the environment head moved; bootstrap
   done`. On the first refusal it logs one `error` and sends nothing more.

The merge call's key is the input digest's first eight bytes, so a rerun over
the same images replays the recorded merge (ADR-0226 decision 11) and moves the
head to the same digest again, which appends one more head-move event. New
images get a new key. The script records nothing of its own.

### What the bootstrap sends

The script proves both actors from its config at `wire`, then sends, one
request at a time:

1. **Import.** `aether.workspace.import { image, source }` to `aether.bloomery.workspace`,
   with `source` the configured journal owner, once
   per reference, the base first. Each answers `Ok { tree }`, and a
   second import of the same reference answers the same tree.
2. **Merge.** After `aether.bloomery.journal.read_head` for the fence, it
   stages an `environment.merge.input` (`MergeInput { base,
   toolchain }`, the two imported trees) and sends
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
3. **Publish.** Programs write no journal record, so the caller moves the
   head. After `aether.bloomery.journal.read_artifact` of the transition's
   result, it sends `aether.bloomery.journal.publish` with no artifacts and one
   `RecordedHeadMove` of the head `(aether.workspace.environment,
   <platform>)`, such as `x86_64-unknown-linux-gnu`, to the transition's
   result, under the journal fence. The head is named by the platform because
   the name exists only at run time, and one head per platform lets a caller
   ask for the environment its executor runs. A caller reads it back with
   `Heads::binding(&RecordedHead::new(Environment::ID, platform)?)` and cites
   it in a `Run` as `Ref<Environment>`.

A fence conflict on either publish is resent at the journal's head.

What each `error` log means:

| Message | Meaning |
|---|---|
| `a peer path does not prove; bootstrap stopped` | the `journal` or `driver` path names no `Live` actor; `error` is the refusal, `Unresolved` with the registry's text or `NotLive` with the canonical path |
| `bootstrap stopped`, `step = import` | an import failed; `detail` is the workspace's failure text |
| `bootstrap stopped`, `step = read the journal head` | the journal could not read its head |
| `bootstrap stopped`, `step = stage the merge input` | the input did not encode, or the journal staged other than one artifact |
| `bootstrap stopped`, `step = environment.merge` | the call faulted or was refused; `refused: HeadUnbound` means step 3 did not run |
| `bootstrap stopped`, `step = read the environment` | the result is missing, is not an environment, does not hash to its digest, or its platform names no head |
| `bootstrap stopped`, `step = publish` | the journal refused a publish |
| `a reply arrived out of phase; bootstrap stopped` | a reply arrived that the script was not waiting on |
| `a reply arrived while the bootstrap is not running` | a reply arrived after the script stopped or before `wire` |

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

## Importing a source tree

A proof runs over a source tree, and a source tree enters the journal from a
commit, never from an image (ADR-0237 decision 3). The operator command runs
outside the engine, on a host that holds the repository:

```text
cargo xtask import-commit <commit> --rpc-port <port> --unit <key>
```

`<commit>` is any revision `git rev-parse` resolves to a commit, looked up in
the repository around the working directory. `--rpc-port` is the Bloomery
engine's RPC port on `127.0.0.1`: the port `list_engines` and
`spawn_substrate` report for a hub-spawned engine, or the `--rpc-port` a
standalone `aether-bloomery` was started with. `--unit` is the key of the unit
whose journal takes the tree, one of the keys `--bloomery-units` names; the
command publishes to that unit's journal owner at
`aether.bloomery.journal:<key>`. The engine never reads Git, a
repository, or a host path; only the tree crosses.

What it reads is exactly the files the commit tracks, listed with
`git ls-tree -r -t` and read through one `git cat-file --batch`. There is no
allowlist: untracked files, build output, and ignored secrets are absent by
construction, because the commit does not hold them. Git modes map onto tree
entries one to one:

| Git mode | Tree entry |
|---|---|
| `100644` | `Node::File`, the blob staged as opaque bytes |
| `100755` | `Node::Executable`, the blob staged as opaque bytes |
| `120000` | `Node::Symlink`, the target inline |
| `040000` | `Node::Directory`, the subtree staged as a `Tree` |

Anything else stops the import and names the path, so nothing is dropped
silently: any other mode (a `160000` gitlink included), a name `Name` refuses,
or a symlink target `Path` refuses (absolute, not UTF-8, or longer than 1024
bytes).

The command builds every blob and tree bottom-up, each distinct digest once and
each artifact after everything it cites, with the root tree last. It stages
them through the journal's fenced `aether.bloomery.journal.publish` in batches
whose payload stays within half the RPC frame cap (`AETHER_MAX_FRAME_SIZE`,
resolved as the engine and `aether-mcp` resolve it); one file over that budget
stops the import and names its path. No batch moves a head, so the journal
appends no event and the fence read once at the start holds across batches. A
batch can land after the batches holding its tree's children, because the
journal checks each citation against rows staged in the same batch or already
stored.

stdout carries exactly two lines; progress goes to stderr:

```text
commit=<40-hex sha>
tree=<64-hex digest>
```

Importing the same commit again prints the same `tree=` line and adds no
journal event: every artifact is already stored, so the journal writes no blob
and no row. The journal records nothing about the commit either. The operator
holds the commit, and a proof cites the tree.

To cite the tree, pass the printed digest as `source` in
`vendor.cargo.input` and as the tree of a proof call (a Muse
session's tree, or the tree of a `Tooled` input), so the vendor tree and the
proof share one `Cargo.lock`. The hex is the same 32 bytes a `Ref<Tree>` field
carries, written two hex digits per byte in order. No head is published for a
source tree: each commit is its own tree, and the proof's caller cites the
digest directly, so a head would have no reader.

Through the MCP harness, the printed hex goes into the input's params as a
`$hex` embed, which writes a `[u8; 32]` field from exactly 64 lowercase hex
characters and encodes the same bytes as the 32-number array:

```json
"source": {"$hex": "<tree>"}
```

Digests come back out of a reply the same way when `send_mail` (or
`send_mail_traced`) carries a `format` mask naming the reply kind and the
digest leaves, such as the artifacts a publish staged or the input and result
a driver call recorded:

```json
{"format": {"aether.bloomery.journal.publish_result": {"Committed": {"artifacts": ["$hex"]}}}}
```

```json
{"format": {"aether.bloomery.driver.call_outcome": {"Transition": {"transition": {"input": "$hex", "result": "$hex"}}}}}
```

Each masked digest then reads as the same 64-character hex `import-commit`
prints. `{"format": {"*": "$hex"}}` does this for every byte-array leaf of
every reply. The embed and mask grammar is in
[The MCP harness](../mcp-harness.md#the-tools).

## Vendoring crate sources

`vendor.cargo` produces the tree [the proofs](#the-proofs) mount
at `/vendor`. It lives in the same `aether-bloomery-workspace-programs` bundle
and runs `cargo vendor --locked` once through the `Workspace` binding, with
the network on (ADR-0237 decision 4). It is Sampled, because the tree depends
on registry state and the executor, not only on the cited trees. Every cargo
proof stays network-free, and the vendor tree is a digest the journal can cite.

Its input, `vendor.cargo.input`, cites two things:

| Field | What it is | Where the run sees it |
|---|---|---|
| `source: Ref<Tree>` | the cargo workspace whose `Cargo.lock` is vendored, such as the tree `import-commit` prints ([Importing a source tree](#importing-a-source-tree)) | `/source`, read-only |
| `environment: Ref<Environment>` | the environment the caller reads from the head `(aether.workspace.environment, <platform>)` | the root |

The program asks for one run, and every argument is fixed:

| Part | Value |
|---|---|
| Tree | the empty tree, so `/work` starts empty |
| Tool | `cargo`, resolved through the environment's `tools` table |
| Args | `vendor --locked --manifest-path /source/Cargo.toml /work` |
| Env | `CARGO_HOME=/work/.tmp/cargo-home`, `TMPDIR=/work/.tmp` |
| Mounts | the source tree at `source` |
| Scratch | `.tmp`, so cargo's home never reaches the output tree |
| Network | `On` |

The run tree is empty because `Outcome::tree` is `/work` after the last step
minus scratch, and a mount is never read back. `/work` itself must therefore
be the vendor directory, so the result cites it with no reshaping and no
second program. The scratch name starts with a dot because, without
`--no-delete`, cargo vendor clears every entry of its destination whose name is
not hidden before it writes, and a scratch path is a tmpfs mount point under
`/work`. Because nothing varies, every vendor run in one environment has the
same run key and shares one allotment estimate.

The answer maps to the result, `vendor.cargo.result`, or to a refusal:

| Run answer | Program answer |
|---|---|
| `Ok`, one step with exit `Some(0)` | `Vendored { tree }`, citing the outcome's tree |
| `Ok`, one step with any other exit | `Failed { stderr }`, such as a stale lock under `--locked` or a registry fetch error |
| `Ok` with any other step count | the program's `Refused`, naming the count |
| `Refused(..)` | the program's `Refused`, naming the workspace refusal |
| `Exhausted(..)` or `Failed { .. }` | never seen: the binding ends the invocation, and the driver records the fault |

**Pairing.** `Vendored.tree` is the `cargo vendor --locked` directory for the
`Cargo.lock` at the root of the input's `source`: one directory per registry
package, each holding its `.cargo-checksum.json`, the layout
`source.vendored.directory` reads. A proof call is well-formed when
its bound `vendor` is the `Vendored.tree` of a `vendor.cargo` transition whose
`source` has the same `Cargo.lock` as the tree under proof, and in practice the
same `source` digest. The proof replaces only `crates-io`, so the pairing
covers registry sources only: a git dependency would be vendored but not wired
into the proof. A dependency-free source vendors to the empty tree, which the
proof already accepts.

Two preconditions hold for every vendor run:

- The source is a mount, not the run tree, so the workspace's
  `rust-toolchain.toml` check does not run. The vendor layout depends on
  cargo, not rustc, and the proofs still run that check over the same
  source.
- Cargo reads config from its working directory, `/work`, so a
  `.cargo/config.toml` in the source does not apply. The proof mounts the
  bound's cargo config at `/.cargo`, an ancestor of `/work`, so every cargo in
  the run reads it. A top-level `--config` flag is not enough: child cargo
  processes, such as the `cargo metadata` a test spawns, do not inherit it and
  would retry crates.io with the network off.

## The proofs

`proof.clippy` is the first program that runs cargo through the `Workspace`
binding (ADR-0237 decision 12), and `proof.test` is the second, in the same
shape. Both live in the `aether-bloomery-workspace-programs` bundle beside
`environment.merge`, bound under the head `workspace-programs`
(`WORKSPACE_PROGRAMS`), and are tools a Muse session calls (ADR-0234 decision
10): each formats a source tree, proves it in a published environment, and
returns the formatted tree. Each is Sampled, because the verdict depends on
the run.

Each proof's input is a tool's, `Tooled<A, ProofBound>`
(`bloomery.program.tooled`):

| Part | What it is | Where the run sees it |
|---|---|---|
| the tree | the cargo workspace under proof: a Muse session's current tree, or a tree such as `import-commit` prints ([Importing a source tree](#importing-a-source-tree)) | `/work` |
| `ClippyArgs` (`proof.clippy.args`) / `TestArgs` (`proof.test.args`) | the arguments the model writes: none, `{}` | nowhere |
| `ProofBound.environment: Ref<Environment>` | the environment the caller reads from the head `(aether.workspace.environment, <platform>)` (the head move in [What the bootstrap sends](#what-the-bootstrap-sends)) | the root |
| `ProofBound.vendor: Ref<Tree>` | the `Vendored.tree` of a `vendor.cargo` run over a source with the same `Cargo.lock` (see [Vendoring crate sources](#vendoring-crate-sources)) | `/vendor`, read-only |
| `ProofBound.cargo_config: Ref<Tree>` | the tree holding `config.toml`, which replaces crates.io with `/vendor` and sets `net.offline`; fixed by the proof and staged beside the bound by `cargo_config_artifacts` | `/.cargo`, read-only |
| `ProofBound.test_env: TestEnv` | the session-supplied test env (see [The test proof](#the-test-proof)) | the test step's env only |

`ProofBound` (`proof.bound`) is the value every proof binds besides its tree;
the session that offers the proof binds it into the offer, so the model never
sees it. The program reads no head and no journal record. The run has the
network off and a read-only root, so crate sources are an input too (ADR-0237
decision 4): cargo replaces crates.io with the vendor tree. A crate with no
dependencies passes an empty tree.

Each proof asks for one run of two steps, and every argument but the test env
is fixed:

| Part | Value |
|---|---|
| Tool | `cargo` for both steps, resolved through the environment's `tools` table; cargo finds `cargo-fmt` and `cargo-clippy` on the environment's `PATH` |
| Step 1 | `fmt --all -- -l`: rustfmt writes its fixes and prints each file it rewrote |
| Step 2 | clippy's lint command (see [The clippy proof](#the-clippy-proof)) or the test command (see [The test proof](#the-test-proof)) |
| Env | `CARGO_HOME=/work/tmp/cargo-home`, `CARGO_TARGET_DIR=/work/target`, `TMPDIR=/work/tmp` on both steps, plus the bound's test env on the test step only |
| Mounts | the vendor tree at `vendor`, the cargo config at `.cargo` |
| Scratch | `target` and `tmp`, so neither the build output nor cargo's home reaches the output tree |
| Network | `Off` |

fmt runs first and fixes, so a proof never fails on formatting alone. Both
cargo steps run workspace-wide and `--offline` rather than `--frozen`, so a
change to a workspace-internal dependency updates `Cargo.lock` inside the run
and the updated lock comes back in the output tree.

The answer is an `Edited<ProofVerdict>` (`bloomery.program.edited`), or a
refusal. The workspace stops after the first step that exits other than 0:

| Run answer | Program answer |
|---|---|
| `Ok`, both steps exit `Some(0)` | `Passed` |
| `Ok`, the cargo step exits other than 0 | `Failed { diagnostics }`: the proof's diagnostics (see below) |
| `Ok`, fmt alone, exiting other than 0 | `Failed { diagnostics }`: rustfmt's stderr; the cargo step did not run |
| `Ok` with any other steps | the program's `Refused`, naming the count |
| `Refused(..)` | the program's `Refused`, naming the workspace refusal |
| `Exhausted(..)` or `Failed { .. }` | never seen: the binding ends the invocation, and the driver records the fault |

The `Edited` carries the run's output tree, with fmt's fixes and any lock
update, which a Muse session takes as its current tree; a summary naming the
verdict and the files fmt rewrote, then the diagnostics after a blank line;
and the `ProofVerdict` (`proof.verdict`) as its detail, whose `diagnostics` are
the same text cited as `Utf8Text`. The diagnostics are capped at 64 KiB
(`DIAGNOSTICS_MAX_BYTES`), cut at a line with a closing line saying how many
bytes were cut. A workspace refusal, such as `ToolchainMismatch` when the
source's `rust-toolchain.toml` asks for a component the environment lacks, is
never recorded as a failed proof of the tree.

### The clippy proof

The clippy step is CI's lint command over the whole workspace (a narrower `-p`
selection unifies features differently and rebuilds shared dependencies):

`clippy --workspace --all-targets --offline --quiet --message-format=json -- -D warnings`

`--quiet` keeps cargo's progress lines out of stderr, and the diagnostics come
as JSON lines on stdout. A crate missing from the vendor tree fails with
cargo's own error. Because nothing varies, every clippy proof in one
environment has the same run key and shares one allotment estimate and one
warm build layer (see [Provisioning](#provisioning)).

A failed clippy step reports the `rendered` text of each `compiler-message`
cargo printed, each once, or cargo's stderr when it rendered none.

### The test proof

The test step runs the workspace tests with cargo's default target selection
(lib, bins, tests, doctests), the set CI's test lane covers through nextest
plus its doctest pass:

`test --workspace --offline --quiet --no-fail-fast --message-format=json`

`--no-fail-fast` lets one run report every failing target, not only the
first. `--quiet` keeps cargo's status lines out of stderr and makes libtest
print one character per passing test. Like clippy, the run takes no
`--all-features`, so every proof run gets the same feature resolution. It
takes no test-name filter either: the run key hashes every step's args and
env, so each distinct filter would build cold into its own warm layer, while
running the tests once the build is warm costs little beside that.

The test step also takes the bound's `test_env`, and only that step does, so
clippy's run key and warm layer never vary with it. `TestEnv` holds at most
32 variables with no repeated key; whoever opens the session supplies them,
and each distinct env value yields its own run key and warm layer.

A failed test step reports, in order: the build errors when the build failed
(the `rendered` text of each `error` `compiler-message`); else the test
failures, starting with cargo's failed-target list from stderr and then each
libtest `---- <name> stdout ----` block with its binary's
`test result: FAILED` line, the interleaved JSON lines skipped and the target
list first so the cap keeps it; else the step's stderr, as the clippy proof
falls back to.
