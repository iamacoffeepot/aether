# Local verification

Two scripts reproduce the CI checks locally before you push.

`scripts/preflight.sh` runs the fast suite — format, clippy, doc build, and
nextest — and on success stamps `.git/aether-preflight-passed` with the HEAD
sha. The pre-push git hook reads that stamp; a re-push of the same commit
short-circuits the whole check. Docs-only and CI-config-only changesets skip
the Rust checks entirely (the script classifies them from the changed-file set).

`scripts/attest.sh` runs a superset: the same checks, each wrapped by `witness`
and signed with your SSH key, producing in-toto attestations the verifier
workflow resolves against your GitHub account instead of re-running the checks
on the CI runner. On success it stamps the same preflight file, so the pre-push
hook treats a passing attest as a passing preflight. Running attest is optional
for most contributors; the CI runner covers the case you skip it.

## Scratch space and `AETHER_ATTEST_BASE`

`scripts/attest.sh` writes three kinds of scratch under a single base directory:

- a persistent `CARGO_TARGET_DIR` (incremental build cache, kept warm across runs)
- a persistent qodana analysis cache (JBR downloads and prior-analysis snapshots)
- a per-run fresh clone of HEAD, `RUNDIR`, removed on exit

All three derive from `AETHER_ATTEST_BASE`, which defaults to `$HOME/.cache`.
On a host where `$HOME` lives on a small root filesystem, one full attest run
overflows the volume. The failure surfaces as `ld: signal 7 (Bus error)` — the
linker hit ENOSPC on an `mmap` — which reads as a compiler crash rather than a
disk-full condition. Pointing `AETHER_ATTEST_BASE` at a larger volume is the
remedy.

There is one constraint on the target: it must be a path Docker can mount,
because qodana runs in a container that mounts the per-run clone. A dedicated
volume on an attached disk or a large mount already available on the host both
work; a tmpfs or an NFS export the container daemon cannot reach would not.

## Per-machine settings via `.env`

`scripts/attest.sh` sources a gitignored `.env` at the main checkout root
before it reads any environment variables, so per-machine settings apply
consistently whether you run the script interactively, from a non-interactive
shell, or from an agent session. The `.env` is resolved through the git
common-dir so it is found from any linked worktree, not just the main
checkout's `$ROOT`.

Create or edit `<main-checkout-root>/.env`:

```sh
AETHER_ATTEST_BASE=/mnt/large-volume/.cache
```

Substitute the path to a filesystem with enough room for build artifacts. On a
fresh machine, a few gigabytes for the Rust target cache plus a few hundred
megabytes for the qodana cache is a reasonable floor; an incremental warm cache
grows as the codebase does.

The `.env` file is gitignored and not sourced by `scripts/preflight.sh` (which
has no comparable scratch cost), so adding the file has no effect on the fast
preflight path.
