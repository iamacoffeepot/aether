# CI conventions

What runs here, when it runs, and the rules every workflow follows. The
detailed rationale for any single workflow lives in its own header comment —
each file opens with one.

## Taxonomy

**Merge gates** — the two verdicts to wait on before landing. `main` has no
branch protection and no required status checks, by the owner's decision, so
nothing enforces them mechanically:

| Workflow | Check | Covers |
| --- | --- | --- |
| `ci.yml` | `CI pass` | fmt, clippy, the component wasm warning gate, rustdoc lints, workspace tests, duplicate-code (jscpd), unused-deps (cargo-machete), Cargo.lock freshness; on pull requests also the new-suppression scan, the reference-mint allowlist, the raw-mailbox ratchet, and those three scanners' regression tests |
| `lint-title.yml` | `Lint title` | Conventional Commit titles (main squash-merges with the title as the commit subject) |

**Advisory checks** — run on pull requests but never block a merge:

| Workflow | Fires on | Purpose |
| --- | --- | --- |
| `docs.yml` | the guide sources (`docs/guide/**`, `docs/book.toml`), the standalone pages bundled into the book, and `docs.yml` itself | mdBook guide build check; deploys to Pages on main |
| `perf-compare.yml` | substrate-runtime paths or the `perf` label | Noise-aware dispatch perf comparison vs merge-base (ADR-0085); sticky comment |

**Nightlies** — scheduled, off the merge critical path, each also
`workflow_dispatch`-able:

| Workflow | Cron (UTC) | Purpose |
| --- | --- | --- |
| `fuzz-nightly.yml` | 06:17 | Coverage-guided fuzz of the codec / wire targets |
| `desktop-nightly.yml` | 07:37 | Chassis tests on the macOS / Windows matrix |

**Security scans** — advisory, never a required check; results land in the
repository's code-scanning tab rather than on a pull request:

| Workflow | Fires on | Purpose |
| --- | --- | --- |
| `codeql.yml` | push to `main`, Mon 03:27 UTC | CodeQL `security-and-quality` over rust / actions / javascript-typescript / python |

Deliberately not on `pull_request`: `ci.yml` is the merge gate, this scan gates
nothing, and source-based Rust extraction over the workspace is slow enough to
be a real per-pull-request cost. A finding a pull request introduces surfaces on
the landing commit instead. The scan is also what *closes* alerts — an alert
whose code is gone stays open until an analysis of the same category runs again
and doesn't find it, so a period with no scheduled scan freezes the tab.

**Release** — the one workflow a `git push` of a tag triggers:

| Workflow | Fires on | Purpose |
| --- | --- | --- |
| `release.yml` | a bare-semver tag (`0.4.0-alpha`), or `workflow_dispatch` | Builds a chassis package per platform and publishes them on the tag's GitHub Release |

`release.yml` is the only workflow carrying a `contents: write` job, and the
only one that publishes an artifact anyone outside the Actions tab can
download. On `ubuntu-latest`, `macos-latest`, and `windows-latest` it builds a
`cargo xtask package` depot from the checked-in demo spec plus one archive per
remaining chassis binary, and attaches every `aether-<version>-<os>-<arch>` /
`aether-<bin>-<version>-<os>-<arch>` archive (`.tar.gz`, `.zip` on Windows) to
a release marked pre-release whenever the version carries a pre-release
suffix. A `workflow_dispatch` run is the dry run: identical build, archives
uploaded as workflow artifacts, no release created. Cutting a release is
therefore bumping `[workspace.package] version`, tagging that version, and
pushing the tag; see
[`docs/guide/building/distribution.md`](../../docs/guide/building/distribution.md).

**On demand:**

| Workflow | Purpose |
| --- | --- |
| `perf-registry.yml` | Replicated real-`Registry` read-scaling + owner-ceiling band on Linux (ADR-0085) |

**Repo hygiene:**

| Workflow | Purpose |
| --- | --- |
| `issue-labels.yml` | Lints issue titles, auto-applies `type:*` / `crate:*` labels |

**Contributor lifecycle:** the tables above are the complete hosted workflow
inventory. Issue scoping, digest-bound approval, implementation, direct review,
dogfood, conflict resolution, and landing are direct-drive repository skills,
not Actions jobs. Their evidence lives in issue bodies, owned branches and
worktrees, draft pull requests, current-head checks/reviews/threads, and dogfood
rollups. A repository script is not hosted behavior unless a checked-in workflow
invokes it.

## Rules

1. **Two verdicts, ever.** `main` has no branch protection and no required
   status checks — the owner keeps it that way — so `CI pass` and `Lint title`
   are the verdicts a landing waits on, not contexts GitHub enforces. A new
   merge-gating signal on the tree becomes a job wired into `ci.yml`'s
   `ci-pass` aggregator — never a third top-level check. One aggregate is what
   keeps the verdict readable in one place, and it is what a required context
   would point at if protection were ever turned on. A future check on
   pull-request metadata rather than the tree follows the `Lint <thing>`
   naming of the title lint (`lint-<thing>.yml`, workflow = job = check name).
2. **Header comment contract.** Every workflow opens with a comment saying
   what it does and whether it gates merges. A reader should never need the
   Actions tab to understand a file's role.
3. **Least privilege.** Every workflow sets a top-level `permissions:` block
   (normally `contents: read`); a job needing more elevates at job level
   with a comment saying why.
4. **Pinned actions.** Third-party actions are pinned to a full commit SHA
   with a trailing version comment (`# v4`).
5. **Concurrency.** Pull-request-triggered workflows cancel a superseded run when the
   branch is pushed again. Main runs are never cancelled — each merge wants
   its full cache-save and signal — and are grouped by sha so back-to-back
   merges don't serialize. The exception is a main run that publishes
   (`docs.yml`): its main runs share one group so an older tree cannot finish
   last and overwrite a newer publish.
6. **Skip is a pass.** Heavy Rust jobs key off the `changes` path filter and
   skip on docs-only diffs; `ci-pass` treats a skipped gated job as success.
   The full unconditional suite still runs on every push to main.
7. **Nightlies fail loudly.** A scheduled workflow that finds a problem
   files (or comments on) a single `alert`-labelled triage issue — one issue
   per failure mode, updated in place — rather than counting on someone
   reading the Actions tab. `alert` issues are machine-filed tickets;
   `issue-labels.yml` exempts them from the title lint.
8. **Cron offsets are unique.** Scheduled workflows spread their minute
   fields (`:17`, `:37`, …) so nothing piles onto the same tick; the nightly
   and security-scan tables above are the registry — check them before
   adding a schedule.
