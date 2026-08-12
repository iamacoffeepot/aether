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

In a multi-worktree checkout with `sccache` installed, run those commands
through `scripts/cargo-cached.sh` to reuse compiled dependencies without
sharing build outputs:

```sh
scripts/cargo-cached.sh fmt -- --check
scripts/cargo-cached.sh clippy --all-targets -- -D warnings
```

`scripts/cargo-cached.sh` always uses the current worktree's `target/`
directory and disables Cargo incremental compilation. It deliberately
overrides ambient `CARGO_TARGET_DIR`, `RUSTC_WRAPPER`, and `CARGO_INCREMENTAL`
for that command. The cache is compiler-level only; never configure multiple
divergent worktrees to share a target directory.

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
unused-dependency checks, wasm packaging, and contract jobs. Path filters can
make a job intentionally inapplicable.

The pull-request-only `New suppressions` job is deliberately outside those
filters. It examines additions between the pull request's resolved merge base
and head and rejects four forms: line-anchored Rust `allow(...)` or
`expect(...)` attributes, Rust `#[ignore]` attributes, new members of the
top-level `.jscpd.json` `ignore` array, and new members of
`package.metadata.cargo-machete.ignored`. The standing suppression population,
removals, exact renames, comments, strings, and unrelated JSON/TOML keys do not
fail the diff. Each finding is printed as `file:line — token — added line`.

The pull-request checkout contains proposed code, so CI does not execute the
scanner from that checkout. It materializes `scripts/check-suppressions.py`
from the event's exact base commit into runner-temporary storage and runs that
trusted blob against the explicit base and head refs. The only fallback to the
head blob is the bootstrap case where the event base does not contain the
newly introduced scanner at all; after this gate lands, a later pull request
cannot make its suppressions pass by weakening the scanner in the same diff.

Run the same mechanical scan locally with:

```sh
python3 scripts/check-suppressions.py
```

It defaults to the merge base of `origin/main` and `HEAD`; `--base` and
`--head` select explicit refs. The `verify.suppress` transform runs that exact
command, and it is a member of `verify.check`. A finding is a typed
`verify.suppress` verifier failure like any other member, and the accounting is
per member: forgiven the first time a member sees it, charged a repair roll on
every later occurrence for that same member, and wedging the member with
`repeated_verifiers = {verify.suppress}` if it keeps repeating. Replacing the
candidate does not reset that memory (ADR-0178).
Bloomery has no pull-request owner context, so the sign-off path below is
closed to it and the lane re-enters `Refine` — the only way a candidate clears
the finding is to remove the suppression.

A repository owner can sign off an intentional pull-request suppression only
by editing the pull request's main body so it contains exactly one canonical
hidden record:

```text
<!-- aether-suppression-signoff:v1 {"base_sha":"<40 lowercase hex>","head_sha":"<40 lowercase hex>","pull_request":123} -->
```

The scanner verifies the current body and latest body editor through GitHub
GraphQL. The latest editor must be the repository owner, and the record must
bind the current pull-request number, resolved merge base, and head exactly.
An agent-authored initial body is not authorization. A push, base change, or
later non-owner body edit invalidates a previous sign-off until the owner edits
the current body again. The sign-off changes only the exit status: findings
remain in the job log.

Branch protection currently requires two status checks: `CI pass` and `Lint
title`. It does not configure required pull-request reviews. `CI pass` proves
the applicable tree checks; it does not prove direct inspection, dogfood, or
lifecycle readiness. Those are separate direct-drive facts: the implementer
directly inspects and repairs the exact current-head diff and appends a hidden
`aether-direct-review:v2` record to the closing issue body. That canonical line
binds the issue, pull request, current head, current Plan digest, and verdict;
its trust comes from effective owner/member/collaborator body-editor provenance.
A push or managed-Plan change makes it stale. Pull-request reviews and comments
remain ordinary human prose and never carry a machine JSON/HTML review marker.
Native change requests and review threads stay independent blockers, and the
scoped dogfood trial still runs when the issue body calls for one. Landing
independently re-reads those facts before clearing draft state.

Do not copy a list from a CI log into a shell and run it. Logs are evidence;
commands come from checked-in workflows and repository guidance.

