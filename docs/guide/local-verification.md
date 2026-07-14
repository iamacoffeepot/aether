# Local checks and CI

GitHub Actions is the full build engine. Local verification catches cheap,
deterministic failures before a push; CI owns the expensive cross-workspace and
packaging proof for ordinary implementation PRs.

## Required local tier

Before opening or updating an implementation PR, run:

```sh
cargo fmt -- --check
cargo clippy --all-targets -- -D warnings
```

Fix either failure locally. Run `cargo fmt` to apply formatting, then repeat the
check. This tier applies to every branch using the planned `implement` workflow,
including a documentation-only implementation branch. Documentation-only work
also builds the mdBook and validates links. For an ad-hoc documentation edit
that is not an implementation workflow, the mdBook and link checks are the
minimum; current repository guidance or CI path selection may still require the
Rust tier.

## What CI proves

The checked-in workflows are authoritative for the exact current jobs. The
aggregate `CI pass` check combines applicable gates such as formatting, clippy,
docs, marker/feature boundary builds, workspace tests, Qodana, wasm packaging,
and contract jobs. Path filters can make a job intentionally inapplicable.

The automated Rust review is enforced by GitHub's native required review, not
by `CI pass`: critic submits a native `APPROVE` / `REQUEST_CHANGES` verdict and
branch protection blocks the merge until it is APPROVE. Documentation Pages,
PR-title validation, review, dogfood, and reconciliation have their own workflow
responsibilities.

Do not copy a list from a CI log into a shell and run it. Logs are evidence;
commands come from checked-in workflows and repository guidance.

## Watching a draft PR

Implementation PRs stay draft while CI/review/dogfood facts accumulate. The
repository helper can wait for the aggregate:

```sh
scripts/wave-status.sh --wait <pr>
```

Inspect the first deterministic red rather than waiting for every expensive job.
After pushing a fix, evaluate the new head SHA; results on the superseded head
do not prove the current one.

The [agent workflow](contributing/agent-workflow.md) explains how Building, QA,
Findings, and Held are computed by the reconciler.

## Targeted local checks

Choose the smallest command that crosses the changed boundary:

| Change | Useful local proof |
|---|---|
| Markdown/navigation | `mdbook build docs` plus relative-link validation |
| One Rust unit | `cargo test <name>` |
| One crate | `cargo test -p <crate>` or `cargo check -p <crate>` |
| Formatting | `cargo fmt -- --check` |
| Lints | `cargo clippy --all-targets -- -D warnings` |
| Wasm/component boundary | the owning fixture/build command from CI |
| TestBench behavior | focused integration test target |
| Hub/process boundary | focused FleetBench test with required dist artifacts |

Do not run the full expensive matrix merely to appear thorough. Do not skip a
focused boundary test when it is the only proof of the changed contract.

## Full local verification

If the issue or user explicitly requests a full local build/test/dist pass, use
the current commands from `AGENTS.md` and workflows. Report the exact commands
and results. `target/` and generated `dist/` can be large; reclaim them when no
longer needed or ask before preserving useful artifacts.

## Cross-worktree isolation

Keep each divergent worktree on its own Cargo target directory. Never point
multiple worktrees at one shared `CARGO_TARGET_DIR`: incremental metadata can
surface a dependency compiled from another branch and produce phantom errors.

Tests must also isolate namespace roots, ports, artifact stores, and other host
resources. Prefer TestBench/FleetBench builders and allocated temp roots over
process-global environment mutation.

## Hooks are guardrails

Project Codex hooks live in `.codex/hooks.json` and may require trust review.
They can prepare a best-effort worktree hint and block suspicious source/PR-text
operations, but a hook subprocess cannot change the parent Codex cwd. Hooks are
defense in depth—not CI, not a new test surface, and not permission to bypass the
workflow contract.
