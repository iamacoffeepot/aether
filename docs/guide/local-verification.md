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
docs, marker/feature boundary builds, workspace tests, duplicate-code and
unused-dependency checks, wasm packaging, and contract jobs. Path filters can make a job intentionally inapplicable.

The automated Rust review is enforced by GitHub's native required review, not
by `CI pass`: critic submits a native `APPROVE` / `REQUEST_CHANGES` verdict and
branch protection blocks the merge until it is APPROVE. Documentation Pages,
PR-title validation, review, dogfood, and reconciliation have their own workflow
responsibilities.

Do not copy a list from a CI log into a shell and run it. Logs are evidence;
commands come from checked-in workflows and repository guidance.

## Coupling-gap triage loop

`cargo xtask affected` narrows a PR's CI to the workspace graph's
reverse-dependency closure plus hand-injected path rules for couplings the
cargo graph cannot express (`PATH_RULES_TOML` in `xtask/src/affected.rs`). A
path matching nothing already escalates to `run_all`, so the one gap left is
mis-attribution: a changed path that matched some package but has an
additional consumer outside the graph (a runtime-read data file, fixture,
golden file, or env contract in another crate). Pushes to `main` keep the
unconditional full suite as the backstop, so a mis-attribution gap surfaces
as a red full suite on `main` after the merged PR's narrowed CI was green.
That suite runs as three parallel `Test (shard N of 3)` jobs, each executing a
third of the tests, so the red lands on whichever shard drew the failing test
and the other two shards are expected to stay green.

When that happens, work the loop:

1. Discriminate the red's shape first. `main` also goes red from timeouts —
   the full builds run long — so the signal is noisy. A **deterministic test
   or compile failure in a package the PR did not select** is the coupling
   signature; a timeout or infra red is not. Treat a recurring timeout red as
   its own `type:flake` defect rather than background noise — a streak of
   them can mask a real coupling red behind it, and "main is just red again"
   is exactly how a gap survives.
2. Identify which changed path should have selected which package. The
   failing test names the consumer.
3. Add one `[[path-rule]]` block (`globs` → `mark-changed`) to
   `PATH_RULES_TOML`, mirroring the existing `approval-policy.yml` →
   `aether-chassis-bloomery` rule, with a comment naming the coupling. The rule
   PR self-validates: `xtask/` is in `RUN_ALL_PREFIXES`, so it runs the full
   suite.
4. Land the rule fix alongside (or before) the breakage fix so the selection
   gap closes with the incident.

Two future options worth a sentence each, not yet done: the Test job already
computes the narrowed set, so persisting it (a run artifact or output line)
would let a `main` red auto-correlate against a recently merged PR's
unselected set, turning this triage mechanical. And if the rule count grows
past a handful, revisit centralized rules vs. crate-local declaration (e.g.
`[package.metadata.affected]` extra-paths read by `xtask`) as a deliberate
design decision.

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
| Doc comments and intra-doc links | `cargo doc --workspace --no-deps --document-private-items` |
| Wasm/component boundary | the owning fixture/build command from CI |
| SubstrateHarness behavior | focused integration test target |
| Hub/process boundary | focused FleetHarness test with required dist artifacts |

The rustdoc row carries `--document-private-items` because rustdoc resolves
intra-doc links only on items it documents. Without the flag a link between two
private items is never resolved, so moving a private item across a module
boundary can dangle every link that named it and no gate notices. CI's `Rustdoc`
job runs the same command under `RUSTDOCFLAGS='-D rustdoc::…'`, so keep the two
identical. One gap remains under the flag: `#[cfg(test)]` modules are not
documented at all, and a doc link inside one stays unchecked.

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
resources. Prefer SubstrateHarness/FleetHarness builders and allocated temp roots over
process-global environment mutation.

## Hooks are guardrails

Project Codex hooks live in `.codex/hooks.json` and may require trust review.
They can prepare a best-effort worktree hint and block suspicious source/PR-text
operations, but a hook subprocess cannot change the parent Codex cwd. Hooks are
defense in depth—not CI, not a new test surface, and not permission to bypass the
workflow contract.
