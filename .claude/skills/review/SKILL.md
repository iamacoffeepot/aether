---
name: review
description: Pre-land review of a code change against five judgment pillars that mechanical gates (clippy/rustc/Qodana/fmt) cannot decide — spec fidelity (asked-vs-changed delta), correctness (named bug-shapes), test integrity (does the test catch an owned bug), economy (fewest chars that still make sense), and convention/architecture (stated rules + ADR conformance). Returns an issue-ready rollup with soft-hold flags on high-severity correctness/spec findings; never gates CI, never touches GitHub. Integrated review of a PR-bound change runs in CI on explicit request — the implement box dispatches the review action once CI is green, and an @iamacritic review comment re-requests it — so do not invoke /review inline at the end of /implement. Invoke manually for backfill mode (auditing existing whole files per crate) or for a change that never becomes a PR. Distinct from the global /code-review skill — this wraps the repo's five-pillar review workflow.
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

- **Integrated mode** runs when an issue and its diff are in hand — the spec-fidelity lens runs (the asked-vs-changed delta). For a PR-bound change this mode runs in CI on explicit request: the implement box dispatches the review action (`.github/workflows/review.yml`) once the PR's CI is green (re-review is an `@iamacritic review` PR comment), and it posts the rollup as PR annotations plus a native critic `APPROVE` / `REQUEST_CHANGES` verdict, so an inline invocation at the end of `/implement` duplicates the pass. Invoke it directly only for a change that never becomes a PR.
  - **Deep vs confirm (issue #3390)** — a PR gets at most one **deep** pass and cheap **confirm** re-passes, so re-reviews converge instead of re-sampling the whole diff's nit distribution every round. `review.yml`'s resolve step decides: a request is `reviewPass: deep` on a `workflow_dispatch`, an `@iamacritic full review`, or when critic carries no standing verdict yet (the first review); it is `reviewPass: confirm` on a plain `@iamacritic review` once critic has a standing verdict, and the diff base becomes that verdict's last-reviewed SHA rather than `origin/main`. The **deep** pass is the full five-pillar fan-out below and runs **at most once per PR** — "needs a full re-review" is never grounds for a second deep pass; it is the restart/bounce signal. The **confirm** pass is a single cheap verifier session over `{priorFindings, delta}` that checks only whether the reported findings were addressed since the deep pass: it emits **no new ordinary findings** (a nit on code that did not change is the deep reviewer's miss, rendered as a non-blocking advisory at most), re-asserts each still-open prior finding unchanged (same fingerprints, so the finding set is frozen across rounds), and terminally either APPROVEs or re-asserts `REQUEST_CHANGES`. `reviewPass` is orthogonal to `reviewMode` (that axis is provenance-only).
  - **Light profile** — the CI gate runs a *light-profile* integrated review over a PR whose diff carries no Rust but touches the fleet's own automation surface (`.github/workflows/**`, `.github/actions/**`, `scripts/**`), where a logic/liveness/state-accounting bug in bash-inside-YAML parses fine and leaves unrelated CI green. It is the same integrated mode with a narrower selection: the caller resolves the reviewable set under those three globs (not `.rs`), and invokes with `lenses: ['correctness', 'convention']`, `noBuild: true`, `depth: gate`, and the same `diffBase`/`reviewMode` threading. So it runs only the correctness and convention pillars, with the cargo-oriented correctness refuter off (`noBuild`) — the pillars judge logic/control-flow/state-accounting invariants in the diff's actual language (shell/YAML/JS) rather than Rust-anchored hazards. A docs/ADR/`.md`/manifest-only PR carries no reviewable surface and is not reviewed at all (the gate auto-approves it out of scope); a Rust-touching PR gets the full five-pillar review, not this profile.
- **Backfill mode** runs against a crate or path's whole-file set with no issue — the spec lens does not run; the other four pillars audit existing code.

## Inputs

The skill assembles the workflow's arg contract (`{issue?, files, testFiles?, diffs?, diffBase?, reviewMode?}`, grounded against `.claude/workflows/review.js`):

- `files` — a non-empty array of absolute reviewable-source paths for the code lenses (correctness, economy, convention). `.rs` paths for the Rust and backfill profiles; the `.github/workflows/**`, `.github/actions/**`, `scripts/**` set for the light profile.
- `testFiles` — absolute `.rs` paths for the test-integrity lens.
- `issue` — the issue or scope text for the spec-fidelity lens. Omit for backfill; its presence is what selects integrated mode.
- `diffs` — per-file diff hunks keyed by path, so the finders judge the change rather than the whole file (integrated mode).
- `noBuild` — passed through to the workflow args; set true in CI so the correctness refuter grounds read-only.
- `depth` — `gate` is the light per-PR gate (correctness + spec fidelity, Sonnet verify, no challenge); `deep` is the default full five-pillar review.
- `diffBase` — the diff base ref the caller reviewed against, passed through to the workflow. `origin/main` on a full review (the merge-base) and the last-reviewed SHA on an incremental one, matching `base` below. The high-severity correctness refuter reads the flagged site on this base to classify a bug's provenance — pre-existing on the base becomes an advisory follow-up, never an in-PR demand.
- `reviewMode` — `full` or `incremental` (default `full`). Gates the provenance classification: it fires only on a full review, where `diffBase` is the merge-base. On incremental the base is the PR's own prior head, so a bug an earlier commit of this PR introduced would read as pre-existing — provenance is therefore not established there.
- `reviewPass` — `deep` or `confirm` (default `deep`; issue #3390). `deep` is the full five-pillar fan-out; `confirm` skips the fan-out and runs one verifier over `{priorFindings, delta}`, emitting no new ordinary findings. Orthogonal to `reviewMode`.
- `priorFindings` — confirm pass only: the findings critic's standing verdict already reported, resolved caller-side from the PR (see Caller-side prep). Each is `{ file, line, pillar, category, symbol, severity, suggested_form, gate }`; the confirm session judges each against the delta and re-asserts the still-open ones unchanged.

## Caller-side prep

The workflow sandbox cannot run `git` or `grep`, so the skill resolves the file set before invoking it:

1. **Integrated** — resolve the changed files and their per-file diffs from the branch against `origin/main` (`git diff --name-only origin/main...HEAD` for the `.rs` set; `git diff origin/main...HEAD -- <file>` per file for `diffs`). Split test files (`tests/`, `#[cfg(test)]`-heavy) into `testFiles`. Read the issue body as `issue`. Pass that base as `diffBase` and `full` as `reviewMode` so the workflow can classify a flagged correctness bug's provenance against the merge-base — CI's `review.yml` reviews every request this same full way and threads both from its resolve step. For the **light profile** (the caller signals it — CI's gate does so on a no-Rust workflow/action/script diff), resolve the reviewable set under `.github/workflows/**`, `.github/actions/**`, and `scripts/**` instead of the `.rs` set (no `testFiles`), and add `lenses: ['correctness', 'convention']` and `noBuild: true` to the args so only those two pillars run with the cargo refuter off.
2. **Backfill** — resolve the crate's whole-file `.rs` set (`git ls-files -- <crate>/src '*.rs'`), shard per crate to keep each run bounded, and pass no `issue`.
3. **Confirm pass** (issue #3390; the caller signals it — CI's resolve step does so on a plain `@iamacritic review` with a standing verdict) — resolve two things instead of the full changed-file set:
   - **priorFindings** — read the PR's critic verdict bodies, its inline review comments, and the `<!-- aether-review -->` summary comment. Each reported finding carries an `aether-review-fp:PATH|LINE|PILLAR` marker beside its rendered text (`**pillar/category** (severity) — symbol recommendation: suggested_form`). Parse each into `{ file, line, pillar, category, symbol, severity, suggested_form, gate }` (`gate: 'soft-hold'` under a soft-hold section, else `'advisory'`), deduplicating by `PATH|LINE|PILLAR`.
   - **delta** — the changed files and per-file hunks since the last-reviewed SHA (`git diff --name-only <diffBase>...HEAD`, `git diff <diffBase>...HEAD -- <file>`), where `diffBase` is that SHA, not `origin/main`.
   Invoke with `reviewPass: 'confirm'`, `priorFindings`, `diffs` (the delta), `diffBase`, and `files` (the delta's changed paths). The workflow runs a single verifier — no fan-out, no new ordinary findings.

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

On a **confirm pass** the rollup carries `reviewPass: 'confirm'`, the still-open prior findings as `confirmed` (re-asserted unchanged), advisories as `followUps`, a `restart` flag, and — when the restart escalates — a `bounce`. The poster (`scripts/post-review-rollup.mjs`) maps it inside ADR-0148's two native outcomes: all addressed and no restart → `APPROVE`; a still-open finding → re-assert the standing `REQUEST_CHANGES` with the same fingerprints (no new findings, no flip); a raised **restart signal** (the delta needs restart-level rework) → the **bounce** outcome below.

### The bounce outcome (issue #3391)

Bounce is a third **terminal** review outcome beside the two native verdicts, for a change that is wrong at the root — a fundamental design flaw or major security defect found by the deep pass, or a confirm pass judging the delta needs restart-level rework. It is not another `REQUEST_CHANGES` round: it sends the work back to re-scoping. Because GitHub's review API has no bounce verb (ADR-0148's [amendment](../../../docs/adr/0148-native-required-review-merge-gate.md)), it is carried **out-of-band**:

- **Signal.** `review.js` normalizes both sources into a single `rollup.bounce = { to, reason }` (`to` = `design` — the default, a fundamental design/security flaw — or `plan` for a scope-level miss). The deep pass raises it as the spec-fidelity agent's whole-PR conclusion; the confirm pass raises it from the restart escalation. `bounce` is `null` in the common case.
- **Stamp.** The poster's `bounceSignal(rollup)` reads it and — beside critic's native `REQUEST_CHANGES`, which keeps the PR merge-blocked — stamps `review:bounce` + `review:bounce-to:<phase>` on the PR and posts the reviewer's reason. `verdictEvent` stays the total two-outcome function ADR-0148 pins; bounce never becomes a third verdict value.
- **Edge.** `reconciler.yml` reads the PR's `review:bounce` label on its next poke and regresses the **linked issue** to `phase:bounced` + `bounce-to:<phase>` (the same mechanism `/bounce` uses), records the reason on the issue, and closes the bounced PR — the work restarts at scoping and a fresh `/implement` supersedes the PR. This replaces #3390's interim `agent:awaiting-answer` ask-and-park with the real phase-regression edge.

## What `/review` does NOT do

- File follow-up issues. Confirmed findings are triaged via a separate `/sketch` step — the user's call which.
- Clear soft-holds. A high-severity spec/correctness soft-hold is resolved by a human before un-draft, not by this skill.
- Touch GitHub or gate CI. The review is advisory; the rollup is the surface.
- Reimplement the orchestration. The `review.js` workflow is the single source of the Scope → Find → Verify logic; this skill only invokes it.
- Duplicate `/code-review`. That is a separate global tool; `/review` wraps the repo's five-pillar workflow.
