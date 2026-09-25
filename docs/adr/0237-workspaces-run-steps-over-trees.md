# ADR-0237: Workspaces Run Steps Over Trees

- **Status:** Proposed
- **Date:** 2026-09-24
- **Amended:** 2026-09-24 — trees cross into and out of a container as canonical tar streams through the Engine API's archive endpoints; no tree is written to a host directory, so a remote daemon needs nothing on its host but the stream. While the blob split stays deferred, the actor's blob writes go through the journal.
- **Amended:** 2026-09-25 — the blob split is no longer deferred: the journal is a root directory holding the SQLite database and a `blobs` directory of digest-named files for every blob, at every size (ADR-0220). Open question 1 is resolved: the journal stays the only writer, of the database and of the blob files. This replaces the previous amendment's deferred-split sentence.

Amends [ADR-0229](0229-program-cap-apis-are-extra-run-arguments.md) (the
closed, sealed set of program APIs, `Http` / `Process`, mapped through
`__macro_internals::api_target`: this ADR adds `Workspace`). Builds on
[ADR-0157](0157-one-shot-process-exec-capability.md) (the reviewed
deadline / drain / group-reap runner in `crates/aether-process/src/runner.rs`,
and argv-only, constructed-environment execution),
[ADR-0224](0224-programs-are-wasm-bundles.md) and
[ADR-0228](0228-async-programs-await-sanctioned-mail.md) (Pure / Sampled
`Mode`, `Env<Async>`), [ADR-0226](0226-native-bundle-driver.md) (the driver
records the one `Transition`), [ADR-0227](0227-reply-contracts-are-type-markers.md)
(`Replies<K, Reply = O>`), [ADR-0093](0093-hold-until-resolve-dispatch-primitive.md)
(the held reply across a worker-thread run), and
[ADR-0090](0090-application-configuration.md) (`#[derive(Config)]`).

## Context

Bloomery needs programs that run cargo: build, clippy, test, fmt. That is
unavoidable, and it is the largest cost in the loop: a proof is a
multi-minute native build, not a millisecond WASM function.

The earlier Bloomery managed where and how proofs ran itself: fixed lanes,
each a long-lived worktree with a warm target directory, swapped between
jobs and kept in step by rolls and janitoring. That coupled the definition
of work to its execution, which ADR-0224 already rejects for programs, and
it was the main source of operational cost.

What is on main:

- `aether.process` (ADR-0157): `Run { binary, args, env, stdin,
  timeout_millis }` → `RunResult::{Ok, TimedOut, Err}`. The working
  directory is confined to the `aether.fs` namespace roots, so every run
  shares one directory; there is no per-run tree, no isolation between
  runs, and no resource limits. It is the right runner and the wrong unit.
- `bloomery.tree` (`crates/aether-bloomery-kinds/src/tree/`): a directory
  as `Name → Node::{File(Ref<OpaqueBytes>), Executable(Ref<OpaqueBytes>),
  Symlink(Path), Directory(Ref<Tree>)}`. Owner, timestamps, and other mode
  bits are dropped by design. Its module doc states that build outputs are
  never entries in any tree.
- The journal stores every artifact's bytes in one SQLite table
  (`ARTIFACTS_DDL` in `crates/aether-bloomery-journal/src/artifact.rs`,
  `bytes BLOB NOT NULL`) beside a `citations` edge table.
- Nothing on main turns a directory into a tree or a tree into a directory.

One principle governs every choice below: **nothing used to produce a
result is ambient.** Tools, libraries, the linker, the platform, the
network, the clock, and paths are either inputs named by digest, pinned by
the sandbox, or they make the program `Sampled`.

## Decision

1. **Bloomery owns what, never where or how.** A program names what to run
   and over which inputs; the driver records the result. Where a run
   executes, and how it is isolated and provisioned, belongs to the actor
   that answers the workspace contract.

