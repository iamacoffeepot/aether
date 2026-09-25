# Workspace imports

> **Governing ADR:** [ADR-0237](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0237-workspaces-run-steps-over-trees.md)
> (workspaces run steps over trees), decisions 3 and 8. The actor answers
> `Import` today; `Run` follows in #6755.

The `aether.workspace` actor is the Bloomery engine's only route to a container.
It talks to the Docker Engine API through a small client it owns privately, and
everything it produces goes into the journal as trees and blobs. No program,
component, or operator addresses the daemon directly, and there is no general
Docker actor.

The first request is `Import`: pull a digest-pinned image and decode its
filesystem into a stored tree. An environment's base and toolchain layers enter
the journal this way before any run can use them.

## The contract

| Request | Fields | Reply | Arms |
|---|---|---|---|
| `aether.workspace.import` | `image: ImageRef` | `aether.workspace.import_result` | `Ok { tree: Ref<Tree> }`, `Failed { detail: Detail }` |

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

- Only `unix://` with an absolute socket path is accepted, and only on Unix. Any
  other scheme refuses boot naming `AETHER_WORKSPACE_ENDPOINT`. TCP with TLS and
  the Windows named pipe follow in #6721.
- `init` does not dial the daemon, so an engine boots without one; the first
  import that cannot connect answers `Failed`.
- Imports past `max_in_flight` queue and are never dropped. The default of 1
  also keeps two runs from building the same environment image at once.
- A zero import bound refuses boot naming its key.

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
   second import of the same reference answers the same tree. Publishing the
   trees under heads and merging them into an `Environment` is #6720.

What the imported trees hold:

- In the base tree, `usr/bin/cc` is the relative symlink
  `../../etc/alternatives/cc`, and `dev/` holds no device node. It holds only
  what Docker creates in every container: an empty `console` file and empty
  `pts` and `shm` directories, beside an empty `.dockerenv` at the root.
- The toolchain tree holds the toolchain at
  `usr/local/rustup/toolchains/<channel>-<triple>`, which the merge program
  selects, so no rustup proxy enters an environment.

Pins are image digests; a tag beside one only names it for the reader.

- To move the channel, edit `rust-toolchain.toml` and repin
  `toolchain.Dockerfile` to that channel's `rust:<channel>-slim-<release>`
  digest. The build fails while the two disagree, because the pinned image
  would end up holding a second toolchain.
- Keep the base on the Debian release the toolchain image is built on, so
  both trees share one libc.
- `apt-get` output changes from day to day, so a rebuilt base can have a new
  digest. An environment records the imported tree digest, not the recipe.
