---
name: review
description: Pre-land review of a code change against five judgment pillars that mechanical gates (clippy/rustc/Qodana/fmt) cannot decide — spec fidelity (asked-vs-changed delta), correctness (named bug-shapes), test integrity (does the test catch an owned bug), economy (fewest chars that still make sense), and convention/architecture (stated rules + ADR conformance). Returns an issue-ready rollup with soft-hold flags on high-severity correctness/spec findings; never gates CI, never touches GitHub. Integrated review of a PR-bound change runs in CI on explicit request — the implement box dispatches the review action once CI is green, and an @barista review comment re-requests it — so do not invoke /review inline at the end of /implement. Invoke manually for backfill mode (auditing existing whole files per crate) or for a change that never becomes a PR. Distinct from the global /code-review skill — this wraps the repo's five-pillar review workflow.
---

# /review — five-pillar pre-land review

The Claude entry point to the `review` workflow (`.claude/workflows/review.js`). Where the workflow holds the multi-agent orchestration — a cheap whole-PR scope filter, then per-file specialist finders, then a two-sided verify funnel — this skill is the thin invocation surface around it: it resolves the caller-side file set the workflow sandbox cannot, selects integrated or backfill mode, invokes the workflow in a single pass, and reports the returned soft-hold rollup. The mirror of the Codex wrapper at `.agents/skills/review/SKILL.md`, invoking the same workflow through the `Workflow` tool.

**Distinct from the global `/code-review` skill.** `/code-review` is a separately-owned tool that reviews the working diff; `/review` here wraps this repo's five-pillar `review.js` workflow. When you mean the repo workflow, use `/review`.

`/review` never touches GitHub, never gates CI, and files nothing — surfacing confirmed findings as follow-up issues and clearing soft-holds are separate human-gated steps.

## Invocation

```
/review                        integrated — review the current change against its issue
/review --backfill <crate|path>   whole-file audit of existing code, no issue (sharded per crate)
```

- **Integrated mode** runs when an issue and its diff are in hand — the spec-fidelity lens runs (the asked-vs-changed delta). For a PR-bound change this mode runs in CI on explicit request: the implement box dispatches the review action (`.github/workflows/review.yml`) once the PR's CI is green (re-review is an `@barista review` PR comment), and it posts the rollup as PR annotations plus a native barista `APPROVE` / `REQUEST_CHANGES` verdict, so an inline invocation at the end of `/implement` duplicates the pass. Invoke it directly only for a change that never becomes a PR.
- **Backfill mode** runs against a crate or path's whole-file set with no issue — the spec lens does not run; the other four pillars audit existing code.

## Inputs

The skill assembles the workflow's arg contract (`{issue?, files, testFiles?, diffs?, diffBase?, reviewMode?}`, grounded against `.claude/workflows/review.js`):

- `files` — a non-empty array of absolute `.rs` paths for the code lenses (correctness, economy, convention).
- `testFiles` — absolute `.rs` paths for the test-integrity lens.
- `issue` — the issue or scope text for the spec-fidelity lens. Omit for backfill; its presence is what selects integrated mode.
- `diffs` — per-file diff hunks keyed by path, so the finders judge the change rather than the whole file (integrated mode).
- `noBuild` — passed through to the workflow args; set true in CI so the correctness refuter grounds read-only.
- `depth` — `gate` is the light per-PR gate (correctness + spec fidelity, Sonnet verify, no challenge); `deep` is the default full five-pillar review.
- `diffBase` — the diff base ref the caller reviewed against, passed through to the workflow. `origin/main` on a full review (the merge-base) and the last-reviewed SHA on an incremental one, matching `base` below. The high-severity correctness refuter reads the flagged site on this base to classify a bug's provenance — pre-existing on the base becomes an advisory follow-up, never an in-PR demand.
- `reviewMode` — `full` or `incremental` (default `full`). Gates the provenance classification: it fires only on a full review, where `diffBase` is the merge-base. On incremental the base is the PR's own prior head, so a bug an earlier commit of this PR introduced would read as pre-existing — provenance is therefore not established there.

## Caller-side prep

The workflow sandbox cannot run `git` or `grep`, so the skill resolves the file set before invoking it:

1. **Integrated** — resolve the changed files and their per-file diffs from the branch against `origin/main` (`git diff --name-only origin/main...HEAD` for the `.rs` set; `git diff origin/main...HEAD -- <file>` per file for `diffs`). Split test files (`tests/`, `#[cfg(test)]`-heavy) into `testFiles`. Read the issue body as `issue`. Pass that base as `diffBase` and `full` as `reviewMode` so the workflow can classify a flagged correctness bug's provenance against the merge-base — CI's `review.yml` reviews every request this same full way and threads both from its resolve step.
2. **Backfill** — resolve the crate's whole-file `.rs` set (`git ls-files -- <crate>/src '*.rs'`), shard per crate to keep each run bounded, and pass no `issue`.

The workflow reads source itself; there is no live MCP harness precondition (unlike `/dogfood`, `/review` does not drive a running engine).

## Workflow handoff

Invoke the workflow with the assembled args, in a single pass — there is no approval gate:

```
Workflow({name: "review", args: {issue, files, testFiles, diffs}})
```

## Rollup report

On completion the workflow returns `{rollup, files}`. Report the rollup to the user:

- **confirmed findings** — grouped by file then pillar, each with a file/line reference, the current form, the suggested action, and the rationale. Ordered most-severe first.
- **soft-holds** — high-severity spec-fidelity or correctness findings. Advisory only — `/review` never blocks a land; the soft-hold is a flag for the reviewer to clear.
- **lint candidates** — mechanically-decidable observations seeded for a `clippy.toml` / custom lint / `check-*.sh` rule, kept out of the judgment rollup.
- **spared / uncertain** — findings refuted in the verify funnel, or ones whose relevant code could not be read.

## What `/review` does NOT do

- File follow-up issues. Confirmed findings are triaged via a separate `/sketch` step — the user's call which.
- Clear soft-holds. A high-severity spec/correctness soft-hold is resolved by a human before un-draft, not by this skill.
- Touch GitHub or gate CI. The review is advisory; the rollup is the surface.
- Reimplement the orchestration. The `review.js` workflow is the single source of the Scope → Find → Verify logic; this skill only invokes it.
- Duplicate `/code-review`. That is a separate global tool; `/review` wraps the repo's five-pillar workflow.