2. **A workspace is a value, served by an actor.** One request, one reply,
   nothing held open:

   ```rust
   // kinds: new crate `aether-workspace` (identity/runtime split, ADR-0122)
   #[aether_data::kind(name = "aether.workspace.run")]
   pub struct Run {
       pub tree: Ref<Tree>,                 // written out at /work
       pub environment: Ref<Environment>,   // the whole visible root filesystem
       pub mounts: Vec<Mount>,              // extra read-only trees (e.g. vendored crates)
       pub steps: Steps,                    // 1..=MAX_STEPS, validated
       pub scratch: Vec<Path>,              // excluded from the output tree (e.g. `target`)
       pub network: Network,                // a capability grant, not a resource: Off unless the program must fetch
   }

   pub struct Mount { pub at: Path, pub tree: Ref<Tree> }

   pub struct Step {
       pub tool: ToolName,                  // resolved through Environment::tools, never a path
       pub args: Vec<String>,               // argv; no shell, ever
       pub env: Vec<EnvVar>,                // merged over the environment's base env
       pub stdin: Option<Ref<OpaqueBytes>>,
   }

   pub enum Network { Off, On }

   #[aether_data::kind(name = "aether.workspace.run_result")]
   pub enum RunResult {
       Ok(Outcome),
       Refused(Refusal),
       Exhausted(Resource),                 // the executor's allotment ran out; never reaches the program
   }

   pub enum Resource { Memory, Time }

   pub struct Outcome {
       pub steps: Vec<StepOutcome>,         // stops after the first non-zero exit
       pub tree: Ref<Tree>,                 // /work after the last run step, minus `scratch`
   }

   pub struct StepOutcome {
       pub exit_code: Option<i32>,          // None when the step died by signal
       pub stdout: Ref<OpaqueBytes>,
       pub stderr: Ref<OpaqueBytes>,
       pub tool: ToolRecord,
   }

   pub struct ToolRecord { pub name: ToolName, pub path: Path, pub file: Ref<OpaqueBytes> }

   pub enum Refusal {
       EnvironmentUnavailable,
       PlatformMismatch { wanted: Platform, provided: Platform },
       ToolchainMismatch { tree_wants: RustToolchain, environment_provides: Option<RustToolchain> },
       UnknownTool(ToolName),
       InputMissing(Digest),
   }
   ```

   The request carries no mailbox id and no actor reference: the reply goes
   to the caller (ADR-0227 `Replies<Run, Reply = RunResult>`). A non-zero
   exit is an `Outcome`, not a refusal, exactly as ADR-0157 treats a
   completed run. The result carries no duration, host name, or timestamp,
   so two correct executors can produce the same digest.

   The request names no resource amounts. Cores, memory, and the deadline
   are how a run is provisioned, which decision 1 leaves to the executor
   (decision 9). Running out of an allotment the executor chose says
   nothing about the tree, so it is a fault about the attempt, never a
   result: the `Workspace` binding turns `Exhausted` into the end of the
   invocation, and the driver records `Fault { TimedOut }` for time or a
   new `FaultReason::ResourceExhausted` for memory. The program never
   observes it, and whether to retry is reactor policy.

3. **An environment is a stored tree plus what it declares.**

   ```rust
   #[aether_data::kind(name = "aether.workspace.environment")]
   pub struct Environment {
       pub root: Ref<Tree>,          // the only files visible inside a run
       pub platform: Platform,       // target triple, e.g. x86_64-unknown-linux-gnu
       pub provides: Provides,       // e.g. rust { channel: "1.97.1", components, targets }
       pub tools: Tools,             // ToolName -> Path inside root; each must be Node::Executable
       pub env: Vec<EnvVar>,         // base environment (PATH, LANG, ...), fixed per environment
   }
   ```

   - Binaries are blobs behind `Node::Executable`; the journal stores no
     container image format. Identical files dedup across environment
     versions.
   - The root is built from two imported trees merged by a Pure tree
     program: a **base layer**, a distro userland imported once from a
     tarball (a slim Debian with `build-essential`, `pkg-config`, and the
     `-dev` packages our crates probe for), and a **toolchain layer**, a
     Rust toolchain directory imported once. The kernel is never part of an
     environment.
   - No rustup proxies inside an environment. Before running, the actor
     reads the tree's `rust-toolchain.toml` (when present) and compares it
     with `provides`; a mismatch is `Refused(ToolchainMismatch)`, so a wrong
     toolchain can neither run silently nor be fetched mid-run.

