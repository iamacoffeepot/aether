# CI conventions

What runs here, when it runs, and the rules every workflow follows. The
detailed rationale for any single workflow lives in its own header comment —
each file opens with one.

## Taxonomy

**Merge gates** — the only two required checks in branch protection:

| Workflow | Check | Covers |
| --- | --- | --- |
| `ci.yml` | `CI pass` | fmt, clippy, rustdoc lints, workspace tests, duplicate-code (jscpd), unused-deps (cargo-machete) |
| `pr-title.yml` | `Lint PR title` | Conventional Commit titles (main squash-merges with the PR title as the commit subject) |

**Advisory PR checks** — run on PRs but never block a merge:

| Workflow | Fires on | Purpose |
| --- | --- | --- |
| `docs.yml` | `docs/**` paths | mdBook guide build check; deploys to Pages on main |
| `perf-compare.yml` | substrate-runtime paths or the `perf` label | Noise-aware dispatch perf comparison vs merge-base (ADR-0085); sticky comment |

**Nightlies** — scheduled, off the merge critical path, each also
`workflow_dispatch`-able:

| Workflow | Cron (UTC) | Purpose |
| --- | --- | --- |
| `fuzz-nightly.yml` | 06:17 | Coverage-guided fuzz of the codec / wire targets |
| `desktop-nightly.yml` | 07:37 | Chassis tests on the macOS / Windows matrix |

**On demand:**

| Workflow | Purpose |
| --- | --- |
| `release.yml` | Build and upload the standalone Windows game bundle |
| `transform.yml` | ADR-0149 zero-secret transform worker lane |
| `transform-model.yml` | ADR-0149 BYO-credential model lane (fork-run only) |

**Repo hygiene:**

| Workflow | Purpose |
| --- | --- |
| `issue-labels.yml` | Lints issue titles, auto-applies `type:*` / `crate:*` labels |

## Rules

1. **Two required checks, ever.** Branch protection requires exactly
   `Lint PR title` and `CI pass`. A new merge-gating signal becomes a job
   wired into `ci.yml`'s `ci-pass` aggregator — never a third required
   context. A required context that stops reporting holds every PR at
   "Expected" forever, so the required set stays small and lives in one
   place.
2. **Header comment contract.** Every workflow opens with a comment saying
   what it does and whether it gates merges. A reader should never need the
   Actions tab to understand a file's role.
3. **Least privilege.** Every workflow sets a top-level `permissions:` block
   (normally `contents: read`); a job needing more elevates at job level
   with a comment saying why.
4. **Pinned actions.** Third-party actions are pinned to a full commit SHA
   with a trailing version comment (`# v4`).
5. **Concurrency.** PR-triggered workflows cancel a superseded run when the
   branch is pushed again. Main runs are never cancelled — each merge wants
   its full cache-save and signal — and are grouped by sha so back-to-back
   merges don't serialize.
6. **Skip is a pass.** Heavy Rust jobs key off the `changes` path filter and
   skip on docs-only diffs; `ci-pass` treats a skipped gated job as success.
   The full unconditional suite still runs on every push to main.
7. **Nightlies fail loudly.** A scheduled workflow that finds a problem
   files (or comments on) a single `alert`-labelled triage issue — one issue
   per failure mode, updated in place — rather than counting on someone
   reading the Actions tab. `alert` issues are machine-filed tickets;
   `issue-labels.yml` exempts them from the title lint.
8. **Cron offsets are unique.** Scheduled workflows spread their minute
   fields (`:17`, `:37`, …) so nothing piles onto the same tick; the table
   above is the registry — check it before adding a schedule.