## Affected-code test run

For a local reproduction of the pull-request test selection, run:

```sh
cargo xtask affected --run
```

The command computes the same conservative selection as PR CI, prints that
selection, and then runs it. An empty selection succeeds without spawning a
test command. A narrowed selection runs xtask's affected-selection invariants,
then the selected packages with the CI nextest profile. Selections that need
runtime artifacts pre-build them with `cargo xtask dist`; a `run_all` result
pre-builds artifacts and runs the one-shard workspace nextest equivalent.

`--run` requires `cargo-nextest`; selections that pre-build artifacts also
require the `wasm32-unknown-unknown` Rust target. The test suite runs with the
runtime requirement and isolated in-memory store used by the repository's
verification lane, so a missing runtime artifact fails rather than silently
skipping a scenario.

This is an additive test-execution shortcut. It does not replace the required
full-workspace formatting and clippy tier above, and it does not narrow compile
coverage.

## Coupling-gap triage loop

`cargo xtask affected` narrows a PR's CI to the workspace graph's
reverse-dependency closure plus hand-injected path rules for couplings the
cargo graph cannot express (`PATH_RULES_TOML` in `xtask/src/affected/rules.rs`). A
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

Implementation PRs stay draft while their required facts accumulate. The
repository helper can wait for the checked-in CI aggregate:

```sh
scripts/wave-status.sh --wait <pr>
```

Inspect the first deterministic red rather than waiting for every expensive job.
After pushing a fix, evaluate the new head SHA; results on the superseded head
do not prove the current one.

The [agent workflow](contributing/agent-workflow.md) explains how Plan,
approval, owned implementation artifacts, current-head proof, and explicit
landing compose without synthetic lifecycle state.

## Targeted local checks

Choose the smallest command that crosses the changed boundary:

| Change | Useful local proof |
|---|---|
| Markdown/navigation | `mdbook build docs` plus relative-link validation |
| One Rust unit | `cargo test <name>` |
| One crate | `cargo test -p <crate>` or `cargo check -p <crate>` |
| Added-suppression diff | `python3 scripts/check-suppressions.py` |
| Formatting | `cargo fmt -- --check` |
| Lints | `cargo clippy --all-targets -- -D warnings` |
| Doc comments and intra-doc links | `cargo doc --workspace --no-deps --document-private-items --all-features --keep-going` |
| Wasm/component boundary | the owning fixture/build command from CI |
| SubstrateHarness behavior | focused integration test target |
| Hub/process boundary | focused FleetHarness test with required dist artifacts |

The rustdoc row carries `--document-private-items` because rustdoc resolves
intra-doc links only on items it documents. Without the flag a link between two
private items is never resolved, so moving a private item across a module
boundary can dangle every link that named it and no gate notices. The row also
carries `--all-features` because a module behind a non-default feature is
otherwise never compiled by `cargo doc`, so a broken or private link inside it
is never resolved either — the feature-gated module has to actually build
before rustdoc can look at it. CI's `Rustdoc` job runs the same command under
`RUSTDOCFLAGS='-D rustdoc::…'`, so keep the two identical.

`--all-features` moves the feature blind spot rather than removing it. With
every feature on, an item gated `#[cfg(not(feature = "…"))]` is the one
compiled out, so its docs are the ones that go unchecked — the mirror image of
the feature-on modules the flag reaches, over a much smaller surface (fallback
stubs). A run without the flag has the blind spot on the other side; no single
feature selection has none. `#[cfg(test)]` modules sit outside the trade
entirely: rustdoc does not document them at all, so a doc link inside one stays
unchecked under any feature set.

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
Use `scripts/cargo-cached.sh` when available to share compiler results through
sccache, not target artifacts.

Tests must also isolate namespace roots, ports, artifact stores, and other host
resources. Prefer SubstrateHarness/FleetHarness builders and allocated temp roots over
process-global environment mutation.

## Hooks are guardrails

Project Codex hooks live in `.codex/hooks.json` and may require trust review.
They can prepare a best-effort worktree hint and block suspicious source/PR-text
operations, but a hook subprocess cannot change the parent Codex cwd. Hooks are
defense in depth—not CI, not a new test surface, and not permission to bypass the
workflow contract.