4. **The sandbox pins what it can; the rest makes a program Sampled.**

   | Hidden input | Handling |
   |---|---|
   | Tools, libc, linker, headers | `Environment::root`; nothing from the host is visible |
   | Paths baked into output | Every run's tree is mounted at `/work`; extra mounts at their declared paths |
   | Environment variables, locale, uid, hostname | Constructed: `Environment::env` then `Step::env`; fixed uid and hostname |
   | Network | Off unless `Run::network` is `On` |
   | Clock in outputs | `SOURCE_DATE_EPOCH` fixed; reading the real clock into output is Sampled |
   | Parallelism | The executor's allotment; the container sees only its assigned cores, and cargo's default job count follows the cores it can see |
   | Directory order | Trees are written out in canonical name order |
   | Randomness | Cannot be pinned; output that depends on it is Sampled |
   | Platform (arch, kernel) | Declared: `Environment::platform` |

   Crate sources are an input like everything else: a fetch run with
   `Network::On` produces a vendored tree, and build runs mount it
   read-only with the network off.

5. **Platform is an input; the host is provenance.** Two executors of the
   same platform class running a Pure program must produce the same result
   digest (ADR-0224's Pure invariant); a disagreement is two recorded
   transitions proving a hidden input. The class belongs to the pair
   (program, platform) and starts at the target triple alone. It is refined
   only when a recorded disagreement proves the triple too coarse. Tests
   stay Sampled.

6. **Replay reuses results.** Replaying a journal re-derives every decision
   from recorded results on any OS: programs and reactors are WASM and a
   recorded result is a memo hit. Re-running a workspace step routes to an
   executor of the recorded platform. Running on a different platform is a
   new input, compared through a Pure comparison program. When to re-run is
   reactor policy, not a workspace property.

7. **The program-side API is a trailing `Workspace` binding** (amends
   ADR-0229's closed set):

   ```rust
   async fn run(input: Self::Input, env: &mut Env<Async>, workspace: Workspace) -> Result<Self::Result, Refusal>
   ```

   `workspace.run(Run { .. }).await` awaits one `RunResult`. `Mode::Sampled`
   is required beside it until decision 5's class agreement is recorded
   mechanically.

8. **The first backend is Docker, inside the actor.** A native workspace
   actor answers the contract by talking to the Docker Engine API. There is
   no general Docker actor.

   | Concern | Mechanism |
   |---|---|
   | Transport | Engine API over HTTP/1.1: a Unix socket (Linux, macOS), a Windows named pipe opened as a file, or TCP with TLS for a remote daemon. One private transport enum; a small hand-rolled client on a worker thread (no async runtime). |
   | Endpoint | Actor `Config` (ADR-0090), with a per-OS default; never a `DOCKER_HOST` read |
   | Environment | Imported once per environment digest: the root tree streams as a filesystem tarball to `POST /images/create?fromSrc=-`, labelled with the environment digest. The label is checked before every use; a mismatch is refused. The daemon's image store is a rebuildable derivative of the journal. |
   | Run | Write the tree to a private host directory, then `containers/create` (argv from the tool table, `Env` constructed, `WorkingDir /work`, `NetworkMode none`, `NanoCpus`, `CpusetCpus`, `Memory` = `MemorySwap`, `PidsLimit`, read-only root, tmpfs scratch) → `start` → `wait` → `logs` → `remove` |
   | Deadline | The actor kills the container at the allotment's deadline and replies `Exhausted(Time)` |
   | Output | stdout / stderr become blobs; `/work` minus `scratch` is snapshotted into the output tree; the private directory is deleted |
   | Admission | Decision 9. Docker enforces each container's allotment (`CpusetCpus`, `NanoCpus`, `Memory` = `MemorySwap`, `PidsLimit`); it does not admit or queue. |

   *(Amended 2026-09-24: no tree touches a host directory. Run: after
   `containers/create`, the run's tree streams as a canonical tar to
   `PUT /containers/{id}/archive?path=/work`, and each `Mount` tree to its
   declared path, before `start`. Output: `GET /containers/{id}/archive?path=/work`
   returns a tar that decodes into the output tree, minus `scratch`; mount
   paths are never read back, so a step cannot change them in any result.
   A bind mount would need the directory on the daemon's host, which a TCP
   or ssh daemon does not share, and would expose a host path and uid to the
   container. How the archive endpoint treats a tmpfs `scratch` path under
   `/work` is checked in step 0; placing scratch outside `/work` (for example
   `CARGO_TARGET_DIR`) is the fallback. Decision 2's "written out at /work"
   and decision 4's "mounted at `/work`" and "written out in canonical name
   order" read as streamed in, with canonical order fixed by the tar
   encoding.)*

   Other backends (unprivileged namespaces, a cluster scheduler) are other
   actors answering the same `Run` / `RunResult` contract. Programs and the
   driver never learn which backend served a run.

9. **The executor provisions every run.** The actor alone chooses a run's
   cores, memory, and deadline, and admits it against its host:

   | Piece | Mechanism |
   |---|---|
   | Host budget | Actor `Config`: the cores it may hand out (a cpuset list), the memory it may reserve, headroom left for the host |
   | Allotment | A `Config` default for a program never seen. After that, an estimate kept per (program name, environment) from observed peak memory and wall time (an EWMA), times headroom, clamped to the budget. The estimate is executor-local state: rebuildable, never in the journal, never an input to a result. |
   | After exhaustion | The estimate for that key grows (memory doubles, the deadline grows), so a reactor's retry receives more without anyone asking for it |
   | Admission | FIFO. A run starts when its allotment fits the free budget and is pinned to free cores; otherwise it waits. A run is never dropped. An allotment larger than the whole budget is clamped to it. |

   No program can ask for resources. The only resource-shaped field a
   program sets is `Run::network`, and that is a capability grant.

10. **Step 0 is a spike on main's machinery.** Before any workspace code, one
   Sampled program runs a single
   `docker run --rm --network none -v <dir>:/work <image> cargo clippy …`
   through `aether.process`, proving the loop on the build host.

## Consequences

- Proof execution leaves Bloomery entirely. There are no lanes, no rolls,
  and no per-lane janitoring; the only execution state is the actor's
  budget, its per-program estimates, and the daemon's rebuildable image
  store, none of which can change a result.
- Every result names its tree, environment, and platform by digest, so the
  journal proves exactly which tools produced it.
- `aether.workspace` is a new privileged surface. The actor holds a
  root-equivalent socket; the contract's fields are the only way to shape a
  container, and no field passes daemon options through.
- Container start-up (sub-second) and first-use image import are added to
  each proof; both are noise against a multi-minute build.
- A build starts cold in every run. Warm build state is deferred (below);
  this ADR trades speed for correctness first.
- ADR-0229's closed set grows by one member, with its SDK table entry and
  load-time check.

Prerequisites (follow-on issues):

1. **Snapshot and materialize.** A directory → tree operation (imports, and
   the output tree) and a tree → directory operation (writing inputs out),
   in canonical order.

   *(Amended 2026-09-24: one codec instead, a tree ↔ tar stream in
   canonical order: sorted entries, fixed ownership, mode, and mtime. It
   serves the run's input and output (decision 8) and the imports, which
   read a distro tarball directly and a toolchain directory tarred once.
   There is no tree ↔ directory operation, since nothing consumes one.)*
2. **Imports.** A native import path that snapshots a distro tarball and a
   toolchain directory into trees once, and the Pure merge program that
   builds an `Environment` root.

Deferred:

- **Blob bytes out of the SQLite file.** The journal stays one SQLite file:
  imports batch their inserts. Split to a per-journal folder of files
  named by digest only on a measured need (write-lock contention during
  imports, cheap forks, or one toolchain shared between journals), and only
  together with an export / import pair (a database snapshot plus every blob
  it reaches, re-hashed on import) so a journal still ships as one file, and
  a garbage collection that walks the journal's heads through `citations`.
  A blob folder shared between journals additionally needs a registry of
  the journals using it and a lock against writes during a sweep.

  *(Amended 2026-09-25: no longer deferred, and not gated on a measured
  need. Every blob is a digest-named file in the journal root's `blobs`
  directory (ADR-0220). A blob or tree write streams into a temporary file
  that the journal renames to its digest name, and writing a tree out
  streams each file. The
  export / import pair and the garbage collection stay deferred, and a blob
  directory shared between journals is not proposed.)*
- **Shipping a journal is an explicit export.** The journal runs SQLite in
  WAL mode, so a plain copy of the live file can miss committed
  transactions. Export uses `VACUUM INTO` (or the online backup API) to
  produce one consistent file.

  *(Amended 2026-09-25: an export is that snapshot of `journal.sqlite`
  followed by a copy of `blobs/` without `tmp/`. A blob file lands before
  the row that names it commits and is never rewritten, so a copy taken
  after the snapshot holds every blob the snapshot cites.)*

- Warm build state (golden target directories keyed by the tree that built
  them, a shared read-only unpacked environment). Allowed later only as
  rebuildable derivatives that never change a result.
- Container image export for backends that cannot import a tarball.
- A Windows executor, and placement across several hosts.

## Alternatives considered

- **A cluster scheduler first (Kubernetes).** It solves placement across
  hosts, not warm state or isolation semantics; one host does not need it.
  It remains a later backend.
- **Nomad.** Lighter than Kubernetes but the same answer to a question we
  do not have yet, under a restrictive licence.
- **Remote Execution API servers.** Their action / input-root / result shape
  is prior art for decision 2's shape; running their servers adds a gRPC
  stack and a second tree digest format for no gain on one host.
- **Container images as the stored environment format.** Layer tarballs
  dedup per layer, not per file, and would put a second format beside trees
  in the journal. Images are produced at the backend edge instead.
- **A computed dynamic-library closure as the environment.** The build
  compiles C in build scripts (`cc` for `ring`, `zstd-sys`, wasmtime's fiber
  code) and probes `pkg-config`; a closure of the Rust binaries misses the
  compiler, headers, and shell.
- **Unprivileged namespaces as the first backend.** No daemon and no
  start-up cost, but Linux-only and more sandbox code to own; Docker is
  already installed where builds run and gives macOS a local executor.
- **Fixed lanes with warm worktrees.** Long-lived mutable state that must be
  kept in step; the cost this ADR exists to remove.
- **A place-style workspace (open a handle, send commands, close).** Live
  state the journal already holds as data, leaks when a close is missed, and
  cannot be forked at a point in time; a chain of runs over tree digests
  gives the same agent loop.
- **A general Docker actor with the workspace on top.** A root-equivalent
  door any addressable actor could use, with one consumer; the Docker code
  is a private backend instead.

## Open questions

1. **Where blob writes land.** The actor produces stdout, stderr, and the
   output tree's changed files, which can be large. Proposed: leaf blobs are
   written straight into the digest-named blob directory (content-addressed
   writes are idempotent), and tree artifacts, which cite, go through the
   journal so citation edges keep one writer. The driver's existing
   existence check on a `Transition`'s result then holds.

   *(Amended 2026-09-25, resolved: the journal stays the only writer. The
   actor hands every blob and tree to the journal, which writes each blob's
   bytes to a digest-named file in its root's `blobs` directory before the
   row that stores it commits (ADR-0220). The actor writes no file itself,
   so blob files and citation edges keep one writer.)*
2. **Where executor provenance lives.** `Transition.executor` was dropped
   with ADR-0224's native executors, and host identity must not enter a
   Pure result. Proposed: a small provenance record beside the
   `Transition`, outside the result digest.
3. **Crate placement.** The kinds cite `Ref<Tree>` and `OpaqueBytes` from
   `aether-bloomery-kinds`. Proposed: a new `aether-workspace` crate that
   depends on that kinds crate, rather than moving tree kinds into
   `aether-data`.
